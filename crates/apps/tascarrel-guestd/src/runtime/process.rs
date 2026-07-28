//! Process launching and observation for local and runc-backed execution.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use nix::errno::Errno;
use nix::pty::Winsize as NixWinsize;
use nix::pty::openpty;
use nix::sys::signal::Signal as NixSignal;
use nix::sys::signal::killpg;
use nix::unistd::Gid;
use nix::unistd::Pid;
use nix::unistd::Uid;
use rustix::termios::Winsize as RustixWinsize;
use rustix::termios::tcsetwinsize;
use serde::Serialize;
use tascarrel_protocol::ErrorCode;
use tascarrel_protocol::ExecRequest;
use tascarrel_protocol::ExitStatus;
use tascarrel_protocol::OutputStream;
use tascarrel_protocol::RemoteError;
use tascarrel_protocol::Signal as ProtocolSignal;
use tascarrel_protocol::TerminalSize;
use tempfile::NamedTempFile;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::debug;
use tracing::warn;

use crate::services::pods::PodExecution;

const MAX_ARG_COUNT: usize = 256;
const MAX_ARG_LEN: usize = 4096;
const MAX_SETUP_ARG_LEN: usize = 64 * 1024;
const MAX_ENV_COUNT: usize = 128;
const MAX_ENV_BYTES: usize = 64 * 1024;
const MAX_WORKING_DIRECTORY_LEN: usize = 4096;
const OUTPUT_CHUNK_LEN: usize = 16 * 1024;
const INPUT_QUEUE_CAPACITY: usize = 8;
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_PATH: &str =
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/run/current-system/sw/bin";
const DOCKER_HOST: &str = "unix:///run/docker.sock";
const NIX_CONFIG: &str = "experimental-features = nix-command flakes";
const NIX_REMOTE: &str = "daemon";
const TASCARREL_NIX_GCROOTS: &str = "TASCARREL_NIX_GCROOTS";
const ROOTLESS_CONTAINER_CAPABILITIES: [&str; 2] = ["CAP_SETGID", "CAP_SETUID"];
const SETUP_CAPABILITIES: [&str; 14] = [
    "CAP_AUDIT_WRITE",
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_MKNOD",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_RAW",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYS_CHROOT",
];
const SYSTEM_SERVICE_CAPABILITIES: [&str; 26] = [
    "CAP_AUDIT_WRITE",
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_KILL",
    "CAP_LEASE",
    "CAP_LINUX_IMMUTABLE",
    "CAP_MKNOD",
    "CAP_NET_ADMIN",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_RAW",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYS_ADMIN",
    "CAP_SYS_CHROOT",
    "CAP_SYS_NICE",
    "CAP_SYS_PTRACE",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TTY_CONFIG",
];

type BoxReader = Pin<Box<dyn AsyncRead + Send>>;
type BoxWriter = Pin<Box<dyn AsyncWrite + Send>>;

pub(crate) struct PreparedExec {
    child: Child,
    process_group: Pid,
    input: Option<BoxWriter>,
    outputs: Vec<(OutputStream, BoxReader)>,
    resize_fd: Option<OwnedFd>,
    // runc opens `--process` after it starts. Retain the securely-created
    // specification until the wrapper exits so its pathname cannot disappear
    // between Command::spawn and runc's open.
    _process_spec: Option<NamedTempFile>,
}

struct ProcessInvocation {
    program: PathBuf,
    arguments: Vec<OsString>,
    process_spec: Option<NamedTempFile>,
}

#[derive(Clone, Copy)]
enum ExecutionProfile {
    User,
    Setup,
    SystemService,
}

#[derive(serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OciExecProcess {
    terminal: bool,
    user: OciExecUser,
    args: Vec<String>,
    env: Vec<String>,
    cwd: String,
    capabilities: OciCapabilities,
    no_new_privileges: bool,
    apparmor_profile: String,
}

#[derive(serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OciExecUser {
    uid: u32,
    gid: u32,
    additional_gids: Vec<u32>,
}

#[derive(Default, serde::Deserialize, Serialize)]
struct OciCapabilities {
    bounding: Vec<String>,
    effective: Vec<String>,
    inheritable: Vec<String>,
    permitted: Vec<String>,
    ambient: Vec<String>,
}

#[derive(Debug)]
pub(crate) enum ExecControl {
    Input(Vec<u8>),
    Resize(TerminalSize),
    Signal(ProtocolSignal),
}

/// Output and completion observed while supervising a prepared process.
#[derive(Debug)]
pub(crate) enum ObservedProcessEvent {
    Output { stream: OutputStream, data: Vec<u8> },
    Finished(Result<ExitStatus, String>),
}

