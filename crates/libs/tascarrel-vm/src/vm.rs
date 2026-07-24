//! Owned QEMU, virtiofsd, and private host control-channel lifecycle.
//!
//! [`Vm`] is the primary interface and owns the QEMU child process, control
//! stream, QMP connection, shared-directory backends, shutdown, and cleanup.

use std::fs;
use std::fs::DirBuilder;
use std::future::Future;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use nix::sys::signal;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use nix::unistd::Uid;
use reportify::Report;
use reportify::ResultExt as _;
use tokio::io::AsyncRead;
use tokio::io::AsyncWriteExt as _;
use tokio::io::DuplexStream;
use tokio::io::ReadBuf;
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::info;
use tracing::warn;

use crate::ExecutablePreflightReport;
use crate::PreflightReport;
use crate::SharedDirectoryTransport;
use crate::SparseRawDiskOutcome;
use crate::VmConfig;
use crate::VmError;
use crate::ensure_sparse_raw_disk;
use crate::preflight;
use crate::qmp::QmpClient;

/// A running QEMU process with a connected virtio-serial control channel.
///
/// The process is never intentionally detached. An internal lifecycle task
/// owns QEMU and its virtiofsd backends so dropping this value can close its
/// channels and asynchronously stop and reap every process without blocking
/// the caller.
#[derive(Debug)]
pub struct Vm {
    config: VmConfig,
    preflight: PreflightReport,
    control: Option<UnixStream>,
    qmp: Option<QmpClient>,
    pid: Arc<AtomicU32>,
    lifecycle: Option<mpsc::Sender<LifecycleCommand>>,
    lifecycle_task: Option<JoinHandle<()>>,
}

/// One managed virtiofsd backend and its guest-visible tag.
#[derive(Debug)]
struct VirtiofsdProcess {
    mount_tag: String,
    child: Child,
}

/// In-progress VM startup with serial output available before readiness.
///
/// Take and drain [`Self::take_serial_output`] before awaiting this value. When
/// the output is not taken, awaiting startup discards it automatically.
#[must_use]
pub struct VmSpawn {
    serial_output: Option<VmSerialOutput>,
    startup: Pin<Box<dyn Future<Output = Result<Vm, Report<VmError>>> + Send>>,
}

impl VmSpawn {
    /// Takes the VM's raw serial-console stream.
    ///
    /// The caller must drain the stream concurrently while awaiting startup so
    /// QEMU cannot block on serial-console backpressure.
    pub fn take_serial_output(&mut self) -> Option<VmSerialOutput> {
        self.serial_output.take()
    }
}

impl Future for VmSpawn {
    type Output = Result<Vm, Report<VmError>>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(output) = self.serial_output.as_mut() {
            let mut bytes = [0_u8; 8192];
            for _ in 0..16 {
                let mut buffer = ReadBuf::new(&mut bytes);
                match Pin::new(&mut output.inner).poll_read(context, &mut buffer) {
                    Poll::Ready(Ok(())) if buffer.filled().is_empty() => break,
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(error)) => {
                        warn!(%error, "failed to discard unclaimed VM serial output");
                        break;
                    }
                    Poll::Pending => break,
                }
            }
        }
        self.startup.as_mut().poll(context)
    }
}

impl std::fmt::Debug for VmSpawn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VmSpawn")
            .field("serial_output_taken", &self.serial_output.is_none())
            .finish_non_exhaustive()
    }
}

/// Raw asynchronous serial-console byte stream for one VM instance.
#[derive(Debug)]
pub struct VmSerialOutput {
    inner: DuplexStream,
}

impl AsyncRead for VmSerialOutput {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl Vm {
    /// Prepares an awaitable QEMU spawn with a separately drainable serial
    /// stream.
    ///
    /// Taking [`VmSpawn::take_serial_output`] before awaiting the result allows
    /// the caller to consume boot output while this operation waits for the
    /// virtio-serial control socket. Awaiting without taking the stream safely
    /// discards serial output.
    ///
    /// # Errors
    ///
    /// Returns [`VmError`] when no Tokio runtime is active, runtime paths are
    /// invalid, the managed data disk cannot be created or grown, a required
    /// executable cannot start, QEMU exits early, or its control socket does
    /// not become usable by the configured deadline. Spawned children are
    /// stopped and reaped on failure. An existing runtime directory must be
    /// owned by the current user with no group or other permission bits; a
    /// missing one is created with mode `0700`.
    pub fn spawn(config: VmConfig) -> VmSpawn {
        let (serial_input, serial_output) = tokio::io::duplex(SERIAL_OUTPUT_BUFFER_CAPACITY);
        VmSpawn {
            serial_output: Some(VmSerialOutput {
                inner: serial_output,
            }),
            startup: Box::pin(Self::spawn_inner(config, serial_input)),
        }
    }

