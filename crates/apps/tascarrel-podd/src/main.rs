//! Pod namespace init process and local connection handoff.
//!
//! The binary supervises pod initialization and forwards guestd requests into
//! pod loopback. It provisions the pod-private guestd listener and transfers
//! that listener to guestd without handling steady-state control traffic.

use std::fs::DirBuilder;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::IoSlice;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::chown;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use clap::Parser;
use nix::errno::Errno;
use nix::sys::signal::SigSet;
use nix::sys::signal::SigmaskHow;
use nix::sys::signal::Signal;
use nix::sys::signal::kill;
use nix::sys::signal::pthread_sigmask;
use nix::sys::signalfd::SfdFlags;
use nix::sys::signalfd::SignalFd;
use nix::sys::socket::ControlMessage;
use nix::sys::socket::MsgFlags;
use nix::sys::socket::sendmsg;
use nix::sys::wait::WaitPidFlag;
use nix::sys::wait::WaitStatus;
use nix::sys::wait::waitpid;
use nix::unistd::Gid;
use nix::unistd::Pid;
use nix::unistd::Uid;
use nix::unistd::getpid;
use nix::unistd::setgid;
use nix::unistd::setgroups;
use nix::unistd::setuid;

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const READINESS_HANDSHAKE_BYTES: usize = 38;
const READINESS_ACK: u8 = 1;
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const PODD_CGROUP_LEAF: &str = "tascarrel-daemon";
const REQUIRED_NESTED_CONTROLLERS: [&str; 2] = ["memory", "pids"];
const CONTROL_SOCKET: &str = "/run/tascarrel/podd-control.sock";
const GUESTD_CONTROL_SOCKET: &str = "/run/tascarrel/guestd-control.sock";

#[derive(Debug, Parser)]
#[command(name = "tascarrel-podd", about = "Run as PID 1 inside a Tascarrel pod")]
struct Args {
    /// Prepare the pod for rootful nested container engines. Starts no daemon.
    #[arg(long)]
    nested_containers: bool,

    /// Per-attempt guestd readiness socket mounted only into this pod.
    #[arg(long)]
    ready_socket: PathBuf,

    /// Fixed-size versioned handshake and nonce supplied by guestd.
    #[arg(long)]
    ready_handshake: String,

    /// Root-owned pod health file observed by guestd.
    #[arg(long, default_value = "/run/tascarrel/podd-health")]
    health_file: PathBuf,

    /// Root-owned directory containing one status and output log per init step.
    #[arg(long, default_value = "/run/tascarrel/init-steps")]
    init_log_directory: PathBuf,

    /// Start and supervise dockerd. Requires nested-container support.
    #[arg(long)]
    start_docker: bool,

    /// Absolute dockerd executable injected by the outer runtime.
    #[arg(long, default_value = "dockerd")]
    dockerd: PathBuf,

    /// Image-user UID receiving rootless-container runtime and cgroup access.
    #[arg(long, requires = "rootless_gid")]
    rootless_uid: Option<u32>,

    /// Image-user GID receiving rootless-container runtime and cgroup access.
    #[arg(long, requires = "rootless_uid")]
    rootless_gid: Option<u32>,

    /// Directory of lexically ordered per-pod init scripts.
    #[arg(long, default_value = "/run/tascarrel/hooks/init")]
    init_directory: PathBuf,

    /// Ordered per-pod init step scripts supplied by the outer runtime.
    #[arg(long)]
    init_step: Vec<String>,

    /// Whether the corresponding init step must complete before readiness.
    #[arg(long)]
    init_step_wait: Vec<bool>,

    /// One init script passed to an internal image-user child.
    #[arg(long, default_value = "", hide = true)]
    init_inline: String,

    /// Immutable shell used to run lifecycle scripts.
    #[arg(long, default_value = "/bin/sh")]
    init_shell: PathBuf,

    #[arg(long, default_value_t = 0)]
    init_uid: u32,

    #[arg(long, default_value_t = 0)]
    init_gid: u32,

    #[arg(long)]
    init_additional_gid: Vec<u32>,

