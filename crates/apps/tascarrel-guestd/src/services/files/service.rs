//! Descriptor-relative pod file operations.
//!
//! [`FilesService`] performs uncached directory reads and opens file bodies for
//! streaming below the workspace or a configured host-share root. Every path
//! component is resolved without following symbolic links.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::os::fd::OwnedFd;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

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
use tascarrel_protocol::valid_workspace_share_name;
use tascarrel_sharefs::DirectoryEntry as ShareDirectoryEntry;
use tascarrel_sharefs::EntryKind as ShareEntryKind;
use thiserror::Error;

use crate::services::changes::ChangesService;
use crate::services::pods::PodService;

/// Immutable pod file roots configured for one guest daemon.
#[derive(Clone, Debug, Default)]
pub struct FilesServiceConfig {
    shares: BTreeMap<String, ConfiguredShare>,
}

impl FilesServiceConfig {
    /// Adds one ordinary host share backed by a guest directory.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request report when the share name or root is unsafe.
    pub fn add_directory_share(
        &mut self,
        name: impl Into<String>,
        root: impl Into<PathBuf>,
    ) -> Result<(), Report<FilesServiceError>> {
        let name = name.into();
        let root = root.into();
        validate_share(&name)?;
        if !root.is_absolute() {
            return Err(invalid("share file root must be absolute"));
        }
        self.shares.insert(name, ConfiguredShare::Directory(root));
        Ok(())
    }

    /// Adds one pod-private overlay host share.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request report when the share name is unsafe.
    pub fn add_overlay_share(
        &mut self,
        name: impl Into<String>,
    ) -> Result<(), Report<FilesServiceError>> {
        let name = name.into();
        validate_share(&name)?;
        self.shares.insert(name, ConfiguredShare::Overlay);
        Ok(())
    }
}

/// Uncached inspection service for pod-visible file roots.
#[derive(Clone, Debug)]
pub struct FilesService {
    config: Arc<FilesServiceConfig>,
}