    #[tracing::instrument(
        name = "tascarrel_vm.spawn",
        level = "info",
        skip_all,
        fields(
            architecture = %config.architecture(),
            requested_qemu = %config.qemu_binary().display(),
            runtime_directory = %config.runtime_directory().display(),
            data_disk = %config.data_disk_image().display(),
            data_disk_minimum_size = config.data_disk_minimum_size(),
            shared_directories = config.shared_directories().len(),
            memory_mib = config.memory_mib(),
            vcpu_count = config.vcpu_count(),
        ),
        err
    )]
    async fn spawn_inner(
        mut config: VmConfig,
        serial_input: DuplexStream,
    ) -> Result<Self, Report<VmError>> {
        tokio::runtime::Handle::try_current()
            .map_err(|_| VmError::MissingRuntime)
            .report()?;
        let preflight = preflight(&config).await;
        apply_preflight_report(&mut config, &preflight)?;
        validate_runtime_paths(&config)?;
        prepare_data_disk(&config).await?;
        prepare_runtime(&config)?;

        let mut virtiofsd = match spawn_virtiofsd(&config).await {
            Ok(processes) => processes,
            Err(error) => {
                cleanup_runtime(&config);
                return Err(error);
            }
        };

        let invocation = match config.qemu_command() {
            Ok(invocation) => invocation,
            Err(error) => {
                force_cleanup_virtiofsd(&mut virtiofsd).await;
                cleanup_runtime(&config);
                return Err(error.escalate(VmError::Invocation));
            }
        };
        let mut command = Command::from(invocation.to_command());
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(source) => {
                force_cleanup_virtiofsd(&mut virtiofsd).await;
                cleanup_runtime(&config);
                return Err(Report::new(VmError::Spawn {
                    program: invocation.program().to_owned(),
                    source,
                }));
            }
        };
        let mut serial_output_task = Some(start_serial_output(&mut child, serial_input));
        let control = match wait_for_control(&mut child, &config).await {
            Ok(control) => control,
            Err(error) => {
                force_cleanup_child(&mut child, "QEMU").await;
                finish_serial_output(&mut serial_output_task).await;
                force_cleanup_virtiofsd(&mut virtiofsd).await;
                cleanup_runtime(&config);
                return Err(error);
            }
        };
        let qmp = if let Some(path) = config.qmp_socket() {
            match QmpClient::connect(path, config.startup_timeout()).await {
                Ok(qmp) => Some(qmp),
                Err(error) => {
                    drop(control);
                    force_cleanup_child(&mut child, "QEMU").await;
                    finish_serial_output(&mut serial_output_task).await;
                    force_cleanup_virtiofsd(&mut virtiofsd).await;
                    cleanup_runtime(&config);
                    return Err(error.escalate(VmError::Qmp));
                }
            }
        } else {
            None
        };
        if let Err(error) = ensure_virtiofsd_running(&mut virtiofsd) {
            drop(control);
            force_cleanup_child(&mut child, "QEMU").await;
            finish_serial_output(&mut serial_output_task).await;
            force_cleanup_virtiofsd(&mut virtiofsd).await;
            cleanup_runtime(&config);
            return Err(error);
        }

        let Some(process_id) = child.id() else {
            force_cleanup_child(&mut child, "QEMU").await;
            finish_serial_output(&mut serial_output_task).await;
            force_cleanup_virtiofsd(&mut virtiofsd).await;
            cleanup_runtime(&config);
            return Err(Report::new(VmError::NotRunning));
        };
        let pid = Arc::new(AtomicU32::new(process_id));
        let (lifecycle, commands) = mpsc::channel(1);
        let lifecycle_task = tokio::spawn(run_lifecycle(
            child,
            serial_output_task
                .take()
                .expect("a running QEMU process owns its serial forwarding task"),
            virtiofsd,
            config.clone(),
            Arc::clone(&pid),
            commands,
        ));
        Ok(Self {
            config,
            preflight,
            control: Some(control),
            qmp,
            pid,
            lifecycle: Some(lifecycle),
            lifecycle_task: Some(lifecycle_task),
        })
    }

    /// Returns the immutable configuration used to launch this VM.
    pub fn config(&self) -> &VmConfig {
        &self.config
    }

    /// Returns the executable and transport report produced before startup.
    pub const fn preflight_report(&self) -> &PreflightReport {
        &self.preflight
    }

    /// Returns QEMU's process identifier while the child handle is owned.
    #[must_use]
    pub fn id(&self) -> Option<u32> {
        match self.pid.load(Ordering::Acquire) {
            0 => None,
            pid => Some(pid),
        }
    }

    /// Takes the connected asynchronous control stream.
    ///
    /// # Errors
    ///
    /// Returns [`VmError::NotRunning`] once the stream has been taken or the
    /// control channel is closed.
    #[tracing::instrument(
        name = "tascarrel_vm.control.take",
        level = "debug",
        skip(self),
        fields(vm_pid = ?self.id()),
        err
    )]
    pub fn take_control_stream(&mut self) -> Result<UnixStream, Report<VmError>> {
        self.control.take().ok_or(VmError::NotRunning).report()
    }

    /// Hot-plugs one physical host USB device into a deterministic xHCI port.
    /// Port numbers are allocated from `1..=USB_FORWARDING_PORT_COUNT`.
    ///
    /// # Errors
    ///
    /// Returns an error when QMP was not configured or QEMU rejects hotplug.
    #[tracing::instrument(
        name = "tascarrel_vm.usb.attach",
        level = "info",
        skip(self),
        fields(
            vm_pid = ?self.id(),
            device_id = %id,
            host_bus = host_bus,
            host_address = host_address,
            port = port,
        ),
        err
    )]
    pub async fn attach_usb(
        &mut self,
        id: &str,
        host_bus: u8,
        host_address: u8,
        port: u8,
    ) -> Result<(), Report<VmError>> {
        let qmp = self.qmp.as_mut().ok_or(VmError::Qmp).report()?;
        qmp.attach_usb(id, host_bus, host_address, port)
            .await
            .map_err(|report| report.escalate(VmError::Qmp))
    }

    /// Hot-unplugs one QMP device by its Tascarrel-owned identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when QMP was not configured or QEMU rejects unplug.
    #[tracing::instrument(
        name = "tascarrel_vm.usb.detach",
        level = "info",
        skip(self),
        fields(vm_pid = ?self.id(), device_id = %id),
        err
    )]
    pub async fn detach_usb(&mut self, id: &str) -> Result<(), Report<VmError>> {
        let result = self
            .qmp
            .as_mut()
            .ok_or(VmError::Qmp)
            .report()?
            .detach_usb(id)
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.error().is_device_not_found() => Ok(()),
            Err(error) => Err(error.escalate(VmError::Qmp)),
        }
    }

    /// Returns the exit status if QEMU has finished, without blocking.
    ///
    /// # Errors
    ///
    /// Returns [`VmError::NotRunning`] after the child has been reaped or
    /// [`VmError::Wait`] if process inspection fails.
    #[tracing::instrument(
        name = "tascarrel_vm.try_wait",
        level = "trace",
        skip(self),
        fields(vm_pid = ?self.id()),
        ret,
        err
    )]
    pub async fn try_wait(&mut self) -> Result<Option<ExitStatus>, Report<VmError>> {
        let (response, result) = oneshot::channel();
        self.send_lifecycle(LifecycleCommand::TryWait { response })
            .await?;
        let status = result.await.map_err(|_| VmError::NotRunning).report()??;
        if status.is_some() {
            self.finish_lifecycle().await;
        }
        Ok(status)
    }

    /// Asynchronously waits until QEMU exits and reaps the process.
    ///
    /// # Errors
    ///
    /// Returns [`VmError::NotRunning`] after the child has already been reaped
    /// or [`VmError::Wait`] when the operating system cannot wait for it.
    #[tracing::instrument(
        name = "tascarrel_vm.wait",
        level = "debug",
        skip(self),
        fields(vm_pid = ?self.id()),
        ret,
        err
    )]
    pub async fn wait(&mut self) -> Result<ExitStatus, Report<VmError>> {
        self.close_channels();
        let (response, result) = oneshot::channel();
        self.send_lifecycle(LifecycleCommand::Wait { response })
            .await?;
        let status = result.await.map_err(|_| VmError::NotRunning).report()?;
        self.finish_lifecycle().await;
        status
    }

    /// Sends SIGTERM and waits for an orderly exit, killing QEMU if the
    /// configured shutdown timeout expires.
    ///
    /// # Errors
    ///
    /// Returns [`VmError`] if the VM is no longer running, signaling fails, or
    /// the child cannot be inspected, killed, or reaped.
    #[tracing::instrument(
        name = "tascarrel_vm.shutdown",
        level = "info",
        skip(self),
        fields(vm_pid = ?self.id()),
        ret,
        err
    )]
    pub async fn shutdown(&mut self) -> Result<ShutdownOutcome, Report<VmError>> {
        self.close_channels();
        let (response, result) = oneshot::channel();
        self.send_lifecycle(LifecycleCommand::Shutdown { response })
            .await?;
        let outcome = result.await.map_err(|_| VmError::NotRunning).report()?;
        self.finish_lifecycle().await;
        outcome
    }

    /// Immediately kills QEMU and waits for it to exit.
    ///
    /// # Errors
    ///
    /// Returns [`VmError`] if the VM is no longer running or the child cannot
    /// be inspected, killed, or reaped.
    #[tracing::instrument(
        name = "tascarrel_vm.kill",
        level = "info",
        skip(self),
        fields(vm_pid = ?self.id()),
        ret,
        err
    )]
    pub async fn kill(&mut self) -> Result<ExitStatus, Report<VmError>> {
        self.close_channels();
        let (response, result) = oneshot::channel();
        self.send_lifecycle(LifecycleCommand::Kill { response })
            .await?;
        let status = result.await.map_err(|_| VmError::NotRunning).report()?;
        self.finish_lifecycle().await;
        status
    }

    fn close_channels(&mut self) {
        self.control = None;
        self.qmp = None;
    }

    async fn send_lifecycle(&self, command: LifecycleCommand) -> Result<(), Report<VmError>> {
        self.lifecycle
            .as_ref()
            .ok_or(VmError::NotRunning)
            .report()?
            .send(command)
            .await
            .map_err(|_| VmError::NotRunning)
            .report()
    }

    async fn finish_lifecycle(&mut self) {
        self.lifecycle = None;
        if let Some(task) = self.lifecycle_task.take()
            && let Err(error) = task.await
        {
            warn!(%error, "QEMU lifecycle task failed");
        }
        self.close_channels();
    }
}

