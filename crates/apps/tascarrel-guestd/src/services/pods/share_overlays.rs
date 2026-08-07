//! Per-pod FUSE lifecycle for copy-on-write host shares.
//!
//! [`ShareOverlayRuntime`] provisions durable `ShareFS` upper subvolumes and
//! owns every transient kernel mount used by runc. Upper state survives an
//! ordinary pod stop and is removed only with the pod.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use sha2::Digest as _;
use sha2::Sha256;
use tascarrel_protocol::MAX_SHARE_OVERLAY_CHANGES;
use tascarrel_protocol::MAX_SHARE_OVERLAY_CONTENT_BYTES;
use tascarrel_protocol::ShareOverlayBase;
use tascarrel_protocol::ShareOverlayChange;
use tascarrel_protocol::ShareOverlayEntry;
use tascarrel_protocol::ShareOverlayEntryKind;
use tascarrel_protocol::ShareOverlayEntryVersion;
use tascarrel_protocol::ShareOverlaySnapshot;
use tascarrel_sharefs::ContentDigest;
use tascarrel_sharefs::DirectoryEntry;
use tascarrel_sharefs::EntryKind;
use tascarrel_sharefs::EntryVersion;
use tascarrel_sharefs::FileWriteOutcome;
use tascarrel_sharefs::FrozenShareFileSystem;
use tascarrel_sharefs::MountedShareFileSystem;
use tascarrel_sharefs::ShareFileSystem;
use tascarrel_sharefs::ShareFileSystemMountOptions;

use crate::runtime::pod::PodId;
use crate::runtime::pod::PodShare;
use crate::runtime::pod::RuntimeError;

/// One host share mounted through a private upper for every pod.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShareOverlay {
    /// Validated workspace-local share name.
    pub name: String,
    /// Host-assigned stable mount tag used as an internal runtime name.
    pub mount_tag: String,
    /// Raw read-only host export mounted in the guest.
    pub lower: PathBuf,
}

/// Durable and transient paths required by the overlay runtime.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShareOverlayRuntimeConfig {
    /// Guest Btrfs directory containing per-pod upper state.
    pub storage_root: PathBuf,
    /// Guest tmpfs directory containing transient FUSE mountpoints.
    pub runtime_root: PathBuf,
    /// Absolute Btrfs executable.
    pub btrfs: PathBuf,
    /// Configured overlay shares.
    pub shares: Vec<ShareOverlay>,
}

/// Mounted overlay sessions owned by the concrete pod runtime.
pub(crate) struct ShareOverlayRuntime {
    config: ShareOverlayRuntimeConfig,
    sessions: Mutex<HashMap<String, Vec<MountedOverlay>>>,
}

