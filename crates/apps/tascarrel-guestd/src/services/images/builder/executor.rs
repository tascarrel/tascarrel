//! Bounded process execution and daemon lifecycle management for image builds.

use super::Arc;
use super::Child;
use super::Command;
use super::CommandExt;
use super::Duration;
use super::Errno;
use super::Instant;
use super::Mutex;
use super::OsString;
use super::PROCESS_POLL_INTERVAL;
use super::Path;
use super::Pid;
use super::Read;
use super::Signal;
use super::Stdio;
use super::io;
use super::kill;
use super::thread;

pub(crate) struct ExecutionOutput {
    pub(crate) success: bool,
    pub(crate) timed_out: bool,
    pub(crate) diagnostic: Vec<u8>,
}

pub(crate) type BuildOutput = dyn Fn(&[u8]) + Send + Sync;

pub(crate) trait BuildDaemon: Send {
    fn try_wait(&mut self) -> io::Result<Option<bool>>;
    fn shutdown(&mut self, timeout: Duration) -> io::Result<()>;
    fn diagnostics(&self) -> Vec<u8>;
}

pub(crate) trait BuildExecutor: Send + Sync {
    fn spawn_daemon(
        &self,
        program: &Path,
        arguments: &[OsString],
        output: Option<Arc<BuildOutput>>,
    ) -> io::Result<Box<dyn BuildDaemon>>;

    fn run(
        &self,
        program: &Path,
        arguments: &[OsString],
        timeout: Duration,
        output: Option<Arc<BuildOutput>>,
    ) -> io::Result<ExecutionOutput>;
}

pub(crate) struct DaemonGuard {
    daemon: Option<Box<dyn BuildDaemon>>,
    shutdown_timeout: Duration,
}

