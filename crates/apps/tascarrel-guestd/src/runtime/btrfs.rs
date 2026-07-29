//! Btrfs transaction durability for guest-owned persistent storage.
//!
//! This module exposes the narrow transaction-commit primitive required by the
//! pod store. It deliberately avoids the broader filesystem-wide data flushing
//! and cleanup work performed by a full Btrfs filesystem sync.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd as _;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Commits the current Btrfs transaction without initiating a full filesystem
/// data sync or waking the deleted-subvolume cleaner.
///
/// The ioctl runs on a disposable thread so a kernel-side wait cannot stop the
/// caller from enforcing its availability deadline. The guest must be recycled
/// after a timeout because the kernel operation may still be running.
#[tracing::instrument(
    name = "tascarrel_guest.btrfs.commit_transaction",
    level = "debug",
    skip(root),
    fields(root = %root.display(), timeout_ms = timeout.as_millis()),
    err
)]
pub(crate) fn commit_transaction(root: &Path, timeout: Duration) -> io::Result<u64> {
    let filesystem = File::open(root)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("btrfs-commit".to_owned())
        .spawn(move || {
            let result = commit_transaction_inner(&filesystem);
            if sender.send(result).is_err() {
                tracing::debug!("Btrfs commit completed after its caller stopped waiting");
            }
        })?;
    receiver
        .recv_timeout(timeout)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "Btrfs transaction commit timed out after {} seconds",
                    timeout.as_secs()
                ),
            ),
            mpsc::RecvTimeoutError::Disconnected => io::Error::other(
                "Btrfs transaction commit worker exited without reporting a result",
            ),
        })?
}

/// Btrfs ioctl type encoded by the stable userspace ABI.
const BTRFS_IOCTL_MAGIC: u8 = 0x94;
/// Command number for `BTRFS_IOC_WAIT_SYNC`.
const BTRFS_WAIT_SYNC_COMMAND: u8 = 22;
/// Command number for `BTRFS_IOC_START_SYNC`.
const BTRFS_START_SYNC_COMMAND: u8 = 24;

nix::ioctl_read!(start_sync, BTRFS_IOCTL_MAGIC, BTRFS_START_SYNC_COMMAND, u64);
nix::ioctl_write_ptr!(wait_sync, BTRFS_IOCTL_MAGIC, BTRFS_WAIT_SYNC_COMMAND, u64);

/// Starts and waits for one Btrfs transaction commit on an open filesystem.
#[allow(
    unsafe_code,
    reason = "Btrfs transaction ioctls have no safe standard-library wrapper"
)]
fn commit_transaction_inner(filesystem: &File) -> io::Result<u64> {
    let mut transaction_id = 0_u64;
    // SAFETY: Both ioctls accept a pointer to one initialized u64. The file
    // descriptor remains open for both calls, and the kernel copies rather
    // than retains the pointer.
    unsafe {
        start_sync(filesystem.as_raw_fd(), &raw mut transaction_id).map_err(errno_to_io_error)?;
        wait_sync(filesystem.as_raw_fd(), &raw const transaction_id).map_err(errno_to_io_error)?;
    }
    Ok(transaction_id)
}

/// Converts a `nix` ioctl failure into the I/O error used by the store runner.
fn errno_to_io_error(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}