    #[arg(long, hide = true)]
    init_child: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.init_child {
        return run_init_child(&args).map_err(Into::into);
    }
    if getpid() != Pid::from_raw(1) {
        return Err("tascarrel-podd must be PID 1 in the pod PID namespace".into());
    }
    if args.init_step.len() != args.init_step_wait.len() {
        return Err("every --init-step requires one --init-step-wait value".into());
    }
    publish_health(&args.health_file, &[])?;

    if args.start_docker && !args.nested_containers {
        return Err("--start-docker requires --nested-containers".into());
    }
    let docker_required = args.start_docker;
    let rootless_user = args.rootless_uid.zip(args.rootless_gid);
    if args.nested_containers || rootless_user.is_some() {
        prepare_nested_cgroup(Path::new(CGROUP_ROOT), getpid())?;
    }
    if let Some((uid, gid)) = rootless_user {
        prepare_rootless_runtime(uid, gid)?;
    }

    let mut signals = SigSet::empty();
    for signal in [
        Signal::SIGHUP,
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGCHLD,
    ] {
        signals.add(signal);
    }
    pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&signals), None)?;
    let mut signal_fd =
        SignalFd::with_flags(&signals, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK)?;

    let mut dockerd = if docker_required {
        Some(start_dockerd(&args.dockerd, args.init_gid)?)
    } else {
        None
    };
    let mut init = match start_init_steps(&args, &mut dockerd, &mut signal_fd)? {
        InitChildren::Running(children) => children,
        InitChildren::Shutdown(signal) => {
            terminate_namespace(signal)?;
            return Ok(());
        }
    };
    let _control = start_control_listener()?;
    notify_ready(&args.ready_socket, &args.ready_handshake)?;

    loop {
        if let Some(signal) = take_shutdown_signal(&mut signal_fd)? {
            terminate_namespace(signal)?;
            return Ok(());
        }
        reap_children(&mut dockerd, Some(&mut init), Some(&args.health_file))?;
        if docker_required && dockerd.is_none() {
            return Err("dockerd exited while the pod was running".into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn start_control_listener() -> io::Result<thread::JoinHandle<()>> {
    match fs::remove_file(CONTROL_SOCKET) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(CONTROL_SOCKET)?;
    fs::set_permissions(CONTROL_SOCKET, fs::Permissions::from_mode(0o600))?;
    Ok(thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    thread::spawn(move || {
                        if let Err(error) = serve_forward(stream) {
                            eprintln!("tascarrel-podd: forwarding connection failed: {error}");
                        }
                    });
                }
                Err(error) => eprintln!("tascarrel-podd: control socket accept failed: {error}"),
            }
        }
    }))
}

fn serve_forward(mut control: UnixStream) -> io::Result<()> {
    use std::net::Ipv4Addr;
    use std::net::SocketAddrV4;
    use std::net::TcpStream;
    let mut port = [0_u8; 2];
    control.read_exact(&mut port)?;
    let port = u16::from_be_bytes(port);
    if port == 0 {
        return send_guestd_control_listener(&control);
    }
    let mut target = match TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)) {
        Ok(target) => target,
        Err(error) => {
            control.write_all(&[1])?;
            return Err(error);
        }
    };
    control.write_all(&[0])?;
    let mut control_read = control.try_clone()?;
    let mut target_write = target.try_clone()?;
    let upload = thread::spawn(move || io::copy(&mut control_read, &mut target_write));
    io::copy(&mut target, &mut control)?;
    upload
        .join()
        .map_err(|_| io::Error::other("forwarding relay thread panicked"))??;
    Ok(())
}

fn send_guestd_control_listener(control: &UnixStream) -> io::Result<()> {
    match fs::remove_file(GUESTD_CONTROL_SOCKET) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(GUESTD_CONTROL_SOCKET)?;
    fs::set_permissions(GUESTD_CONTROL_SOCKET, fs::Permissions::from_mode(0o666))?;
    let payload = [0_u8];
    let descriptor = [listener.as_raw_fd()];
    sendmsg::<()>(
        control.as_raw_fd(),
        &[IoSlice::new(&payload)],
        &[ControlMessage::ScmRights(&descriptor)],
        MsgFlags::empty(),
        None,
    )
    .map_err(io::Error::other)?;
    Ok(())
}

