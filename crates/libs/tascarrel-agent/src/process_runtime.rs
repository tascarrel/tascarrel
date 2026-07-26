//! Standalone local process supervision shared by shell-facing tools.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::Signal;
use nix::sys::signal::killpg;
use nix::unistd::Pid;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use tokio::fs;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt as _;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ToolError;
use crate::ToolResult;

/// Standalone supervisor for commands started by agent tools.
pub struct ProcessRuntime {
    root: PathBuf,
    config: ProcessRuntimeConfig,
    sequence: AtomicU64,
    processes: RwLock<BTreeMap<String, Arc<ProcessRecord>>>,
}

impl ProcessRuntime {
    /// Opens a process runtime rooted in an existing workspace directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be resolved or is not a directory.
    pub async fn open(root: impl AsRef<Path>) -> ToolResult<Self> {
        Self::open_with_config(root, ProcessRuntimeConfig::default()).await
    }

    /// Opens a process runtime with caller-controlled shell and output limits.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be resolved, is not a directory,
    /// or a configured limit is zero.
    pub async fn open_with_config(
        root: impl AsRef<Path>,
        config: ProcessRuntimeConfig,
    ) -> ToolResult<Self> {
        if config.retained_output_bytes == 0 || config.observation_bytes == 0 {
            return Err(process_error(
                "process output limits must both be greater than zero",
            ));
        }
        let root = fs::canonicalize(root.as_ref())
            .await
            .map_err(|source| io_error("open the process workspace", source))?;
        let metadata = fs::metadata(&root)
            .await
            .map_err(|source| io_error("inspect the process workspace", source))?;
        if !metadata.is_dir() {
            return Err(process_error("process workspace root is not a directory"));
        }
        Ok(Self {
            root,
            config,
            sequence: AtomicU64::new(1),
            processes: RwLock::new(BTreeMap::new()),
        })
    }

