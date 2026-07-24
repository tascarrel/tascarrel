//! External command execution for pod storage and runtime operations.

use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use nix::sys::signal::Signal;
use nix::sys::signal::killpg;
use nix::unistd::Pid;

/// Captured output from a storage or low-level runtime command.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandOutput {
    /// Whether the command exited successfully.
    pub success: bool,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    /// Constructs successful output with no captured data.
    #[must_use]
    pub const fn success() -> Self {
        Self {
            success: true,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    /// Constructs failed output with a diagnostic on standard error.
    #[must_use]
    pub fn failure(message: impl Into<Vec<u8>>) -> Self {
        Self {
            success: false,
            stdout: Vec::new(),
            stderr: message.into(),
        }
    }
}

/// Executes external commands requested by a
/// [`crate::runtime::pod::BtrfsStore`] or [`crate::runtime::pod::PodRuntime`].
///
/// The narrow interface makes command execution deterministic in tests while
/// keeping the production implementation free from shell parsing.
pub trait CommandRunner: Send + Sync + 'static {
    /// Runs `program` with the exact argument vector in `arguments`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the command cannot be started or waited for.
    fn run(&self, program: &Path, arguments: &[OsString]) -> io::Result<CommandOutput>;

    /// Runs a command whose descendants may intentionally retain standard
    /// output and error after the direct child exits.
    ///
    /// The default keeps injected runners source-compatible and deterministic;
    /// the production runner waits only for the direct child and enforces the
    /// supplied deadline.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the direct child cannot be started or waited
    /// for, or when it exceeds `timeout` and is killed.
    fn run_detached(
        &self,
        program: &Path,
        arguments: &[OsString],
        timeout: Duration,
    ) -> io::Result<CommandOutput> {
        let _ = timeout;
        self.run(program, arguments)
    }

    /// Runs a detached command while retaining a bounded tail of the output
    /// inherited by its descendants.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the direct child cannot be started or waited
    /// for, when the log pipe cannot be established, or when the deadline is
    /// exceeded.
    fn run_detached_logged(
        &self,
        program: &Path,
        arguments: &[OsString],
        timeout: Duration,
        log: &Path,
        limit: usize,
    ) -> io::Result<CommandOutput> {
        let _ = (log, limit);
        self.run_detached(program, arguments, timeout)
    }
}

/// Runs commands directly, without a shell, and captures their output.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, program: &Path, arguments: &[OsString]) -> io::Result<CommandOutput> {
        let output = Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn run_detached(
        &self,
        program: &Path,
        arguments: &[OsString],
        timeout: Duration,
    ) -> io::Result<CommandOutput> {
        Self::run_detached_inner(program, arguments, timeout, None)
    }

    fn run_detached_logged(
        &self,
        program: &Path,
        arguments: &[OsString],
        timeout: Duration,
        log: &Path,
        limit: usize,
    ) -> io::Result<CommandOutput> {
        let (reader, writer) = nix::unistd::pipe()?;
        let reader = File::from(reader);
        let writer = File::from(writer);
        let stderr = writer.try_clone()?;
        thread::spawn({
            let log = log.to_path_buf();
            move || collect_bounded_output(reader, log, limit)
        });
        Self::run_detached_inner(
            program,
            arguments,
            timeout,
            Some((Stdio::from(writer), Stdio::from(stderr))),
        )
    }
}