enum InitStartup {
    Complete,
    Shutdown(Signal),
}

enum InitChildren {
    Running(Vec<RunningInit>),
    Shutdown(Signal),
}

struct RunningInit {
    index: usize,
    wait: bool,
    child: Child,
    status_file: PathBuf,
}

fn take_shutdown_signal(signal_fd: &mut SignalFd) -> Result<Option<Signal>, Errno> {
    loop {
        match signal_fd.read_signal() {
            Ok(Some(info)) => {
                let number = i32::try_from(info.ssi_signo).map_err(|_| Errno::EINVAL)?;
                let signal = Signal::try_from(number)?;
                if matches!(signal, Signal::SIGHUP | Signal::SIGINT | Signal::SIGTERM) {
                    return Ok(Some(signal));
                }
            }
            Ok(None) | Err(Errno::EAGAIN) => return Ok(None),
            Err(Errno::EINTR) => {}
            Err(error) => return Err(error),
        }
    }
}

/// Connects only after podd's control listener is active and waits for guestd
/// to authenticate and acknowledge this exact startup attempt.
fn notify_ready(socket: &Path, handshake: &str) -> io::Result<()> {
    if handshake.len() != READINESS_HANDSHAKE_BYTES || !handshake.is_ascii() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "readiness handshake has an invalid size or encoding",
        ));
    }
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(handshake.as_bytes())?;
    let mut acknowledgment = [0_u8; 1];
    stream.read_exact(&mut acknowledgment)?;
    if acknowledgment != [READINESS_ACK] {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "guestd rejected the readiness handshake",
        ));
    }
    Ok(())
}

fn prepare_nested_cgroup(root: &Path, pid: Pid) -> io::Result<()> {
    let controllers_path = root.join("cgroup.controllers");
    let advertised = read_cgroup_file(
        &controllers_path,
        "read controllers advertised to the nested pod",
    )?;
    let controllers = advertised.split_ascii_whitespace().collect::<Vec<_>>();
    for required in REQUIRED_NESTED_CONTROLLERS {
        if !controllers.contains(&required) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "nested containers require cgroup v2 controller {required:?}, but {} advertises only {:?}",
                    controllers_path.display(),
                    controllers
                ),
            ));
        }
    }

    let leaf = root.join(PODD_CGROUP_LEAF);
    ensure_cgroup_leaf(&leaf)?;
    let leaf_processes = leaf.join("cgroup.procs");
    let existing = read_cgroup_file(&leaf_processes, "inspect the nested pod daemon cgroup")?;
    if !existing.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "nested pod daemon cgroup {} already contains processes",
                leaf.display()
            ),
        ));
    }
    write_cgroup_file(
        &leaf_processes,
        format!("{}\n", pid.as_raw()).as_bytes(),
        "move the nested pod supervisor into its leaf cgroup",
    )?;

    let root_processes_path = root.join("cgroup.procs");
    let root_processes = read_cgroup_file(
        &root_processes_path,
        "verify the delegated nested cgroup root is empty",
    )?;
    if !root_processes.trim().is_empty() {
        return Err(io::Error::other(format!(
            "delegated nested cgroup root {} still contains processes after moving PID {}",
            root.display(),
            pid.as_raw()
        )));
    }

    let enable = controllers
        .iter()
        .map(|controller| format!("+{controller}"))
        .collect::<Vec<_>>()
        .join(" ");
    let subtree_control = root.join("cgroup.subtree_control");
    write_cgroup_file(
        &subtree_control,
        format!("{enable}\n").as_bytes(),
        "enable controllers in the delegated nested cgroup root",
    )?;
    let enabled = read_cgroup_file(
        &subtree_control,
        "verify controllers enabled in the delegated nested cgroup root",
    )?;
    let enabled = enabled
        .split_ascii_whitespace()
        .map(|controller| {
            controller
                .strip_prefix('+')
                .or_else(|| controller.strip_prefix('-'))
                .unwrap_or(controller)
        })
        .collect::<Vec<_>>();
    let missing = controllers
        .iter()
        .copied()
        .filter(|controller| !enabled.contains(controller))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(io::Error::other(format!(
            "delegated nested cgroup root {} did not enable advertised controllers {:?}",
            root.display(),
            missing
        )));
    }
    Ok(())
}