impl DaemonGuard {
    pub(crate) fn new(daemon: Box<dyn BuildDaemon>, shutdown_timeout: Duration) -> Self {
        Self {
            daemon: Some(daemon),
            shutdown_timeout,
        }
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<bool>> {
        self.daemon
            .as_mut()
            .expect("daemon guard always contains a daemon before shutdown")
            .try_wait()
    }

    pub(crate) fn diagnostics(&self) -> Vec<u8> {
        self.daemon
            .as_ref()
            .map_or_else(Vec::new, |daemon| daemon.diagnostics())
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        if let Some(mut daemon) = self.daemon.take() {
            daemon.shutdown(self.shutdown_timeout)
        } else {
            Ok(())
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcessBuildExecutor {
    pub(crate) diagnostic_limit: usize,
}

impl BuildExecutor for ProcessBuildExecutor {
    fn spawn_daemon(
        &self,
        program: &Path,
        arguments: &[OsString],
        output: Option<Arc<BuildOutput>>,
    ) -> io::Result<Box<dyn BuildDaemon>> {
        ProcessDaemon::spawn(program, arguments, self.diagnostic_limit, output)
            .map(|daemon| Box::new(daemon) as Box<_>)
    }

    fn run(
        &self,
        program: &Path,
        arguments: &[OsString],
        timeout: Duration,
        output: Option<Arc<BuildOutput>>,
    ) -> io::Result<ExecutionOutput> {
        let mut process =
            CapturedProcess::spawn(program, arguments, self.diagnostic_limit, output)?;
        let deadline = Instant::now() + timeout;
        let mut status = None;
        while status.is_none() && Instant::now() < deadline {
            status = process.child.try_wait()?;
            if status.is_none() {
                thread::sleep(PROCESS_POLL_INTERVAL);
            }
        }
        let timed_out = status.is_none();
        if timed_out {
            process.signal(Signal::SIGKILL)?;
            status = Some(process.child.wait()?);
        }
        process.finish_readers()?;
        Ok(ExecutionOutput {
            success: status.is_some_and(|status| status.success()) && !timed_out,
            timed_out,
            diagnostic: process.log.snapshot(),
        })
    }
}

struct ProcessDaemon {
    process: CapturedProcess,
}

impl ProcessDaemon {
    fn spawn(
        program: &Path,
        arguments: &[OsString],
        diagnostic_limit: usize,
        output: Option<Arc<BuildOutput>>,
    ) -> io::Result<Self> {
        CapturedProcess::spawn(program, arguments, diagnostic_limit, output)
            .map(|process| Self { process })
    }
}

impl BuildDaemon for ProcessDaemon {
    fn try_wait(&mut self) -> io::Result<Option<bool>> {
        self.process
            .child
            .try_wait()
            .map(|status| status.map(|status| status.success()))
    }

    fn shutdown(&mut self, timeout: Duration) -> io::Result<()> {
        if self.process.child.try_wait()?.is_none() {
            self.process.signal(Signal::SIGTERM)?;
            let deadline = Instant::now() + timeout;
            while self.process.child.try_wait()?.is_none() && Instant::now() < deadline {
                thread::sleep(PROCESS_POLL_INTERVAL);
            }
            if self.process.child.try_wait()?.is_none() {
                self.process.signal(Signal::SIGKILL)?;
                self.process.child.wait()?;
            }
        }
        self.process.finish_readers()?;
        Ok(())
    }

    fn diagnostics(&self) -> Vec<u8> {
        self.process.log.snapshot()
    }
}

impl Drop for ProcessDaemon {
    fn drop(&mut self) {
        let _ = self.shutdown(Duration::from_secs(1));
    }
}

struct CapturedProcess {
    child: Child,
    process_group: i32,
    log: Arc<BoundedLog>,
    readers: Vec<thread::JoinHandle<io::Result<()>>>,
}

impl CapturedProcess {
    fn spawn(
        program: &Path,
        arguments: &[OsString],
        diagnostic_limit: usize,
        output: Option<Arc<BuildOutput>>,
    ) -> io::Result<Self> {
        let mut child = Command::new(program)
            .args(arguments)
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?;
        let process_group = i32::try_from(child.id()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "child process ID does not fit i32",
            )
        })?;
        let log = Arc::new(BoundedLog::new(diagnostic_limit));
        let mut readers = Vec::with_capacity(2);
        if let Some(stdout) = child.stdout.take() {
            readers.push(spawn_log_reader(stdout, Arc::clone(&log), output.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            readers.push(spawn_log_reader(stderr, Arc::clone(&log), output));
        }
        Ok(Self {
            child,
            process_group,
            log,
            readers,
        })
    }

    fn signal(&self, signal: Signal) -> io::Result<()> {
        match kill(Pid::from_raw(-self.process_group), signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
        }
    }

    fn finish_readers(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_millis(100);
        while self.readers.iter().any(|reader| !reader.is_finished()) && Instant::now() < deadline {
            thread::sleep(PROCESS_POLL_INTERVAL);
        }
        if self.readers.iter().any(|reader| !reader.is_finished()) {
            // A command may not daemonize and retain the bounded diagnostic
            // pipes after its direct child has exited.
            self.signal(Signal::SIGKILL)?;
        }
        join_log_readers(self.readers.drain(..))
    }
}

pub(crate) fn spawn_log_reader<R: Read + Send + 'static>(
    mut reader: R,
    log: Arc<BoundedLog>,
    output: Option<Arc<BuildOutput>>,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || -> io::Result<()> {
        let mut buffer = [0_u8; 4096];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(());
            }
            let bytes = &buffer[..count];
            log.push(bytes);
            if let Some(output) = &output {
                output(bytes);
            }
        }
    })
}

pub(crate) fn join_log_readers(
    readers: impl IntoIterator<Item = thread::JoinHandle<io::Result<()>>>,
) -> io::Result<()> {
    let mut failure = None;
    for reader in readers {
        let result = reader
            .join()
            .map_err(|_| io::Error::other("image build output reader panicked"))
            .and_then(|result| result);
        if failure.is_none() {
            failure = result.err();
        }
    }
    failure.map_or(Ok(()), Err)
}

#[derive(Debug)]
pub(crate) struct BoundedLog {
    limit: usize,
    bytes: Mutex<Vec<u8>>,
}

impl BoundedLog {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            bytes: Mutex::new(Vec::with_capacity(limit)),
        }
    }

    pub(crate) fn push(&self, incoming: &[u8]) {
        let mut bytes = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if incoming.len() >= self.limit {
            bytes.clear();
            bytes.extend_from_slice(&incoming[incoming.len() - self.limit..]);
            return;
        }
        let excess = bytes
            .len()
            .saturating_add(incoming.len())
            .saturating_sub(self.limit);
        if excess != 0 {
            bytes.drain(..excess);
        }
        bytes.extend_from_slice(incoming);
    }

    pub(crate) fn snapshot(&self) -> Vec<u8> {
        self.bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
