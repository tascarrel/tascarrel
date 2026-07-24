//! Sparse raw disk image preparation.
//!
//! [`ensure_sparse_raw_disk`] creates or grows a thin-provisioned raw image
//! without ever shrinking an existing image.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use reportify::Report;
use thiserror::Error;
use tracing::warn;

/// The change made by [`ensure_sparse_raw_disk`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum SparseRawDiskOutcome {
    /// A new sparse image was created at the requested size.
    Created,
    /// An existing image was grown to the requested size.
    Grown {
        /// The image's virtual size before it was grown.
        previous_size: u64,
    },
    /// The existing image was already at least the requested size.
    Unchanged {
        /// The image's current virtual size.
        current_size: u64,
    },
}

/// An error while creating or growing a sparse raw disk image.
#[derive(Debug, Error)]
pub enum SparseRawDiskError {
    /// A zero-length image cannot provide guest storage.
    #[error("sparse raw disk image size must be greater than zero")]
    ZeroSize,
    /// An existing path was not a regular non-symlink file.
    #[error("sparse raw disk image is not a regular non-symlink file: {0}")]
    InvalidImage(PathBuf),
    /// A host filesystem operation failed.
    #[error("failed to {operation} sparse raw disk image {path}: {source}")]
    Io {
        /// The operation that failed.
        operation: &'static str,
        /// The image path being prepared.
        path: PathBuf,
        /// The underlying host filesystem error.
        #[source]
        source: io::Error,
    },
}

/// Creates or grows a thin-provisioned raw disk image.
///
/// New images are created with mode `0600`. On Linux, a new image on Btrfs is
/// marked NOCOW before its virtual size is assigned. An existing image is
/// grown when `minimum_size` exceeds its current virtual size and is otherwise
/// left unchanged. Images are never shrunk.
///
/// # Errors
///
/// Returns an error when `minimum_size` is zero, the path exists but is not a
/// regular non-symlink file, or the host filesystem cannot prepare the image.
#[tracing::instrument(
    name = "tascarrel_vm.disk.ensure_sparse_raw",
    level = "debug",
    skip_all,
    fields(image = %path.display(), minimum_size = minimum_size),
    ret,
    err
)]
pub fn ensure_sparse_raw_disk(
    path: &Path,
    minimum_size: u64,
) -> Result<SparseRawDiskOutcome, Report<SparseRawDiskError>> {
    if minimum_size == 0 {
        return Err(Report::new(SparseRawDiskError::ZeroSize));
    }

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(Report::new(SparseRawDiskError::InvalidImage(
                    path.to_owned(),
                )));
            }
            grow_sparse_raw_disk(path, minimum_size)
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            create_sparse_raw_disk(path, minimum_size)
        }
        Err(source) => Err(disk_io_error("inspect", path, source)),
    }
}

/// Creates a new sparse image and removes incomplete output after a failure.
fn create_sparse_raw_disk(
    path: &Path,
    size: u64,
) -> Result<SparseRawDiskOutcome, Report<SparseRawDiskError>> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| disk_io_error("create", path, source))?;
    if let Err(source) = initialize_sparse_raw_disk(path, &file, size) {
        drop(file);
        if let Err(cleanup_error) = fs::remove_file(path) {
            warn!(
                image = %path.display(),
                error = %cleanup_error,
                "failed to remove incomplete sparse raw disk image"
            );
        }
        return Err(source);
    }
    Ok(SparseRawDiskOutcome::Created)
}

