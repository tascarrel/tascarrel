//! Descriptor-relative pod file operations.
//!
//! [`FilesService`] performs uncached directory reads, opens file bodies for
//! streaming, and revision-safely replaces complete text files below the
//! workspace or a configured host-share root. Every path component is resolved
//! without following symbolic links.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read as _;
use std::io::Write as _;
use std::os::fd::AsFd as _;
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
use rustix::fs::Gid;
use rustix::fs::Mode;
use rustix::fs::OFlags;
use rustix::fs::Uid;
use rustix::fs::fstat;
use rustix::fs::open;
use rustix::fs::openat;
use rustix::fs::statat;
use sha2::Digest as _;
use sha2::Sha256;
use tascarrel_api::MAX_RELATIVE_PATH_BYTES;
use tascarrel_api::types::files as api;
use tascarrel_api::types::pods::PodId;
use tascarrel_protocol::MAX_POD_FILE_WRITE_BYTES;
use tascarrel_protocol::valid_workspace_share_name;
use tascarrel_sharefs::ContentDigest;
use tascarrel_sharefs::DirectoryEntry as ShareDirectoryEntry;
use tascarrel_sharefs::EntryKind as ShareEntryKind;
use tascarrel_sharefs::FileWriteOutcome as ShareFileWriteOutcome;
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
    /// `writable` indicates whether browser replacements may pass through to
    /// the host directory.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request report when the share name or root is unsafe.
    pub fn add_directory_share(
        &mut self,
        name: impl Into<String>,
        root: impl Into<PathBuf>,
        writable: bool,
    ) -> Result<(), Report<FilesServiceError>> {
        let name = name.into();
        let root = root.into();
        validate_share(&name)?;
        if !root.is_absolute() {
            return Err(invalid("share file root must be absolute"));
        }
        self.shares
            .insert(name, ConfiguredShare::Directory { root, writable });
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

/// Uncached access service for pod-visible file roots.
#[derive(Clone, Debug)]
pub struct FilesService {
    config: Arc<FilesServiceConfig>,
    direct_write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl FilesService {
    /// Creates a pod file service from the roots pinned to the workspace VM.
    #[must_use]
    pub fn new(config: FilesServiceConfig) -> Self {
        Self {
            config: Arc::new(config),
            direct_write_lock: Arc::new(tokio::sync::Mutex::new(())),
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
                    ConfiguredShare::Directory { root, .. } => {
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
                ConfiguredShare::Directory { root, writable } => {
                    open_file_blocking(root.clone(), path.to_owned())
                        .await?
                        .with_writable(*writable)
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
                        writable: true,
                    }
                }
            },
        };
        Ok(FileRead {
            file: tokio::fs::File::from_std(std::fs::File::from(opened.descriptor)),
            size: opened.size,
            writable: opened.writable,
        })
    }

    /// Replaces one complete UTF-8 file when its contents match a revision.
    ///
    /// # Errors
    ///
    /// Returns a contract report for invalid content or paths, a read-only
    /// report for immutable roots, a conflict report for stale revisions, and
    /// an unavailable report when the replacement cannot be committed.
    #[tracing::instrument(level = "debug", skip_all, fields(pod_id = %pod_id.0, root = ?root, path, bytes = contents.len()))]
    pub async fn replace_file(
        &self,
        pod_id: &PodId,
        root: &api::FileRoot,
        path: &str,
        expected_revision: &str,
        contents: Vec<u8>,
        pods: &PodService,
    ) -> Result<FileWrite, Report<FilesServiceError>> {
        validate_relative(path, false)?;
        validate_file_contents(&contents)?;
        let expected = parse_revision(expected_revision)?;
        let revision = match root {
            api::FileRoot::Workspace => {
                let _write = self.direct_write_lock.lock().await;
                let workspace = pods.workspace_root(pod_id).await.map_err(|report| {
                    report.escalate(FilesServiceError::Unavailable(
                        "failed to resolve pod workspace".to_owned(),
                    ))
                })?;
                replace_file_blocking(workspace, path.to_owned(), expected, contents).await?
            }
            api::FileRoot::Share(share) => match self.configured_share(share.name.as_ref())? {
                ConfiguredShare::Directory {
                    root: _,
                    writable: false,
                } => return Err(FilesServiceError::ReadOnly.report()),
                ConfiguredShare::Directory {
                    root,
                    writable: true,
                } => {
                    let _write = self.direct_write_lock.lock().await;
                    replace_file_blocking(root.clone(), path.to_owned(), expected, contents).await?
                }
                ConfiguredShare::Overlay => {
                    let expected = ContentDigest::from_array(expected);
                    match pods
                        .write_share_overlay_file_if_revision(
                            pod_id,
                            share.name.as_ref(),
                            Path::new(path),
                            expected,
                            &contents,
                        )
                        .await
                        .map_err(|report| {
                            report.escalate(FilesServiceError::Unavailable(
                                "failed to replace pod overlay share file".to_owned(),
                            ))
                        })? {
                        ShareFileWriteOutcome::Written { revision } => revision.to_string(),
                        ShareFileWriteOutcome::Conflict { .. } => {
                            return Err(FilesServiceError::Conflict.report());
                        }
                    }
                }
            },
        };
        Ok(FileWrite { revision })
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

/// One safely opened file ready for raw data-plane streaming.
pub struct FileRead {
    /// Open file handle pinned to the validated inode.
    pub file: tokio::fs::File,
    /// Byte length observed on the pinned file handle before streaming.
    pub size: u64,
    /// Whether the selected file root accepts replacements.
    pub writable: bool,
}

/// Result of one revision-checked complete-file replacement.
pub struct FileWrite {
    /// Lowercase hexadecimal SHA-256 revision of the new contents.
    pub revision: String,
}

/// Failure from pod file access.
#[derive(Debug, Error)]
pub enum FilesServiceError {
    /// The supplied root or path violates the file request contract.
    #[error("invalid file request: {0}")]
    InvalidRequest(String),
    /// The selected root does not allow file replacement.
    #[error("failed to replace pod file: root is read-only")]
    ReadOnly,
    /// The file changed since its contents were observed.
    #[error("failed to replace pod file: file changed since it was read")]
    Conflict,
    /// The requested pod or file is not currently available.
    #[error("pod file is unavailable: {0}")]
    Unavailable(String),
    /// Guest filesystem inspection failed unexpectedly.
    #[error("file service failed: {0}")]
    Internal(String),
}

#[derive(Clone, Debug)]
enum ConfiguredShare {
    Directory { root: PathBuf, writable: bool },
    Overlay,
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
    writable: bool,
}

impl OpenedRegularFile {
    fn with_writable(mut self, writable: bool) -> Self {
        self.writable = writable;
        self
    }
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

/// Runs revision validation and complete-file replacement outside the async
/// runtime.
async fn replace_file_blocking(
    root: PathBuf,
    relative: String,
    expected: [u8; 32],
    contents: Vec<u8>,
) -> Result<String, Report<FilesServiceError>> {
    tokio::task::spawn_blocking(move || replace_regular_file(&root, &relative, expected, &contents))
        .await
        .map_err(|error| internal(format!("failed to join file replacement: {error}")))?
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
        writable: true,
    })
}

/// Publishes a descriptor-relative replacement after revalidating the target.
fn replace_regular_file(
    root: &Path,
    relative: &str,
    expected: [u8; 32],
    contents: &[u8],
) -> Result<String, Report<FilesServiceError>> {
    let path = Path::new(relative);
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let name = path
        .file_name()
        .ok_or_else(|| invalid("file path must name one file"))?;
    let parent = parent
        .to_str()
        .ok_or_else(|| invalid("file path must be UTF-8"))?;
    let directory = open_directory(root, parent)?;
    let current = open_regular_file_at(&directory, name)?;
    let ownership = file_ownership(&current.descriptor)?;
    if read_revision(current)? != expected {
        return Err(FilesServiceError::Conflict.report());
    }

    let temporary = OsString::from(format!(".tascarrel-edit-{}", uuid::Uuid::new_v4()));
    let descriptor = openat(
        &directory,
        &temporary,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|error| unavailable(format!("failed to create file replacement: {error}")))?;
    let mut replacement = File::from(descriptor);
    let result = (|| {
        replacement
            .write_all(contents)
            .map_err(|error| unavailable(format!("failed to write file replacement: {error}")))?;
        let replacement_metadata = fstat(&replacement)
            .map_err(|error| unavailable(format!("failed to inspect file replacement: {error}")))?;
        if replacement_metadata.st_uid != ownership.owner.as_raw()
            || replacement_metadata.st_gid != ownership.group.as_raw()
        {
            rustix::fs::fchown(
                replacement.as_fd(),
                Some(ownership.owner),
                Some(ownership.group),
            )
            .map_err(|error| unavailable(format!("failed to preserve file ownership: {error}")))?;
        }
        rustix::fs::fchmod(replacement.as_fd(), ownership.mode)
            .map_err(|error| unavailable(format!("failed to preserve file mode: {error}")))?;
        replacement
            .sync_all()
            .map_err(|error| unavailable(format!("failed to sync file replacement: {error}")))?;
        if read_revision(open_regular_file_at(&directory, name)?)? != expected {
            return Err(FilesServiceError::Conflict.report());
        }
        rustix::fs::renameat(&directory, &temporary, &directory, name)
            .map_err(|error| unavailable(format!("failed to publish file replacement: {error}")))?;
        rustix::fs::fsync(&directory)
            .map_err(|error| unavailable(format!("failed to sync file directory: {error}")))?;
        Ok(content_revision(contents))
    })();
    if result.is_err()
        && let Err(error) = rustix::fs::unlinkat(&directory, &temporary, AtFlags::empty())
    {
        tracing::warn!(temporary = ?temporary, %error, "could not remove an unpublished file replacement");
    }
    result
}

/// Opens one regular child without following a symbolic link.
fn open_regular_file_at(
    directory: &OwnedFd,
    name: &std::ffi::OsStr,
) -> Result<OpenedRegularFile, Report<FilesServiceError>> {
    let file = openat(
        directory,
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
        writable: true,
    })
}

/// Reads and hashes one editor-sized file from its pinned descriptor.
fn read_revision(opened: OpenedRegularFile) -> Result<[u8; 32], Report<FilesServiceError>> {
    if opened.size > MAX_POD_FILE_WRITE_BYTES {
        return Err(invalid(format!(
            "file exceeds the {MAX_POD_FILE_WRITE_BYTES}-byte editor limit"
        )));
    }
    let mut file = File::from(opened.descriptor);
    let capacity = usize::try_from(opened.size)
        .map_err(|error| internal(format!("failed to allocate file revision buffer: {error}")))?;
    let mut contents = Vec::with_capacity(capacity);
    file.read_to_end(&mut contents)
        .map_err(|error| unavailable(format!("failed to read file revision: {error}")))?;
    if contents.len() as u64 > MAX_POD_FILE_WRITE_BYTES {
        return Err(invalid(format!(
            "file exceeds the {MAX_POD_FILE_WRITE_BYTES}-byte editor limit"
        )));
    }
    Ok(Sha256::digest(&contents).into())
}

/// Captures metadata that a complete-file replacement must preserve.
fn file_ownership(descriptor: &OwnedFd) -> Result<FileOwnership, Report<FilesServiceError>> {
    let metadata = fstat(descriptor)
        .map_err(|error| unavailable(format!("failed to inspect file ownership: {error}")))?;
    Ok(FileOwnership {
        owner: Uid::from_raw(metadata.st_uid),
        group: Gid::from_raw(metadata.st_gid),
        mode: Mode::from_raw_mode(metadata.st_mode & 0o7777),
    })
}

/// Formats the SHA-256 content revision used by the browser contract.
fn content_revision(contents: &[u8]) -> String {
    ContentDigest::from_bytes(contents).to_string()
}

/// Parses a hexadecimal SHA-256 revision from the data-plane request.
fn parse_revision(value: &str) -> Result<[u8; 32], Report<FilesServiceError>> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid(
            "file revision must be a lowercase hexadecimal SHA-256 digest",
        ));
    }
    let mut revision = [0_u8; 32];
    for (index, output) in revision.iter_mut().enumerate() {
        *output = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| invalid("file revision must be a lowercase hexadecimal SHA-256 digest"))?;
    }
    Ok(revision)
}