impl Drop for Vm {
    #[tracing::instrument(
        name = "tascarrel_vm.drop",
        level = "debug",
        skip(self),
        fields(vm_pid = ?self.id())
    )]
    fn drop(&mut self) {
        self.close_channels();
        self.lifecycle = None;
        if let Some(task) = self.lifecycle_task.take() {
            drop(task);
        }
    }
}

/// Result of a graceful shutdown attempt.
#[derive(Clone, Debug)]
#[must_use]
pub struct ShutdownOutcome {
    status: ExitStatus,
    forced: bool,
}

impl ShutdownOutcome {
    /// Returns QEMU's final process status.
    #[must_use]
    pub const fn status(&self) -> ExitStatus {
        self.status
    }

    /// Reports whether QEMU had to be killed after the graceful deadline.
    #[must_use]
    pub const fn was_forced(&self) -> bool {
        self.forced
    }
}

/// Commands sent to the task that exclusively owns all VM child processes.
enum LifecycleCommand {
    TryWait {
        response: oneshot::Sender<Result<Option<ExitStatus>, Report<VmError>>>,
    },
    Wait {
        response: oneshot::Sender<Result<ExitStatus, Report<VmError>>>,
    },
    Shutdown {
        response: oneshot::Sender<Result<ShutdownOutcome, Report<VmError>>>,
    },
    Kill {
        response: oneshot::Sender<Result<ExitStatus, Report<VmError>>>,
    },
}

const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(20);
const BACKEND_HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DROP_GRACE_PERIOD: Duration = Duration::from_millis(250);
const SERIAL_OUTPUT_BUFFER_CAPACITY: usize = 64 * 1024;

/// Applies executable resolution and transport selection to a spawn config.
fn apply_preflight_report(
    config: &mut VmConfig,
    preflight: &PreflightReport,
) -> Result<(), Report<VmError>> {
    config.qemu_binary = required_executable_path(preflight.qemu(), "QEMU")?;
    if let Some(virtiofsd) = preflight.virtiofsd() {
        if virtiofsd.is_available()
            && let Some(path) = virtiofsd.resolved_path()
        {
            path.clone_into(&mut config.virtiofsd_binary);
        } else {
            warn!(
                program = %virtiofsd.requested_path().display(),
                reason = virtiofsd.failure().unwrap_or("executable did not pass preflight"),
                "virtiofsd is unavailable; falling back to QEMU virtio-9p"
            );
        }
    }
    config.shared_directory_transport = preflight.shared_directory_transport();
    Ok(())
}

/// Returns a checked executable path or a startup error with probe details.
fn required_executable_path(
    executable: &ExecutablePreflightReport,
    name: &'static str,
) -> Result<PathBuf, Report<VmError>> {
    if executable.is_available()
        && let Some(path) = executable.resolved_path()
    {
        return Ok(path.to_owned());
    }
    Err(Report::new(VmError::RequiredExecutableUnavailable {
        name,
        program: executable.requested_path().to_owned(),
        reason: executable
            .failure()
            .unwrap_or("executable did not pass preflight")
            .to_owned(),
    }))
}

/// Creates or grows the managed sparse raw data disk off the async executor.
#[tracing::instrument(
    name = "tascarrel_vm.disk.prepare_data",
    level = "debug",
    skip(config),
    fields(
        image = %config.data_disk_image().display(),
        minimum_size = config.data_disk_minimum_size(),
    ),
    ret,
    err
)]
async fn prepare_data_disk(config: &VmConfig) -> Result<(), Report<VmError>> {
    let image = config.data_disk_image().to_owned();
    let minimum_size = config.data_disk_minimum_size();
    let task_image = image.clone();
    let result =
        tokio::task::spawn_blocking(move || ensure_sparse_raw_disk(&task_image, minimum_size))
            .await
            .map_err(|source| VmError::PrepareDataDiskTask {
                path: image.clone(),
                source,
            })
            .report()?;
    let outcome = result.map_err(|report| {
        report.escalate(VmError::PrepareDataDisk {
            path: image.clone(),
        })
    })?;
    match outcome {
        SparseRawDiskOutcome::Created => {
            info!(image = %image.display(), minimum_size, "created persistent VM data disk");
        }
        SparseRawDiskOutcome::Grown { previous_size } => {
            info!(
                image = %image.display(),
                previous_size,
                minimum_size,
                "grew persistent VM data disk"
            );
        }
        SparseRawDiskOutcome::Unchanged { .. } => {}
    }
    Ok(())
}

/// Continuously forwards QEMU stdout to the caller's serial stream.
fn start_serial_output(child: &mut Child, mut output: DuplexStream) -> JoinHandle<()> {
    let mut stdout = child
        .stdout
        .take()
        .expect("QEMU was configured with piped serial output");
    tokio::spawn(async move {
        if let Err(error) = tokio::io::copy(&mut stdout, &mut output).await {
            warn!(%error, "VM serial output receiver closed; discarding later output");
            if let Err(error) = tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await {
                warn!(%error, "failed to discard VM serial output");
            }
        }
        if let Err(error) = output.shutdown().await {
            warn!(%error, "failed to close VM serial output sink");
        }
    })
}

/// Waits briefly for serial forwarding after QEMU has been reaped.
async fn finish_serial_output(task: &mut Option<JoinHandle<()>>) {
    let Some(mut task) = task.take() else {
        return;
    };
    match tokio::time::timeout(DROP_GRACE_PERIOD, &mut task).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => warn!(%error, "VM serial forwarding task failed"),
        Err(_) => {
            warn!("VM serial forwarding did not stop after QEMU exited; aborting it");
            task.abort();
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                warn!(%error, "failed to abort VM serial forwarding task");
            }
        }
    }
}

/// Prepares the private directory and every managed runtime socket.
#[tracing::instrument(
    name = "tascarrel_vm.runtime.prepare",
    level = "debug",
    skip(config),
    fields(runtime_directory = %config.runtime_directory().display()),
    err
)]
fn prepare_runtime(config: &VmConfig) -> Result<(), Report<VmError>> {
    prepare_runtime_directory(config.runtime_directory())?;
    // A live control socket identifies an active owner of the complete runtime
    // directory. Return without touching any of its other managed artifacts.
    prepare_runtime_socket(config.control_socket())?;
    let result = (|| {
        if let Some(path) = config.qmp_socket() {
            prepare_runtime_socket(path)?;
        }
        if config.shared_directory_transport() == SharedDirectoryTransport::Virtiofs {
            for directory in config.shared_directories() {
                prepare_runtime_socket(directory.socket_path())?;
            }
        }
        Ok(())
    })();
    if result.is_err() {
        cleanup_runtime(config);
    }
    result
}

/// Starts each configured Linux backend and waits for its listener.
#[tracing::instrument(
    name = "tascarrel_vm.virtiofsd.spawn",
    level = "debug",
    skip(config),
    fields(
        program = %config.virtiofsd_binary().display(),
        shared_directories = config.shared_directories().len(),
        transport = ?config.shared_directory_transport(),
        timeout = ?config.startup_timeout(),
    ),
    err
)]
async fn spawn_virtiofsd(config: &VmConfig) -> Result<Vec<VirtiofsdProcess>, Report<VmError>> {
    let timeout = config.startup_timeout();
    let deadline = Instant::now() + timeout;
    let mut processes = Vec::with_capacity(config.shared_directories().len());
    for (directory, invocation) in config
        .shared_directories()
        .iter()
        .zip(config.virtiofsd_commands())
    {
        let mut command = Command::from(invocation.to_command());
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(source) => {
                force_cleanup_virtiofsd(&mut processes).await;
                return Err(Report::new(VmError::SpawnVirtiofsd {
                    mount_tag: directory.mount_tag().to_owned(),
                    program: invocation.program().to_owned(),
                    source,
                }));
            }
        };
        let process_index = processes.len();
        processes.push(VirtiofsdProcess {
            mount_tag: directory.mount_tag().to_owned(),
            child,
        });
        let process = &mut processes[process_index];
        if let Err(error) =
            wait_for_virtiofsd(process, directory.socket_path(), timeout, deadline).await
        {
            force_cleanup_virtiofsd(&mut processes).await;
            return Err(error);
        }
    }
    Ok(processes)
}