impl ShareOverlayRuntime {
    /// Validates paths and prepares the durable and transient namespace.
    #[tracing::instrument(level = "debug", skip_all, err)]
    pub(crate) fn open(config: ShareOverlayRuntimeConfig) -> Result<Self, Report<RuntimeError>> {
        if config.shares.is_empty() {
            return Ok(Self {
                config,
                sessions: Mutex::new(HashMap::new()),
            });
        }
        for (label, path) in [
            ("share overlay storage root", &config.storage_root),
            ("share overlay runtime root", &config.runtime_root),
            ("Btrfs executable", &config.btrfs),
        ] {
            if !path.is_absolute() {
                return Err(RuntimeError::InvalidConfig(format!(
                    "{label} must be absolute: {}",
                    path.display()
                ))
                .report());
            }
        }
        for share in &config.shares {
            if !tascarrel_protocol::valid_workspace_share_name(&share.name)
                || share.mount_tag.is_empty()
                || !share.lower.is_absolute()
                || !share.lower.is_dir()
            {
                return Err(RuntimeError::InvalidConfig(format!(
                    "invalid overlay host share {:?}",
                    share.name
                ))
                .report());
            }
        }
        ensure_directory(&config.storage_root, 0o700)?;
        ensure_directory(&config.runtime_root, 0o700)?;
        Ok(Self {
            config,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Mounts every configured overlay for one pod.
    #[tracing::instrument(level = "debug", skip(self), fields(pod_id = %pod), err)]
    pub(crate) fn mount(
        &self,
        pod: &PodId,
        uid: u32,
        gid: u32,
    ) -> Result<Vec<PodShare>, Report<RuntimeError>> {
        if self.config.shares.is_empty() {
            return Ok(Vec::new());
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| RuntimeError::LockPoisoned)
            .report()?;
        if sessions.contains_key(pod.as_str()) {
            return Err(RuntimeError::AlreadyPrepared(pod.clone()).report());
        }
        let pod_runtime = self.config.runtime_root.join(pod.as_str());
        ensure_directory(&pod_runtime, 0o700)?;

        let mut mounted = Vec::with_capacity(self.config.shares.len());
        let mut pod_shares = Vec::with_capacity(self.config.shares.len());
        for share in &self.config.shares {
            let active = provision_active_state(&self.config, pod, share)?;
            let mountpoint = pod_runtime.join(&share.name);
            ensure_directory(&mountpoint, 0o700)?;
            let session = match MountedShareFileSystem::mount(
                &share.lower,
                &active,
                &mountpoint,
                ShareFileSystemMountOptions {
                    uid,
                    gid,
                    allow_other: true,
                },
            ) {
                Ok(session) => session,
                Err(error) => {
                    unmount_all(mounted);
                    return Err(RuntimeError::InvalidConfig(format!(
                        "failed to mount overlay host share {:?}: {error}",
                        share.name
                    ))
                    .report());
                }
            };
            let pod_share = match PodShare::host_overlay(
                format!("overlay-{}", share.mount_tag),
                &share.name,
                &mountpoint,
            ) {
                Ok(pod_share) => pod_share,
                Err(error) => {
                    drop(session);
                    unmount_all(mounted);
                    return Err(error.report());
                }
            };
            mounted.push(MountedOverlay {
                name: share.name.clone(),
                active,
                session,
            });
            pod_shares.push(pod_share);
        }
        sessions.insert(pod.as_str().to_owned(), mounted);
        Ok(pod_shares)
    }

    /// Reads one directory from a pod's merged overlay-share view.
    #[tracing::instrument(level = "debug", skip(self), fields(pod_id = %pod, share, path = %path.display()), err)]
    pub(crate) fn read_directory(
        &self,
        pod: &PodId,
        share: &str,
        path: &Path,
    ) -> Result<Vec<DirectoryEntry>, Report<RuntimeError>> {
        self.inspection_filesystem(pod, share)?
            .read_directory(path)
            .map_err(|error| {
                RuntimeError::InvalidConfig(format!(
                    "failed to read overlay host share {share:?}: {error}"
                ))
                .report()
            })
    }

    /// Opens one regular file from a pod's merged overlay-share view.
    #[tracing::instrument(level = "debug", skip(self), fields(pod_id = %pod, share, path = %path.display()), err)]
    pub(crate) fn open_file(
        &self,
        pod: &PodId,
        share: &str,
        path: &Path,
    ) -> Result<fs::File, Report<RuntimeError>> {
        self.inspection_filesystem(pod, share)?
            .open_file(path)
            .map_err(|error| {
                RuntimeError::InvalidConfig(format!(
                    "failed to open overlay host share file {share:?}: {error}"
                ))
                .report()
            })
    }

    /// Replaces one regular file when its merged contents match a revision.
    #[tracing::instrument(level = "debug", skip(self, contents), fields(pod_id = %pod, share, path = %path.display(), bytes = contents.len()), err)]
    pub(crate) fn write_file_if_revision(
        &self,
        pod: &PodId,
        share: &str,
        path: &Path,
        expected: ContentDigest,
        contents: &[u8],
    ) -> Result<FileWriteOutcome, Report<RuntimeError>> {
        self.inspection_filesystem(pod, share)?
            .write_file_if_revision(path, expected, contents)
            .map_err(|error| {
                RuntimeError::InvalidConfig(format!(
                    "failed to replace overlay host share file {share:?}: {error}"
                ))
                .report()
            })
    }

    /// Unmounts every transient overlay for one stopped pod.
    #[tracing::instrument(level = "debug", skip(self), fields(pod_id = %pod), err)]
    pub(crate) fn unmount(&self, pod: &PodId) -> Result<(), Report<RuntimeError>> {
        let mounted = self
            .sessions
            .lock()
            .map_err(|_| RuntimeError::LockPoisoned)
            .report()?
            .remove(pod.as_str())
            .unwrap_or_default();
        let mut first_error = None;
        for overlay in mounted.into_iter().rev() {
            if let Err(error) = overlay.session.unmount() {
                if first_error.is_none() {
                    first_error = Some(error.to_string());
                } else {
                    tracing::warn!(
                        share = %overlay.name,
                        state = %overlay.active.display(),
                        %error,
                        "additional ShareFS unmount failed"
                    );
                }
            }
        }
        if let Some(error) = first_error {
            return Err(RuntimeError::InvalidConfig(format!(
                "failed to unmount pod ShareFS overlays: {error}"
            ))
            .report());
        }
        let pod_runtime = self.config.runtime_root.join(pod.as_str());
        if pod_runtime.exists() {
            fs::remove_dir_all(&pod_runtime).map_err(|error| {
                RuntimeError::InvalidConfig(format!(
                    "failed to remove ShareFS runtime directory {}: {error}",
                    pod_runtime.display()
                ))
                .report()
            })?;
        }
        Ok(())
    }

    /// Removes every durable upper owned by one destroyed pod.
    #[tracing::instrument(level = "debug", skip(self), fields(pod_id = %pod), err)]
    pub(crate) fn destroy(&self, pod: &PodId) -> Result<(), Report<RuntimeError>> {
        self.unmount(pod)?;
        if self.config.shares.is_empty() {
            return Ok(());
        }
        let pod_storage = self.config.storage_root.join(pod.as_str());
        if !pod_storage.exists() {
            return Ok(());
        }
        for share in fs::read_dir(&pod_storage).map_err(|error| {
            RuntimeError::InvalidConfig(format!(
                "failed to enumerate ShareFS pod state {}: {error}",
                pod_storage.display()
            ))
            .report()
        })? {
            let share = share.map_err(|error| {
                RuntimeError::InvalidConfig(format!(
                    "failed to read ShareFS pod state entry: {error}"
                ))
                .report()
            })?;
            let metadata = share.metadata().map_err(|error| {
                RuntimeError::InvalidConfig(format!(
                    "failed to inspect ShareFS state {}: {error}",
                    share.path().display()
                ))
                .report()
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(RuntimeError::UnsafePath(share.path()).report());
            }
            for state in fs::read_dir(share.path()).map_err(|error| {
                RuntimeError::InvalidConfig(format!(
                    "failed to enumerate ShareFS share state {}: {error}",
                    share.path().display()
                ))
                .report()
            })? {
                let state = state.map_err(|error| {
                    RuntimeError::InvalidConfig(format!(
                        "failed to read ShareFS share state entry: {error}"
                    ))
                    .report()
                })?;
                if state
                    .file_type()
                    .map_err(|error| {
                        RuntimeError::InvalidConfig(format!(
                            "failed to inspect ShareFS subvolume {}: {error}",
                            state.path().display()
                        ))
                        .report()
                    })?
                    .is_dir()
                {
                    run_btrfs(
                        &self.config.btrfs,
                        "delete ShareFS upper subvolume",
                        &["subvolume", "delete", "--commit-after"],
                        &state.path(),
                    )?;
                }
            }
        }
        fs::remove_dir_all(&pod_storage).map_err(|error| {
            RuntimeError::InvalidConfig(format!(
                "failed to remove ShareFS pod state {}: {error}",
                pod_storage.display()
            ))
            .report()
        })
    }

    /// Resolves the active mounted filesystem or opens its durable state for
    /// inspection.
    fn inspection_filesystem(
        &self,
        pod: &PodId,
        share_name: &str,
    ) -> Result<Arc<ShareFileSystem>, Report<RuntimeError>> {
        let share = self
            .config
            .shares
            .iter()
            .find(|share| share.name == share_name)
            .ok_or_else(|| {
                RuntimeError::InvalidConfig(format!(
                    "host share {share_name:?} is not configured in overlay mode"
                ))
                .report()
            })?;
        if let Some(filesystem) = self
            .sessions
            .lock()
            .map_err(|_| RuntimeError::LockPoisoned)
            .report()?
            .get(pod.as_str())
            .and_then(|mounted| mounted.iter().find(|mounted| mounted.name == share.name))
            .map(|mounted| Arc::clone(mounted.session.filesystem()))
        {
            return Ok(filesystem);
        }
        let active = provision_active_state(&self.config, pod, share)?;
        ShareFileSystem::open(&share.lower, active)
            .map(Arc::new)
            .map_err(|error| {
                RuntimeError::InvalidConfig(format!(
                    "failed to open stopped pod overlay share {:?}: {error}",
                    share.name
                ))
                .report()
            })
    }

    /// Freezes one upper and captures a Btrfs snapshot for an approval round.
    #[tracing::instrument(
        level = "debug",
        skip(self),
        fields(pod_id = %pod, share = share_name),
        err
    )]
    pub(crate) fn prepare_approval(
        &self,
        pod: &PodId,
        share_name: &str,
    ) -> Result<PreparedShareOverlay, Report<RuntimeError>> {
        let share = self
            .config
            .shares
            .iter()
            .find(|share| share.name == share_name)
            .ok_or_else(|| {
                RuntimeError::InvalidConfig(format!(
                    "host share {share_name:?} is not configured in overlay mode"
                ))
                .report()
            })?;
        let active = self
            .config
            .storage_root
            .join(pod.as_str())
            .join(&share.name)
            .join("active");
        if !active.is_dir() {
            return Err(RuntimeError::InvalidConfig(format!(
                "pod {} has no upper state for overlay share {:?}",
                pod, share.name
            ))
            .report());
        }
        let mounted_filesystem = self
            .sessions
            .lock()
            .map_err(|_| RuntimeError::LockPoisoned)
            .report()?
            .get(pod.as_str())
            .and_then(|mounted| mounted.iter().find(|mounted| mounted.name == share.name))
            .map(|mounted| Arc::clone(mounted.session.filesystem()));
        let filesystem = match mounted_filesystem {
            Some(filesystem) => filesystem,
            None => Arc::new(
                ShareFileSystem::open(&share.lower, &active).map_err(|error| {
                    RuntimeError::InvalidConfig(format!(
                        "failed to open stopped pod overlay share {:?}: {error}",
                        share.name
                    ))
                    .report()
                })?,
            ),
        };
        let frozen = filesystem.freeze().map_err(|error| {
            RuntimeError::InvalidConfig(format!(
                "failed to freeze overlay share {:?}: {error}",
                share.name
            ))
            .report()
        })?;
        let changes = frozen.snapshot().map_err(|error| {
            RuntimeError::InvalidConfig(format!(
                "failed to synchronize overlay share {:?}: {error}",
                share.name
            ))
            .report()
        })?;
        if changes.len() > MAX_SHARE_OVERLAY_CHANGES {
            return Err(RuntimeError::InvalidConfig(format!(
                "overlay share {:?} has more than {MAX_SHARE_OVERLAY_CHANGES} changes",
                share.name
            ))
            .report());
        }
        let snapshot_path = active
            .parent()
            .ok_or_else(|| RuntimeError::UnsafePath(active.clone()))
            .report()?
            .join(format!(".approval-{}", uuid::Uuid::new_v4()));
        run_btrfs_snapshot(
            &self.config.btrfs,
            "snapshot ShareFS upper for approval",
            &active,
            &snapshot_path,
        )?;
        let snapshot = match encode_snapshot(&share.lower, &snapshot_path, &changes) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if let Err(cleanup) = delete_snapshot(&self.config.btrfs, &snapshot_path) {
                    tracing::warn!(
                        path = %snapshot_path.display(),
                        %cleanup,
                        "could not remove failed ShareFS approval snapshot"
                    );
                }
                return Err(error);
            }
        };
        Ok(PreparedShareOverlay {
            frozen,
            btrfs: self.config.btrfs.clone(),
            snapshot_path: Some(snapshot_path),
            snapshot,
        })
    }
}

