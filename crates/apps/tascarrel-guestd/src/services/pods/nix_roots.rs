//! Safe ownership of per-pod Nix direct-root directories.

use std::collections::BTreeSet;
use std::fs::DirBuilder;
use std::fs::{self};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use nix::unistd::Gid;
use nix::unistd::Uid;
use nix::unistd::chown;
use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use thiserror::Error;

use crate::runtime::pod::PodId;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const POD_DIRECTORY_MODE: u32 = 0o711;

/// Persistent direct Nix GC-root directories owned by individual pods.
#[derive(Debug)]
pub(crate) struct NixRoots {
    root: PathBuf,
    trash: PathBuf,
}

impl NixRoots {
    /// Opens pre-created root-owned directories and removes interrupted trash.
    pub(crate) fn open(
        root: impl Into<PathBuf>,
        trash: impl Into<PathBuf>,
    ) -> Result<Self, Report<NixRootsError>> {
        let roots = Self {
            root: root.into(),
            trash: trash.into(),
        };
        validate_managed_root(&roots.root, "pod GC-root directory")?;
        validate_managed_root(&roots.trash, "pod GC-root trash directory")?;
        if roots.root == roots.trash
            || roots.root.starts_with(&roots.trash)
            || roots.trash.starts_with(&roots.root)
        {
            return Err(failure(
                "pod GC-root and trash directories must be disjoint",
            ));
        }
        let root_device = fs::metadata(&roots.root)
            .map_err(|error| {
                NixRootsError::new(format!("could not inspect pod GC-root filesystem: {error}"))
            })
            .report()?
            .dev();
        let trash_device = fs::metadata(&roots.trash)
            .map_err(|error| {
                NixRootsError::new(format!(
                    "could not inspect pod GC-root trash filesystem: {error}"
                ))
            })
            .report()?
            .dev();
        if root_device != trash_device {
            return Err(failure(
                "pod GC-root and trash directories must use the same filesystem",
            ));
        }
        roots.cleanup_trash()?;
        Ok(roots)
    }

    /// Returns the direct-root directory mounted into a pod.
    #[must_use]
    pub(crate) fn pod_path(&self, pod: &PodId) -> PathBuf {
        self.root.join(pod.as_str())
    }

    /// Creates the fixed root-owned boundary and pod-user-owned writable roots.
    ///
    /// This operation is idempotent so recovery can finish a provision which
    /// was interrupted before the pod runtime was started.
    pub(crate) fn provision(
        &self,
        pod: &PodId,
        uid: u32,
        gid: u32,
    ) -> Result<(), Report<NixRootsError>> {
        let pod_path = self.pod_path(pod);
        match path_metadata(&pod_path)? {
            Some(metadata) => validate_directory(
                &pod_path,
                &metadata,
                Uid::effective().as_raw(),
                Gid::effective().as_raw(),
                POD_DIRECTORY_MODE,
            )?,
            None => create_owned_directory(
                &pod_path,
                Uid::effective().as_raw(),
                Gid::effective().as_raw(),
                POD_DIRECTORY_MODE,
            )?,
        }

        let state = pod_path.join("state");
        let profiles = state.join("profiles");
        let roots = pod_path.join("roots");
        let result: Result<(), Report<NixRootsError>> = (|| {
            ensure_owned_directory(&state, uid, gid, PRIVATE_DIRECTORY_MODE)?;
            ensure_owned_directory(&profiles, uid, gid, PRIVATE_DIRECTORY_MODE)?;
            ensure_owned_directory(&roots, uid, gid, PRIVATE_DIRECTORY_MODE)?;
            Ok(())
        })();
        if let Err(cause) = result {
            let rollback = self.withdraw(pod);
            return match rollback {
                Ok(()) => Err(cause),
                Err(rollback) => Err(failure(format!(
                    "{cause}; could not roll back pod GC roots: {rollback}"
                ))),
            };
        }
        Ok(())
    }

    /// Atomically removes a pod from Nix's scanned tree before deleting data.
    pub(crate) fn withdraw(&self, pod: &PodId) -> Result<(), Report<NixRootsError>> {
        let source = self.pod_path(pod);
        let trash = self.trash.join(pod.as_str());
        remove_trash_path(&trash)?;
        match path_metadata(&source)? {
            None => return Ok(()),
            Some(metadata) => validate_directory(
                &source,
                &metadata,
                Uid::effective().as_raw(),
                Gid::effective().as_raw(),
                POD_DIRECTORY_MODE,
            )?,
        }
        fs::rename(&source, &trash)
            .map_err(|error| {
                NixRootsError::new(format!(
                    "could not atomically withdraw pod GC roots {}: {error}",
                    source.display()
                ))
            })
            .report()?;
        remove_trash_path(&trash)
    }