/// Waits until one backend creates its vhost-user socket or exits.
#[tracing::instrument(
    name = "tascarrel_vm.virtiofsd.ready",
    level = "trace",
    skip(process, deadline),
    fields(
        mount_tag = %process.mount_tag,
        pid = ?process.child.id(),
        socket = %socket.display(),
        ?timeout,
    ),
    err
)]
async fn wait_for_virtiofsd(
    process: &mut VirtiofsdProcess,
    socket: &Path,
    timeout: Duration,
    deadline: Instant,
) -> Result<(), Report<VmError>> {
    loop {
        if let Some(status) = process
            .child
            .try_wait()
            .map_err(|source| VmError::InspectVirtiofsd {
                mount_tag: process.mount_tag.clone(),
                source,
            })
            .report()?
        {
            return Err(Report::new(VmError::VirtiofsdExitedBeforeReady {
                mount_tag: process.mount_tag.clone(),
                status,
            }));
        }
        match fs::symlink_metadata(socket) {
            Ok(metadata) if metadata.file_type().is_socket() => return Ok(()),
            Ok(_) => {
                return Err(Report::new(VmError::InspectVirtiofsdSocket {
                    path: socket.to_owned(),
                    source: io::Error::new(
                        io::ErrorKind::InvalidData,
                        "virtiofsd created a non-socket path",
                    ),
                }));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Report::new(VmError::InspectVirtiofsdSocket {
                    path: socket.to_owned(),
                    source,
                }));
            }
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(Report::new(VmError::VirtiofsdReadinessTimeout {
                mount_tag: process.mount_tag.clone(),
                timeout,
                socket: socket.to_owned(),
            }));
        }
        tokio::time::sleep(READINESS_POLL_INTERVAL.min(deadline - now)).await;
    }
}

/// Confirms that all backends survived the complete VM startup sequence.
fn ensure_virtiofsd_running(processes: &mut [VirtiofsdProcess]) -> Result<(), Report<VmError>> {
    for process in processes {
        let status = process
            .child
            .try_wait()
            .map_err(|source| VmError::InspectVirtiofsd {
                mount_tag: process.mount_tag.clone(),
                source,
            })
            .report()?;
        if let Some(status) = status {
            return Err(Report::new(VmError::VirtiofsdExitedBeforeReady {
                mount_tag: process.mount_tag.clone(),
                status,
            }));
        }
    }
    Ok(())
}

/// Waits for the guest control listener while checking for early QEMU exit.
#[tracing::instrument(
    name = "tascarrel_vm.control.ready",
    level = "debug",
    skip(child, config),
    fields(
        vm_pid = ?child.id(),
        socket = %config.control_socket().display(),
        timeout = ?config.startup_timeout(),
    ),
    err
)]
async fn wait_for_control(
    child: &mut Child,
    config: &VmConfig,
) -> Result<UnixStream, Report<VmError>> {
    let timeout = config.startup_timeout();
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(VmError::Wait).report()? {
            return Err(Report::new(VmError::ExitedBeforeReady { status }));
        }

        match UnixStream::connect(config.control_socket()).await {
            Ok(stream) => return Ok(stream),
            Err(error) if is_transient_connect_error(&error) => {}
            Err(source) => {
                return Err(Report::new(VmError::ConnectControl {
                    path: config.control_socket().to_owned(),
                    source,
                }));
            }
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(Report::new(VmError::ReadinessTimeout {
                timeout,
                socket: config.control_socket().to_owned(),
            }));
        }
        tokio::time::sleep(READINESS_POLL_INTERVAL.min(deadline - now)).await;
    }
}

/// Exclusively owns child-process inspection, shutdown, and cleanup.
#[tracing::instrument(
    name = "tascarrel_vm.lifecycle",
    level = "debug",
    skip_all,
    fields(
        vm_pid = ?child.id(),
        virtiofsd_processes = virtiofsd.len(),
        runtime_directory = %config.runtime_directory().display(),
    )
)]
async fn run_lifecycle(
    mut child: Child,
    serial_output: JoinHandle<()>,
    mut virtiofsd: Vec<VirtiofsdProcess>,
    config: VmConfig,
    pid: Arc<AtomicU32>,
    mut commands: mpsc::Receiver<LifecycleCommand>,
) {
    let mut serial_output = Some(serial_output);
    loop {
        let command = tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                command
            }
            () = tokio::time::sleep(BACKEND_HEALTH_POLL_INTERVAL), if !virtiofsd.is_empty() => {
                if virtiofsd_failed(&mut virtiofsd) {
                    force_cleanup_child(&mut child, "QEMU").await;
                    finish_serial_output(&mut serial_output).await;
                    force_cleanup_virtiofsd(&mut virtiofsd).await;
                    finish_process(&config, &pid);
                    return;
                }
                continue;
            }
        };
        match command {
            LifecycleCommand::TryWait { response } => {
                let result = child.try_wait().map_err(VmError::Wait).report();
                let finished = matches!(result, Ok(Some(_)));
                if finished {
                    finish_serial_output(&mut serial_output).await;
                    cleanup_virtiofsd(&mut virtiofsd, config.shutdown_timeout()).await;
                    finish_process(&config, &pid);
                }
                if response.send(result).is_err() {
                    warn!("VM owner dropped a process-status response");
                }
                if finished {
                    return;
                }
            }
            LifecycleCommand::Wait { response } => {
                let result = child.wait().await.map_err(VmError::Wait).report();
                finish_lifecycle_process(
                    &mut child,
                    &mut virtiofsd,
                    &config,
                    &pid,
                    result.is_err(),
                )
                .await;
                finish_serial_output(&mut serial_output).await;
                if response.send(result).is_err() {
                    warn!("VM owner dropped a process-wait response");
                }
                return;
            }
            LifecycleCommand::Shutdown { response } => {
                let result = shutdown_child(&mut child, config.shutdown_timeout()).await;
                finish_lifecycle_process(
                    &mut child,
                    &mut virtiofsd,
                    &config,
                    &pid,
                    result.is_err(),
                )
                .await;
                finish_serial_output(&mut serial_output).await;
                if response.send(result).is_err() {
                    warn!("VM owner dropped a shutdown response");
                }
                return;
            }
            LifecycleCommand::Kill { response } => {
                let result = kill_child(&mut child).await;
                finish_lifecycle_process(
                    &mut child,
                    &mut virtiofsd,
                    &config,
                    &pid,
                    result.is_err(),
                )
                .await;
                finish_serial_output(&mut serial_output).await;
                if response.send(result).is_err() {
                    warn!("VM owner dropped a process-kill response");
                }
                return;
            }
        }
    }
    drop_cleanup(&mut child, config.shutdown_timeout(), "QEMU").await;
    finish_serial_output(&mut serial_output).await;
    cleanup_virtiofsd(&mut virtiofsd, config.shutdown_timeout()).await;
    finish_process(&config, &pid);
}