/// Provisions the durable empty upper used by mounts and stopped-pod
/// inspection.
fn provision_active_state(
    config: &ShareOverlayRuntimeConfig,
    pod: &PodId,
    share: &ShareOverlay,
) -> Result<PathBuf, Report<RuntimeError>> {
    let share_storage = config.storage_root.join(pod.as_str()).join(&share.name);
    ensure_directory(&share_storage, 0o700)?;
    let active = share_storage.join("active");
    if !active.exists() {
        run_btrfs(
            &config.btrfs,
            "create ShareFS upper subvolume",
            &["subvolume", "create"],
            &active,
        )?;
    }
    Ok(active)
}

impl std::fmt::Debug for ShareOverlayRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShareOverlayRuntime")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

struct MountedOverlay {
    name: String,
    active: PathBuf,
    session: MountedShareFileSystem,
}

/// Frozen exact revision retained while hostd validates and applies it.
pub(crate) struct PreparedShareOverlay {
    frozen: FrozenShareFileSystem,
    btrfs: PathBuf,
    snapshot_path: Option<PathBuf>,
    snapshot: ShareOverlaySnapshot,
}

impl PreparedShareOverlay {
    /// Returns the exact encoded snapshot.
    pub(crate) const fn snapshot(&self) -> &ShareOverlaySnapshot {
        &self.snapshot
    }

