//! Process lifecycle ownership and resumable state.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::ArcVec;
use tascarrel_api::types::pods::PodId;
use tascarrel_api::types::processes as api;
use tascarrel_api::types::protocol::Actor;
use tascarrel_api::types::store as store_api;
use tascarrel_protocol::ExecRequest;
use tascarrel_protocol::OutputStream;
use tascarrel_protocol::PodId as RuntimePodId;
use tascarrel_protocol::Signal;
use tascarrel_protocol::TerminalSize;
use tascarrel_store::Store;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::warn;

use super::log::LineDecoder;
use super::log::LogBuffer;
use super::log::LogSubscription;
use super::terminal::TerminalBuffer;
use super::terminal::TerminalSubscription;
use crate::Executor;
use crate::GuestNetworkService;
use crate::runtime::process::ExecControl;
use crate::runtime::process::ObservedProcessEvent;
use crate::runtime::process::run_observed;
use crate::services::pods::EphemeralPod;
use crate::services::pods::PodService;

/// In-memory process supervisor shared by every control-plane connection.
pub struct ProcessSupervisor {
    inner: Arc<SupervisorInner>,
}

impl Clone for ProcessSupervisor {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl ProcessSupervisor {
    /// Creates an empty process supervisor.
    #[must_use]
    pub fn new(executor: Executor, config: ProcessSupervisorConfig) -> Self {
        let store = Store::new(
            api::ProcessList {
                processes: ArcVec::new(),
            },
            reduce_process_list,
            config.store_history_limit,
        );
        Self {
            inner: Arc::new(SupervisorInner {
                executor,
                config,
                processes: Mutex::new(BTreeMap::new()),
                store,
            }),
        }
    }

    /// Returns the current process entries belonging to one pod.
    #[must_use]
    pub(crate) fn get_pod_processes(&self, pod_id: &PodId) -> api::GetPodProcessesOutput {
        let processes = self
            .inner
            .store
            .snapshot()
            .value
            .processes
            .iter()
            .filter(|process| process.pod_id == *pod_id)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        api::GetPodProcessesOutput { processes }
    }