    /// Lists every safely named pod directory in the direct-root tree.
    pub(crate) fn list(&self) -> Result<BTreeSet<PodId>, Report<NixRootsError>> {
        let entries = fs::read_dir(&self.root)
            .map_err(|error| {
                NixRootsError::new(format!(
                    "could not list pod GC roots {}: {error}",
                    self.root.display()
                ))
            })
            .report()?;
        let mut pods = BTreeSet::new();
        for entry in entries {
            let entry = entry
                .map_err(|error| {
                    NixRootsError::new(format!(
                        "could not read pod GC-root entry in {}: {error}",
                        self.root.display()
                    ))
                })
                .report()?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| NixRootsError::new("pod GC-root entry name is not UTF-8"))
                .report()?;
            let pod = PodId::new(name)
                .map_err(|error| NixRootsError::new(format!("invalid pod GC-root entry: {error}")))
                .report()?;
            let path = entry.path();
            let metadata = path_metadata(&path)?.ok_or_else(|| {
                failure(format!("pod GC-root entry disappeared: {}", path.display()))
            })?;
            if !metadata.is_dir() {
                return Err(failure(format!(
                    "pod GC-root entry is not a real directory: {}",
                    path.display()
                )));
            }
            pods.insert(pod);
        }
        Ok(pods)
    }

    fn cleanup_trash(&self) -> Result<(), Report<NixRootsError>> {
        let entries = fs::read_dir(&self.trash)
            .map_err(|error| {
                NixRootsError::new(format!(
                    "could not list pod GC-root trash {}: {error}",
                    self.trash.display()
                ))
            })
            .report()?;
        for entry in entries {
            let entry = entry
                .map_err(|error| {
                    NixRootsError::new(format!(
                        "could not read pod GC-root trash entry in {}: {error}",
                        self.trash.display()
                    ))
                })
                .report()?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| NixRootsError::new("pod GC-root trash entry name is not UTF-8"))
                .report()?;
            PodId::new(name)
                .map_err(|error| {
                    NixRootsError::new(format!("invalid pod GC-root trash entry: {error}"))
                })
                .report()?;
            remove_trash_path(&entry.path())?;
        }
        Ok(())
    }
}

/// Failure while managing a pod's Nix direct-root directory.
#[derive(Debug, Error)]
#[error("{message}")]
pub(crate) struct NixRootsError {
    message: String,
}

impl NixRootsError {
    /// Creates an error without exposing nested implementation types.
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Validates one configured root-owned management boundary.
fn validate_managed_root(path: &Path, purpose: &str) -> Result<(), Report<NixRootsError>> {
    validate_absolute_normal_path(path, purpose)?;
    let metadata = path_metadata(path)?
        .ok_or_else(|| failure(format!("{purpose} is missing: {}", path.display())))?;
    validate_directory(
        path,
        &metadata,
        Uid::effective().as_raw(),
        Gid::effective().as_raw(),
        PRIVATE_DIRECTORY_MODE,
    )
}

/// Rejects relative paths and lexical traversal components.
fn validate_absolute_normal_path(path: &Path, purpose: &str) -> Result<(), Report<NixRootsError>> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(failure(format!(
            "{purpose} must be an absolute normalized path: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Inspects a managed path without following symlinks.
fn path_metadata(path: &Path) -> Result<Option<fs::Metadata>, Report<NixRootsError>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(failure(format!(
            "managed pod GC-root path is a symlink: {}",
            path.display()
        ))),
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(failure(format!(
            "could not inspect pod GC-root path {}: {error}",
            path.display()
        ))),
    }
}

/// Checks a directory's type, owner, group, and exact mode.
fn validate_directory(
    path: &Path,
    metadata: &fs::Metadata,
    uid: u32,
    gid: u32,
    mode: u32,
) -> Result<(), Report<NixRootsError>> {
    if !metadata.is_dir()
        || metadata.uid() != uid
        || metadata.gid() != gid
        || metadata.permissions().mode() & 0o7777 != mode
    {
        return Err(failure(format!(
            "pod GC-root path has unsafe type, owner, or mode: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Creates or validates one managed directory.
fn ensure_owned_directory(
    path: &Path,
    uid: u32,
    gid: u32,
    mode: u32,
) -> Result<(), Report<NixRootsError>> {
    match path_metadata(path)? {
        Some(metadata) => validate_directory(path, &metadata, uid, gid, mode),
        None => create_owned_directory(path, uid, gid, mode),
    }
}

/// Creates one directory with its final owner and mode.
fn create_owned_directory(
    path: &Path,
    uid: u32,
    gid: u32,
    mode: u32,
) -> Result<(), Report<NixRootsError>> {
    DirBuilder::new()
        .mode(mode)
        .create(path)
        .map_err(|error| {
            NixRootsError::new(format!(
                "could not create pod GC-root path {}: {error}",
                path.display()
            ))
        })
        .report()?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| {
            NixRootsError::new(format!(
                "could not set pod GC-root path mode {}: {error}",
                path.display()
            ))
        })
        .report()?;
    chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
        .map_err(|error| {
            NixRootsError::new(format!(
                "could not set pod GC-root path owner {}: {error}",
                path.display()
            ))
        })
        .report()?;
    Ok(())
}

/// Removes one validated directory below the withdrawal staging root.
fn remove_trash_path(path: &Path) -> Result<(), Report<NixRootsError>> {
    match path_metadata(path)? {
        None => Ok(()),
        Some(metadata) if metadata.is_dir() => fs::remove_dir_all(path)
            .map_err(|error| {
                NixRootsError::new(format!(
                    "could not remove withdrawn pod GC roots {}: {error}",
                    path.display()
                ))
            })
            .report(),
        Some(_) => Err(failure(format!(
            "pod GC-root trash entry is not a real directory: {}",
            path.display()
        ))),
    }
}

/// Creates a report for one Nix direct-root invariant or operation failure.
fn failure(message: impl Into<String>) -> Report<NixRootsError> {
    NixRootsError::new(message).report()
}