    /// Clears the applied upper revision and releases its transient snapshot.
    pub(crate) fn commit(mut self) -> Result<(), Report<RuntimeError>> {
        self.frozen.clear().map_err(|error| {
            RuntimeError::InvalidConfig(format!(
                "failed to clear an applied ShareFS upper: {error}"
            ))
            .report()
        })?;
        self.cleanup_snapshot();
        Ok(())
    }

    /// Retains the upper and releases its transient snapshot.
    pub(crate) fn retain(mut self) {
        self.cleanup_snapshot();
    }

    fn cleanup_snapshot(&mut self) {
        let Some(snapshot) = self.snapshot_path.take() else {
            return;
        };
        if let Err(error) = delete_snapshot(&self.btrfs, &snapshot) {
            tracing::warn!(
                path = %snapshot.display(),
                %error,
                "could not remove ShareFS approval snapshot"
            );
        }
    }
}

impl std::fmt::Debug for PreparedShareOverlay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedShareOverlay")
            .field("snapshot_path", &self.snapshot_path)
            .field("revision", &self.snapshot.revision)
            .finish_non_exhaustive()
    }
}

impl Drop for PreparedShareOverlay {
    fn drop(&mut self) {
        self.cleanup_snapshot();
    }
}

fn unmount_all(mounted: Vec<MountedOverlay>) {
    for overlay in mounted.into_iter().rev() {
        if let Err(error) = overlay.session.unmount() {
            tracing::warn!(
                share = %overlay.name,
                state = %overlay.active.display(),
                %error,
                "could not roll back ShareFS mount"
            );
        }
    }
}

