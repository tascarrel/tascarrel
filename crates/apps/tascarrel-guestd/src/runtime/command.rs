//! Reliable asynchronous command launching for guest runtime components.

use std::io;
use std::time::Duration;

use nix::errno::Errno;
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::sleep;

const EXECUTABLE_BUSY_RETRIES: usize = 10;
const EXECUTABLE_BUSY_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Spawns a command, retrying Linux's transient executable-writer race.
pub(crate) async fn spawn(command: &mut Command) -> io::Result<Child> {
    let mut retries = 0;
    loop {
        match command.spawn() {
            Err(error)
                if error.raw_os_error() == Some(Errno::ETXTBSY as i32)
                    && retries < EXECUTABLE_BUSY_RETRIES =>
            {
                retries += 1;
                tracing::debug!(retries, "retrying temporarily busy executable");
                sleep(EXECUTABLE_BUSY_RETRY_DELAY).await;
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::fs::OpenOptions;
    use std::os::unix::fs::PermissionsExt as _;
    use std::thread;

    use tempfile::tempdir;

    use super::*;

    /// Verifies a command waits for an inherited writable descriptor to close.
    #[tokio::test]
    async fn retries_a_temporarily_busy_executable() {
        let directory = tempdir().unwrap();
        let executable = directory.path().join("executable");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let writer = OpenOptions::new().write(true).open(&executable).unwrap();
        let release = thread::spawn(move || {
            thread::sleep(EXECUTABLE_BUSY_RETRY_DELAY * 2);
            drop(writer);
        });

        let mut command = Command::new(executable);
        let status = spawn(&mut command).await.unwrap().wait().await.unwrap();
        release.join().unwrap();

        assert!(status.success());
    }
}
