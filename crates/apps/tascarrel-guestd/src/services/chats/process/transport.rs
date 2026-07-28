//! Process transports used by coding-harness protocol adaptors.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use futures_util::future::BoxFuture;
use reportify::Report;
use tascarrel_api::types::pods::PodId;
use tascarrel_api::types::processes as process_api;
use tascarrel_protocol::OutputStream;
use tascarrel_protocol::Signal;
use tokio::io::AsyncRead;
use tokio::io::AsyncWriteExt as _;
use tokio::io::ReadBuf;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::GuestNetworkService;
use crate::ProcessSupervisor;
use crate::runtime::process::ExecControl;
use crate::services::chats::harness::protocol::HarnessError;
use crate::services::chats::harness::protocol::HarnessErrorKind;
use crate::services::pods::PodService;
use crate::services::processes::OwnedProcessOutput;
use crate::services::processes::OwnedProcessParts;

/// Complete specification for one harness protocol process.
pub(crate) struct HarnessProcessSpec {
    /// User-facing title shown by the process service.
    pub(crate) title: String,
    /// Executable pathname in the selected execution environment.
    pub(crate) executable: PathBuf,
    /// Arguments excluding argument zero.
    pub(crate) arguments: Vec<String>,
    /// Environment values owned by the chat feature.
    pub(crate) environment: HashMap<String, String>,
    /// Working directory used by the harness.
    pub(crate) working_directory: PathBuf,
}

/// Bidirectional transport for a running harness process.
pub(crate) struct HarnessProcess {
    /// Raw standard output carrying the harness protocol.
    pub(crate) stdout: Pin<Box<dyn AsyncRead + Send>>,
    /// Concurrent process input and lifecycle control.
    pub(crate) control: Arc<dyn HarnessProcessControl>,
}

/// Starts harness protocol processes in a selected execution environment.
pub(crate) trait HarnessProcessLauncher: Send + Sync {
    /// Starts one process and returns its protocol transport.
    fn launch(
        &self,
        spec: HarnessProcessSpec,
    ) -> BoxFuture<'_, Result<HarnessProcess, HarnessError>>;
}

/// Concurrent control plane for one harness process.
pub(crate) trait HarnessProcessControl: Send + Sync {
    /// Writes one complete protocol frame to standard input.
    fn write(&self, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), HarnessError>>;

    /// Stops the process and waits for supervision to finish.
    fn stop(&self) -> BoxFuture<'_, Result<(), HarnessError>>;
}

/// Launcher for a visible process inside one workspace pod.
pub(crate) struct PodHarnessProcessLauncher {
    pod_id: PodId,
    processes: ProcessSupervisor,
    pods: PodService,
    network_service: Arc<GuestNetworkService>,
}

impl PodHarnessProcessLauncher {
    /// Creates an operation-scoped launcher for one pod.
    pub(crate) fn new(
        pod_id: PodId,
        processes: ProcessSupervisor,
        pods: PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> Self {
        Self {
            pod_id,
            processes,
            pods,
            network_service,
        }
    }
}

impl HarnessProcessLauncher for PodHarnessProcessLauncher {
    fn launch(
        &self,
        spec: HarnessProcessSpec,
    ) -> BoxFuture<'_, Result<HarnessProcess, HarnessError>> {
        Box::pin(async move {
            let process = self
                .processes
                .spawn_owned(
                    process_api::SpawnProcessAction {
                        pod_id: self.pod_id.clone(),
                        start_pod: Some(true),
                        title: spec.title.into(),
                        executable: spec.executable.to_string_lossy().into_owned().into(),
                        arguments: spec.arguments.into_iter().map(Into::into).collect(),
                        environment: spec
                            .environment
                            .into_iter()
                            .map(|(name, value)| (name.into(), value.into()))
                            .collect(),
                        working_directory: Some(
                            spec.working_directory.to_string_lossy().into_owned().into(),
                        ),
                        terminal: None,
                        log_stdout: Some(false),
                        profile: process_api::ProcessExecutionProfile::User,
                    },
                    &self.pods,
                    Arc::clone(&self.network_service),
                )
                .map_err(process_report)?;
            Ok(supervised_transport(process.into_parts()))
        })
    }
}

/// Launcher that drops to the dedicated VM harness account.
pub(crate) struct LocalHarnessProcessLauncher {
    uid: u32,
    gid: u32,
}

impl LocalHarnessProcessLauncher {
    /// Creates a launcher for one resolved VM account.
    pub(crate) const fn new(uid: u32, gid: u32) -> Self {
        Self { uid, gid }
    }
}