    /// Returns the canonical command workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Starts one command under a fresh shell and process group.
    ///
    /// # Errors
    ///
    /// Returns an error when the working directory escapes the workspace or
    /// the command cannot be started.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn start(
        &self,
        command: String,
        working_directory: Option<PathBuf>,
    ) -> ToolResult<ProcessSnapshot> {
        if command.trim().is_empty() {
            return Err(process_error("command must not be empty"));
        }
        let working_directory = self.resolve_working_directory(working_directory).await?;
        let id = format!("process-{}", self.sequence.fetch_add(1, Ordering::Relaxed));
        let mut child = Command::new(&self.config.shell);
        child
            .arg("-lc")
            .arg(&command)
            .current_dir(&working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = child
            .spawn()
            .map_err(|source| io_error("start a supervised process", source))?;
        let process_group = child_process_group(&child)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| process_error("supervised process stdout was not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| process_error("supervised process stderr was not piped"))?;
        let (control, controls) = mpsc::channel(4);
        let shared = Arc::new(ProcessShared {
            id: id.clone(),
            command,
            working_directory: workspace_relative(&self.root, &working_directory)?,
            process_id: child.id(),
            retained_output_bytes: self.config.retained_output_bytes,
            state: Mutex::new(ProcessState::default()),
            notify: Notify::new(),
        });
        let record = Arc::new(ProcessRecord {
            shared: Arc::clone(&shared),
            control,
        });
        self.processes.write().await.insert(id, Arc::clone(&record));
        tokio::spawn(supervise_process(
            shared,
            child,
            process_group,
            controls,
            stdout,
            stderr,
        ));
        Ok(record.snapshot().await)
    }

    /// Lists all retained running and completed processes.
    pub async fn list(&self) -> Vec<ProcessSnapshot> {
        let records = self
            .processes
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut snapshots = Vec::with_capacity(records.len());
        for record in records {
            snapshots.push(record.snapshot().await);
        }
        snapshots
    }

    /// Returns output and state at the requested stream cursor.
    ///
    /// # Errors
    ///
    /// Returns an error when the process identifier is unknown.
    pub async fn poll(&self, id: &str, cursor: u64) -> ToolResult<ProcessObservation> {
        let record = self.record(id).await?;
        Ok(record.observe(cursor, self.config.observation_bytes).await)
    }

    /// Waits for state or output to change, up to the supplied duration.
    ///
    /// # Errors
    ///
    /// Returns an error when the process identifier is unknown.
    pub async fn wait(
        &self,
        id: &str,
        cursor: u64,
        timeout: Duration,
    ) -> ToolResult<ProcessObservation> {
        let record = self.record(id).await?;
        let initial = record.observe(cursor, self.config.observation_bytes).await;
        if initial.snapshot.status.is_terminal() || !initial.output.is_empty() {
            return Ok(initial);
        }
        let notified = record.shared.notify.notified();
        if tokio::time::timeout(timeout, notified).await.is_err() {
            return Ok(record.observe(cursor, self.config.observation_bytes).await);
        }
        Ok(record.observe(cursor, self.config.observation_bytes).await)
    }

    /// Sends a graceful terminate signal to a process group.
    ///
    /// # Errors
    ///
    /// Returns an error when the process identifier is unknown or its
    /// supervisor is unavailable.
    pub async fn terminate(&self, id: &str) -> ToolResult<()> {
        self.signal(id, ProcessControl::Terminate).await
    }

    /// Sends an immediate kill signal to a process group.
    ///
    /// # Errors
    ///
    /// Returns an error when the process identifier is unknown or its
    /// supervisor is unavailable.
    pub async fn kill(&self, id: &str) -> ToolResult<()> {
        self.signal(id, ProcessControl::Kill).await
    }

    /// Removes a completed process and its retained output.
    ///
    /// # Errors
    ///
    /// Returns an error when the process is unknown or still running.
    pub async fn remove(&self, id: &str) -> ToolResult<()> {
        let record = self.record(id).await?;
        if !record.snapshot().await.status.is_terminal() {
            return Err(process_error("a running process cannot be removed"));
        }
        self.processes.write().await.remove(id);
        Ok(())
    }

    /// Returns the configured terminate-to-kill grace period.
    #[must_use]
    pub fn termination_grace(&self) -> Duration {
        self.config.termination_grace
    }

    async fn record(&self, id: &str) -> ToolResult<Arc<ProcessRecord>> {
        self.processes
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| process_error(format!("process {id} is not supervised")))
    }

    async fn signal(&self, id: &str, control: ProcessControl) -> ToolResult<()> {
        let record = self.record(id).await?;
        if record.snapshot().await.status.is_terminal() {
            return Ok(());
        }
        record
            .control
            .send(control)
            .await
            .map_err(|_| process_error(format!("process {id} supervisor is unavailable")))
    }

    async fn resolve_working_directory(&self, requested: Option<PathBuf>) -> ToolResult<PathBuf> {
        let requested = requested.map_or_else(
            || self.root.clone(),
            |path| {
                if path.is_absolute() {
                    path
                } else {
                    self.root.join(path)
                }
            },
        );
        let canonical = fs::canonicalize(&requested)
            .await
            .map_err(|source| io_error("resolve a process working directory", source))?;
        if !canonical.starts_with(&self.root) {
            return Err(Report::new(ToolError::PathOutsideWorkspace {
                path: canonical,
            }));
        }
        let metadata = fs::metadata(&canonical)
            .await
            .map_err(|source| io_error("inspect a process working directory", source))?;
        if !metadata.is_dir() {
            return Err(process_error(
                "process working directory is not a directory",
            ));
        }
        Ok(canonical)
    }
}

impl Drop for ProcessRuntime {
    fn drop(&mut self) {
        if let Ok(processes) = self.processes.try_read() {
            for (process_id, process) in processes.iter() {
                if process.control.try_send(ProcessControl::Kill).is_err() {
                    tracing::debug!(
                        process_id,
                        "process supervisor did not accept its shutdown signal"
                    );
                }
            }
        }
    }
}

/// Configuration for a standalone process runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRuntimeConfig {
    /// Shell executable used with `-lc`.
    pub shell: PathBuf,
    /// Maximum combined stdout and stderr bytes retained per process.
    pub retained_output_bytes: usize,
    /// Maximum output bytes returned by one observation.
    pub observation_bytes: usize,
    /// Grace period before a timed-out command is killed.
    pub termination_grace: Duration,
}