/// Configures and launches processes owned by a pod user.
#[derive(Debug, Clone)]
pub struct Executor {
    setsid: PathBuf,
}

impl Default for Executor {
    fn default() -> Self {
        Self::new("setsid")
    }
}

impl Executor {
    #[must_use]
    pub fn new(setsid: impl Into<PathBuf>) -> Self {
        Self {
            setsid: setsid.into(),
        }
    }

    pub(crate) fn prepare(
        &self,
        account: &PodExecution,
        request: &ExecRequest,
    ) -> Result<PreparedExec, RemoteError> {
        validate_request(request)?;
        match request.terminal {
            Some(size) => self.prepare_terminal(account, request, size, ExecutionProfile::User),
            None => Self::prepare_pipes(account, request, ExecutionProfile::User),
        }
    }

    /// Validates a process request without launching it.
    pub(crate) fn validate(request: &ExecRequest) -> Result<(), RemoteError> {
        validate_request(request)
    }

    /// Validates an internal setup request, whose script argument may use the
    /// workspace configuration's larger bounded script size.
    pub(crate) fn validate_setup(request: &ExecRequest) -> Result<(), RemoteError> {
        validate_request_with_max_arg(request, MAX_SETUP_ARG_LEN)
    }

    /// Prepares a trusted infrastructure process with pod-policy privileges.
    pub(crate) fn prepare_system_service(
        &self,
        account: &PodExecution,
        request: &ExecRequest,
    ) -> Result<PreparedExec, RemoteError> {
        validate_request(request)?;
        let account = system_service_account(account);
        match request.terminal {
            Some(size) => {
                self.prepare_terminal(&account, request, size, ExecutionProfile::SystemService)
            }
            None => Self::prepare_pipes(&account, request, ExecutionProfile::SystemService),
        }
    }

    /// Prepares a trusted, non-interactive workspace setup process as root
    /// with the narrower setup capability set.
    pub(crate) fn prepare_setup(
        account: &PodExecution,
        request: &ExecRequest,
    ) -> Result<PreparedExec, RemoteError> {
        Self::validate_setup(request)?;
        let account = system_service_account(account);
        Self::prepare_pipes(&account, request, ExecutionProfile::Setup)
    }

    fn prepare_pipes(
        account: &PodExecution,
        request: &ExecRequest,
        profile: ExecutionProfile,
    ) -> Result<PreparedExec, RemoteError> {
        let invocation = process_invocation(account, request, false, profile)?;
        let mut command = Command::new(&invocation.program);
        command.args(&invocation.arguments);
        configure_command(&mut command, account, request)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = command.spawn().map_err(|error| spawn_error(&error))?;
        let process_group = child_process_group(&child)?;
        let input = child
            .stdin
            .take()
            .map(|stream| Box::pin(stream) as BoxWriter);
        let stdout = child.stdout.take().ok_or_else(|| {
            RemoteError::new(ErrorCode::Internal, "child stdout pipe was not created")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            RemoteError::new(ErrorCode::Internal, "child stderr pipe was not created")
        })?;
        Ok(PreparedExec {
            child,
            process_group,
            input,
            outputs: vec![
                (OutputStream::Stdout, Box::pin(stdout)),
                (OutputStream::Stderr, Box::pin(stderr)),
            ],
            resize_fd: None,
            _process_spec: invocation.process_spec,
        })
    }

    fn prepare_terminal(
        &self,
        account: &PodExecution,
        request: &ExecRequest,
        size: TerminalSize,
        profile: ExecutionProfile,
    ) -> Result<PreparedExec, RemoteError> {
        let winsize = NixWinsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&winsize), None).map_err(|error| {
            RemoteError::new(
                ErrorCode::ExecutionFailed,
                format!("could not allocate pseudo-terminal: {error}"),
            )
        })?;
        let resize_fd = pty
            .master
            .try_clone()
            .map_err(|error| internal_io_error(&error))?;
        let stdin_fd = pty
            .slave
            .try_clone()
            .map_err(|error| internal_io_error(&error))?;
        let stdout_fd = pty
            .slave
            .try_clone()
            .map_err(|error| internal_io_error(&error))?;

        let invocation = process_invocation(account, request, true, profile)?;
        // `setsid --ctty` safely establishes the slave as the controlling
        // terminal without a post-fork callback in this multi-threaded daemon.
        let mut command = Command::new(&self.setsid);
        command
            .arg("--ctty")
            .arg("--wait")
            .arg("--")
            .arg(&invocation.program)
            .args(&invocation.arguments);
        configure_command(&mut command, account, request)?;
        command
            .stdin(Stdio::from(stdin_fd))
            .stdout(Stdio::from(stdout_fd))
            .stderr(Stdio::from(pty.slave))
            .kill_on_drop(true);
        let child = command.spawn().map_err(|error| spawn_error(&error))?;
        let process_group = child_process_group(&child)?;

        let master_writer = pty
            .master
            .try_clone()
            .map_err(|error| internal_io_error(&error))?;
        let reader = tokio::fs::File::from_std(std::fs::File::from(pty.master));
        let writer = tokio::fs::File::from_std(std::fs::File::from(master_writer));
        Ok(PreparedExec {
            child,
            process_group,
            input: Some(Box::pin(writer)),
            outputs: vec![(OutputStream::Terminal, Box::pin(reader))],
            resize_fd: Some(resize_fd),
            _process_spec: invocation.process_spec,
        })
    }
}