fn prepare_rootless_runtime(uid: u32, gid: u32) -> io::Result<()> {
    prepare_rootless_runtime_at(Path::new("/run/user"), uid, gid)
}

fn prepare_rootless_runtime_at(root: &Path, uid: u32, gid: u32) -> io::Result<()> {
    match fs::create_dir(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let root_metadata = fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "runtime user root is not a real directory: {}",
                root.display()
            ),
        ));
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o755))?;

    let path = root.join(uid.to_string());
    let mut directory = DirBuilder::new();
    directory.mode(0o700).create(&path)?;
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "rootless runtime path is not a real directory: {}",
                path.display()
            ),
        ));
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    chown(&path, Some(uid), Some(gid))?;
    let metadata = fs::metadata(&path)?;
    if (metadata.uid(), metadata.gid(), metadata.mode() & 0o777) != (uid, gid, 0o700) {
        return Err(io::Error::other(format!(
            "rootless runtime path {} did not retain ownership {uid}:{gid} and mode 0700",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_cgroup_leaf(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(cgroup_io_error(
                "create the nested pod daemon cgroup",
                path,
                &error,
            ));
        }
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| cgroup_io_error("inspect the nested pod daemon cgroup", path, &error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "nested pod daemon cgroup path is not a real directory: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn read_cgroup_file(path: &Path, operation: &'static str) -> io::Result<String> {
    fs::read_to_string(path).map_err(|error| cgroup_io_error(operation, path, &error))
}

fn write_cgroup_file(path: &Path, contents: &[u8], operation: &'static str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| cgroup_io_error(operation, path, &error))?;
    file.write_all(contents)
        .map_err(|error| cgroup_io_error(operation, path, &error))
}

fn cgroup_io_error(operation: &'static str, path: &Path, source: &io::Error) -> io::Error {
    io::Error::new(
        source.kind(),
        format!("could not {operation} at {}: {source}", path.display()),
    )
}

fn start_dockerd(program: &Path, socket_gid: u32) -> io::Result<Child> {
    for directory in ["/run/docker", "/var/lib/docker"] {
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700).create(directory)?;
    }
    Command::new(program)
        .args(dockerd_arguments(socket_gid))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
}

fn dockerd_arguments(socket_gid: u32) -> Vec<String> {
    vec![
        "--host=unix:///run/docker.sock".to_owned(),
        format!("--group={socket_gid}"),
        "--data-root=/var/lib/docker".to_owned(),
        "--exec-root=/run/docker".to_owned(),
        "--pidfile=/run/docker.pid".to_owned(),
        "--userland-proxy=false".to_owned(),
    ]
}

fn start_init_steps(
    args: &Args,
    dockerd: &mut Option<Child>,
    signal_fd: &mut SignalFd,
) -> Result<InitChildren, Box<dyn std::error::Error>> {
    let mut asynchronous = Vec::new();
    for (offset, (script, wait)) in args.init_step.iter().zip(&args.init_step_wait).enumerate() {
        let child = start_init(args, script, Path::new("/dev/null"), offset + 1, *wait)?;
        if *wait {
            let mut child = Some(child);
            if let InitStartup::Shutdown(signal) = wait_for_init(
                &mut child,
                &mut asynchronous,
                dockerd,
                signal_fd,
                &args.health_file,
            )? {
                return Ok(InitChildren::Shutdown(signal));
            }
        } else {
            asynchronous.push(child);
        }
    }
    asynchronous.push(start_init(
        args,
        "",
        &args.init_directory,
        args.init_step.len() + 1,
        false,
    )?);
    Ok(InitChildren::Running(asynchronous))
}

fn start_init(
    args: &Args,
    script: &str,
    directory: &Path,
    index: usize,
    wait: bool,
) -> io::Result<RunningInit> {
    let (log, status_file) = prepare_init_log(&args.init_log_directory, index, wait)?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--init-child")
        .arg("--init-directory")
        .arg(directory)
        .arg("--init-inline")
        .arg(script)
        .arg("--init-shell")
        .arg(&args.init_shell)
        .arg("--init-uid")
        .arg(args.init_uid.to_string())
        .arg("--init-gid")
        .arg(args.init_gid.to_string());
    for gid in &args.init_additional_gid {
        command.arg("--init-additional-gid").arg(gid.to_string());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    let child = command.spawn()?;
    Ok(RunningInit {
        index,
        wait,
        child,
        status_file,
    })
}

fn prepare_init_log(directory: &Path, index: usize, wait: bool) -> io::Result<(fs::File, PathBuf)> {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o755).create(directory)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o755))?;
    let stem = format!("{index:02}");
    let log_file = directory.join(format!("{stem}.log"));
    let status_file = directory.join(format!("{stem}.status"));
    let log = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o644)
        .open(log_file)?;
    log.set_permissions(fs::Permissions::from_mode(0o644))?;
    write_init_status(&status_file, "running", wait, None)?;
    Ok((log, status_file))
}

