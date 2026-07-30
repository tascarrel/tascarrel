//! Unix path, metadata, hashing, and durable-directory helpers.
//!
//! This module centralizes platform-specific operations shared by the public
//! interface and durable namespace core. Its internal functions preserve raw
//! Unix names and translate operating-system failures into share errors.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use reportify::Report;
use rustix::fs::FlockOperation;
use rustix::fs::flock;
use rustix::io::Errno;
use sha2::Digest as _;
use sha2::Sha256;

use super::LOCK_FILE;
use super::LOGICAL_MODE_MASK;
use crate::ContentDigest;
use crate::DirectoryEntry;
use crate::EntryKind;
use crate::EntryMetadata;
use crate::EntryVersion;
use crate::FileTime;
use crate::LowerLease;
use crate::ShareFsError;
use crate::ShareFsResult;
use crate::state::BaseRecord;

pub(crate) fn prepare_lower_directory(path: &Path) -> ShareFsResult<PathBuf> {
    if !path.is_absolute() {
        return Err(Report::new(ShareFsError::InvalidLowerDirectory {
            path: path.to_owned(),
        }));
    }
    let metadata = optional_symlink_metadata(path, "inspect the share lower directory")?
        .ok_or_else(|| {
            Report::new(ShareFsError::InvalidLowerDirectory {
                path: path.to_owned(),
            })
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Report::new(ShareFsError::InvalidLowerDirectory {
            path: path.to_owned(),
        }));
    }
    fs::canonicalize(path).map_err(|source| io_error("resolve the share lower directory", source))
}

pub(crate) fn prepare_state_directory(path: &Path) -> ShareFsResult<PathBuf> {
    if !path.is_absolute() {
        return Err(Report::new(ShareFsError::InvalidStateDirectory {
            path: path.to_owned(),
        }));
    }
    ensure_private_directory(path)?;
    let metadata = symlink_metadata(path, "inspect the share state directory")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Report::new(ShareFsError::InvalidStateDirectory {
            path: path.to_owned(),
        }));
    }
    fs::canonicalize(path).map_err(|source| io_error("resolve the share state directory", source))
}

/// Acquires exclusive ownership of one durable upper state.
pub(crate) fn acquire_state_lock(state_root: &Path) -> ShareFsResult<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(state_root.join(LOCK_FILE))
        .map_err(|source| io_error("open the share state lock", source))?;
    if let Err(source) = flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        if source == Errno::WOULDBLOCK {
            return Err(Report::new(ShareFsError::StateInUse));
        }
        return Err(io_error("lock the share state", source.into()));
    }
    Ok(lock)
}

pub(crate) fn ensure_private_directory(path: &Path) -> ShareFsResult<()> {
    let mut builder = fs::DirBuilder::new();
    builder
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|source| io_error("create a private share state directory", source))?;
    let metadata = symlink_metadata(path, "inspect a private share state directory")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Report::new(ShareFsError::InvalidStateDirectory {
            path: path.to_owned(),
        }));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error("set private share state permissions", source))
}

pub(crate) fn clean_staging_directory(path: &Path) -> ShareFsResult<()> {
    for entry in
        fs::read_dir(path).map_err(|source| io_error("inventory staged upper objects", source))?
    {
        let entry = entry.map_err(|source| io_error("read a staged upper object entry", source))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|source| io_error("inspect a staged upper object", source))?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            return Err(Report::new(ShareFsError::CorruptState));
        }
        fs::remove_file(entry.path())
            .map_err(|source| io_error("remove an interrupted staged upper object", source))?;
    }
    Ok(())
}

pub(crate) fn normalize_path(path: &Path) -> ShareFsResult<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) if !name.as_bytes().contains(&0) => normalized.push(name),
            _ => {
                return Err(Report::new(ShareFsError::InvalidPath {
                    path: path.to_owned(),
                }));
            }
        }
    }
    Ok(normalized)
}

pub(crate) fn normalize_non_root_path(path: &Path) -> ShareFsResult<PathBuf> {
    let path = normalize_path(path)?;
    if path.as_os_str().is_empty() {
        return Err(Report::new(ShareFsError::InvalidPath { path }));
    }
    Ok(path)
}