/// Derives the in-pod root identity used by trusted infrastructure services.
fn system_service_account(account: &PodExecution) -> PodExecution {
    let mut account = account.clone();
    account.home = PathBuf::from("/root");
    "root".clone_into(&mut account.user);
    account.uid = 0;
    account.gid = 0;
    if let Some(container) = account.container.as_mut() {
        container.uid = 0;
        container.gid = 0;
        container.additional_gids.clear();
    }
    account
}

fn process_invocation(
    account: &PodExecution,
    request: &ExecRequest,
    terminal: bool,
    profile: ExecutionProfile,
) -> Result<ProcessInvocation, RemoteError> {
    let (program, arguments) = if request.argv.is_empty() {
        (account.shell.as_os_str(), vec![OsString::from("-l")])
    } else {
        (
            OsStr::new(&request.argv[0]),
            request.argv[1..].iter().map(OsString::from).collect(),
        )
    };
    let Some(container) = &account.container else {
        return Ok(ProcessInvocation {
            program: PathBuf::from(program),
            arguments,
            process_spec: None,
        });
    };

    let mut runc = vec![
        OsString::from("--root"),
        container.root.as_os_str().to_owned(),
    ];
    if container.systemd_cgroup {
        runc.push(OsString::from("--systemd-cgroup"));
    }
    runc.push(OsString::from("exec"));
    let working_directory = container_working_directory(account, request);
    let mut environment = default_container_environment(account);
    environment.extend(request.env.clone());
    let executable = program.to_str().ok_or_else(|| {
        RemoteError::new(
            ErrorCode::Internal,
            "container executable path is not valid UTF-8",
        )
    })?;
    let mut process_arguments = vec![executable.to_owned()];
    for argument in arguments {
        process_arguments.push(
            argument
                .to_str()
                .ok_or_else(|| {
                    RemoteError::new(
                        ErrorCode::Internal,
                        "container command argument is not valid UTF-8",
                    )
                })?
                .to_owned(),
        );
    }
    let cwd = working_directory.to_str().ok_or_else(|| {
        RemoteError::new(
            ErrorCode::Internal,
            "container working directory is not valid UTF-8",
        )
    })?;
    let process = OciExecProcess {
        terminal,
        user: OciExecUser {
            uid: container.uid,
            gid: container.gid,
            additional_gids: container.additional_gids.clone(),
        },
        args: process_arguments,
        env: environment
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect(),
        cwd: cwd.to_owned(),
        capabilities: exec_capabilities(container.policy, profile),
        no_new_privileges: matches!(profile, ExecutionProfile::User),
        apparmor_profile: container.policy.apparmor_profile().to_owned(),
    };
    let process_spec = write_process_spec(&process)?;
    runc.extend([
        OsString::from("--process"),
        process_spec.path().as_os_str().to_owned(),
        OsString::from(&container.id),
    ]);
    Ok(ProcessInvocation {
        program: container.runc.clone(),
        arguments: runc,
        process_spec: Some(process_spec),
    })
}