fn write_init_status(
    path: &Path,
    status: &str,
    wait: bool,
    detail: Option<&str>,
) -> io::Result<()> {
    let temporary = path.with_extension("status.new");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o644)
        .open(&temporary)?;
    file.set_permissions(fs::Permissions::from_mode(0o644))?;
    writeln!(file, "{status}")?;
    writeln!(file, "{wait}")?;
    if let Some(detail) = detail {
        writeln!(file, "{detail}")?;
    }
    file.sync_all()?;
    fs::rename(temporary, path)
}

fn finish_init(init: &RunningInit, status: std::process::ExitStatus) -> io::Result<()> {
    let state = if status.success() {
        "succeeded"
    } else {
        "failed"
    };
    write_init_status(
        &init.status_file,
        state,
        init.wait,
        (!status.success()).then(|| status.to_string()).as_deref(),
    )
}

fn run_init_child(args: &Args) -> io::Result<()> {
    const RUN_HOOKS: &str = "if [ -n \"$1\" ]; then \"$0\" -eu -c \"$1\"; fi; if [ -n \"${ZSH_VERSION:-}\" ]; then setopt NULL_GLOB; fi; for hook in \"$2\"/*; do [ ! -f \"$hook\" ] || \"$0\" -eu \"$hook\"; done";
    let groups = args
        .init_additional_gid
        .iter()
        .copied()
        .map(Gid::from_raw)
        .collect::<Vec<_>>();
    setgroups(&groups).map_err(io::Error::from)?;
    setgid(Gid::from_raw(args.init_gid)).map_err(io::Error::from)?;
    setuid(Uid::from_raw(args.init_uid)).map_err(io::Error::from)?;
    let error = Command::new(&args.init_shell)
        .args(["-eu", "-c", RUN_HOOKS])
        .arg(&args.init_shell)
        .arg(&args.init_inline)
        .arg(&args.init_directory)
        .current_dir("/workspace")
        .env("LC_ALL", "C")
        .exec();
    Err(error)
}