fn ensure_directory(path: &Path, mode: u32) -> Result<(), Report<RuntimeError>> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(mode);
    builder.create(path).map_err(|error| {
        RuntimeError::InvalidConfig(format!(
            "failed to create ShareFS directory {}: {error}",
            path.display()
        ))
        .report()
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        RuntimeError::InvalidConfig(format!(
            "failed to inspect ShareFS directory {}: {error}",
            path.display()
        ))
        .report()
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RuntimeError::UnsafePath(path.to_owned()).report());
    }
    Ok(())
}

fn run_btrfs(
    program: &Path,
    operation: &'static str,
    arguments: &[&str],
    path: &Path,
) -> Result<(), Report<RuntimeError>> {
    let output = Command::new(program)
        .args(arguments)
        .arg("--")
        .arg(path)
        .output()
        .map_err(|error| {
            RuntimeError::InvalidConfig(format!(
                "failed to start {} to {operation}: {error}",
                program.display()
            ))
            .report()
        })?;
    if !output.status.success() {
        return Err(RuntimeError::InvalidConfig(format!(
            "{operation} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .report());
    }
    Ok(())
}

fn run_btrfs_snapshot(
    program: &Path,
    operation: &'static str,
    source: &Path,
    destination: &Path,
) -> Result<(), Report<RuntimeError>> {
    let output = Command::new(program)
        .args(["subvolume", "snapshot"])
        .arg(source)
        .arg(destination)
        .output()
        .map_err(|error| {
            RuntimeError::InvalidConfig(format!(
                "failed to start {} to {operation}: {error}",
                program.display()
            ))
            .report()
        })?;
    if !output.status.success() {
        return Err(RuntimeError::InvalidConfig(format!(
            "{operation} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .report());
    }
    Ok(())
}

fn delete_snapshot(program: &Path, snapshot: &Path) -> Result<(), Report<RuntimeError>> {
    run_btrfs(
        program,
        "delete ShareFS approval snapshot",
        &["subvolume", "delete", "--commit-after"],
        snapshot,
    )
}

fn encode_snapshot(
    lower: &Path,
    state: &Path,
    changes: &[tascarrel_sharefs::ShareChange],
) -> Result<ShareOverlaySnapshot, Report<RuntimeError>> {
    let filesystem = ShareFileSystem::open(lower, state).map_err(|error| {
        RuntimeError::InvalidConfig(format!("failed to open ShareFS approval snapshot: {error}"))
            .report()
    })?;
    let mut encoded = Vec::with_capacity(changes.len());
    let mut content_bytes = 0_u64;
    for change in changes {
        let proposed = change
            .proposed
            .as_ref()
            .map(|version| {
                let contents = match version.kind {
                    EntryKind::File => Some(filesystem.read_file(&change.path).map_err(|error| {
                        RuntimeError::InvalidConfig(format!(
                            "failed to read proposed file {}: {error}",
                            change.path.display()
                        ))
                        .report()
                    })?),
                    EntryKind::Symlink => Some(
                        filesystem
                            .read_link(&change.path)
                            .map_err(|error| {
                                RuntimeError::InvalidConfig(format!(
                                    "failed to read proposed symbolic link {}: {error}",
                                    change.path.display()
                                ))
                                .report()
                            })?
                            .as_os_str()
                            .as_bytes()
                            .to_vec(),
                    ),
                    EntryKind::Directory => None,
                };
                if let Some(contents) = &contents {
                    content_bytes = content_bytes
                        .checked_add(contents.len() as u64)
                        .ok_or_else(|| {
                            RuntimeError::InvalidConfig(
                                "ShareFS approval content size overflowed".to_owned(),
                            )
                            .report()
                        })?;
                    if content_bytes > MAX_SHARE_OVERLAY_CONTENT_BYTES {
                        return Err(RuntimeError::InvalidConfig(format!(
                            "ShareFS approval content exceeds {MAX_SHARE_OVERLAY_CONTENT_BYTES} bytes"
                        ))
                        .report());
                    }
                }
                Ok(ShareOverlayEntry {
                    version: encode_version(version),
                    contents: contents.map(|contents| BASE64.encode(contents)),
                })
            })
            .transpose()?;
        encoded.push(ShareOverlayChange {
            path: change
                .path
                .components()
                .map(|component| match component {
                    Component::Normal(name) => Ok(BASE64.encode(name.as_bytes())),
                    _ => Err(RuntimeError::UnsafePath(change.path.clone()).report()),
                })
                .collect::<Result<Vec<_>, _>>()?,
            base: change.base.as_ref().map(|base| ShareOverlayBase {
                version: encode_version(&base.version),
                modified_seconds: base.modified_at.seconds,
                modified_nanoseconds: base.modified_at.nanoseconds,
                changed_seconds: base.changed_at.seconds,
                changed_nanoseconds: base.changed_at.nanoseconds,
            }),
            proposed,
            opaque: change.opaque,
        });
    }
    let canonical = serde_json::to_vec(&encoded).map_err(|error| {
        RuntimeError::InvalidConfig(format!(
            "failed to encode ShareFS approval revision: {error}"
        ))
        .report()
    })?;
    let digest = Sha256::digest(canonical);
    let mut revision = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(revision, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(ShareOverlaySnapshot {
        revision,
        changes: encoded,
    })
}

fn encode_version(version: &EntryVersion) -> ShareOverlayEntryVersion {
    ShareOverlayEntryVersion {
        kind: match version.kind {
            EntryKind::File => ShareOverlayEntryKind::File,
            EntryKind::Directory => ShareOverlayEntryKind::Directory,
            EntryKind::Symlink => ShareOverlayEntryKind::Symlink,
        },
        size: version.size,
        mode: version.mode,
        content_digest: version.content_digest.map(|digest| digest.to_string()),
    }
}