impl FilesService {
    /// Creates a pod file service from the roots pinned to the workspace VM.
    #[must_use]
    pub fn new(config: FilesServiceConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Lists the roots available to one existing pod.
    ///
    /// # Errors
    ///
    /// Returns an unavailable report when the pod workspace cannot be resolved.
    #[tracing::instrument(level = "debug", skip_all, fields(pod_id = %pod_id.0))]
    pub async fn list_roots(
        &self,
        pod_id: &PodId,
        pods: &PodService,
    ) -> Result<api::ListRootsOutput, Report<FilesServiceError>> {
        pods.workspace_root(pod_id).await.map_err(|report| {
            report.escalate(FilesServiceError::Unavailable(
                "failed to resolve pod file roots".to_owned(),
            ))
        })?;
        let mut roots = Vec::with_capacity(self.config.shares.len() + 1);
        roots.push(api::FileRoot::Workspace);
        roots.extend(self.config.shares.keys().map(|name| {
            api::FileRoot::Share(api::ShareFileRoot {
                name: name.clone().into(),
            })
        }));
        Ok(api::ListRootsOutput {
            roots: roots.into(),
        })
    }

    /// Reads the immediate children of one directory below a pod file root.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an invalid path, an unavailable report
    /// when the root cannot be read, and an internal report if the
    /// blocking inspection task fails.
    #[tracing::instrument(level = "debug", skip_all, fields(pod_id = %input.pod_id.0, root = ?input.root, path = %input.path))]
    pub async fn read_directory(
        &self,
        input: api::ReadDirectoryAction,
        pods: &PodService,
        changes: &ChangesService,
    ) -> Result<api::ReadDirectoryOutput, Report<FilesServiceError>> {
        validate_relative(input.path.as_str(), true)?;
        let relative = input.path.as_str();
        let (entries, git_statuses) = match &input.root {
            None | Some(api::FileRoot::Workspace) => {
                let workspace = pods.workspace_root(&input.pod_id).await.map_err(|report| {
                    report.escalate(FilesServiceError::Unavailable(
                        "failed to resolve pod workspace".to_owned(),
                    ))
                })?;
                let entries =
                    list_directory_blocking(workspace.clone(), relative.to_owned()).await?;
                changes
                    .watch_directory(&input.pod_id, &workspace, relative)
                    .await;
                let statuses = changes.directory_statuses(&input.pod_id, relative).await;
                (entries, Some(statuses))
            }
            Some(api::FileRoot::Share(share)) => {
                let configured = self.configured_share(share.name.as_ref())?;
                let entries = match configured {
                    ConfiguredShare::Directory(root) => {
                        list_directory_blocking(root.clone(), relative.to_owned()).await?
                    }
                    ConfiguredShare::Overlay => pods
                        .read_share_overlay_directory(
                            &input.pod_id,
                            share.name.as_ref(),
                            Path::new(relative),
                        )
                        .await
                        .map_err(|report| {
                            report.escalate(FilesServiceError::Unavailable(
                                "failed to read pod overlay share".to_owned(),
                            ))
                        })?
                        .into_iter()
                        .map(listed_share_entry)
                        .collect::<Result<Vec<_>, _>>()?,
                };
                (entries, None)
            }
        };
        let mut output = Vec::with_capacity(entries.len());
        for entry in entries {
            let git_status = if entry.kind == api::FileKind::File {
                git_statuses
                    .as_ref()
                    .and_then(|statuses| statuses.get(&entry.name).cloned())
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

    /// Opens one regular file below a pod file root for data-plane streaming.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an invalid path and an unavailable
    /// report when the file cannot be opened without following links.
    #[tracing::instrument(level = "debug", skip_all, fields(pod_id = %pod_id.0, root = ?root, path))]
    pub async fn open_file(
        &self,
        pod_id: &PodId,
        root: &api::FileRoot,
        path: &str,
        pods: &PodService,
    ) -> Result<FileRead, Report<FilesServiceError>> {
        validate_relative(path, false)?;
        let opened = match root {
            api::FileRoot::Workspace => {
                let workspace = pods.workspace_root(pod_id).await.map_err(|report| {
                    report.escalate(FilesServiceError::Unavailable(
                        "failed to resolve pod workspace".to_owned(),
                    ))
                })?;
                open_file_blocking(workspace, path.to_owned()).await?
            }
            api::FileRoot::Share(share) => match self.configured_share(share.name.as_ref())? {
                ConfiguredShare::Directory(root) => {
                    open_file_blocking(root.clone(), path.to_owned()).await?
                }
                ConfiguredShare::Overlay => {
                    let descriptor = pods
                        .open_share_overlay_file(pod_id, share.name.as_ref(), Path::new(path))
                        .await
                        .map_err(|report| {
                            report.escalate(FilesServiceError::Unavailable(
                                "failed to open pod overlay share file".to_owned(),
                            ))
                        })?;
                    let size = descriptor
                        .metadata()
                        .map_err(|error| {
                            unavailable(format!("failed to inspect share file: {error}"))
                        })?
                        .len();
                    OpenedRegularFile {
                        descriptor: descriptor.into(),
                        size,
                    }
                }
            },
        };
        Ok(FileRead {
            file: tokio::fs::File::from_std(std::fs::File::from(opened.descriptor)),
            size: opened.size,
        })
    }

    /// Resolves one configured share while rejecting arbitrary `/mnt` roots.
    fn configured_share(&self, name: &str) -> Result<&ConfiguredShare, Report<FilesServiceError>> {
        validate_share(name)?;
        self.config
            .shares
            .get(name)
            .ok_or_else(|| invalid(format!("host share {name:?} is not configured")))
    }
}

#[derive(Clone, Debug)]
enum ConfiguredShare {
    Directory(PathBuf),
    Overlay,
}

/// One safely opened file ready for raw data-plane streaming.
pub struct FileRead {
    /// Open file handle pinned to the validated inode.
    pub file: tokio::fs::File,
    /// Byte length observed on the pinned file handle before streaming.
    pub size: u64,
}

/// Failure from pod file inspection.
#[derive(Debug, Error)]
pub enum FilesServiceError {
    /// The supplied root or path violates the file request contract.
    #[error("invalid file request: {0}")]
    InvalidRequest(String),
    /// The requested pod or file is not currently available.
    #[error("pod file is unavailable: {0}")]
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

/// Runs descriptor-relative directory inspection outside the async runtime.
async fn list_directory_blocking(
    root: PathBuf,
    relative: String,
) -> Result<Vec<ListedEntry>, Report<FilesServiceError>> {
    tokio::task::spawn_blocking(move || list_directory(&root, &relative))
        .await
        .map_err(|error| internal(format!("failed to join directory read: {error}")))?
}

/// Runs descriptor-relative file opening outside the async runtime.
async fn open_file_blocking(
    root: PathBuf,
    relative: String,
) -> Result<OpenedRegularFile, Report<FilesServiceError>> {
    tokio::task::spawn_blocking(move || open_regular_file(&root, &relative))
        .await
        .map_err(|error| internal(format!("failed to join file open: {error}")))?
}

/// Converts `ShareFS` metadata into the Files API directory representation.
fn listed_share_entry(
    entry: ShareDirectoryEntry,
) -> Result<ListedEntry, Report<FilesServiceError>> {
    let name = entry
        .name
        .into_string()
        .map_err(|_| unavailable("share contains a non-UTF-8 file name"))?;
    let (kind, size) = match entry.metadata.kind {
        ShareEntryKind::Directory => (api::FileKind::Directory, None),
        ShareEntryKind::File => (api::FileKind::File, Some(entry.metadata.size)),
        ShareEntryKind::Symlink => (api::FileKind::Symlink, None),
    };
    Ok(ListedEntry { name, kind, size })
}

fn list_directory(
    root: &Path,
    relative: &str,
) -> Result<Vec<ListedEntry>, Report<FilesServiceError>> {
    let requested = open_directory(root, relative)?;
    let mut directory = Dir::read_from(&requested)
        .map_err(|error| unavailable(format!("failed to read file directory: {error}")))?;
    let mut entries = Vec::new();
    for entry in &mut directory {
        let entry = entry.map_err(|error| {
            unavailable(format!("failed to read file directory entry: {error}"))
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
                        unavailable(format!("failed to inspect file directory entry: {error}"))
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

/// Opens a file-root directory through no-follow, descriptor-relative
/// traversal.
///
/// # Errors
///
/// Returns a contract report for an invalid path or an unavailable report when
/// any directory component cannot be opened safely.
pub(crate) fn open_directory(
    root: &Path,
    relative: &str,
) -> Result<OwnedFd, Report<FilesServiceError>> {
    validate_relative(relative, true)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = open(root, flags, Mode::empty())
        .map_err(|error| unavailable(format!("failed to open file root: {error}")))?;
    for component in Path::new(relative).components() {
        let Component::Normal(name) = component else {
            return Err(invalid("path must be relative to the selected file root"));
        };
        directory = openat(&directory, name, flags, Mode::empty())
            .map_err(|error| unavailable(format!("failed to open file directory: {error}")))?;
    }
    Ok(directory)
}

fn open_regular_file(
    root: &Path,
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
    let directory = open_directory(root, parent)?;
    let file = openat(
        &directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| unavailable(format!("failed to open pod file: {error}")))?;
    let metadata = fstat(&file)
        .map_err(|error| unavailable(format!("failed to inspect pod file: {error}")))?;
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
        .map_err(|error| unavailable(format!("pod file has an invalid size: {error}")))
}

/// Validates a logical host-share name before it participates in path
/// resolution.
fn validate_share(name: &str) -> Result<(), Report<FilesServiceError>> {
    if !valid_workspace_share_name(name) {
        return Err(invalid("share must use a valid workspace-local name"));
    }
    Ok(())
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
            "path must be a normalized file-root-relative UTF-8 path",
        ));
    }
    Ok(())
}

fn utf8_name(name: &CStr) -> Result<String, Report<FilesServiceError>> {
    std::str::from_utf8(name.to_bytes())
        .map(str::to_owned)
        .map_err(|_| unavailable("file root contains a non-UTF-8 file name"))
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

    /// Rejects share names and lookups which are not part of the pinned root
    /// inventory.
    #[test]
    fn file_service_restricts_share_roots_to_its_configuration() {
        let directory = tempfile::tempdir().expect("share fixture is created");
        let mut config = FilesServiceConfig::default();
        config
            .add_directory_share("source", directory.path())
            .expect("portable share is configured");
        assert!(config.add_overlay_share("invalid.name").is_err());

        let service = FilesService::new(config);
        assert!(service.configured_share("source").is_ok());
        assert!(service.configured_share("other").is_err());
        assert!(service.configured_share("../source").is_err());
    }

    /// Rejects path spellings which could escape or ambiguously address the
    /// selected root.
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