fn exec_capabilities(
    policy: crate::runtime::pod::PodPolicy,
    profile: ExecutionProfile,
) -> OciCapabilities {
    let capabilities =
        if matches!(profile, ExecutionProfile::SystemService) && policy.nested_containers() {
            SYSTEM_SERVICE_CAPABILITIES
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        } else if matches!(
            profile,
            ExecutionProfile::Setup | ExecutionProfile::SystemService
        ) {
            SETUP_CAPABILITIES
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        } else if policy.rootless_containers() {
            ROOTLESS_CONTAINER_CAPABILITIES
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
    OciCapabilities {
        bounding: capabilities.clone(),
        effective: capabilities.clone(),
        inheritable: capabilities.clone(),
        permitted: capabilities.clone(),
        ambient: capabilities,
    }
}

fn write_process_spec(process: &OciExecProcess) -> Result<NamedTempFile, RemoteError> {
    let mut file = tempfile::Builder::new()
        .prefix("tascarrel-exec-")
        .suffix(".json")
        .tempfile()
        .map_err(|error| process_spec_error("create", &error))?;
    serde_json::to_writer(file.as_file_mut(), process)
        .map_err(|error| process_spec_error("serialize", &error))?;
    file.as_file_mut()
        .write_all(b"\n")
        .map_err(|error| process_spec_error("write", &error))?;
    file.as_file_mut()
        .sync_all()
        .map_err(|error| process_spec_error("sync", &error))?;
    Ok(file)
}

fn process_spec_error(operation: &str, error: &dyn std::fmt::Display) -> RemoteError {
    RemoteError::new(
        ErrorCode::Internal,
        format!("could not {operation} secure container process specification: {error}"),
    )
}

fn container_working_directory(account: &PodExecution, request: &ExecRequest) -> PathBuf {
    let default = account.container.as_ref().map_or_else(
        || account.home.clone(),
        |container| container.working_directory.clone(),
    );
    request.working_directory.as_deref().map_or_else(
        || default.clone(),
        |path| {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                default.join(path)
            }
        },
    )
}

fn default_container_environment(account: &PodExecution) -> BTreeMap<String, String> {
    let mut environment = account
        .container
        .as_ref()
        .map_or_else(BTreeMap::new, |container| container.environment.clone());
    environment.insert("HOME".into(), account.home.to_string_lossy().into_owned());
    environment.insert("USER".into(), account.user.clone());
    environment.insert("LOGNAME".into(), account.user.clone());
    environment.insert("SHELL".into(), account.shell.to_string_lossy().into_owned());
    environment
        .entry("PATH".into())
        .or_insert_with(|| DEFAULT_PATH.into());
    if account
        .container
        .as_ref()
        .is_some_and(|container| container.policy.docker_daemon())
    {
        environment.insert("DOCKER_HOST".into(), DOCKER_HOST.into());
    }
    if let Some(container) = account
        .container
        .as_ref()
        .filter(|container| container.policy.nix_daemon())
    {
        let gc_root = container
            .nix_gc_root
            .as_ref()
            .expect("Nix-enabled container has a private GC-root directory");
        environment.insert("NIX_CONFIG".into(), NIX_CONFIG.into());
        environment.insert("NIX_REMOTE".into(), NIX_REMOTE.into());
        environment.insert(
            "NIX_STATE_HOME".into(),
            gc_root.join("state").to_string_lossy().into_owned(),
        );
        environment.insert(
            "NIX_PROFILE".into(),
            gc_root
                .join("state/profiles/profile")
                .to_string_lossy()
                .into_owned(),
        );
        environment.insert(
            TASCARREL_NIX_GCROOTS.into(),
            gc_root.join("roots").to_string_lossy().into_owned(),
        );
    }
    if let Some(container) = account
        .container
        .as_ref()
        .filter(|container| container.policy.rootless_containers())
    {
        environment.insert(
            "XDG_RUNTIME_DIR".into(),
            format!("/run/user/{}", container.uid),
        );
    }
    environment
}

fn configure_command(
    command: &mut Command,
    account: &PodExecution,
    request: &ExecRequest,
) -> Result<(), RemoteError> {
    let effective_identity = (Uid::effective().as_raw(), Gid::effective().as_raw());
    if account.container.is_some() {
        command.current_dir("/").env_clear();
        return Ok(());
    }
    if effective_identity.0 == 0 {
        // `CommandExt::uid` also schedules `setgroups(0, NULL)` when no
        // supplementary groups are specified. Keep the explicit UID switch:
        // it prevents guestd's root groups from reaching the child.
        command.uid(account.uid).gid(account.gid);
    } else if effective_identity != (account.uid, account.gid) {
        return Err(RemoteError::new(
            ErrorCode::PermissionDenied,
            "the daemon cannot switch to the requested pod user",
        ));
    }
    let working_directory = request
        .working_directory
        .as_deref()
        .map(PathBuf::from)
        .map_or_else(
            || account.home.clone(),
            |path| {
                if path.is_absolute() {
                    path
                } else {
                    account.home.join(path)
                }
            },
        );
    command
        .current_dir(working_directory)
        .env_clear()
        .env("HOME", &account.home)
        .env("USER", &account.user)
        .env("LOGNAME", &account.user)
        .env("SHELL", &account.shell)
        .env("PATH", DEFAULT_PATH)
        .envs(&request.env);
    Ok(())
}

fn validate_request(request: &ExecRequest) -> Result<(), RemoteError> {
    validate_request_with_max_arg(request, MAX_ARG_LEN)
}