/// Detects and logs a backend failure after VM startup.
fn virtiofsd_failed(processes: &mut [VirtiofsdProcess]) -> bool {
    for process in processes {
        match process.child.try_wait() {
            Ok(Some(status)) => {
                warn!(
                    mount_tag = %process.mount_tag,
                    %status,
                    "virtiofsd exited while its VM was running"
                );
                return true;
            }
            Ok(None) => {}
            Err(error) => {
                warn!(
                    mount_tag = %process.mount_tag,
                    %error,
                    "failed to inspect virtiofsd while its VM was running"
                );
                return true;
            }
        }
    }
    false
}

/// Gracefully stops QEMU and escalates to a forced kill after the deadline.
#[tracing::instrument(
    name = "tascarrel_vm.process.shutdown",
    level = "debug",
    skip(child),
    fields(pid = ?child.id(), ?timeout),
    ret,
    err
)]
async fn shutdown_child(
    child: &mut Child,
    timeout: Duration,
) -> Result<ShutdownOutcome, Report<VmError>> {
    if let Some(status) = child.try_wait().map_err(VmError::Wait).report()? {
        return Ok(ShutdownOutcome {
            status,
            forced: false,
        });
    }
    let pid = child.id().ok_or(VmError::NotRunning).report()?;
    send_signal(pid, Signal::SIGTERM)?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => Ok(ShutdownOutcome {
            status: result.map_err(VmError::Wait).report()?,
            forced: false,
        }),
        Err(_) => Ok(ShutdownOutcome {
            status: kill_child(child).await?,
            forced: true,
        }),
    }
}

/// Immediately kills and reaps QEMU unless it has already exited.
#[tracing::instrument(
    name = "tascarrel_vm.process.kill",
    level = "debug",
    skip(child),
    fields(pid = ?child.id()),
    ret,
    err
)]
async fn kill_child(child: &mut Child) -> Result<ExitStatus, Report<VmError>> {
    if let Some(status) = child.try_wait().map_err(VmError::Wait).report()? {
        return Ok(status);
    }
    let pid = child.id().ok_or(VmError::NotRunning).report()?;
    child
        .kill()
        .await
        .map_err(|source| VmError::Signal { pid, source })
        .report()?;
    child.wait().await.map_err(VmError::Wait).report()
}

/// Gives an implicitly dropped process a short graceful cleanup window.
#[tracing::instrument(
    name = "tascarrel_vm.process.drop_cleanup",
    level = "debug",
    skip(child),
    fields(pid = ?child.id(), process_name = %process_name, ?shutdown_timeout)
)]
async fn drop_cleanup(child: &mut Child, shutdown_timeout: Duration, process_name: &'static str) {
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(_)) => return,
        Ok(None) => {
            if let Some(pid) = pid
                && let Err(error) = send_signal(pid, Signal::SIGTERM)
            {
                warn!(pid, %error, %process_name, "failed to signal process during drop cleanup");
            }
        }
        Err(error) => {
            warn!(?pid, %error, %process_name, "failed to inspect process during drop cleanup");
        }
    }
    let grace_period = DROP_GRACE_PERIOD.min(shutdown_timeout);
    match tokio::time::timeout(grace_period, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            warn!(?pid, %error, %process_name, "failed to reap process during drop cleanup");
            kill_during_cleanup(child, pid, process_name).await;
        }
        Err(_) => kill_during_cleanup(child, pid, process_name).await,
    }
}

/// Best-effort kills a process during startup rollback.
async fn force_cleanup_child(child: &mut Child, process_name: &'static str) {
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) => {
            if let Err(error) = child.kill().await {
                warn!(?pid, %error, %process_name, "failed to kill process during forced cleanup");
            }
        }
        Err(error) => {
            warn!(?pid, %error, %process_name, "failed to inspect process during forced cleanup");
            if let Err(error) = child.kill().await {
                warn!(?pid, %error, %process_name, "failed to kill process during forced cleanup");
            }
        }
    }
}

/// Immediately stops every backend during error recovery.
async fn force_cleanup_virtiofsd(processes: &mut Vec<VirtiofsdProcess>) {
    for process in processes.iter_mut() {
        force_cleanup_child(&mut process.child, "virtiofsd").await;
    }
    processes.clear();
}

/// Gracefully stops every backend during normal lifecycle cleanup.
async fn cleanup_virtiofsd(processes: &mut Vec<VirtiofsdProcess>, shutdown_timeout: Duration) {
    for process in processes.iter_mut() {
        drop_cleanup(&mut process.child, shutdown_timeout, "virtiofsd").await;
    }
    processes.clear();
}

/// Logs a best-effort forced kill during implicit cleanup.
async fn kill_during_cleanup(child: &mut Child, pid: Option<u32>, process_name: &'static str) {
    if let Err(error) = child.kill().await {
        warn!(?pid, %error, %process_name, "failed to kill process during drop cleanup");
    }
}

/// Performs common child cleanup after an explicit lifecycle command.
async fn finish_lifecycle_process(
    child: &mut Child,
    virtiofsd: &mut Vec<VirtiofsdProcess>,
    config: &VmConfig,
    pid: &AtomicU32,
    requires_force_cleanup: bool,
) {
    if requires_force_cleanup {
        force_cleanup_child(child, "QEMU").await;
    }
    cleanup_virtiofsd(virtiofsd, config.shutdown_timeout()).await;
    finish_process(config, pid);
}

/// Publishes process completion and removes managed runtime artifacts.
fn finish_process(config: &VmConfig, pid: &AtomicU32) {
    pid.store(0, Ordering::Release);
    cleanup_runtime(config);
}

/// Confirms every consumer-owned VM input still has the expected file type.
fn validate_runtime_paths(config: &VmConfig) -> Result<(), Report<VmError>> {
    if !config.system_disk_image().is_file() {
        return Err(Report::new(VmError::InvalidSystemDiskImage(
            config.system_disk_image().to_owned(),
        )));
    }
    for directory in config.shared_directories() {
        if !directory.host_path().is_dir() {
            return Err(Report::new(VmError::InvalidSharedDirectory {
                mount_tag: directory.mount_tag().to_owned(),
                path: directory.host_path().to_owned(),
            }));
        }
    }
    if !config.kernel().is_file() {
        return Err(Report::new(VmError::InvalidKernel(
            config.kernel().to_owned(),
        )));
    }
    if !config.initrd().is_file() {
        return Err(Report::new(VmError::InvalidInitrd(
            config.initrd().to_owned(),
        )));
    }
    Ok(())
}

/// Creates the runtime directory and enforces its ownership and permissions.
fn prepare_runtime_directory(path: &Path) -> Result<(), Report<VmError>> {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .map_err(|source| VmError::PrepareRuntimePath {
            path: path.to_owned(),
            source,
        })
        .report()?;
    validate_runtime_directory(path)
}

/// Preserves active or non-socket paths and removes only a stale socket.
fn prepare_runtime_socket(path: &Path) -> Result<(), Report<VmError>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => probe_existing_socket(path, &metadata),
        Ok(_) => Err(Report::new(VmError::UnsafeRuntimeSocketPath(
            path.to_owned(),
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Report::new(VmError::PrepareRuntimePath {
            path: path.to_owned(),
            source,
        })),
    }
}

/// Enforces the private real-directory invariant after creation.
fn validate_runtime_directory(path: &Path) -> Result<(), Report<VmError>> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| VmError::PrepareRuntimePath {
            path: path.to_owned(),
            source,
        })
        .report()?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != Uid::effective().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(Report::new(VmError::UnsafeRuntimeDirectory(
            path.to_owned(),
        )));
    }
    Ok(())
}