fn wait_for_init(
    init: &mut Option<RunningInit>,
    asynchronous_init: &mut Vec<RunningInit>,
    dockerd: &mut Option<Child>,
    signal_fd: &mut SignalFd,
    health_file: &Path,
) -> Result<InitStartup, Box<dyn std::error::Error>> {
    loop {
        if let Some(signal) = take_shutdown_signal(signal_fd)? {
            return Ok(InitStartup::Shutdown(signal));
        }
        if let Some(status) = init.as_mut().expect("init child exists").child.try_wait()? {
            let completed = init.take().expect("init child exists");
            finish_init(&completed, status)?;
            record_waited_init_status(status, health_file)?;
            return Ok(InitStartup::Complete);
        }
        poll_asynchronous_init(asynchronous_init, health_file)?;
        if let Some(child) = dockerd.as_mut()
            && let Some(status) = child.try_wait()?
        {
            *dockerd = None;
            return Err(format!("dockerd exited while workspace init hooks ran: {status}").into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn record_waited_init_status(
    status: std::process::ExitStatus,
    health_file: &Path,
) -> io::Result<()> {
    if status.success() {
        return Ok(());
    }
    let message = format!("workspace init step exited with {status}");
    eprintln!("tascarrel-podd: {message}");
    record_degraded(health_file, &message)
}

fn poll_asynchronous_init(init: &mut Vec<RunningInit>, health_file: &Path) -> io::Result<()> {
    let mut index = 0;
    while index < init.len() {
        if let Some(status) = init[index].child.try_wait()? {
            let mut completed = init.remove(index);
            finish_init(&completed, status)?;
            eprintln!(
                "tascarrel-podd: asynchronous init step {} exited: {status}",
                completed.index
            );
            if !status.success() {
                record_degraded(
                    health_file,
                    &format!(
                        "workspace init step {} exited with {status}",
                        completed.index
                    ),
                )?;
            }
            completed.child.wait()?;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn reap_children(
    dockerd: &mut Option<Child>,
    mut init: Option<&mut Vec<RunningInit>>,
    health_file: Option<&Path>,
) -> Result<(), Errno> {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => break,
            Ok(status) => {
                if dockerd.as_ref().is_some_and(|child| {
                    let child_pid = i32::try_from(child.id()).ok();
                    status
                        .pid()
                        .is_some_and(|reaped| Some(reaped.as_raw()) == child_pid)
                }) {
                    eprintln!("tascarrel-podd: dockerd exited: {status:?}");
                    *dockerd = None;
                } else if let Some(position) = init.as_ref().and_then(|init| {
                    status.pid().and_then(|reaped| {
                        init.iter().position(|init| {
                            i32::try_from(init.child.id()).ok() == Some(reaped.as_raw())
                        })
                    })
                }) {
                    let completed = init.as_mut().expect("init children exist").remove(position);
                    let successful = matches!(status, WaitStatus::Exited(_, 0));
                    let detail = format!("{status:?}");
                    if let Err(error) = write_init_status(
                        &completed.status_file,
                        if successful { "succeeded" } else { "failed" },
                        completed.wait,
                        (!successful).then_some(detail.as_str()),
                    ) {
                        eprintln!("tascarrel-podd: failed to record init step status: {error}");
                    }
                    eprintln!(
                        "tascarrel-podd: asynchronous init step {} exited: {status:?}",
                        completed.index
                    );
                    if !matches!(status, WaitStatus::Exited(_, 0))
                        && let Some(path) = health_file
                        && let Err(error) = record_degraded(
                            path,
                            &format!("workspace init step {} exited: {status:?}", completed.index),
                        )
                    {
                        eprintln!("tascarrel-podd: failed to record degraded health: {error}");
                    }
                }
            }
            Err(Errno::EINTR) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn publish_health(path: &Path, messages: &[String]) -> io::Result<()> {
    const MAX_HEALTH_BYTES: usize = 16 * 1024;
    let mut content = if messages.is_empty() {
        String::from("healthy\n")
    } else {
        String::from("degraded\n")
    };
    for message in messages {
        if content.len() + message.len() + 1 > MAX_HEALTH_BYTES {
            break;
        }
        content.push_str(message);
        content.push('\n');
    }
    let temporary = path.with_extension("new");
    fs::write(&temporary, content)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644))?;
    fs::rename(temporary, path)
}

fn record_degraded(path: &Path, message: &str) -> io::Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut messages = existing
        .lines()
        .skip(1)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !messages.iter().any(|existing| existing == message) {
        messages.push(message.to_owned());
    }
    publish_health(path, &messages)
}

fn terminate_namespace(signal: Signal) -> Result<(), Errno> {
    // PID 1 is excluded from this broadcast. All pod descendants receive the
    // requested shutdown signal without relying on process-group membership.
    match kill(Pid::from_raw(-1), signal) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(error) => return Err(error),
    }
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while Instant::now() < deadline {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => thread::sleep(POLL_INTERVAL),
            Ok(_) | Err(Errno::EINTR) => {}
            Err(Errno::ECHILD) => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    match kill(Pid::from_raw(-1), Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Read;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::UnixListener;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::ExitStatusExt;
    use std::thread;

    use tempfile::TempDir;

    use super::PODD_CGROUP_LEAF;
    use super::dockerd_arguments;
    use super::notify_ready;
    use super::prepare_init_log;
    use super::prepare_nested_cgroup;
    use super::prepare_rootless_runtime_at;
    use super::publish_health;
    use super::record_degraded;
    use super::record_waited_init_status;
    use super::serve_forward;
    use super::write_init_status;

    #[test]
    fn managed_docker_socket_uses_the_image_users_primary_group() {
        let arguments = dockerd_arguments(4242);

        assert!(arguments.iter().any(|argument| argument == "--group=4242"));
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--group=docker")
        );
    }

    #[test]
    fn pod_health_reports_are_atomic_bounded_and_accumulate_failures() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("health");

        publish_health(&path, &[]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "healthy\n");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);

        record_degraded(&path, "first init hook failed").unwrap();
        record_degraded(&path, "first init hook failed").unwrap();
        record_degraded(&path, "second init hook failed").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "degraded\nfirst init hook failed\nsecond init hook failed\n"
        );

        publish_health(&path, &["x".repeat(32 * 1024)]).unwrap();
        assert!(fs::metadata(&path).unwrap().len() <= 16 * 1024);
        assert!(!path.with_extension("new").exists());
    }

    #[test]
    fn failed_waited_init_degrades_without_becoming_a_startup_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("health");
        publish_health(&path, &[]).unwrap();

        record_waited_init_status(std::process::ExitStatus::from_raw(7 << 8), &path).unwrap();

        let health = fs::read_to_string(path).unwrap();
        assert!(health.starts_with("degraded\n"));
        assert!(health.contains("exit status: 7"));
    }

    #[test]
    fn init_steps_get_independent_world_readable_status_and_log_files() {
        let directory = tempfile::tempdir().unwrap();
        let (mut log, status) = prepare_init_log(directory.path(), 2, true).unwrap();
        writeln!(log, "step output").unwrap();
        write_init_status(&status, "failed", true, Some("exit status: 7")).unwrap();

        assert_eq!(
            fs::read_to_string(status).unwrap(),
            "failed\ntrue\nexit status: 7\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("02.log")).unwrap(),
            "step output\n"
        );
        assert_eq!(
            fs::metadata(directory.path().join("02.log"))
                .unwrap()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(directory.path()).unwrap().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(directory.path().join("02.status"))
                .unwrap()
                .mode()
                & 0o777,
            0o644
        );
    }

    #[test]
    fn forwarding_control_connects_inside_pod_loopback() {
        use std::net::Ipv4Addr;
        use std::net::SocketAddrV4;
        use std::net::TcpListener;
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut value = [0_u8; 4];
            stream.read_exact(&mut value).unwrap();
            stream.write_all(&value).unwrap();
        });
        let (mut client, control) = UnixStream::pair().unwrap();
        let relay = thread::spawn(move || serve_forward(control).unwrap());
        client.write_all(&port.to_be_bytes()).unwrap();
        let mut status = [0_u8; 1];
        client.read_exact(&mut status).unwrap();
        assert_eq!(status, [0]);
        client.write_all(b"ping").unwrap();
        let mut echoed = [0_u8; 4];
        client.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"ping");
        drop(client);
        relay.join().unwrap();
        server.join().unwrap();
    }

    fn fake_cgroup(controllers: &str, root_processes: &str) -> TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("cgroup.controllers"), controllers).unwrap();
        fs::write(directory.path().join("cgroup.subtree_control"), b"").unwrap();
        fs::write(directory.path().join("cgroup.procs"), root_processes).unwrap();
        fs::write(directory.path().join("cgroup.threads"), b"").unwrap();
        let leaf = directory.path().join(PODD_CGROUP_LEAF);
        fs::create_dir(&leaf).unwrap();
        fs::write(leaf.join("cgroup.procs"), b"").unwrap();
        directory
    }

    #[test]
    fn nested_cgroup_moves_pid_and_enables_every_advertised_controller() {
        let controllers = "cpuset cpu io memory hugetlb pids rdma misc";
        let cgroup = fake_cgroup(controllers, "");

        prepare_nested_cgroup(cgroup.path(), nix::unistd::Pid::from_raw(4242)).unwrap();

        assert_eq!(
            fs::read_to_string(cgroup.path().join(PODD_CGROUP_LEAF).join("cgroup.procs")).unwrap(),
            "4242\n"
        );
        assert_eq!(
            fs::read_to_string(cgroup.path().join("cgroup.procs")).unwrap(),
            "",
            "the delegated root must remain an internal node without processes"
        );
        assert_eq!(
            fs::read_to_string(cgroup.path().join("cgroup.subtree_control")).unwrap(),
            "+cpuset +cpu +io +memory +hugetlb +pids +rdma +misc\n"
        );
    }

    #[test]
    fn rootless_runtime_directory_is_owned_by_the_image_user() {
        let runtime = tempfile::tempdir().unwrap();
        let metadata = fs::metadata(runtime.path()).unwrap();
        let (uid, gid) = (metadata.uid(), metadata.gid());
        prepare_rootless_runtime_at(runtime.path(), uid, gid).unwrap();
        let user_runtime = runtime.path().join(uid.to_string());
        let metadata = fs::metadata(user_runtime).unwrap();
        assert_eq!((metadata.uid(), metadata.gid()), (uid, gid));
        assert_eq!(metadata.mode() & 0o777, 0o700);
    }

    #[test]
    fn nested_cgroup_requires_memory_and_pids_before_moving_pid() {
        for (controllers, missing) in [("cpu pids", "memory"), ("cpu memory", "pids")] {
            let cgroup = fake_cgroup(controllers, "");
            let error =
                prepare_nested_cgroup(cgroup.path(), nix::unistd::Pid::from_raw(1)).unwrap_err();

            assert!(error.to_string().contains(missing), "{error}");
            assert_eq!(
                fs::read_to_string(cgroup.path().join(PODD_CGROUP_LEAF).join("cgroup.procs"))
                    .unwrap(),
                "",
                "missing required controllers must fail before moving PID 1"
            );
            assert_eq!(
                fs::read_to_string(cgroup.path().join("cgroup.subtree_control")).unwrap(),
                ""
            );
        }
    }

    #[test]
    fn nested_cgroup_fails_if_the_delegated_root_is_not_empty() {
        let cgroup = fake_cgroup("cpu memory pids", "99\n");
        let error =
            prepare_nested_cgroup(cgroup.path(), nix::unistd::Pid::from_raw(1)).unwrap_err();

        assert!(error.to_string().contains("still contains processes"));
        assert_eq!(
            fs::read_to_string(cgroup.path().join("cgroup.subtree_control")).unwrap(),
            "",
            "controllers must not be enabled while the delegated root has processes"
        );
    }

    #[test]
    fn nested_cgroup_reports_an_unwritable_subtree_control() {
        let cgroup = fake_cgroup("cpu memory pids", "");
        let subtree = cgroup.path().join("cgroup.subtree_control");
        fs::remove_file(&subtree).unwrap();
        fs::create_dir(&subtree).unwrap();

        let error =
            prepare_nested_cgroup(cgroup.path(), nix::unistd::Pid::from_raw(1)).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("enable controllers in the delegated nested cgroup root"),
            "{error}"
        );
    }

    /// Verifies podd reports readiness only after receiving guestd's ACK.
    #[test]
    fn readiness_requires_guestd_acknowledgment() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("ready.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let handshake = "TSRD01aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = [0_u8; super::READINESS_HANDSHAKE_BYTES];
            stream.read_exact(&mut received).unwrap();
            assert_eq!(received, handshake.as_bytes());
            stream.write_all(&[super::READINESS_ACK]).unwrap();
        });

        notify_ready(&socket, handshake).unwrap();
        server.join().unwrap();
    }
}