/// Enforces the browser editor's size and encoding contract.
fn validate_file_contents(contents: &[u8]) -> Result<(), Report<FilesServiceError>> {
    if contents.len() as u64 > MAX_POD_FILE_WRITE_BYTES {
        return Err(invalid(format!(
            "replacement exceeds the {MAX_POD_FILE_WRITE_BYTES}-byte editor limit"
        )));
    }
    std::str::from_utf8(contents)
        .map(|_| ())
        .map_err(|_| invalid("replacement content must be UTF-8"))
}

#[derive(Clone, Copy)]
struct FileOwnership {
    owner: Uid,
    group: Gid,
    mode: Mode,
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
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// Rejects share names and lookups which are not part of the pinned root
    /// inventory.
    #[test]
    fn file_service_restricts_share_roots_to_its_configuration() {
        let directory = tempfile::tempdir().expect("share fixture is created");
        let mut config = FilesServiceConfig::default();
        config
            .add_directory_share("source", directory.path(), false)
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

    /// Verifies matching content is replaced and a stale revision leaves it
    /// intact.
    #[test]
    fn file_replacement_requires_the_observed_revision() {
        let root = tempfile::tempdir().expect("file root fixture is created");
        let path = root.path().join("document.md");
        fs::write(&path, b"before\n").expect("file fixture is written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("file fixture mode is set");
        let before: [u8; 32] = Sha256::digest(b"before\n").into();

        let revision = replace_regular_file(root.path(), "document.md", before, b"after\n")
            .expect("matching revision is replaced");
        assert_eq!(revision, content_revision(b"after\n"));
        assert_eq!(fs::read(&path).unwrap(), b"after\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o640
        );

        let error = replace_regular_file(root.path(), "document.md", before, b"stale\n")
            .expect_err("stale revision is rejected");
        assert!(matches!(error.error(), FilesServiceError::Conflict));
        assert_eq!(fs::read(path).unwrap(), b"after\n");
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