pub(crate) fn split_parent(path: &Path) -> ShareFsResult<(&Path, &OsStr)> {
    let parent = match path.parent() {
        Some(parent) => parent,
        None => Path::new(""),
    };
    let name = path.file_name().ok_or_else(|| {
        Report::new(ShareFsError::InvalidPath {
            path: path.to_owned(),
        })
    })?;
    Ok((parent, name))
}

pub(crate) fn components_to_path(components: &[Component<'_>]) -> PathBuf {
    components.iter().collect()
}

pub(crate) fn read_lower_directory(
    lower: &Path,
    logical: &Path,
) -> ShareFsResult<Vec<DirectoryEntry>> {
    let metadata = symlink_metadata(lower, "inspect a lower directory")?;
    if entry_kind(&metadata, logical)? != EntryKind::Directory {
        return Err(Report::new(ShareFsError::NotDirectory {
            path: logical.to_owned(),
        }));
    }
    let mut entries = BTreeMap::new();
    for entry in
        fs::read_dir(lower).map_err(|source| io_error("enumerate a lower directory", source))?
    {
        let entry = entry.map_err(|source| io_error("read a lower directory entry", source))?;
        let entry_path = entry.path();
        let metadata = match fs::symlink_metadata(&entry_path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                tracing::trace!(
                    path = %entry_path.display(),
                    "lower directory entry disappeared during enumeration"
                );
                continue;
            }
            Err(source) => return Err(io_error("inspect a lower directory entry", source)),
        };
        let child_logical = logical.join(entry.file_name());
        let metadata = metadata_value(&metadata, &child_logical)?;
        let name = entry.file_name();
        entries.insert(name.clone(), DirectoryEntry { name, metadata });
    }
    Ok(entries.into_values().collect())
}

pub(crate) fn real_lower_directory(path: &Path) -> ShareFsResult<Option<PathBuf>> {
    let Some(metadata) =
        optional_symlink_metadata(path, "inspect a dynamically merged lower directory")?
    else {
        return Ok(None);
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(Some(path.to_owned()))
    } else {
        Ok(None)
    }
}

pub(crate) fn lower_metadata(path: &Path, logical: &Path) -> ShareFsResult<EntryMetadata> {
    let metadata = symlink_metadata(path, "inspect lower entry metadata")?;
    metadata_value(&metadata, logical)
}

pub(crate) fn metadata_value(
    metadata: &fs::Metadata,
    logical: &Path,
) -> ShareFsResult<EntryMetadata> {
    let kind = entry_kind(metadata, logical)?;
    let size = match kind {
        EntryKind::Directory => 0,
        EntryKind::File | EntryKind::Symlink => metadata.size(),
    };
    Ok(EntryMetadata {
        kind,
        size,
        mode: metadata.mode() & LOGICAL_MODE_MASK,
        modified_at: metadata_modified_time(metadata),
    })
}