impl ProcessCommandRunner {
    fn run_detached_inner(
        program: &Path,
        arguments: &[OsString],
        timeout: Duration,
        output: Option<(Stdio, Stdio)>,
    ) -> io::Result<CommandOutput> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "detached command timeout overflow",
            )
        })?;
        let (stdout, stderr) = output.unwrap_or_else(|| (Stdio::inherit(), Stdio::inherit()));
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0);
        let mut child = command.spawn()?;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(CommandOutput {
                    success: status.success(),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            if Instant::now() >= deadline {
                let group = Pid::from_raw(i32::try_from(child.id()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "child PID exceeds pid_t")
                })?);
                if killpg(group, Signal::SIGKILL).is_err() {
                    // Cover a race with process-group creation or direct-child
                    // exit. `wait` below is still mandatory so no zombie leaks.
                    let _ = child.kill();
                }
                child.wait()?;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("detached command exceeded {} seconds", timeout.as_secs()),
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn collect_bounded_output(mut reader: File, path: PathBuf, limit: usize) {
    let Ok(mut log) = OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
    else {
        return;
    };
    let mut tail = Vec::new();
    let mut chunk = [0_u8; 4096];
    while let Ok(count) = reader.read(&mut chunk) {
        if count == 0 {
            break;
        }
        tail.extend_from_slice(&chunk[..count]);
        if tail.len() > limit {
            tail.drain(..tail.len() - limit);
        }
        if log.seek(SeekFrom::Start(0)).is_err()
            || log.write_all(&tail).is_err()
            || log
                .set_len(u64::try_from(tail.len()).unwrap_or(u64::MAX))
                .is_err()
            || log.flush().is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;

    use nix::sys::signal::killpg;
    use tempfile::TempDir;

    use super::*;

    fn shell() -> PathBuf {
        let shell = PathBuf::from(env::var_os("SHELL").expect("tests require an absolute $SHELL"));
        assert!(shell.is_absolute());
        shell
    }

    fn process_path(pid: i32) -> PathBuf {
        PathBuf::from("/proc").join(pid.to_string())
    }

    fn wait_until_gone(pid: i32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_path(pid).exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!process_path(pid).exists(), "process {pid} was not reaped");
    }

    fn read_pids(path: &Path) -> (i32, i32) {
        let contents = fs::read_to_string(path).unwrap();
        let mut pids = contents
            .split_ascii_whitespace()
            .map(|pid| pid.parse::<i32>().unwrap());
        let leader = pids.next().unwrap();
        let descendant = pids.next().unwrap();
        assert!(pids.next().is_none());
        (leader, descendant)
    }

    /// Verifies detached execution does not inherit a descendant's standard
    /// I/O.
    #[test]
    fn detached_runner_does_not_wait_for_a_descendant_holding_stdio() {
        let temporary = TempDir::new().unwrap();
        let pid_file = temporary.path().join("pids");
        let started = Instant::now();
        let output = ProcessCommandRunner
            .run_detached(
                &shell(),
                &[
                    OsString::from("-c"),
                    OsString::from("sleep 30 & printf '%s %s' \"$$\" \"$!\" > \"$1\""),
                    OsString::from("tascarrel-detached-test"),
                    pid_file.as_os_str().to_owned(),
                ],
                Duration::from_secs(5),
            )
            .unwrap();
        let elapsed = started.elapsed();
        let (leader, descendant) = read_pids(&pid_file);
        let _ = killpg(Pid::from_raw(leader), Signal::SIGKILL);
        wait_until_gone(leader);
        wait_until_gone(descendant);
        assert!(output.success);
        assert!(elapsed < Duration::from_secs(1));
    }

    /// Verifies timed-out detached process groups are terminated and reaped.
    #[test]
    fn detached_runner_kills_the_process_group_and_reaps_on_timeout() {
        let temporary = TempDir::new().unwrap();
        let pid_file = temporary.path().join("pid");
        let output = ProcessCommandRunner.run_detached(
            &shell(),
            &[
                OsString::from("-c"),
                OsString::from("sleep 30 & printf '%s %s' \"$$\" \"$!\" > \"$1\"; wait"),
                OsString::from("tascarrel-detached-test"),
                pid_file.as_os_str().to_owned(),
            ],
            Duration::from_millis(250),
        );
        let error = output.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let (leader, descendant) = read_pids(&pid_file);
        wait_until_gone(leader);
        wait_until_gone(descendant);
    }

    /// Verifies detached command diagnostics retain only the configured tail.
    #[test]
    fn detached_runner_retains_only_the_configured_output_tail() {
        let temporary = TempDir::new().unwrap();
        let log = temporary.path().join("startup.log");
        File::create(&log).unwrap();
        let output = ProcessCommandRunner
            .run_detached_logged(
                &shell(),
                &[OsString::from("-c"), OsString::from("printf 0123456789")],
                Duration::from_secs(5),
                &log,
                5,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while fs::read(&log).unwrap() != b"56789" && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(output.success);
        assert_eq!(fs::read(log).unwrap(), b"56789");
    }
}