fn validate_request_with_max_arg(
    request: &ExecRequest,
    max_arg_len: usize,
) -> Result<(), RemoteError> {
    if request.argv.len() > MAX_ARG_COUNT {
        return invalid(format!("argv may contain at most {MAX_ARG_COUNT} entries"));
    }
    if request.argv.first().is_some_and(String::is_empty)
        || request
            .argv
            .iter()
            .any(|arg| arg.len() > max_arg_len || arg.as_bytes().contains(&b'\0'))
    {
        return invalid(format!(
            "the program must be non-empty; arguments may contain at most {max_arg_len} bytes and no NUL byte"
        ));
    }
    if request.env.len() > MAX_ENV_COUNT {
        return invalid(format!(
            "environment may contain at most {MAX_ENV_COUNT} entries"
        ));
    }
    let env_bytes = request.env.iter().try_fold(0_usize, |total, (key, value)| {
        if !valid_env_key(key) || value.as_bytes().contains(&b'\0') {
            return None;
        }
        total.checked_add(key.len() + value.len())
    });
    if env_bytes.is_none_or(|bytes| bytes > MAX_ENV_BYTES) {
        return invalid("environment has an invalid name, NUL byte, or excessive size");
    }
    if request.working_directory.as_ref().is_some_and(|path| {
        path.len() > MAX_WORKING_DIRECTORY_LEN || path.as_bytes().contains(&b'\0')
    }) {
        return invalid("working directory is invalid or too long");
    }
    if request
        .terminal
        .is_some_and(|size| size.rows == 0 || size.cols == 0)
    {
        return invalid("terminal rows and columns must both be non-zero");
    }
    Ok(())
}

fn valid_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn invalid<T>(message: impl Into<String>) -> Result<T, RemoteError> {
    Err(RemoteError::new(ErrorCode::InvalidRequest, message))
}

fn spawn_error(error: &io::Error) -> RemoteError {
    RemoteError::new(
        ErrorCode::ExecutionFailed,
        format!("could not start process: {error}"),
    )
}

fn internal_io_error(error: &io::Error) -> RemoteError {
    RemoteError::new(
        ErrorCode::Internal,
        format!("file descriptor error: {error}"),
    )
}

fn child_process_group(child: &Child) -> Result<Pid, RemoteError> {
    child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .map(Pid::from_raw)
        .ok_or_else(|| {
            RemoteError::new(
                ErrorCode::ExecutionFailed,
                "spawned process did not expose a valid process-group ID",
            )
        })
}

/// Runs one prepared process and emits raw output followed by one terminal
/// result.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn run_observed(
    mut prepared: PreparedExec,
    mut controls: mpsc::Receiver<ExecControl>,
    events: mpsc::Sender<ObservedProcessEvent>,
) {
    let (input_controls, input_receiver) = mpsc::channel(INPUT_QUEUE_CAPACITY);
    let input_task = prepared
        .input
        .take()
        .map(|input| spawn_observed_input_writer(input, input_receiver));
    let mut output_tasks = prepared
        .outputs
        .drain(..)
        .map(|(stream, reader)| spawn_observed_output_reader(stream, reader, events.clone()))
        .collect::<Vec<_>>();

    let mut control_error = None;
    let status = loop {
        tokio::select! {
            result = prepared.child.wait() => break result,
            control = controls.recv() => {
                let Some(control) = control else {
                    terminate_process_group(
                        &mut prepared.child,
                        prepared.process_group,
                        NixSignal::SIGKILL,
                    );
                    break prepared.child.wait().await;
                };
                if let Err(error) = handle_observed_control(
                    &prepared,
                    &input_controls,
                    control,
                ) {
                    control_error = Some(error);
                    terminate_process_group(
                        &mut prepared.child,
                        prepared.process_group,
                        NixSignal::SIGKILL,
                    );
                    break prepared.child.wait().await;
                }
            }
        }
    };

    drop(input_controls);
    if let Some(task) = input_task {
        task.abort();
        if let Err(error) = task.await
            && !error.is_cancelled()
        {
            warn!(%error, "observed process input task failed while being stopped");
        }
    }
    if let Err(error) = killpg(prepared.process_group, NixSignal::SIGKILL)
        && error != Errno::ESRCH
    {
        warn!(%error, "could not reap the supervised process group");
    }
    let output_result = tokio::time::timeout(OUTPUT_DRAIN_TIMEOUT, async {
        for task in &mut output_tasks {
            task.await
                .map_err(|error| format!("process output task failed: {error}"))??;
        }
        Ok::<(), String>(())
    })
    .await;
    let output_error = match output_result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(_) => {
            for task in output_tasks {
                task.abort();
                if let Err(error) = task.await
                    && !error.is_cancelled()
                {
                    warn!(%error, "process output task failed while being stopped");
                }
            }
            Some("process output did not close after the process exited".to_owned())
        }
    };

    let result = match (status, control_error, output_error) {
        (_, Some(error), _) | (_, _, Some(error)) => Err(error),
        (Ok(status), None, None) => Ok(ExitStatus {
            code: status.code(),
            signal: status.signal(),
        }),
        (Err(error), None, None) => Err(format!("could not wait for process: {error}")),
    };
    if events
        .send(ObservedProcessEvent::Finished(result))
        .await
        .is_err()
    {
        warn!("process supervisor stopped before receiving process completion");
    }
}