impl Default for ProcessRuntimeConfig {
    fn default() -> Self {
        Self {
            shell: PathBuf::from("bash"),
            retained_output_bytes: DEFAULT_RETAINED_PROCESS_OUTPUT_BYTES,
            observation_bytes: DEFAULT_PROCESS_OBSERVATION_BYTES,
            termination_grace: DEFAULT_PROCESS_TERMINATION_GRACE,
        }
    }
}

/// Current state of one supervised process.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSnapshot {
    /// Harness-assigned process identifier.
    pub id: String,
    /// Original shell command.
    pub command: String,
    /// Workspace-relative working directory.
    pub working_directory: PathBuf,
    /// Operating-system process identifier when available.
    pub process_id: Option<u32>,
    /// Current lifecycle state.
    pub status: ProcessStatus,
}

/// Process output returned from a stream cursor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessObservation {
    /// Current process metadata and status.
    pub snapshot: ProcessSnapshot,
    /// Combined stdout and stderr since the effective cursor.
    pub output: String,
    /// Cursor to use in the next poll or wait.
    pub next_cursor: u64,
    /// Whether older output was discarded before the requested cursor.
    pub output_truncated: bool,
    /// Whether more retained output is immediately available.
    pub has_more_output: bool,
}

/// Lifecycle state of a supervised process.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ProcessStatus {
    /// The process group is still running.
    #[default]
    Running,
    /// The process group exited or was terminated by a signal.
    Exited {
        /// Shell exit code, or `None` when terminated by a signal.
        exit_code: Option<i32>,
    },
    /// The supervisor failed while waiting for the process.
    Failed {
        /// Safe failure diagnostic.
        message: String,
    },
}

impl ProcessStatus {
    /// Returns whether the supervisor observed a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Default maximum output retained for each supervised process.
pub const DEFAULT_RETAINED_PROCESS_OUTPUT_BYTES: usize = 1_024 * 1_024;

/// Default maximum output returned by one process observation.
pub const DEFAULT_PROCESS_OBSERVATION_BYTES: usize = 50 * 1_024;

/// Default grace period between terminate and kill signals.
pub const DEFAULT_PROCESS_TERMINATION_GRACE: Duration = Duration::from_secs(2);

struct ProcessRecord {
    shared: Arc<ProcessShared>,
    control: mpsc::Sender<ProcessControl>,
}

impl ProcessRecord {
    async fn snapshot(&self) -> ProcessSnapshot {
        self.shared.snapshot().await
    }

    async fn observe(&self, cursor: u64, limit: usize) -> ProcessObservation {
        self.shared.observe(cursor, limit).await
    }
}

struct ProcessShared {
    id: String,
    command: String,
    working_directory: PathBuf,
    process_id: Option<u32>,
    retained_output_bytes: usize,
    state: Mutex<ProcessState>,
    notify: Notify,
}

impl ProcessShared {
    async fn snapshot(&self) -> ProcessSnapshot {
        let status = self.state.lock().await.status.clone();
        ProcessSnapshot {
            id: self.id.clone(),
            command: self.command.clone(),
            working_directory: self.working_directory.clone(),
            process_id: self.process_id,
            status,
        }
    }

    async fn observe(&self, cursor: u64, limit: usize) -> ProcessObservation {
        let state = self.state.lock().await;
        let effective_cursor = cursor.max(state.output.start_cursor);
        let relative_start =
            usize::try_from(effective_cursor.saturating_sub(state.output.start_cursor))
                .unwrap_or(usize::MAX)
                .min(state.output.bytes.len());
        let available = state.output.bytes.len().saturating_sub(relative_start);
        let returned = available.min(limit);
        let output = state
            .output
            .bytes
            .iter()
            .skip(relative_start)
            .take(returned)
            .copied()
            .collect::<Vec<_>>();
        ProcessObservation {
            snapshot: ProcessSnapshot {
                id: self.id.clone(),
                command: self.command.clone(),
                working_directory: self.working_directory.clone(),
                process_id: self.process_id,
                status: state.status.clone(),
            },
            output: String::from_utf8_lossy(&output).into_owned(),
            next_cursor: effective_cursor.saturating_add(returned as u64),
            output_truncated: cursor < state.output.start_cursor,
            has_more_output: returned < available,
        }
    }

