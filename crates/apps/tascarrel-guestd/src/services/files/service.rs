//! Descriptor-relative workspace file operations.
//!
//! [`FilesService`] performs uncached directory reads and opens file bodies for
//! streaming. Every path component is resolved relative to a pinned workspace
//! directory descriptor without following symbolic links.

use std::ffi::CStr;
use std::os::fd::OwnedFd;
use std::path::Component;
use std::path::Path;

use reportify::ErrorExt as _;
use reportify::Report;
use rustix::fs::AtFlags;
use rustix::fs::Dir;
use rustix::fs::FileType;
use rustix::fs::Mode;
use rustix::fs::OFlags;
use rustix::fs::fstat;
use rustix::fs::open;
use rustix::fs::openat;
use rustix::fs::statat;
use tascarrel_api::MAX_RELATIVE_PATH_BYTES;
use tascarrel_api::types::files as api;
use tascarrel_api::types::pods::PodId;
use thiserror::Error;

use crate::services::changes::ChangesService;
use crate::services::pods::PodService;

/// Stateless workspace file inspection service.
#[derive(Clone, Copy, Debug, Default)]
pub struct FilesService;

impl FilesService {
    /// Creates a workspace file service.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Reads the immediate children of one workspace directory.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an invalid path, an unavailable report
    /// when the workspace cannot be read, and an internal report if the
    /// blocking inspection task fails.
    #[tracing::instrument(level = "debug", skip_all, fields(pod_id = %input.pod_id.0, path = %input.path))]
    pub async fn read_directory(
        &self,
        input: api::ReadDirectoryAction,
        pods: &PodService,
        changes: &ChangesService,
    ) -> Result<api::ReadDirectoryOutput, Report<FilesServiceError>> {
        validate_relative(input.path.as_str(), true)?;
        let workspace = pods.workspace_root(&input.pod_id).await.map_err(|report| {
            report.escalate(FilesServiceError::Unavailable(
                "failed to resolve pod workspace".to_owned(),
            ))
        })?;
        let relative = input.path.as_str().to_owned();
        let listed_workspace = workspace.clone();
        let listed_relative = relative.clone();
        let entries = tokio::task::spawn_blocking(move || {
            list_directory(&listed_workspace, &listed_relative)
        })
        .await
        .map_err(|error| internal(format!("failed to join directory read: {error}")))??;

        changes
            .watch_directory(&input.pod_id, &workspace, &relative)
            .await;
        let git_statuses = changes.directory_statuses(&input.pod_id, &relative).await;
        let mut output = Vec::with_capacity(entries.len());
        for entry in entries {
            let git_status = if entry.kind == api::FileKind::File {
                git_statuses.get(&entry.name).cloned()
            } else {
                None
            };
            output.push(api::FileEntry {
                name: entry.name.into(),
                kind: entry.kind,
                size: entry.size,
                git_status,
            });
        }
        Ok(api::ReadDirectoryOutput {
            entries: output.into(),
        })
    }

    /// Opens one regular workspace file for data-plane streaming.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an invalid path and an unavailable
    /// report when the file cannot be opened without following links.
    #[tracing::instrument(level = "debug", skip_all, fields(pod_id = %pod_id.0, path))]
    pub async fn open_file(
        &self,
        pod_id: &PodId,
        path: &str,
        pods: &PodService,
    ) -> Result<FileRead, Report<FilesServiceError>> {
        validate_relative(path, false)?;
        let workspace = pods.workspace_root(pod_id).await.map_err(|report| {
            report.escalate(FilesServiceError::Unavailable(
                "failed to resolve pod workspace".to_owned(),
            ))
        })?;
        let relative = path.to_owned();
        let opened = tokio::task::spawn_blocking(move || open_regular_file(&workspace, &relative))
            .await
            .map_err(|error| internal(format!("failed to join file open: {error}")))??;
        Ok(FileRead {
            file: tokio::fs::File::from_std(std::fs::File::from(opened.descriptor)),
            size: opened.size,
        })
    }
}