/// Applies one supervisor control without coupling it to the wire protocol.
fn handle_observed_control(
    prepared: &PreparedExec,
    input: &mpsc::Sender<Vec<u8>>,
    control: ExecControl,
) -> Result<(), String> {
    match control {
        ExecControl::Input(data) => input
            .try_send(data)
            .map_err(|error| format!("process input queue is unavailable: {error}")),
        ExecControl::Resize(size) => match prepared.resize_fd.as_ref() {
            Some(fd) if size.rows != 0 && size.cols != 0 => tcsetwinsize(
                fd,
                RustixWinsize {
                    ws_row: size.rows,
                    ws_col: size.cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                },
            )
            .map_err(|error| format!("could not resize process terminal: {error}")),
            Some(_) => Err("terminal rows and columns must both be non-zero".to_owned()),
            None => Err("this execution does not have a terminal".to_owned()),
        },
        ExecControl::Signal(signal) => signal_process_group(prepared.process_group, signal),
    }
}

/// Forwards supervisor input controls to a prepared process.
fn spawn_observed_input_writer(
    mut input: BoxWriter,
    mut controls: mpsc::Receiver<Vec<u8>>,
) -> JoinHandle<Result<(), String>> {
    tokio::spawn(async move {
        while let Some(data) = controls.recv().await {
            input
                .write_all(&data)
                .await
                .map_err(|error| format!("could not write process input: {error}"))?;
        }
        Ok(())
    })
}

fn spawn_observed_output_reader(
    stream: OutputStream,
    mut reader: BoxReader,
    events: mpsc::Sender<ObservedProcessEvent>,
) -> JoinHandle<Result<(), String>> {
    tokio::spawn(async move {
        let mut buffer = vec![0; OUTPUT_CHUNK_LEN];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    if events
                        .send(ObservedProcessEvent::Output {
                            stream,
                            data: buffer[..count].to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        debug!("process supervisor stopped before receiving output");
                        return Ok(());
                    }
                }
                Err(error) if error.raw_os_error() == Some(Errno::EIO as i32) => return Ok(()),
                Err(error) => return Err(format!("could not read process output: {error}")),
            }
        }
    })
}

fn signal_process_group(process_group: Pid, signal: ProtocolSignal) -> Result<(), String> {
    let signal = match signal {
        ProtocolSignal::Hangup => NixSignal::SIGHUP,
        ProtocolSignal::Interrupt => NixSignal::SIGINT,
        ProtocolSignal::Terminate => NixSignal::SIGTERM,
        ProtocolSignal::Kill => NixSignal::SIGKILL,
    };
    match killpg(process_group, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(format!("could not signal process group: {error}")),
    }
}

