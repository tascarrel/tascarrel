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