/// One safely opened file ready for raw data-plane streaming.
pub struct FileRead {
    /// Open file handle pinned to the validated inode.
    pub file: tokio::fs::File,
    /// Byte length observed on the pinned file handle before streaming.
    pub size: u64,
}

/// Failure from workspace file inspection.
#[derive(Debug, Error)]
pub enum FilesServiceError {
    /// The supplied path violates the workspace-relative path contract.
    #[error("invalid file request: {0}")]
    InvalidRequest(String),
    /// The requested pod or file is not currently available.
    #[error("workspace file is unavailable: {0}")]
    Unavailable(String),
    /// Guest filesystem inspection failed unexpectedly.
    #[error("file service failed: {0}")]
    Internal(String),
}

#[derive(Debug)]
struct ListedEntry {
    name: String,
    kind: api::FileKind,
    size: Option<u64>,
}

#[derive(Debug)]
struct OpenedRegularFile {
    descriptor: OwnedFd,
    size: u64,
}

fn list_directory(
    workspace: &Path,
    relative: &str,
) -> Result<Vec<ListedEntry>, Report<FilesServiceError>> {
    let requested = open_directory(workspace, relative)?;
    let mut directory = Dir::read_from(&requested)
        .map_err(|error| unavailable(format!("failed to read workspace directory: {error}")))?;
    let mut entries = Vec::new();
    for entry in &mut directory {
        let entry = entry.map_err(|error| {
            unavailable(format!("failed to read workspace directory entry: {error}"))
        })?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let name = utf8_name(name)?;
        let directory_entry_type = entry.file_type();
        let (file_type, size) =
            if directory_entry_type == FileType::Unknown || directory_entry_type.is_file() {
                let metadata = statat(&requested, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|error| {
                        unavailable(format!(
                            "failed to inspect workspace directory entry: {error}"
                        ))
                    })?;
                let file_type = FileType::from_raw_mode(metadata.st_mode);
                let size = if file_type.is_file() {
                    Some(file_size(metadata.st_size)?)
                } else {
                    None
                };
                (file_type, size)
            } else {
                (directory_entry_type, None)
            };
        entries.push(ListedEntry {
            name,
            kind: classify(file_type),
            size,
        });
    }
    entries.sort_by(|left, right| {
        file_kind_order(&left.kind)
            .cmp(&file_kind_order(&right.kind))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

/// Opens a workspace directory through no-follow, descriptor-relative
/// traversal.
///
/// # Errors
///
/// Returns a contract report for an invalid path or an unavailable report when
/// any directory component cannot be opened safely.
pub(crate) fn open_directory(
    workspace: &Path,
    relative: &str,
) -> Result<OwnedFd, Report<FilesServiceError>> {
    validate_relative(relative, true)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = open(workspace, flags, Mode::empty())
        .map_err(|error| unavailable(format!("failed to open workspace root: {error}")))?;
    for component in Path::new(relative).components() {
        let Component::Normal(name) = component else {
            return Err(invalid("path must be relative to the workspace"));
        };
        directory = openat(&directory, name, flags, Mode::empty())
            .map_err(|error| unavailable(format!("failed to open workspace directory: {error}")))?;
    }
    Ok(directory)
}

fn open_regular_file(
    workspace: &Path,
    relative: &str,
) -> Result<OpenedRegularFile, Report<FilesServiceError>> {
    let path = Path::new(relative);
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let name = path
        .file_name()
        .ok_or_else(|| invalid("file path must name one file"))?;
    let parent = parent
        .to_str()
        .ok_or_else(|| invalid("file path must be UTF-8"))?;
    let directory = open_directory(workspace, parent)?;
    let file = openat(
        &directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| unavailable(format!("failed to open workspace file: {error}")))?;
    let metadata = fstat(&file)
        .map_err(|error| unavailable(format!("failed to inspect workspace file: {error}")))?;
    if !FileType::from_raw_mode(metadata.st_mode).is_file() {
        return Err(invalid("path does not identify a regular file"));
    }
    Ok(OpenedRegularFile {
        descriptor: file,
        size: file_size(metadata.st_size)?,
    })
}

fn file_size(size: i64) -> Result<u64, Report<FilesServiceError>> {
    u64::try_from(size)
        .map_err(|error| unavailable(format!("workspace file has an invalid size: {error}")))
}

/// Validates the shared normalized UTF-8 relative-path contract.
///
/// # Errors
///
/// Returns a contract report when the path is not normalized or exceeds the
/// shared encoded-length bound.
pub(crate) fn validate_relative(
    value: &str,
    allow_empty: bool,
) -> Result<(), Report<FilesServiceError>> {
    if value.len() > MAX_RELATIVE_PATH_BYTES
        || value.as_bytes().contains(&0)
        || (!allow_empty && value.is_empty())
        || value.starts_with('/')
        || value.ends_with('/')
        || (!value.is_empty()
            && value
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | "..")))
    {
        return Err(invalid(
            "path must be a normalized workspace-relative UTF-8 path",
        ));
    }
    Ok(())
}

fn utf8_name(name: &CStr) -> Result<String, Report<FilesServiceError>> {
    std::str::from_utf8(name.to_bytes())
        .map(str::to_owned)
        .map_err(|_| unavailable("workspace contains a non-UTF-8 file name"))
}

fn classify(file_type: FileType) -> api::FileKind {
    if file_type.is_dir() {
        api::FileKind::Directory
    } else if file_type.is_file() {
        api::FileKind::File
    } else if file_type.is_symlink() {
        api::FileKind::Symlink
    } else {
        api::FileKind::Other
    }
}

const fn file_kind_order(kind: &api::FileKind) -> u8 {
    match kind {
        api::FileKind::Directory => 0,
        api::FileKind::File => 1,
        api::FileKind::Symlink => 2,
        api::FileKind::Other => 3,
    }
}

fn invalid(message: impl Into<String>) -> Report<FilesServiceError> {
    FilesServiceError::InvalidRequest(message.into()).report()
}

fn unavailable(message: impl Into<String>) -> Report<FilesServiceError> {
    FilesServiceError::Unavailable(message.into()).report()
}

fn internal(message: impl Into<String>) -> Report<FilesServiceError> {
    FilesServiceError::Internal(message.into()).report()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rejects path spellings which could escape or ambiguously address the
    /// workspace.
    #[test]
    fn relative_path_validation_rejects_unsafe_spellings() {
        for path in ["/etc", "a/../b", "a/./b", "a//b", "a/", "\0"] {
            assert!(validate_relative(path, true).is_err(), "accepted {path:?}");
        }
        assert!(validate_relative("", true).is_ok());
        assert!(validate_relative("repo/src/lib.rs", false).is_ok());
    }

    /// Lists ordinary files, directories, and links without following the link
    /// target.
    #[test]
    fn directory_listing_classifies_entries() {
        let root = tempfile::tempdir().expect("workspace fixture is created");
        std::fs::create_dir(root.path().join("directory")).expect("directory fixture is created");
        std::fs::write(root.path().join("file"), b"contents").expect("file fixture is created");
        std::os::unix::fs::symlink("file", root.path().join("link"))
            .expect("link fixture is created");

        let entries = list_directory(root.path(), "").expect("workspace root is listed");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, api::FileKind::Directory);
        assert_eq!(entries[0].size, None);
        assert_eq!(entries[1].kind, api::FileKind::File);
        assert_eq!(entries[1].size, Some(8));
        assert_eq!(entries[2].kind, api::FileKind::Symlink);
        assert_eq!(entries[2].size, None);
    }

    /// Rejects direct and parent-directory symbolic links when opening file
    /// bodies for data-plane streaming.
    #[test]
    fn file_open_does_not_follow_symbolic_links() {
        let root = tempfile::tempdir().expect("workspace fixture is created");
        let outside = tempfile::tempdir().expect("outside fixture is created");
        std::fs::write(outside.path().join("secret"), b"outside")
            .expect("outside fixture is written");
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("file-link"))
            .expect("file link fixture is created");
        std::os::unix::fs::symlink(outside.path(), root.path().join("directory-link"))
            .expect("directory link fixture is created");

        assert!(open_regular_file(root.path(), "file-link").is_err());
        assert!(open_regular_file(root.path(), "directory-link/secret").is_err());
    }
}