pub(crate) fn base_record(
    metadata: &fs::Metadata,
    kind: EntryKind,
    digest: Option<ContentDigest>,
) -> BaseRecord {
    LowerLease {
        version: EntryVersion {
            kind,
            size: match kind {
                EntryKind::Directory => 0,
                EntryKind::File | EntryKind::Symlink => metadata.size(),
            },
            mode: metadata.mode() & LOGICAL_MODE_MASK,
            content_digest: digest,
        },
        modified_at: metadata_modified_time(metadata),
        changed_at: FileTime {
            seconds: metadata.ctime(),
            nanoseconds: u32::try_from(metadata.ctime_nsec()).map_or(0, |value| value),
        },
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

pub(crate) fn entry_kind(metadata: &fs::Metadata, path: &Path) -> ShareFsResult<EntryKind> {
    let file_type = metadata.file_type();
    if file_type.is_file() {
        Ok(EntryKind::File)
    } else if file_type.is_dir() {
        Ok(EntryKind::Directory)
    } else if file_type.is_symlink() {
        Ok(EntryKind::Symlink)
    } else {
        Err(Report::new(ShareFsError::UnsupportedEntryType {
            path: path.to_owned(),
        }))
    }
}

pub(crate) fn entry_type_error(path: &Path, metadata: &fs::Metadata) -> Report<ShareFsError> {
    if metadata.is_dir() {
        Report::new(ShareFsError::IsDirectory {
            path: path.to_owned(),
        })
    } else {
        Report::new(ShareFsError::UnsupportedEntryType {
            path: path.to_owned(),
        })
    }
}

pub(crate) fn entry_type_error_for_kind(path: &Path, kind: EntryKind) -> Report<ShareFsError> {
    if kind == EntryKind::Directory {
        Report::new(ShareFsError::IsDirectory {
            path: path.to_owned(),
        })
    } else {
        Report::new(ShareFsError::UnsupportedEntryType {
            path: path.to_owned(),
        })
    }
}

pub(crate) fn metadata_modified_time(metadata: &fs::Metadata) -> FileTime {
    FileTime {
        seconds: metadata.mtime(),
        nanoseconds: u32::try_from(metadata.mtime_nsec()).map_or(0, |value| value),
    }
}

pub(crate) fn same_fingerprint(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.mode() == right.mode()
        && left.size() == right.size()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

/// Checks the fast, non-hashing fields retained in one lower lease.
pub(crate) fn matches_lease_fingerprint(
    metadata: &fs::Metadata,
    kind: EntryKind,
    lease: &LowerLease,
) -> bool {
    kind == lease.version.kind
        && metadata.dev() == lease.device
        && metadata.ino() == lease.inode
        && (kind == EntryKind::Directory || metadata.size() == lease.version.size)
        && metadata.mode() & LOGICAL_MODE_MASK == lease.version.mode
        && metadata.mtime() == lease.modified_at.seconds
        && u32::try_from(metadata.mtime_nsec())
            .is_ok_and(|value| value == lease.modified_at.nanoseconds)
        && metadata.ctime() == lease.changed_at.seconds
        && u32::try_from(metadata.ctime_nsec())
            .is_ok_and(|value| value == lease.changed_at.nanoseconds)
}

pub(crate) fn open_lower_file(path: &Path) -> ShareFsResult<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|source| io_error("open a lower share file", source))
}

pub(crate) fn digest_file(path: &Path) -> ShareFsResult<ContentDigest> {
    let mut file =
        File::open(path).map_err(|source| io_error("open a proposed upper file", source))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|source| io_error("seek a proposed upper file", source))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 128 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|source| io_error("hash a proposed upper file", source))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(ContentDigest(hasher.finalize().into()))
}

pub(crate) fn digest_bytes(bytes: &[u8]) -> ContentDigest {
    ContentDigest(Sha256::digest(bytes).into())
}

pub(crate) fn now() -> FileTime {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => FileTime {
            seconds: i64::try_from(duration.as_secs()).map_or(i64::MAX, |value| value),
            nanoseconds: duration.subsec_nanos(),
        },
        Err(error) => {
            let duration = error.duration();
            FileTime {
                seconds: -i64::try_from(duration.as_secs()).map_or(i64::MAX, |value| value),
                nanoseconds: duration.subsec_nanos(),
            }
        }
    }
}

pub(crate) fn symlink_metadata(path: &Path, action: &'static str) -> ShareFsResult<fs::Metadata> {
    fs::symlink_metadata(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Report::new(ShareFsError::NotFound {
                path: path.to_owned(),
            })
        } else {
            io_error(action, source)
        }
    })
}

pub(crate) fn optional_symlink_metadata(
    path: &Path,
    action: &'static str,
) -> ShareFsResult<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error(action, source)),
    }
}

pub(crate) fn sync_directory(path: &Path) -> ShareFsResult<()> {
    File::open(path)
        .map_err(|source| io_error("open a share state directory for synchronization", source))?
        .sync_all()
        .map_err(|source| io_error("synchronize a share state directory", source))
}

pub(crate) fn remove_file_if_exists(path: &Path) -> ShareFsResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            tracing::trace!(path = %path.display(), "upper object was already absent");
            Ok(())
        }
        Err(source) => Err(io_error("remove an upper object", source)),
    }
}

pub(crate) fn concurrent_change(path: &Path) -> Report<ShareFsError> {
    Report::new(ShareFsError::ConcurrentLowerChange {
        path: path.to_owned(),
    })
}

pub(crate) fn io_error(action: &'static str, source: std::io::Error) -> Report<ShareFsError> {
    Report::new(ShareFsError::Io { action, source })
}