    /// Admits a process and starts its asynchronous supervision task.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessSupervisorError::InvalidRequest`] when the process
    /// specification is malformed.
    #[tracing::instrument(level = "debug", skip_all, fields(pod_id = %input.pod_id.0))]
    pub fn spawn(
        &self,
        input: api::SpawnProcessAction,
        started_by: Actor,
        pods: &PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> Result<api::SpawnProcessOutput, Report<ProcessSupervisorError>> {
        let process_id = self.admit(
            input,
            ProcessCommand::Specified,
            started_by,
            ProcessOperationServices {
                pods: pods.clone(),
                network_service: Some(network_service),
            },
            None,
            None,
            None,
        )?;
        Ok(api::SpawnProcessOutput { process_id })
    }

    /// Launches the pod's effective login shell as a visible terminal process.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessSupervisorError::InvalidRequest`] when the terminal
    /// dimensions or resulting process specification are malformed.
    #[tracing::instrument(level = "debug", skip_all, fields(pod_id = %input.pod_id.0))]
    pub fn spawn_terminal(
        &self,
        input: api::SpawnProcessTerminalAction,
        started_by: Actor,
        pods: &PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> Result<api::SpawnProcessTerminalOutput, Report<ProcessSupervisorError>> {
        let process_input = api::SpawnProcessAction {
            pod_id: input.pod_id,
            start_pod: input.start_pod,
            title: input.title,
            executable: "".into(),
            arguments: ArcVec::new(),
            environment: HashMap::new(),
            working_directory: None,
            terminal: Some(input.terminal),
            log_stdout: None,
            profile: api::ProcessExecutionProfile::User,
        };
        let process_id = self.admit(
            process_input,
            ProcessCommand::LoginShell,
            started_by,
            ProcessOperationServices {
                pods: pods.clone(),
                network_service: Some(network_service),
            },
            None,
            None,
            None,
        )?;
        Ok(api::SpawnProcessTerminalOutput { process_id })
    }

    /// Runs one hidden privileged setup script inside an internal pod.
    ///
    /// The process is supervised and logged like an ordinary process, but it
    /// is not emitted through the public process list or log APIs.
    pub(crate) fn spawn_setup(
        &self,
        pods: &PodService,
        pod: &EphemeralPod,
        title: impl Into<tascarrel_api::ArcStr>,
        script: impl Into<tascarrel_api::ArcStr>,
    ) -> Result<InternalProcess, Report<ProcessSupervisorError>> {
        let input = api::SpawnProcessAction {
            pod_id: pod.id().clone(),
            start_pod: Some(false),
            title: title.into(),
            executable: pod.setup_shell().to_string_lossy().into_owned().into(),
            arguments: vec!["-eu".into(), "-c".into(), script.into()].into(),
            environment: HashMap::new(),
            working_directory: Some("/workspace".into()),
            terminal: None,
            log_stdout: None,
            profile: api::ProcessExecutionProfile::SystemService,
        };
        let (output_sender, output) = self.internal_output();
        let (completed_sender, completed) = oneshot::channel();
        let process_id = self.admit(
            input,
            ProcessCommand::Specified,
            Actor::Host,
            ProcessOperationServices {
                pods: pods.clone(),
                network_service: None,
            },
            Some(pod.clone()),
            Some(output_sender),
            Some(completed_sender),
        )?;
        Ok(InternalProcess {
            process_id,
            output,
            completed,
        })
    }

    /// Runs one hidden user command inside an already-running durable pod.
    pub(crate) fn spawn_internal_user(
        &self,
        pods: &PodService,
        pod_id: PodId,
        title: impl Into<tascarrel_api::ArcStr>,
        executable: impl Into<tascarrel_api::ArcStr>,
        arguments: Vec<tascarrel_api::ArcStr>,
    ) -> Result<InternalProcess, Report<ProcessSupervisorError>> {
        let input = api::SpawnProcessAction {
            pod_id,
            start_pod: Some(false),
            title: title.into(),
            executable: executable.into(),
            arguments: arguments.into(),
            environment: HashMap::new(),
            working_directory: Some("/workspace".into()),
            terminal: None,
            log_stdout: None,
            profile: api::ProcessExecutionProfile::User,
        };
        let (output_sender, output) = self.internal_output();
        let (completed_sender, completed) = oneshot::channel();
        let process_id = self.admit(
            input,
            ProcessCommand::Specified,
            Actor::Host,
            ProcessOperationServices {
                pods: pods.clone(),
                network_service: None,
            },
            None,
            Some(output_sender),
            Some(completed_sender),
        )?;
        Ok(InternalProcess {
            process_id,
            output,
            completed,
        })
    }

    /// Starts one visible process whose protocol I/O remains owned by guestd.
    pub(crate) fn spawn_owned(
        &self,
        input: api::SpawnProcessAction,
        pods: &PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> Result<OwnedProcess, Report<ProcessSupervisorError>> {
        let (output_sender, output) = mpsc::channel(self.inner.config.output_queue_capacity.get());
        let process_id = self.admit(
            input,
            ProcessCommand::Specified,
            Actor::Host,
            ProcessOperationServices {
                pods: pods.clone(),
                network_service: Some(network_service),
            },
            None,
            Some(output_sender),
            None,
        )?;
        let processes = lock(&self.inner.processes);
        let managed = processes.get(&process_id).ok_or_else(|| {
            internal_error("owned process disappeared immediately after admission")
                .field_display("process_id", &process_id.0)
        })?;
        Ok(OwnedProcess {
            process_id,
            output,
            controls: managed.controls.clone(),
            completion: managed.state.completion.subscribe(),
        })
    }

    /// Starts one visible workspace initialization process.
    pub(crate) fn spawn_init(
        &self,
        pods: &PodService,
        pod_id: PodId,
        shell: &Path,
        number: usize,
        script: impl Into<tascarrel_api::ArcStr>,
    ) -> Result<api::ProcessId, Report<ProcessSupervisorError>> {
        let process_id = self.admit(
            api::SpawnProcessAction {
                pod_id,
                start_pod: Some(false),
                title: format!("Pod init step {number}").into(),
                executable: shell.to_string_lossy().into_owned().into(),
                arguments: vec!["-eu".into(), "-c".into(), script.into()].into(),
                environment: HashMap::new(),
                working_directory: Some("/workspace".into()),
                terminal: None,
                log_stdout: None,
                profile: api::ProcessExecutionProfile::User,
            },
            ProcessCommand::Specified,
            Actor::Host,
            ProcessOperationServices {
                pods: pods.clone(),
                network_service: None,
            },
            None,
            None,
            None,
        )?;
        Ok(process_id)
    }

    /// Waits for one visible supervised process to exit successfully.
    pub(crate) async fn wait_for_success(
        &self,
        process_id: &api::ProcessId,
    ) -> Result<(), Report<ProcessSupervisorError>> {
        let mut completion = {
            let processes = lock(&self.inner.processes);
            let managed = processes
                .get(process_id)
                .filter(|process| !process.internal)
                .ok_or_else(|| {
                    internal_error("supervised process disappeared")
                        .field_display("process_id", &process_id.0)
                })?;
            managed.state.completion.subscribe()
        };
        loop {
            if let Some(result) = completion.borrow().clone() {
                return result.map_err(internal_error);
            }
            completion.changed().await.map_err(|_| {
                internal_error("supervised process completion channel closed")
                    .field_display("process_id", &process_id.0)
            })?;
        }
    }

    /// Terminates one visible process, waits for it, and removes its retained
    /// process-list entry. A process already removed by another caller is
    /// treated as released.
    pub(crate) async fn terminate_and_remove(
        &self,
        process_id: &api::ProcessId,
        stop_timeout: Duration,
    ) -> Result<(), Report<ProcessSupervisorError>> {
        if stop_timeout.is_zero() {
            return Err(invalid_request("process stop timeout must be positive"));
        }
        let (controls, mut completion) = {
            let processes = lock(&self.inner.processes);
            let Some(managed) = processes
                .get(process_id)
                .filter(|process| !process.internal)
            else {
                return Ok(());
            };
            (
                managed.controls.clone(),
                managed.state.completion.subscribe(),
            )
        };

        if completion.borrow().is_none() {
            if controls
                .try_send(ExecControl::Signal(Signal::Terminate))
                .is_ok()
            {
                self.publish_stopping(process_id);
            }
            if let Ok(result) = timeout(stop_timeout, wait_for_completion(&mut completion)).await {
                result?;
            } else {
                if controls.try_send(ExecControl::Signal(Signal::Kill)).is_ok() {
                    self.publish_stopping(process_id);
                }
                timeout(stop_timeout, wait_for_completion(&mut completion))
                    .await
                    .map_err(|_| {
                        internal_error("process did not stop after a forced termination")
                            .field_display("process_id", &process_id.0)
                    })??;
            }
        }

        let mut processes = lock(&self.inner.processes);
        let Some(managed) = processes
            .get(process_id)
            .filter(|process| !process.internal)
        else {
            return Ok(());
        };
        if !managed.state.is_terminated() {
            return Err(
                internal_error("process remained active after termination completed")
                    .field_display("process_id", &process_id.0),
            );
        }
        processes.remove(process_id);
        self.inner
            .store
            .apply(api::ProcessListMutation::Remove(process_id.clone()));
        Ok(())
    }

    fn publish_stopping(&self, process_id: &api::ProcessId) {
        let processes = lock(&self.inner.processes);
        let Some(managed) = processes
            .get(process_id)
            .filter(|process| !process.internal)
        else {
            return;
        };
        if managed.state.is_terminated() {
            return;
        }
        let process = managed.state.set_status(api::ProcessState::Stopping);
        self.inner
            .store
            .apply(api::ProcessListMutation::Upsert(process));
    }

    /// Drains an internal process's raw output, removes its retained state,
    /// and verifies successful exit.
    pub(crate) async fn wait_internal<F>(
        &self,
        mut process: InternalProcess,
        mut observe: F,
    ) -> Result<(), Report<ProcessSupervisorError>>
    where
        F: FnMut(&[u8]),
    {
        while let Some(bytes) = process.output.recv().await {
            observe(&bytes);
        }
        let completion = process.completed.await;
        let managed = lock(&self.inner.processes)
            .remove(&process.process_id)
            .ok_or_else(|| {
                internal_error("internal process disappeared")
                    .field_display("process_id", &process.process_id.0)
            })?;
        if !managed.internal {
            return Err(internal_error("internal process identity was replaced")
                .field_display("process_id", &process.process_id.0));
        }
        completion.map_err(|_| {
            internal_error("internal process supervisor stopped without completing")
                .field_display("process_id", &process.process_id.0)
        })?;
        let status = lock(&managed.state.process).status.clone();
        match status {
            api::ProcessState::Exited(exit) if exit.code == Some(0) && exit.signal.is_none() => {
                Ok(())
            }
            api::ProcessState::Exited(exit) => Err(internal_error(format!(
                "internal process exited unsuccessfully (code {:?}, signal {:?})",
                exit.code, exit.signal
            ))),
            api::ProcessState::Failed(failure) => Err(internal_error(format!(
                "internal process failed: {}",
                failure.message
            ))),
            _ => Err(internal_error(
                "internal process completed without a terminal state",
            )),
        }
    }

    /// Creates the raw-output bridge used by a hidden internal process.
    fn internal_output(&self) -> (mpsc::Sender<OwnedProcessOutput>, mpsc::Receiver<Vec<u8>>) {
        let (output_sender, mut observed) =
            mpsc::channel::<OwnedProcessOutput>(self.inner.config.output_queue_capacity.get());
        let (output_forwarder, output) =
            mpsc::channel(self.inner.config.output_queue_capacity.get());
        tokio::spawn(async move {
            while let Some(chunk) = observed.recv().await {
                if output_forwarder.send(chunk.data).await.is_err() {
                    break;
                }
            }
        });
        (output_sender, output)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Admission atomically constructs all process supervision state.
    fn admit(
        &self,
        input: api::SpawnProcessAction,
        command: ProcessCommand,
        started_by: Actor,
        services: ProcessOperationServices,
        ephemeral_pod: Option<EphemeralPod>,
        raw_output: Option<mpsc::Sender<OwnedProcessOutput>>,
        completed: Option<oneshot::Sender<()>>,
    ) -> Result<api::ProcessId, Report<ProcessSupervisorError>> {
        let request = execution_request(&input, command);
        let validation = if ephemeral_pod.is_some() {
            Executor::validate_setup(&request)
        } else {
            Executor::validate(&request)
        };
        validation.map_err(|error| {
            ProcessSupervisorError::InvalidRequest(error.message)
                .report()
                .field_display("pod_id", &input.pod_id.0)
        })?;

        let internal = completed.is_some();
        let process_id = api::ProcessId::generate();
        let process = api::Process {
            id: process_id.clone(),
            pod_id: input.pod_id.clone(),
            title: input.title.clone(),
            started_by,
            status: api::ProcessState::Starting,
            terminal: input.terminal.clone(),
            created_at: Timestamp::now(),
        };
        let state = Arc::new(ProcessState {
            process: Mutex::new(process.clone()),
            log: LogBuffer::new(
                self.inner.config.log_capacity,
                self.inner.config.log_batch_capacity,
            ),
            terminal: Mutex::new(
                input
                    .terminal
                    .as_ref()
                    .map(|terminal| vt100::Parser::new(terminal.rows, terminal.cols, 0)),
            ),
            terminal_output: input.terminal.as_ref().map(|_| {
                TerminalBuffer::new(
                    self.inner.config.terminal_buffer_bytes,
                    self.inner.config.terminal_event_bytes,
                )
            }),
            completion: watch::channel(None).0,
        });
        let (controls, control_receiver) =
            mpsc::channel(self.inner.config.control_queue_capacity.get());
        let supervision_controls = controls.clone();
        lock(&self.inner.processes).insert(
            process_id.clone(),
            ManagedProcess {
                state: Arc::clone(&state),
                controls,
                internal,
            },
        );
        if !internal {
            self.inner
                .store
                .apply(api::ProcessListMutation::Upsert(process));
        }

        let inner = Arc::clone(&self.inner);
        let supervisor = self.clone();
        let max_line_bytes = self.inner.config.max_log_line_bytes.get();
        let output_queue_capacity = self.inner.config.output_queue_capacity.get();
        let internal_timeout = internal.then_some(self.inner.config.internal_process_timeout);
        let task_state = Arc::clone(&state);
        let task_process_id = process_id.clone();
        tokio::spawn(async move {
            let monitor_inner = Arc::clone(&inner);
            let monitor_state = Arc::clone(&task_state);
            let monitor_process_id = task_process_id.clone();
            let mut task = tokio::spawn(supervise_process(
                inner,
                task_state,
                task_process_id,
                input,
                request,
                control_receiver,
                max_line_bytes,
                output_queue_capacity,
                ephemeral_pod,
                raw_output,
                services,
                supervisor,
            ));
            let mut timed_out = false;
            let joined = if let Some(duration) = internal_timeout {
                if let Ok(joined) = timeout(duration, &mut task).await {
                    joined
                } else {
                    timed_out = true;
                    if let Err(error) = supervision_controls
                        .send(ExecControl::Signal(Signal::Kill))
                        .await
                    {
                        warn!(%error, "could not stop timed-out internal process");
                    }
                    task.await
                }
            } else {
                task.await
            };
            if let Err(error) = joined {
                warn!(%error, "process supervision task failed");
                monitor_state.close_output();
                finish_process(
                    &monitor_inner,
                    &monitor_state,
                    &monitor_process_id,
                    api::ProcessState::Failed(api::ProcessFailure {
                        message: "process supervision task failed".into(),
                        failed_at: Timestamp::now(),
                    }),
                );
            } else if timed_out {
                finish_process(
                    &monitor_inner,
                    &monitor_state,
                    &monitor_process_id,
                    api::ProcessState::Failed(api::ProcessFailure {
                        message: "internal process exceeded its time limit".into(),
                        failed_at: Timestamp::now(),
                    }),
                );
            }
            if let Some(completed) = completed
                && completed.send(()).is_err()
            {
                warn!(process_id = %monitor_process_id.0, "internal process completion receiver was dropped");
            }
        });

        Ok(process_id)
    }

    /// Delivers a termination signal to one active process group.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessSupervisorError::InvalidRequest`] when the process is
    /// unknown, has terminated, or can no longer receive controls.
    #[tracing::instrument(level = "debug", skip(self), fields(process_id = %input.process_id.0))]
    pub fn kill(
        &self,
        input: api::KillProcessAction,
    ) -> Result<api::KillProcessOutput, Report<ProcessSupervisorError>> {
        let api::KillProcessAction { process_id, signal } = input;
        let processes = lock(&self.inner.processes);
        let managed = processes
            .get(&process_id)
            .filter(|process| !process.internal)
            .ok_or_else(|| {
                invalid_request("process does not exist").field_display("process_id", &process_id.0)
            })?;
        if managed.state.is_terminated() {
            return Err(invalid_request("process has already terminated")
                .field_display("process_id", &process_id.0));
        }
        managed
            .controls
            .try_send(ExecControl::Signal(runtime_signal(&signal)))
            .map_err(|error| {
                ProcessSupervisorError::InvalidRequest(format!(
                    "process cannot receive a termination signal: {error}"
                ))
                .report()
                .field_display("process_id", &process_id.0)
            })?;
        let process = managed.state.set_status(api::ProcessState::Stopping);
        self.inner
            .store
            .apply(api::ProcessListMutation::Upsert(process));
        Ok(api::KillProcessOutput {})
    }

    /// Writes unmodified bytes to one active process terminal.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessSupervisorError::InvalidRequest`] when the process is
    /// unknown, not a terminal, terminated, or cannot receive the input.
    #[tracing::instrument(level = "debug", skip(self, input), fields(process_id = %input.process_id.0, bytes = input.data.len()))]
    pub async fn write_terminal(
        &self,
        input: api::WriteProcessTerminalAction,
    ) -> Result<api::WriteProcessTerminalOutput, Report<ProcessSupervisorError>> {
        let api::WriteProcessTerminalAction { process_id, data } = input;
        if data.is_empty() {
            return Err(invalid_request("terminal input must not be empty")
                .field_display("process_id", &process_id.0));
        }
        if data.len() > tascarrel_protocol::MAX_IO_CHUNK_LEN {
            return Err(invalid_request(format!(
                "terminal input exceeds {} bytes",
                tascarrel_protocol::MAX_IO_CHUNK_LEN
            ))
            .field_display("process_id", &process_id.0));
        }
        let controls = {
            let processes = lock(&self.inner.processes);
            let managed = visible_process(&processes, &process_id)?;
            require_active_terminal(managed, &process_id)?;
            managed.controls.clone()
        };
        controls
            .send(ExecControl::Input(data.as_bytes().to_vec()))
            .await
            .map_err(|error| {
                invalid_request(format!("process cannot receive terminal input: {error}"))
                    .field_display("process_id", &process_id.0)
            })?;
        Ok(api::WriteProcessTerminalOutput {})
    }

    /// Changes the dimensions of one active process terminal.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessSupervisorError::InvalidRequest`] when the dimensions
    /// are zero or the process cannot receive terminal controls.
    #[tracing::instrument(level = "debug", skip(self), fields(process_id = %input.process_id.0, rows = input.terminal.rows, cols = input.terminal.cols))]
    pub async fn resize_terminal(
        &self,
        input: api::ResizeProcessTerminalAction,
    ) -> Result<api::ResizeProcessTerminalOutput, Report<ProcessSupervisorError>> {
        let api::ResizeProcessTerminalAction {
            process_id,
            terminal,
        } = input;
        if terminal.rows == 0 || terminal.cols == 0 {
            return Err(
                invalid_request("terminal rows and columns must both be non-zero")
                    .field_display("process_id", &process_id.0),
            );
        }
        let (controls, state) = {
            let processes = lock(&self.inner.processes);
            let managed = visible_process(&processes, &process_id)?;
            require_active_terminal(managed, &process_id)?;
            (managed.controls.clone(), Arc::clone(&managed.state))
        };
        controls
            .send(ExecControl::Resize(TerminalSize {
                rows: terminal.rows,
                cols: terminal.cols,
            }))
            .await
            .map_err(|error| {
                invalid_request(format!("process cannot receive a terminal resize: {error}"))
                    .field_display("process_id", &process_id.0)
            })?;
        let process = state.set_terminal_size(terminal);
        self.inner
            .store
            .apply(api::ProcessListMutation::Upsert(process));
        Ok(api::ResizeProcessTerminalOutput {})
    }

    /// Removes one terminated process from retained state.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessSupervisorError::InvalidRequest`] when the process is
    /// unknown or still active.
    #[tracing::instrument(level = "debug", skip(self), fields(process_id = %input.process_id.0))]
    pub fn remove(
        &self,
        input: api::RemoveProcessAction,
    ) -> Result<api::RemoveProcessOutput, Report<ProcessSupervisorError>> {
        let mut processes = lock(&self.inner.processes);
        let managed = processes
            .get(&input.process_id)
            .filter(|process| !process.internal)
            .ok_or_else(|| {
                invalid_request("process does not exist")
                    .field_display("process_id", &input.process_id.0)
            })?;
        if !managed.state.is_terminated() {
            return Err(invalid_request("process is still active")
                .field_display("process_id", &input.process_id.0));
        }
        processes.remove(&input.process_id);
        self.inner
            .store
            .apply(api::ProcessListMutation::Remove(input.process_id));
        Ok(api::RemoveProcessOutput {})
    }

    /// Captures the emulated screen of one process terminal.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessSupervisorError::InvalidRequest`] when the process is
    /// unknown or was not launched with a terminal.
    #[tracing::instrument(level = "debug", skip(self), fields(process_id = %input.process_id.0))]
    pub fn snapshot_terminal(
        &self,
        input: api::SnapshotProcessTerminalAction,
    ) -> Result<api::SnapshotProcessTerminalOutput, Report<ProcessSupervisorError>> {
        let api::SnapshotProcessTerminalAction { process_id } = input;
        let processes = lock(&self.inner.processes);
        let managed = processes
            .get(&process_id)
            .filter(|process| !process.internal)
            .ok_or_else(|| {
                invalid_request("process does not exist").field_display("process_id", &process_id.0)
            })?;
        let terminal = lock(&managed.state.terminal);
        let parser = terminal.as_ref().ok_or_else(|| {
            invalid_request("process does not have a terminal")
                .field_display("process_id", &process_id.0)
        })?;
        let snapshot = String::from_utf8_lossy(&parser.screen().contents_formatted()).into_owned();
        Ok(api::SnapshotProcessTerminalOutput {
            snapshot: snapshot.into(),
        })
    }

    /// Opens a resumable subscription to the process list.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessSupervisorError::InvalidRequest`] when the cursor
    /// generation is not a UUID.
    pub(crate) fn subscribe_process_list(
        &self,
        input: api::ProcessListChangedSubscription,
    ) -> Result<ProcessListSubscription, Report<ProcessSupervisorError>> {
        let cursor = input.cursor.map(runtime_stamp).transpose()?;
        Ok(self.inner.store.subscribe(cursor))
    }

    /// Returns the pod which owns one visible supervised process.
    #[must_use]
    pub(crate) fn process_pod_id(&self, process_id: &api::ProcessId) -> Option<PodId> {
        self.inner
            .store
            .snapshot()
            .value
            .processes
            .iter()
            .find(|process| process.id == *process_id)
            .map(|process| process.pod_id.clone())
    }

    /// Opens a line-resumable subscription to one process log.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessSupervisorError::InvalidRequest`] when the process is
    /// unknown.
    pub(crate) fn subscribe_log(
        &self,
        input: api::ProcessLogSubscription,
    ) -> Result<LogSubscription, Report<ProcessSupervisorError>> {
        let api::ProcessLogSubscription {
            process_id,
            last_line,
        } = input;
        let processes = lock(&self.inner.processes);
        let managed = processes
            .get(&process_id)
            .filter(|process| !process.internal)
            .ok_or_else(|| {
                invalid_request("process does not exist").field_display("process_id", &process_id.0)
            })?;
        Ok(managed.state.log.subscribe(last_line))
    }

    /// Opens a byte-resumable subscription to one process terminal.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessSupervisorError::InvalidRequest`] when the process is
    /// unknown, does not have a terminal, or the requested offset is in the
    /// future.
    pub(crate) fn subscribe_terminal(
        &self,
        input: api::ProcessTerminalSubscription,
    ) -> Result<TerminalSubscription, Report<ProcessSupervisorError>> {
        let api::ProcessTerminalSubscription { process_id, offset } = input;
        let processes = lock(&self.inner.processes);
        let managed = visible_process(&processes, &process_id)?;
        let terminal = managed.state.terminal_output.as_ref().ok_or_else(|| {
            invalid_request("process does not have a terminal")
                .field_display("process_id", &process_id.0)
        })?;
        terminal.subscribe(offset).map_err(|error| {
            invalid_request("terminal offset is beyond the current output")
                .field_display("process_id", &process_id.0)
                .field_display("requested_offset", error.requested)
                .field_display("end_offset", error.end_offset)
        })
    }
}

async fn wait_for_completion(
    completion: &mut watch::Receiver<Option<Result<(), String>>>,
) -> Result<(), Report<ProcessSupervisorError>> {
    loop {
        if completion.borrow().is_some() {
            return Ok(());
        }
        completion
            .changed()
            .await
            .map_err(|_| internal_error("supervised process completion channel closed"))?;
    }
}

/// Resource limits used by one process supervisor.
#[derive(Clone, Debug)]
pub struct ProcessSupervisorConfig {
    /// Maximum time a process waits for implicit pod startup.
    pub pod_start_timeout: Duration,
    /// Maximum runtime allowed for one hidden internal process.
    pub internal_process_timeout: Duration,
    /// Mutations retained for process-list resumption.
    pub store_history_limit: NonZeroUsize,
    /// Sanitized lines retained for each process.
    pub log_capacity: NonZeroUsize,
    /// Maximum number of process log lines emitted in one event.
    pub log_batch_capacity: NonZeroUsize,
    /// Maximum UTF-8 byte length retained for one sanitized line.
    pub max_log_line_bytes: NonZeroUsize,
    /// Unmodified terminal output bytes retained for each terminal process.
    pub terminal_buffer_bytes: NonZeroUsize,
    /// Maximum unmodified terminal output bytes emitted in one event.
    pub terminal_event_bytes: NonZeroUsize,
    /// Pending controls accepted for one process.
    pub control_queue_capacity: NonZeroUsize,
    /// Raw process output chunks awaiting parsing.
    pub output_queue_capacity: NonZeroUsize,
}

impl Default for ProcessSupervisorConfig {
    fn default() -> Self {
        Self {
            pod_start_timeout: Duration::from_secs(30),
            internal_process_timeout: Duration::from_mins(30),
            store_history_limit: NonZeroUsize::new(1024).expect("default is non-zero"),
            log_capacity: NonZeroUsize::new(2048).expect("default is non-zero"),
            log_batch_capacity: NonZeroUsize::new(16).expect("default is non-zero"),
            max_log_line_bytes: NonZeroUsize::new(16 * 1024).expect("default is non-zero"),
            terminal_buffer_bytes: NonZeroUsize::new(16 * 1024 * 1024)
                .expect("default is non-zero"),
            terminal_event_bytes: NonZeroUsize::new(16 * 1024).expect("default is non-zero"),
            control_queue_capacity: NonZeroUsize::new(8).expect("default is non-zero"),
            output_queue_capacity: NonZeroUsize::new(64).expect("default is non-zero"),
        }
    }
}

/// Process supervision failure that callers may classify as contract or
/// internal.
#[derive(Debug, Error)]
pub enum ProcessSupervisorError {
    /// The requested operation violates the process lifecycle contract.
    #[error("invalid process request: {0}")]
    InvalidRequest(String),
    /// A guestd-internal supervised process failed.
    #[error("internal process supervision failed: {0}")]
    Internal(String),
}

type ProcessStore = Store<api::ProcessList, api::ProcessListMutation>;
pub(crate) type ProcessListSubscription =
    tascarrel_store::Subscription<api::ProcessList, api::ProcessListMutation>;

/// A hidden process owned by another guestd feature.
pub(crate) struct InternalProcess {
    /// Supervisor identifier used to remove the retained process state.
    process_id: api::ProcessId,
    /// Raw process output forwarded into the owning feature's log.
    output: mpsc::Receiver<Vec<u8>>,
    /// Notification sent after the supervision task reaches a terminal state.
    completed: oneshot::Receiver<()>,
}

/// Guestd-owned protocol handle for one visible supervised process.
pub(crate) struct OwnedProcess {
    process_id: api::ProcessId,
    output: mpsc::Receiver<OwnedProcessOutput>,
    controls: mpsc::Sender<ExecControl>,
    completion: watch::Receiver<Option<Result<(), String>>>,
}

impl OwnedProcess {
    /// Splits the process into independently owned protocol channels.
    pub(crate) fn into_parts(self) -> OwnedProcessParts {
        OwnedProcessParts {
            process_id: self.process_id,
            output: self.output,
            controls: self.controls,
            completion: self.completion,
        }
    }
}

/// Independently owned channels for a guestd-owned process.
pub(crate) struct OwnedProcessParts {
    /// Identifier exposed by the process service.
    pub(crate) process_id: api::ProcessId,
    /// Raw process output.
    pub(crate) output: mpsc::Receiver<OwnedProcessOutput>,
    /// Process input and lifecycle controls.
    pub(crate) controls: mpsc::Sender<ExecControl>,
    /// Terminal result published by the supervisor.
    pub(crate) completion: watch::Receiver<Option<Result<(), String>>>,
}

/// Raw output emitted by a guestd-owned process.
pub(crate) struct OwnedProcessOutput {
    /// Stream that produced the bytes.
    pub(crate) stream: OutputStream,
    /// Unmodified process bytes.
    pub(crate) data: Vec<u8>,
}

struct SupervisorInner {
    executor: Executor,
    config: ProcessSupervisorConfig,
    processes: Mutex<BTreeMap<api::ProcessId, ManagedProcess>>,
    store: ProcessStore,
}

struct ManagedProcess {
    state: Arc<ProcessState>,
    controls: mpsc::Sender<ExecControl>,
    internal: bool,
}

/// Operation-time services retained by one asynchronous process supervision.
struct ProcessOperationServices {
    pods: PodService,
    network_service: Option<Arc<GuestNetworkService>>,
}

/// Selects an explicit command or the pod's effective login shell.
#[derive(Clone, Copy)]
enum ProcessCommand {
    Specified,
    LoginShell,
}

struct ProcessState {
    process: Mutex<api::Process>,
    log: LogBuffer,
    terminal: Mutex<Option<vt100::Parser>>,
    terminal_output: Option<TerminalBuffer>,
    completion: watch::Sender<Option<Result<(), String>>>,
}

impl ProcessState {
    fn set_status(&self, status: api::ProcessState) -> api::Process {
        let mut process = lock(&self.process);
        process.status = status;
        process.clone()
    }

    fn is_terminated(&self) -> bool {
        matches!(
            lock(&self.process).status,
            api::ProcessState::Exited(_) | api::ProcessState::Failed(_)
        )
    }

    fn set_terminal_size(&self, terminal: api::ProcessTerminal) -> api::Process {
        if let Some(parser) = lock(&self.terminal).as_mut() {
            parser.screen_mut().set_size(terminal.rows, terminal.cols);
        }
        let mut process = lock(&self.process);
        process.terminal = Some(terminal);
        process.clone()
    }

    fn close_output(&self) {
        self.log.close();
        if let Some(terminal) = &self.terminal_output {
            terminal.close();
        }
    }

    fn mark_running(&self) -> Option<api::Process> {
        let mut process = lock(&self.process);
        if matches!(process.status, api::ProcessState::Stopping) {
            None
        } else {
            process.status = api::ProcessState::Running;
            Some(process.clone())
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Supervision keeps process output, shutdown, and terminal publication ordered.
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(process_id = %process_id.0, pod_id = %input.pod_id.0)
)]
async fn supervise_process(
    inner: Arc<SupervisorInner>,
    state: Arc<ProcessState>,
    process_id: api::ProcessId,
    input: api::SpawnProcessAction,
    request: ExecRequest,
    controls: mpsc::Receiver<ExecControl>,
    max_line_bytes: usize,
    output_queue_capacity: usize,
    ephemeral_pod: Option<EphemeralPod>,
    mut raw_output: Option<mpsc::Sender<OwnedProcessOutput>>,
    services: ProcessOperationServices,
    supervisor: ProcessSupervisor,
) {
    if input.start_pod.unwrap_or(true) {
        let Some(network_service) = services.network_service else {
            fail_process(
                &inner,
                &state,
                &process_id,
                "implicit pod startup has no guest network service".to_owned(),
            );
            return;
        };
        let pods = services.pods.clone();
        let pod_id = input.pod_id.clone();
        if let Err(error) = pods.wait_until_created(&pod_id).await {
            fail_process(&inner, &state, &process_id, error.to_string());
            return;
        }
        let mut startup = tokio::spawn(async move {
            pods.ensure_running(&pod_id, &supervisor, &network_service)
                .await
        });
        match timeout(inner.config.pod_start_timeout, &mut startup).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                fail_process(&inner, &state, &process_id, error.to_string());
                return;
            }
            Ok(Err(error)) => {
                warn!(%error, "implicit pod startup task failed");
                fail_process(
                    &inner,
                    &state,
                    &process_id,
                    format!("implicit pod startup task failed: {error}"),
                );
                return;
            }
            Err(_) => {
                // Pod startup owns partially established runtime resources and
                // must reach its own success or rollback path after this wait.
                drop(startup);
                fail_process(
                    &inner,
                    &state,
                    &process_id,
                    format!(
                        "pod did not start within {:?}",
                        inner.config.pod_start_timeout
                    ),
                );
                return;
            }
        }
    }
    let execution = match &ephemeral_pod {
        Some(pod) => services.pods.ephemeral_execution(pod).await,
        None => services.pods.execution(&input.pod_id).await,
    };
    let execution = match execution {
        Ok(execution) => execution,
        Err(error) => {
            fail_process(&inner, &state, &process_id, error.to_string());
            return;
        }
    };
    let prepared = match (&ephemeral_pod, input.profile) {
        (Some(_), _) => Executor::prepare_setup(&execution, &request),
        (None, api::ProcessExecutionProfile::User) => inner.executor.prepare(&execution, &request),
        (None, api::ProcessExecutionProfile::SystemService) => {
            inner.executor.prepare_system_service(&execution, &request)
        }
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            fail_process(&inner, &state, &process_id, error.message);
            return;
        }
    };
    mark_process_running(&inner, &state, &process_id);

    let (events, mut event_receiver) = mpsc::channel(output_queue_capacity);
    let runner = tokio::spawn(run_observed(prepared, controls, events));
    let mut stdout = LineDecoder::new(max_line_bytes);
    let mut stderr = LineDecoder::new(max_line_bytes);
    let mut terminal = LineDecoder::new(max_line_bytes);
    let mut outcome = None;

    while let Some(event) = event_receiver.recv().await {
        match event {
            ObservedProcessEvent::Output { stream, data } => {
                if let Some(output) = &raw_output
                    && output
                        .send(OwnedProcessOutput {
                            stream,
                            data: data.clone(),
                        })
                        .await
                        .is_err()
                {
                    raw_output = None;
                }
                if stream == OutputStream::Terminal
                    && let Some(parser) = lock(&state.terminal).as_mut()
                {
                    parser.process(&data);
                }
                if stream == OutputStream::Terminal
                    && let Some(terminal) = &state.terminal_output
                {
                    terminal.append(&data);
                }
                if stream == OutputStream::Stdout && input.log_stdout == Some(false) {
                    continue;
                }
                let (source, decoder) = match stream {
                    OutputStream::Stdout => (api::ProcessLogSource::Stdout, &mut stdout),
                    OutputStream::Stderr => (api::ProcessLogSource::Stderr, &mut stderr),
                    OutputStream::Terminal => (api::ProcessLogSource::Terminal, &mut terminal),
                };
                for line in decoder.push(&data) {
                    state
                        .log
                        .append(source.clone(), line.content, line.truncated);
                }
            }
            ObservedProcessEvent::Finished(result) => {
                outcome = Some(result);
                break;
            }
        }
    }

    let join_result = runner.await;
    if let Err(error) = join_result {
        warn!(%error, "supervised process runner task failed");
        outcome = Some(Err(format!("process runner task failed: {error}")));
    } else if outcome.is_none() {
        outcome = Some(Err(
            "process runner stopped without reporting completion".to_owned()
        ));
    }
    flush_decoder(&state.log, api::ProcessLogSource::Stdout, &mut stdout);
    flush_decoder(&state.log, api::ProcessLogSource::Stderr, &mut stderr);
    flush_decoder(&state.log, api::ProcessLogSource::Terminal, &mut terminal);
    state.close_output();

    let status = match outcome.expect("process outcome was assigned") {
        Ok(exit) => api::ProcessState::Exited(api::ProcessExit {
            code: exit.code,
            signal: exit.signal,
            exited_at: Timestamp::now(),
        }),
        Err(message) => api::ProcessState::Failed(api::ProcessFailure {
            message: message.into(),
            failed_at: Timestamp::now(),
        }),
    };
    finish_process(&inner, &state, &process_id, status);
}

fn fail_process(
    inner: &SupervisorInner,
    state: &Arc<ProcessState>,
    process_id: &api::ProcessId,
    message: String,
) {
    state.close_output();
    finish_process(
        inner,
        state,
        process_id,
        api::ProcessState::Failed(api::ProcessFailure {
            message: message.into(),
            failed_at: Timestamp::now(),
        }),
    );
}

fn finish_process(
    inner: &SupervisorInner,
    state: &Arc<ProcessState>,
    process_id: &api::ProcessId,
    status: api::ProcessState,
) {
    let processes = lock(&inner.processes);
    let Some(managed) = processes.get(process_id) else {
        return;
    };
    if !Arc::ptr_eq(&managed.state, state) {
        return;
    }
    let completion = match &status {
        api::ProcessState::Exited(exit) if exit.code == Some(0) && exit.signal.is_none() => Ok(()),
        api::ProcessState::Exited(exit) => Err(format!(
            "process exited unsuccessfully (code {:?}, signal {:?})",
            exit.code, exit.signal
        )),
        api::ProcessState::Failed(failure) => Err(failure.message.to_string()),
        _ => Err("process completed without a terminal state".to_owned()),
    };
    let process = state.set_status(status);
    state.completion.send_replace(Some(completion));
    if !managed.internal {
        inner.store.apply(api::ProcessListMutation::Upsert(process));
    }
}

fn mark_process_running(
    inner: &SupervisorInner,
    state: &Arc<ProcessState>,
    process_id: &api::ProcessId,
) {
    let processes = lock(&inner.processes);
    let Some(managed) = processes.get(process_id) else {
        return;
    };
    if !Arc::ptr_eq(&managed.state, state) {
        return;
    }
    if let Some(process) = state.mark_running()
        && !managed.internal
    {
        inner.store.apply(api::ProcessListMutation::Upsert(process));
    }
}

fn flush_decoder(log: &LogBuffer, source: api::ProcessLogSource, decoder: &mut LineDecoder) {
    if let Some(line) = decoder.finish() {
        log.append(source, line.content, line.truncated);
    }
}

/// Terminal capability and locale defaults applied at process admission.
const TERMINAL_ENVIRONMENT_DEFAULTS: [(&str, &str); 4] = [
    ("TERM", "xterm-256color"),
    ("COLORTERM", "truecolor"),
    ("LANG", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
];

fn execution_request(input: &api::SpawnProcessAction, command: ProcessCommand) -> ExecRequest {
    let argv = match command {
        ProcessCommand::Specified => {
            let mut argv = Vec::with_capacity(input.arguments.len() + 1);
            argv.push(input.executable.to_string());
            argv.extend(input.arguments.iter().map(ToString::to_string));
            argv
        }
        ProcessCommand::LoginShell => Vec::new(),
    };
    let mut env = input
        .environment
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect::<BTreeMap<_, _>>();
    if input.terminal.is_some() {
        apply_terminal_environment_defaults(&mut env);
    }
    ExecRequest {
        pod_id: RuntimePodId(input.pod_id.0.to_string()),
        argv,
        env,
        working_directory: input.working_directory.as_ref().map(ToString::to_string),
        terminal: input.terminal.as_ref().map(|terminal| TerminalSize {
            rows: terminal.rows,
            cols: terminal.cols,
        }),
    }
}

/// Adds terminal defaults without replacing values supplied by the action.
fn apply_terminal_environment_defaults(environment: &mut BTreeMap<String, String>) {
    for (name, value) in TERMINAL_ENVIRONMENT_DEFAULTS {
        environment
            .entry(name.to_owned())
            .or_insert_with(|| value.to_owned());
    }
}

fn runtime_signal(signal: &api::ProcessSignal) -> Signal {
    match signal {
        api::ProcessSignal::Terminate => Signal::Terminate,
        api::ProcessSignal::Kill => Signal::Kill,
        api::ProcessSignal::Hangup => Signal::Hangup,
        api::ProcessSignal::Interrupt => Signal::Interrupt,
    }
}

fn visible_process<'a>(
    processes: &'a BTreeMap<api::ProcessId, ManagedProcess>,
    process_id: &api::ProcessId,
) -> Result<&'a ManagedProcess, Report<ProcessSupervisorError>> {
    processes
        .get(process_id)
        .filter(|process| !process.internal)
        .ok_or_else(|| {
            invalid_request("process does not exist").field_display("process_id", &process_id.0)
        })
}

fn require_active_terminal(
    managed: &ManagedProcess,
    process_id: &api::ProcessId,
) -> Result<(), Report<ProcessSupervisorError>> {
    if managed.state.terminal_output.is_none() {
        return Err(invalid_request("process does not have a terminal")
            .field_display("process_id", &process_id.0));
    }
    if managed.state.is_terminated() {
        return Err(invalid_request("process has already terminated")
            .field_display("process_id", &process_id.0));
    }
    Ok(())
}

fn reduce_process_list(list: &mut api::ProcessList, mutation: &api::ProcessListMutation) {
    match mutation {
        api::ProcessListMutation::Upsert(process) => {
            if let Some(index) = list
                .processes
                .iter()
                .position(|existing| existing.id == process.id)
            {
                list.processes[index] = process.clone();
            } else {
                list.processes.push(process.clone());
            }
        }
        api::ProcessListMutation::Remove(process_id) => {
            if let Some(index) = list
                .processes
                .iter()
                .position(|process| process.id == *process_id)
            {
                list.processes.remove(index);
            }
        }
    }
}

fn runtime_stamp(
    stamp: store_api::Stamp,
) -> Result<tascarrel_store::Stamp, Report<ProcessSupervisorError>> {
    let store_api::Stamp {
        generation,
        version,
    } = stamp;
    let generation = generation.parse::<uuid::Uuid>().map_err(|error| {
        ProcessSupervisorError::InvalidRequest("process-list cursor generation is invalid".into())
            .report()
            .message(error.to_string())
    })?;
    Ok(tascarrel_store::Stamp {
        generation,
        version,
    })
}

fn invalid_request(message: impl Into<String>) -> Report<ProcessSupervisorError> {
    ProcessSupervisorError::InvalidRequest(message.into()).report()
}

fn internal_error(message: impl Into<String>) -> Report<ProcessSupervisorError> {
    ProcessSupervisorError::Internal(message.into()).report()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use tascarrel_api::ids::PodId as ApiPodId;

    use super::*;

    /// Verifies request normalization selects the login shell, supplies
    /// terminal-safe defaults, and retains explicit environment values.
    #[test]
    fn normalizes_terminal_process_requests() {
        let mut input = api::SpawnProcessAction {
            pod_id: ApiPodId("pod_1111111111111111111111".into()),
            start_pod: None,
            title: "Terminal".into(),
            executable: "/bin/example".into(),
            arguments: vec!["--version".into()].into(),
            environment: HashMap::from([
                ("TERM".into(), "screen-256color".into()),
                ("LANG".into(), "en_US.UTF-8".into()),
            ]),
            working_directory: None,
            terminal: Some(api::ProcessTerminal { rows: 24, cols: 80 }),
            log_stdout: None,
            profile: api::ProcessExecutionProfile::User,
        };

        let terminal = execution_request(&input, ProcessCommand::LoginShell);
        assert!(terminal.argv.is_empty());
        assert_eq!(terminal.env["TERM"], "screen-256color");
        assert_eq!(terminal.env["COLORTERM"], "truecolor");
        assert_eq!(terminal.env["LANG"], "en_US.UTF-8");
        assert_eq!(terminal.env["LC_ALL"], "C.UTF-8");

        input.terminal = None;
        let piped = execution_request(&input, ProcessCommand::Specified);
        assert_eq!(piped.argv, ["/bin/example", "--version"]);
        assert!(!piped.env.contains_key("COLORTERM"));
        assert!(!piped.env.contains_key("LC_ALL"));
    }
}