/// Grows an existing regular image when the requested size is larger.
fn grow_sparse_raw_disk(
    path: &Path,
    minimum_size: u64,
) -> Result<SparseRawDiskOutcome, Report<SparseRawDiskError>> {
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| disk_io_error("open", path, source))?;
    let metadata = file
        .metadata()
        .map_err(|source| disk_io_error("inspect", path, source))?;
    if !metadata.is_file() {
        return Err(Report::new(SparseRawDiskError::InvalidImage(
            path.to_owned(),
        )));
    }
    let current_size = metadata.len();
    if current_size >= minimum_size {
        return Ok(SparseRawDiskOutcome::Unchanged { current_size });
    }
    file.set_len(minimum_size)
        .map_err(|source| disk_io_error("grow", path, source))?;
    file.sync_all()
        .map_err(|source| disk_io_error("sync", path, source))?;
    Ok(SparseRawDiskOutcome::Grown {
        previous_size: current_size,
    })
}

/// Applies creation-only filesystem policy before assigning virtual capacity.
fn initialize_sparse_raw_disk(
    path: &Path,
    file: &File,
    size: u64,
) -> Result<(), Report<SparseRawDiskError>> {
    mark_nocow_if_btrfs(path, file)
        .map_err(|source| disk_io_error("mark as NOCOW", path, source))?;
    file.set_len(size)
        .map_err(|source| disk_io_error("size", path, source))?;
    file.sync_all()
        .map_err(|source| disk_io_error("sync", path, source))
}

#[cfg(target_os = "linux")]
fn mark_nocow_if_btrfs(path: &Path, file: &File) -> io::Result<()> {
    use rustix::fs::IFlags;
    use rustix::fs::ioctl_getflags;
    use rustix::fs::ioctl_setflags;
    use rustix::fs::statfs;

    const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if statfs(parent)?.f_type as i64 == BTRFS_SUPER_MAGIC {
        let flags = ioctl_getflags(file)?;
        ioctl_setflags(file, flags | IFlags::NOCOW)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn mark_nocow_if_btrfs(_path: &Path, _file: &File) -> io::Result<()> {
    Ok(())
}

fn disk_io_error(
    operation: &'static str,
    path: &Path,
    source: io::Error,
) -> Report<SparseRawDiskError> {
    Report::new(SparseRawDiskError::Io {
        operation,
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::fs::File;
    use std::os::unix::fs::MetadataExt;

    #[cfg(target_os = "linux")]
    use rustix::fs::IFlags;
    #[cfg(target_os = "linux")]
    use rustix::fs::ioctl_getflags;
    #[cfg(target_os = "linux")]
    use rustix::fs::statfs;
    use tempfile::tempdir_in;

    use super::*;

    /// Verifies sparse creation, grow-only resizing, and Linux Btrfs NOCOW
    /// policy.
    #[test]
    fn sparse_raw_disks_are_created_and_only_grown() {
        let directory = tempdir_in(".").unwrap();
        let disk = directory.path().join("state.raw");
        let initial_size = 1024_u64.pow(4);

        assert_eq!(
            ensure_sparse_raw_disk(&disk, initial_size).unwrap(),
            SparseRawDiskOutcome::Created
        );
        let metadata = fs::metadata(&disk).unwrap();
        assert_eq!(metadata.len(), initial_size);
        assert!(metadata.blocks() * 512 < 1024 * 1024);

        #[cfg(target_os = "linux")]
        {
            if statfs(directory.path()).unwrap().f_type as i64 == 0x9123_683e {
                assert!(
                    ioctl_getflags(File::open(&disk).unwrap())
                        .unwrap()
                        .contains(IFlags::NOCOW)
                );
            }
        }

        assert_eq!(
            ensure_sparse_raw_disk(&disk, initial_size * 2).unwrap(),
            SparseRawDiskOutcome::Grown {
                previous_size: initial_size
            }
        );
        assert_eq!(fs::metadata(&disk).unwrap().len(), initial_size * 2);
        assert_eq!(
            ensure_sparse_raw_disk(&disk, initial_size).unwrap(),
            SparseRawDiskOutcome::Unchanged {
                current_size: initial_size * 2
            }
        );
        assert_eq!(fs::metadata(&disk).unwrap().len(), initial_size * 2);
    }
}