/// Classifies an existing socket as live, stale, or unsafe to replace.
fn probe_existing_socket(path: &Path, observed: &fs::Metadata) -> Result<(), Report<VmError>> {
    loop {
        match StdUnixStream::connect(path) {
            Ok(stream) => {
                drop(stream);
                return Err(Report::new(VmError::RuntimeSocketInUse(path.to_owned())));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => break,
            Err(source) => {
                return Err(Report::new(VmError::ProbeRuntimeSocket {
                    path: path.to_owned(),
                    source,
                }));
            }
        }
    }

    // Avoid unlinking a node replaced between the probe and removal. The
    // caller's private-parent-directory guarantee protects the remaining
    // metadata-check/unlink interval from an untrusted same-host user.
    match fs::symlink_metadata(path) {
        Ok(current)
            if current.file_type().is_socket()
                && current.dev() == observed.dev()
                && current.ino() == observed.ino() =>
        {
            fs::remove_file(path)
                .map_err(|source| VmError::PrepareRuntimePath {
                    path: path.to_owned(),
                    source,
                })
                .report()
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(current) if current.file_type().is_socket() => {
            Err(Report::new(VmError::RuntimeSocketChanged(path.to_owned())))
        }
        Ok(_) => Err(Report::new(VmError::UnsafeRuntimeSocketPath(
            path.to_owned(),
        ))),
        Err(source) => Err(Report::new(VmError::PrepareRuntimePath {
            path: path.to_owned(),
            source,
        })),
    }
}

/// Outcome of attempting to remove one managed socket node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SocketCleanupOutcome {
    RemovedOrAbsent,
    Preserved,
}

/// Removes a stale socket while preserving live or unclassifiable nodes.
fn cleanup_socket(path: &Path) -> SocketCleanupOutcome {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return SocketCleanupOutcome::RemovedOrAbsent;
        }
        Err(error) => {
            warn!(path = %path.display(), %error, "failed to inspect managed runtime socket");
            return SocketCleanupOutcome::Preserved;
        }
    };
    if !metadata.file_type().is_socket() {
        warn!(path = %path.display(), "refusing to remove non-socket runtime path");
        return SocketCleanupOutcome::Preserved;
    }
    match probe_existing_socket(path, &metadata) {
        Ok(()) => SocketCleanupOutcome::RemovedOrAbsent,
        Err(error) => {
            warn!(path = %path.display(), %error, "failed to clean up managed runtime socket");
            SocketCleanupOutcome::Preserved
        }
    }
}

/// Removes one managed regular file without following links.
fn cleanup_managed_file(path: &Path, artifact: &'static str) {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            warn!(path = %path.display(), %error, %artifact, "failed to inspect managed runtime file");
            return;
        }
    };
    if !metadata.file_type().is_file() {
        warn!(path = %path.display(), %artifact, "refusing to remove non-regular runtime path");
        return;
    }
    if let Err(error) = fs::remove_file(path) {
        warn!(path = %path.display(), %error, %artifact, "failed to remove managed runtime file");
    }
}

/// Removes every managed runtime artifact.
fn cleanup_runtime(config: &VmConfig) {
    cleanup_runtime_sockets(config);
    cleanup_runtime_directory(config);
}

/// Removes managed sockets and their companion PID files.
fn cleanup_runtime_sockets(config: &VmConfig) {
    cleanup_socket(config.control_socket());
    if let Some(path) = config.qmp_socket() {
        cleanup_socket(path);
    }
    for directory in config.shared_directories() {
        if !directory.socket_path().as_os_str().is_empty()
            && cleanup_socket(directory.socket_path()) == SocketCleanupOutcome::RemovedOrAbsent
        {
            cleanup_managed_file(&directory.pid_file_path(), "virtiofsd PID file");
        }
    }
}

/// Removes the runtime directory only when no artifacts remain.
fn cleanup_runtime_directory(config: &VmConfig) {
    match fs::remove_dir(config.runtime_directory()) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(error) => {
            warn!(
                path = %config.runtime_directory().display(),
                %error,
                "failed to remove VM runtime directory"
            );
        }
    }
}

/// Classifies control-socket connection errors that may resolve before timeout.
fn is_transient_connect_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