impl HarnessProcessLauncher for LocalHarnessProcessLauncher {
    fn launch(
        &self,
        spec: HarnessProcessSpec,
    ) -> BoxFuture<'_, Result<HarnessProcess, HarnessError>> {
        Box::pin(async move {
            let mut command = Command::new(&spec.executable);
            // CommandExt clears supplementary groups as part of this explicit
            // UID transition, so the VM service account cannot inherit root's.
            command
                .args(&spec.arguments)
                .envs(&spec.environment)
                .current_dir(&spec.working_directory)
                .uid(self.uid)
                .gid(self.gid)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let mut child = command.spawn().map_err(|error| {
                process_error(format!(
                    "failed to start harness executable {}: {error}",
                    spec.executable.display()
                ))
            })?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| process_error("failed to acquire harness process standard input"))?;
            let stdout = child.stdout.take().ok_or_else(|| {
                process_error("failed to acquire harness process standard output")
            })?;
            Ok(HarnessProcess {
                stdout: Box::pin(stdout),
                control: Arc::new(LocalProcessControl {
                    child: AsyncMutex::new(child),
                    stdin: AsyncMutex::new(stdin),
                }),
            })
        })
    }
}

struct LocalProcessControl {
    child: AsyncMutex<Child>,
    stdin: AsyncMutex<ChildStdin>,
}

impl HarnessProcessControl for LocalProcessControl {
    fn write(&self, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async move {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(&bytes).await.map_err(io_error)?;
            stdin.flush().await.map_err(io_error)
        })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async move {
            let mut child = self.child.lock().await;
            if child.try_wait().map_err(io_error)?.is_none() {
                child.kill().await.map_err(io_error)?;
            }
            child.wait().await.map_err(io_error)?;
            Ok(())
        })
    }
}

fn supervised_transport(parts: OwnedProcessParts) -> HarnessProcess {
    let OwnedProcessParts {
        process_id,
        output,
        controls,
        completion,
    } = parts;
    HarnessProcess {
        stdout: Box::pin(SupervisedStdout::new(output)),
        control: Arc::new(SupervisedProcessControl {
            process_id,
            controls,
            completion: AsyncMutex::new(completion),
        }),
    }
}

struct SupervisedProcessControl {
    process_id: process_api::ProcessId,
    controls: mpsc::Sender<ExecControl>,
    completion: AsyncMutex<watch::Receiver<Option<Result<(), String>>>>,
}

impl HarnessProcessControl for SupervisedProcessControl {
    fn write(&self, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async move {
            self.controls
                .send(ExecControl::Input(bytes))
                .await
                .map_err(|error| process_error(format!("failed to write process input: {error}")))
        })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async move {
            let mut completion = self.completion.lock().await;
            if completion.borrow().is_none() {
                self.controls
                    .send(ExecControl::Signal(Signal::Kill))
                    .await
                    .map_err(|error| {
                        process_error(format!("failed to stop harness process: {error}"))
                    })?;
            }
            loop {
                if let Some(result) = completion.borrow().clone() {
                    if let Err(error) = result {
                        tracing::debug!(
                            process_id = %self.process_id.0,
                            %error,
                            "harness process exited unsuccessfully during requested shutdown"
                        );
                    }
                    return Ok(());
                }
                completion.changed().await.map_err(|_| {
                    process_error(format!(
                        "harness process {} completion channel closed",
                        self.process_id.0
                    ))
                })?;
            }
        })
    }
}

struct SupervisedStdout {
    output: mpsc::Receiver<OwnedProcessOutput>,
    buffered: Vec<u8>,
    offset: usize,
}

impl SupervisedStdout {
    fn new(output: mpsc::Receiver<OwnedProcessOutput>) -> Self {
        Self {
            output,
            buffered: Vec::new(),
            offset: 0,
        }
    }
}

impl AsyncRead for SupervisedStdout {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if self.offset < self.buffered.len() {
                let available = &self.buffered[self.offset..];
                let length = available.len().min(buffer.remaining());
                buffer.put_slice(&available[..length]);
                self.offset += length;
                if self.offset == self.buffered.len() {
                    self.buffered.clear();
                    self.offset = 0;
                }
                return Poll::Ready(Ok(()));
            }
            match self.output.poll_recv(context) {
                Poll::Ready(Some(output)) if output.stream == OutputStream::Stdout => {
                    self.buffered = output.data;
                    self.offset = 0;
                }
                Poll::Ready(Some(_)) => {}
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)] // This signature is used directly with Result::map_err.
fn process_report(report: Report<crate::ProcessSupervisorError>) -> HarnessError {
    process_error(report.to_string())
}

#[allow(clippy::needless_pass_by_value)] // This signature is used directly with Result::map_err.
fn io_error(error: io::Error) -> HarnessError {
    process_error(error.to_string())
}

fn process_error(message: impl Into<String>) -> HarnessError {
    HarnessError {
        kind: HarnessErrorKind::ProcessStart,
        message: message.into(),
        retryable: true,
    }
}