    async fn append(&self, bytes: &[u8]) {
        let mut state = self.state.lock().await;
        state.output.bytes.extend(bytes);
        while state.output.bytes.len() > self.retained_output_bytes {
            state.output.bytes.pop_front();
            state.output.start_cursor = state.output.start_cursor.saturating_add(1);
        }
        drop(state);
        self.notify.notify_waiters();
    }

    async fn finish(&self, status: ProcessStatus) {
        self.state.lock().await.status = status;
        self.notify.notify_waiters();
    }
}

#[derive(Default)]
struct ProcessState {
    status: ProcessStatus,
    output: ProcessOutputBuffer,
}

#[derive(Default)]
struct ProcessOutputBuffer {
    start_cursor: u64,
    bytes: VecDeque<u8>,
}

#[derive(Clone, Copy)]
enum ProcessControl {
    Terminate,
    Kill,
}

async fn supervise_process<Stdout, Stderr>(
    process: Arc<ProcessShared>,
    mut child: Child,
    process_group: Pid,
    mut controls: mpsc::Receiver<ProcessControl>,
    stdout: Stdout,
    stderr: Stderr,
) where
    Stdout: AsyncRead + Unpin + Send + 'static,
    Stderr: AsyncRead + Unpin + Send + 'static,
{
    let stdout_task = tokio::spawn(read_process_output(Arc::clone(&process), stdout));
    let stderr_task = tokio::spawn(read_process_output(Arc::clone(&process), stderr));
    let wait_result = loop {
        tokio::select! {
            result = child.wait() => break result,
            control = controls.recv() => {
                let Some(control) = control else {
                    terminate_process_group(&mut child, process_group, Signal::SIGKILL);
                    break child.wait().await;
                };
                let signal = match control {
                    ProcessControl::Terminate => Signal::SIGTERM,
                    ProcessControl::Kill => Signal::SIGKILL,
                };
                terminate_process_group(&mut child, process_group, signal);
            }
        }
    };
    let output_result = join_output_tasks(stdout_task, stderr_task).await;
    let status = match (wait_result, output_result) {
        (Ok(status), Ok(())) => ProcessStatus::Exited {
            exit_code: status.code(),
        },
        (Err(source), _) => ProcessStatus::Failed {
            message: format!("failed to wait for process: {source}"),
        },
        (_, Err(error)) => ProcessStatus::Failed {
            message: error.error().to_string(),
        },
    };
    process.finish(status).await;
}

async fn read_process_output<R>(process: Arc<ProcessShared>, mut reader: R) -> ToolResult<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0; 8 * 1_024];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|source| io_error("read supervised process output", source))?;
        if count == 0 {
            return Ok(());
        }
        process.append(&buffer[..count]).await;
    }
}

async fn join_output_tasks(
    stdout: JoinHandle<ToolResult<()>>,
    stderr: JoinHandle<ToolResult<()>>,
) -> ToolResult<()> {
    stdout
        .await
        .map_err(|source| process_error(format!("stdout reader task failed: {source}")))??;
    stderr
        .await
        .map_err(|source| process_error(format!("stderr reader task failed: {source}")))??;
    Ok(())
}

fn terminate_process_group(child: &mut Child, process_group: Pid, signal: Signal) {
    match killpg(process_group, signal) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(error) => {
            tracing::debug!(%error, "failed to signal process group; falling back to child kill");
            if let Err(error) = child.start_kill() {
                tracing::warn!(%error, "failed to terminate supervised child process");
            }
        }
    }
}

fn child_process_group(child: &Child) -> ToolResult<Pid> {
    child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .map(Pid::from_raw)
        .ok_or_else(|| process_error("spawned process did not expose a valid process-group ID"))
}

fn workspace_relative(root: &Path, path: &Path) -> ToolResult<PathBuf> {
    path.strip_prefix(root).map(Path::to_path_buf).map_err(|_| {
        Report::new(ToolError::PathOutsideWorkspace {
            path: path.to_path_buf(),
        })
    })
}

fn process_error(message: impl Into<String>) -> Report<ToolError> {
    Report::new(ToolError::Process {
        message: message.into(),
    })
}

fn io_error(action: &'static str, source: std::io::Error) -> Report<ToolError> {
    Report::new(ToolError::Io { action, source })
}