fn terminate_process_group(child: &mut Child, process_group: Pid, signal: NixSignal) {
    if let Err(error) = killpg(process_group, signal) {
        debug!(%error, "could not signal supervised process group; falling back to child kill");
        if let Err(error) = child.start_kill() {
            warn!(%error, "could not terminate supervised process");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tascarrel_protocol::PodId;

    use super::*;
    use crate::services::pods::runc::ContainerExecution;

    fn request() -> ExecRequest {
        ExecRequest {
            pod_id: PodId("pod".into()),
            argv: vec!["/bin/true".into()],
            env: BTreeMap::new(),
            working_directory: None,
            terminal: None,
        }
    }

    fn container_account() -> PodExecution {
        PodExecution {
            user: "develop".into(),
            uid: 1_000_000,
            gid: 1_000_000,
            home: PathBuf::from("/home/develop"),
            shell: PathBuf::from("/bin/bash"),
            container: Some(ContainerExecution {
                runc: PathBuf::from("/nix/store/runc/bin/runc"),
                root: PathBuf::from("/run/tascarrel/runc"),
                systemd_cgroup: true,
                id: "pod-id".into(),
                uid: 1000,
                gid: 1000,
                additional_gids: vec![999],
                working_directory: PathBuf::from("/workspace"),
                environment: BTreeMap::from([
                    ("IMAGE_DEFAULT".into(), "from-dockerfile".into()),
                    ("PATH".into(), "/image/bin".into()),
                    ("TERM".into(), "image-term".into()),
                ]),
                policy: crate::runtime::pod::PodPolicy::default()
                    .with_docker_daemon(true)
                    .with_nix_daemon(true),
                nix_gc_root: Some(PathBuf::from("/nix/var/nix/gcroots/tascarrel/pods/pod-id")),
            }),
        }
    }

    fn process_specification(invocation: &ProcessInvocation) -> OciExecProcess {
        let specification = invocation
            .process_spec
            .as_ref()
            .expect("container invocation has an OCI process specification");
        assert_eq!(
            fs::metadata(specification.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        serde_json::from_slice(&fs::read(specification.path()).unwrap()).unwrap()
    }

    /// Verifies malformed environment, terminal, and argument metadata is
    /// rejected before process launch.
    #[test]
    fn rejects_invalid_execution_metadata() {
        let mut value = request();
        value.env.insert("BAD=KEY".into(), "x".into());
        assert_eq!(
            validate_request(&value).unwrap_err().code,
            ErrorCode::InvalidRequest
        );

        let mut value = request();
        value.terminal = Some(TerminalSize { rows: 0, cols: 80 });
        assert!(validate_request(&value).is_err());

        let mut value = request();
        value.argv = vec![String::new()];
        assert!(validate_request(&value).is_err());
    }

    /// Verifies an empty argument vector selects the account's login shell.
    #[test]
    fn accepts_an_empty_argv_as_the_login_shell() {
        let mut value = request();
        value.argv.clear();
        assert!(validate_request(&value).is_ok());
    }

    /// Verifies container executions use the configured runc state and OCI
    /// process metadata.
    #[test]
    fn container_execution_is_scoped_to_the_configured_runc_root() {
        let account = container_account();
        let mut request = request();
        request.argv = vec!["/usr/bin/env".into(), "hello".into()];
        request.working_directory = Some("src".into());
        request.env.insert("TERM".into(), "xterm-256color".into());
        let invocation =
            process_invocation(&account, &request, true, ExecutionProfile::User).unwrap();
        assert_eq!(
            invocation.program,
            PathBuf::from("/nix/store/runc/bin/runc")
        );
        let arguments = invocation
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        assert_eq!(
            &arguments[..5],
            [
                "--root",
                "/run/tascarrel/runc",
                "--systemd-cgroup",
                "exec",
                "--process"
            ]
        );
        assert_eq!(arguments.len(), 7);
        assert_eq!(
            arguments.last(),
            Some(&std::borrow::Cow::Borrowed("pod-id"))
        );
        assert_eq!(
            arguments[5],
            invocation
                .process_spec
                .as_ref()
                .unwrap()
                .path()
                .to_string_lossy()
        );

        let process = process_specification(&invocation);
        assert!(process.terminal);
        assert_eq!(process.cwd, "/workspace/src");
        assert_eq!(process.args, ["/usr/bin/env", "hello"]);
        assert_eq!(process.user.uid, 1000);
        assert_eq!(process.user.gid, 1000);
        assert_eq!(process.user.additional_gids, [999]);
        assert!(process.no_new_privileges);
        assert_eq!(process.apparmor_profile, "tascarrel-pod-containers");
        for set in [
            &process.capabilities.bounding,
            &process.capabilities.effective,
            &process.capabilities.inheritable,
            &process.capabilities.permitted,
            &process.capabilities.ambient,
        ] {
            assert!(set.is_empty());
        }
        let environment = process.env.iter().map(String::as_str).collect::<Vec<_>>();
        for expected in [
            "TERM=xterm-256color",
            "IMAGE_DEFAULT=from-dockerfile",
            "PATH=/image/bin",
            "HOME=/home/develop",
            "USER=develop",
            "LOGNAME=develop",
            "SHELL=/bin/bash",
            "DOCKER_HOST=unix:///run/docker.sock",
            "NIX_CONFIG=experimental-features = nix-command flakes",
            "NIX_REMOTE=daemon",
            "NIX_STATE_HOME=/nix/var/nix/gcroots/tascarrel/pods/pod-id/state",
            "NIX_PROFILE=/nix/var/nix/gcroots/tascarrel/pods/pod-id/state/profiles/profile",
            "TASCARREL_NIX_GCROOTS=/nix/var/nix/gcroots/tascarrel/pods/pod-id/roots",
        ] {
            assert!(environment.contains(&expected), "missing {expected}");
        }
        assert!(!environment.contains(&"TERM=image-term"));

        let mut cgroupfs_account = account;
        cgroupfs_account.container.as_mut().unwrap().systemd_cgroup = false;
        let invocation =
            process_invocation(&cgroupfs_account, &request, false, ExecutionProfile::User).unwrap();
        let arguments = &invocation.arguments;
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--systemd-cgroup")
        );
        assert_eq!(&arguments[..3], ["--root", "/run/tascarrel/runc", "exec"]);
        assert!(!process_specification(&invocation).terminal);
    }

    /// Verifies an empty container command uses the image shell and working
    /// directory.
    #[test]
    fn empty_container_exec_uses_the_image_login_shell_and_working_directory() {
        let mut request = request();
        request.argv.clear();
        let invocation =
            process_invocation(&container_account(), &request, true, ExecutionProfile::User)
                .unwrap();
        let process = process_specification(&invocation);
        assert_eq!(process.cwd, "/workspace");
        assert_eq!(process.args, ["/bin/bash", "-l"]);
        assert!(process.terminal);
        assert!(process.no_new_privileges);
    }

    /// Verifies virtualization retains the standard `AppArmor` profile for
    /// execs.
    #[test]
    fn standard_exec_uses_the_standard_apparmor_profile() {
        let mut account = container_account();
        account.container.as_mut().unwrap().policy =
            crate::runtime::pod::PodPolicy::default().with_virtualization(true);
        let invocation =
            process_invocation(&account, &request(), false, ExecutionProfile::User).unwrap();
        let process = process_specification(&invocation);

        assert_eq!(process.apparmor_profile, "tascarrel-pod");
    }

    /// Verifies rootless container execution grants only set-ID capabilities
    /// and supplies its private runtime directory.
    #[test]
    fn rootless_container_policy_grants_only_setid_and_runtime_directory() {
        let mut account = container_account();
        let container = account.container.as_mut().unwrap();
        container.policy = container.policy.with_podman(true);
        let invocation =
            process_invocation(&account, &request(), false, ExecutionProfile::User).unwrap();
        let process = process_specification(&invocation);
        assert!(process.no_new_privileges);
        assert_eq!(process.apparmor_profile, "tascarrel-pod-containers");
        let expected = ["CAP_SETGID", "CAP_SETUID"];
        for set in [
            &process.capabilities.bounding,
            &process.capabilities.effective,
            &process.capabilities.inheritable,
            &process.capabilities.permitted,
            &process.capabilities.ambient,
        ] {
            assert_eq!(set, &expected);
        }
        assert!(
            process
                .env
                .iter()
                .any(|entry| entry == "XDG_RUNTIME_DIR=/run/user/1000")
        );
    }

    /// Verifies trusted infrastructure processes receive the root identity
    /// and capabilities selected by the pod's nesting policy.
    #[test]
    fn system_service_profile_supplies_nested_runtime_privileges() {
        let account = system_service_account(&container_account());
        let invocation =
            process_invocation(&account, &request(), false, ExecutionProfile::SystemService)
                .unwrap();
        let process = process_specification(&invocation);

        assert_eq!(process.user.uid, 0);
        assert_eq!(process.user.gid, 0);
        assert!(process.user.additional_gids.is_empty());
        assert!(!process.no_new_privileges);
        assert_eq!(process.apparmor_profile, "tascarrel-pod-containers");
        let capabilities = process.capabilities.bounding;
        assert!(capabilities.iter().any(|value| value == "CAP_SYS_ADMIN"));
        assert!(!capabilities.iter().any(|value| value == "CAP_SYS_BOOT"));
        assert!(process.env.iter().any(|entry| entry == "HOME=/root"));
    }

    /// Verifies image setup retains its narrower capability profile even when
    /// nested containers are enabled for ordinary infrastructure services.
    #[test]
    fn setup_profile_uses_narrow_root_privileges() {
        let account = system_service_account(&container_account());
        let invocation =
            process_invocation(&account, &request(), false, ExecutionProfile::Setup).unwrap();
        let process = process_specification(&invocation);
        let capabilities = process.capabilities.bounding;

        assert_eq!(process.user.uid, 0);
        assert!(!process.no_new_privileges);
        assert!(capabilities.iter().any(|value| value == "CAP_CHOWN"));
        assert!(!capabilities.iter().any(|value| value == "CAP_SYS_ADMIN"));
    }

    /// Verifies validated workspace setup scripts may exceed the ordinary RPC
    /// argument bound without relaxing that public bound.
    #[test]
    fn setup_validation_has_a_separate_script_bound() {
        let mut request = request();
        request.argv.push("x".repeat(MAX_ARG_LEN + 1));

        assert!(Executor::validate(&request).is_err());
        assert!(Executor::validate_setup(&request).is_ok());
    }
}