/// Sends a Unix signal after safely converting the child identifier.
fn send_signal(pid: u32, signal: Signal) -> Result<(), Report<VmError>> {
    let pid_i32 = i32::try_from(pid)
        .map_err(|_| VmError::Signal {
            pid,
            source: io::Error::new(io::ErrorKind::InvalidInput, "process id exceeds i32"),
        })
        .report()?;
    signal::kill(Pid::from_raw(pid_i32), signal)
        .map_err(|error| VmError::Signal {
            pid,
            source: io::Error::from_raw_os_error(error as i32),
        })
        .report()
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    use std::thread;

    use tempfile::TempDir;
    use tokio::io::AsyncReadExt as _;

    use super::*;
    use crate::Acceleration;
    use crate::Architecture;
    #[cfg(target_os = "linux")]
    use crate::SharedDirectory;
    use crate::VmConfigBuilder;

    const TEST_DATA_DISK_SIZE: u64 = 1024 * 1024;

    fn private_temp_dir() -> TempDir {
        let temp = TempDir::new().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        temp
    }

    fn config_builder(temp: &TempDir, executable: &str) -> VmConfigBuilder {
        let disk = temp.path().join("system.erofs");
        File::create(&disk).unwrap();
        let data_disk = temp.path().join("data.raw");
        let kernel = temp.path().join("kernel");
        File::create(&kernel).unwrap();
        let initrd = temp.path().join("initrd");
        File::create(&initrd).unwrap();
        VmConfig::builder()
            .architecture(Architecture::X86_64)
            .qemu_binary(executable)
            .system_disk_image(disk)
            .data_disk(data_disk, TEST_DATA_DISK_SIZE)
            .direct_kernel_boot(kernel, initrd, "init=/nix/store/system/init")
            .runtime_directory(temp.path().join("run"))
            .acceleration(Acceleration::Tcg)
            .startup_timeout(Duration::from_millis(200))
            .shutdown_timeout(Duration::from_millis(100))
    }

    fn config(temp: &TempDir, executable: &str) -> VmConfig {
        config_builder(temp, executable).build().unwrap()
    }

    fn fake_qemu_wrapper(temp: &TempDir) -> String {
        fake_qemu_wrapper_for(temp, "vm::tests::fake_qemu_process")
    }

    fn fake_qemu_wrapper_for(temp: &TempDir, helper_test: &str) -> String {
        let socket = temp.path().join("run/control.sock");
        let executable = std::env::current_exe().unwrap();
        let wrapper = temp.path().join("fake-qemu");
        write_executable_wrapper(
            &wrapper,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'tascarrel-test-qemu 1.0'; exit 0; fi\nTASCARREL_VM_FAKE_SOCKET={} exec {} --ignored --exact {}\n",
                shell_quote(socket.as_os_str()),
                shell_quote(executable.as_os_str()),
                shell_quote(helper_test.as_ref())
            ),
        )
    }

    #[cfg(target_os = "linux")]
    fn fake_virtiofsd_wrapper(temp: &TempDir) -> String {
        fake_virtiofsd_wrapper_for(temp, "vm::tests::fake_virtiofsd_process")
    }

    #[cfg(target_os = "linux")]
    fn fake_virtiofsd_wrapper_for(temp: &TempDir, helper_test: &str) -> String {
        let executable = std::env::current_exe().unwrap();
        let wrapper = temp.path().join("fake-virtiofsd");
        write_executable_wrapper(
            &wrapper,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'tascarrel-test-virtiofsd 1.0'; exit 0; fi\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --socket-path) TASCARREL_VM_FAKE_VIRTIOFSD_SOCKET=$2; export TASCARREL_VM_FAKE_VIRTIOFSD_SOCKET; shift 2 ;;\n    *) shift ;;\n  esac\ndone\nexec {} --ignored --exact {}\n",
                shell_quote(executable.as_os_str()),
                shell_quote(helper_test.as_ref())
            ),
        )
    }

    fn sleeping_wrapper(temp: &TempDir) -> String {
        let wrapper = temp.path().join("sleeping-qemu");
        write_executable_wrapper(
            &wrapper,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'tascarrel-test-qemu 1.0'; exit 0; fi\nexec sleep 10\n",
        )
    }

    fn exiting_wrapper(temp: &TempDir) -> String {
        let wrapper = temp.path().join("exiting-qemu");
        write_executable_wrapper(
            &wrapper,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'tascarrel-test-executable 1.0'; exit 0; fi\nexit 0\n",
        )
    }

    fn write_executable_wrapper(path: &Path, script: &str) -> String {
        let staging = path.with_extension("new");
        let mut file = File::create(&staging).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        let mut permissions = file.metadata().unwrap().permissions();
        permissions.set_mode(0o755);
        file.flush().unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::set_permissions(&staging, permissions).unwrap();
        fs::rename(staging, path).unwrap();
        // Some overlay-backed test sandboxes briefly retain the writable
        // staging inode after rename and otherwise report ETXTBSY on exec.
        thread::sleep(Duration::from_millis(25));
        path.to_string_lossy().into_owned()
    }

    fn shell_quote(value: &std::ffi::OsStr) -> String {
        format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
    }

    async fn wait_for_path_removal(path: &Path) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    /// Emulates a QEMU process that keeps the control connection open.
    #[test]
    #[ignore = "helper process launched by the lifecycle test"]
    fn fake_qemu_process() {
        let socket = std::env::var_os("TASCARREL_VM_FAKE_SOCKET").unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        let (_stream, _) = listener.accept().unwrap();
        loop {
            thread::park();
        }
    }

    /// Emulates a QEMU process that writes to its serial console before
    /// accepting the control connection.
    #[test]
    #[ignore = "helper process launched by the serial-output test"]
    fn fake_qemu_with_serial_output() {
        std::io::stdout().write_all(b"guest booted\n").unwrap();
        std::io::stdout().flush().unwrap();
        fake_qemu_process();
    }

    /// Emulates QEMU exiting immediately after accepting the control channel.
    #[test]
    #[ignore = "helper process launched by the shutdown regression test"]
    fn fake_qemu_exits_after_connect() {
        let socket = std::env::var_os("TASCARREL_VM_FAKE_SOCKET").unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        let (_stream, _) = listener.accept().unwrap();
    }

    /// Exposes QEMU serial output as an asynchronous reader during startup.
    #[tokio::test]
    async fn streams_serial_output_while_the_vm_starts() {
        let temp = private_temp_dir();
        let executable = fake_qemu_wrapper_for(&temp, "vm::tests::fake_qemu_with_serial_output");
        let mut spawn = Vm::spawn(config(&temp, &executable));
        let mut serial = spawn
            .take_serial_output()
            .expect("new VM spawn has serial output");
        let serial_task = tokio::spawn(async move {
            let mut output = Vec::new();
            let mut bytes = [0_u8; 256];
            while !output
                .windows(b"guest booted\n".len())
                .any(|window| window == b"guest booted\n")
            {
                let length = serial.read(&mut bytes).await.unwrap();
                assert_ne!(length, 0, "serial output ended before the fixture line");
                output.extend_from_slice(&bytes[..length]);
                assert!(
                    output.len() <= 4096,
                    "fixture serial output is unexpectedly large"
                );
            }
            output
        });

        let mut vm = spawn.await.unwrap();
        assert!(
            serial_task
                .await
                .unwrap()
                .windows(b"guest booted\n".len())
                .any(|window| window == b"guest booted\n")
        );
        let _ = vm.shutdown().await.unwrap();
    }

    /// Emulates virtiofsd by creating its configured vhost-user socket.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "helper process launched by the virtiofs lifecycle test"]
    fn fake_virtiofsd_process() {
        let socket = std::env::var_os("TASCARREL_VM_FAKE_VIRTIOFSD_SOCKET").unwrap();
        File::create(Path::new(&socket).with_extension("sock.pid")).unwrap();
        let _listener = UnixListener::bind(socket).unwrap();
        loop {
            thread::park();
        }
    }

    /// Emulates a virtiofsd backend that fails after initially becoming ready.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "helper process launched by the virtiofs health test"]
    fn fake_virtiofsd_exits_after_ready() {
        let socket = std::env::var_os("TASCARREL_VM_FAKE_VIRTIOFSD_SOCKET").unwrap();
        let _listener = UnixListener::bind(socket).unwrap();
        thread::sleep(Duration::from_millis(500));
    }

    /// Starts, owns, and cleans the virtiofsd backend with the VM lifecycle.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn manages_virtiofsd_with_the_qemu_process() {
        let temp = private_temp_dir();
        let qemu = fake_qemu_wrapper(&temp);
        let virtiofsd = fake_virtiofsd_wrapper(&temp);
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let config = config_builder(&temp, &qemu)
            .virtiofsd_binary(virtiofsd)
            .shared_directory(SharedDirectory::read_write(&shared, "source"))
            .build()
            .unwrap();
        let backend_socket = config.shared_directories()[0].socket_path().to_owned();
        let backend_pid_file = config.shared_directories()[0].pid_file_path();

        let mut vm = Vm::spawn(config).await.unwrap();

        assert!(backend_socket.exists());
        assert!(backend_pid_file.exists());
        let _outcome = vm.shutdown().await.unwrap();
        assert!(!backend_socket.exists());
        assert!(!backend_pid_file.exists());
        assert!(!temp.path().join("run").exists());
    }

    /// Reports executable details and selects 9p when virtiofsd is unavailable.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn preflight_falls_back_to_9p_without_virtiofsd() {
        let temp = private_temp_dir();
        let qemu = fake_qemu_wrapper(&temp);
        let unavailable_virtiofsd = temp.path().join("missing-virtiofsd");
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let config = config_builder(&temp, &qemu)
            .virtiofsd_binary(&unavailable_virtiofsd)
            .shared_directory(SharedDirectory::read_write(&shared, "source"))
            .build()
            .unwrap();

        let mut vm = Vm::spawn(config).await.unwrap();
        let report = vm.preflight_report();

        assert_eq!(report.qemu().version(), Some("tascarrel-test-qemu 1.0"));
        assert_eq!(
            report.qemu().resolved_path(),
            Some(fs::canonicalize(&qemu).unwrap().as_path())
        );
        let virtiofsd = report.virtiofsd().unwrap();
        assert_eq!(virtiofsd.requested_path(), unavailable_virtiofsd);
        assert!(!virtiofsd.is_available());
        assert!(virtiofsd.failure().is_some());
        assert_eq!(
            report.shared_directory_transport(),
            SharedDirectoryTransport::Virtio9p
        );
        assert_eq!(
            vm.config().shared_directory_transport(),
            SharedDirectoryTransport::Virtio9p
        );

        let _outcome = vm.shutdown().await.unwrap();
    }

    /// Stops the VM if a required virtiofsd backend fails while it is running.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stops_qemu_when_virtiofsd_exits() {
        let temp = private_temp_dir();
        let qemu = fake_qemu_wrapper(&temp);
        let virtiofsd =
            fake_virtiofsd_wrapper_for(&temp, "vm::tests::fake_virtiofsd_exits_after_ready");
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let config = config_builder(&temp, &qemu)
            .virtiofsd_binary(virtiofsd)
            .shared_directory(SharedDirectory::read_write(&shared, "source"))
            .build()
            .unwrap();
        let backend_socket = config.shared_directories()[0].socket_path().to_owned();
        let vm = Vm::spawn(config).await.unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            while vm.id().is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        assert!(!backend_socket.exists());
    }

    /// Reports a backend that exits before creating its vhost-user socket.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn rejects_virtiofsd_that_exits_before_readiness() {
        let temp = private_temp_dir();
        let qemu = fake_qemu_wrapper(&temp);
        let virtiofsd = exiting_wrapper(&temp);
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let config = config_builder(&temp, &qemu)
            .virtiofsd_binary(virtiofsd)
            .shared_directory(SharedDirectory::read_only(&shared, "source"))
            .build()
            .unwrap();

        let error = Vm::spawn(config).await.unwrap_err();

        assert!(
            matches!(
                error.error(),
                VmError::VirtiofsdExitedBeforeReady { mount_tag, .. }
                    if mount_tag == "source"
            ),
            "unexpected startup error: {error}"
        );
    }

    /// Grows the managed data disk and preserves consumer-owned runtime
    /// artifacts.
    #[tokio::test]
    async fn connects_shuts_down_and_preserves_consumer_runtime_artifacts() {
        let temp = private_temp_dir();
        let executable = fake_qemu_wrapper(&temp);
        let config = config(&temp, &executable);
        let data_disk = config.data_disk_image().to_owned();
        File::create(&data_disk)
            .unwrap()
            .set_len(TEST_DATA_DISK_SIZE / 2)
            .unwrap();
        let mut vm = Vm::spawn(config).await.unwrap();
        let consumer_artifact = vm.config().runtime_directory().join("consumer.marker");
        File::create(&consumer_artifact).unwrap();
        assert_eq!(fs::metadata(data_disk).unwrap().len(), TEST_DATA_DISK_SIZE);
        assert!(vm.id().is_some());
        let control = vm.take_control_stream().unwrap();
        drop(control);

        let outcome = vm.shutdown().await.unwrap();
        assert!(!outcome.was_forced());
        assert!(vm.id().is_none());
        assert!(consumer_artifact.exists());
    }

    /// Treats an already-exited connected child as a graceful shutdown.
    #[tokio::test]
    async fn shutdown_handles_a_child_that_exited_after_connecting() {
        let temp = private_temp_dir();
        let executable = fake_qemu_wrapper_for(&temp, "vm::tests::fake_qemu_exits_after_connect");
        let mut vm = Vm::spawn(config(&temp, &executable)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;

        let outcome = vm.shutdown().await.unwrap();

        assert!(!outcome.was_forced());
        assert!(vm.id().is_none());
    }

    /// Enforces the startup deadline and removes resources from failed startup.
    #[tokio::test]
    async fn enforces_readiness_timeout_and_reaps_process() {
        let temp = private_temp_dir();
        let executable = sleeping_wrapper(&temp);
        let result = Vm::spawn(config(&temp, &executable)).await;
        assert!(
            matches!(
                result,
                Err(ref error) if matches!(error.error(), VmError::ReadinessTimeout { .. })
            ),
            "unexpected startup result: {result:?}"
        );
        assert!(!temp.path().join("run").exists());
    }

    /// Stops the child and removes runtime artifacts when the VM handle drops.
    #[tokio::test]
    async fn drop_stops_process_and_removes_socket() {
        let temp = private_temp_dir();
        let executable = fake_qemu_wrapper(&temp);
        let vm = Vm::spawn(config(&temp, &executable)).await.unwrap();
        let runtime_directory = temp.path().join("run");
        let socket = runtime_directory.join("control.sock");
        assert!(socket.exists());
        drop(vm);
        wait_for_path_removal(&runtime_directory).await;
    }

    /// Cleans runtime artifacts when a completed child handle is dropped.
    #[tokio::test]
    async fn drop_cleans_up_a_child_that_already_exited() {
        let temp = private_temp_dir();
        let executable = fake_qemu_wrapper_for(&temp, "vm::tests::fake_qemu_exits_after_connect");
        let vm = Vm::spawn(config(&temp, &executable)).await.unwrap();
        let runtime_directory = temp.path().join("run");
        tokio::time::sleep(Duration::from_millis(25)).await;
        drop(vm);

        wait_for_path_removal(&runtime_directory).await;
    }

    /// Reports QEMU exit that occurs before control-channel readiness.
    #[tokio::test]
    async fn reports_exit_before_readiness_without_qemu() {
        let temp = private_temp_dir();
        let executable = exiting_wrapper(&temp);
        let result = Vm::spawn(config(&temp, &executable)).await;
        assert!(matches!(
            result,
            Err(error) if matches!(error.error(), VmError::ExitedBeforeReady { .. })
        ));
    }

    /// Refuses to replace a regular file at a managed socket path.
    #[tokio::test]
    async fn refuses_to_replace_regular_control_file() {
        let temp = private_temp_dir();
        let executable = fake_qemu_wrapper(&temp);
        let runtime_directory = temp.path().join("run");
        fs::create_dir(&runtime_directory).unwrap();
        fs::set_permissions(&runtime_directory, fs::Permissions::from_mode(0o700)).unwrap();
        File::create(runtime_directory.join("control.sock")).unwrap();
        let result = Vm::spawn(config(&temp, &executable)).await;
        assert!(matches!(
            result,
            Err(error) if matches!(error.error(), VmError::UnsafeRuntimeSocketPath(_))
        ));
    }

    /// Preserves and reports a managed socket with an active listener.
    #[test]
    fn preserves_live_control_socket() {
        let temp = private_temp_dir();
        let socket = temp.path().join("control.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        let result = prepare_runtime_socket(&socket);

        assert!(matches!(
            result,
            Err(error) if matches!(error.error(), VmError::RuntimeSocketInUse(_))
        ));
        assert!(socket.exists());
    }

    /// Removes a stale socket after confirming that it has no listener.
    #[test]
    fn removes_only_stale_control_socket() {
        let temp = private_temp_dir();
        let socket = temp.path().join("control.sock");
        drop(UnixListener::bind(&socket).unwrap());
        thread::sleep(Duration::from_millis(25));

        prepare_runtime_socket(&socket).unwrap();

        assert!(!socket.exists());
    }

    /// Preserves a symbolic link placed at a managed socket path.
    #[test]
    fn preserves_symlink_control_path() {
        let temp = private_temp_dir();
        let target = temp.path().join("target.sock");
        let _listener = UnixListener::bind(&target).unwrap();
        let socket = temp.path().join("control.sock");
        symlink(&target, &socket).unwrap();

        let result = prepare_runtime_socket(&socket);

        assert!(matches!(
            result,
            Err(error) if matches!(error.error(), VmError::UnsafeRuntimeSocketPath(_))
        ));
        assert!(
            fs::symlink_metadata(&socket)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// Creates private descendants without changing existing ancestor modes.
    #[test]
    fn creates_private_runtime_directory_without_chmodding_ancestors() {
        let temp = private_temp_dir();
        let existing = temp.path().join("existing");
        fs::create_dir(&existing).unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o755)).unwrap();
        let private = existing.join("private/nested");

        prepare_runtime_directory(&private).unwrap();

        assert_eq!(
            fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(existing.join("private"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(private).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    /// Rejects an existing runtime directory with group or other access.
    #[test]
    fn rejects_non_private_runtime_directory() {
        let temp = private_temp_dir();
        let parent = temp.path().join("shared");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();

        let result = prepare_runtime_directory(&parent);

        assert!(matches!(
            result,
            Err(error) if matches!(error.error(), VmError::UnsafeRuntimeDirectory(_))
        ));
    }
}
