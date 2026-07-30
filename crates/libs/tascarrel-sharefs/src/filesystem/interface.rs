//! Public path-based interface for one copy-on-write share.
//!
//! [`ShareFileSystem`] validates caller paths, serializes operations, and
//! delegates durable namespace transitions to the internal filesystem core.

use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::MutexGuard;

use reportify::Report;

use super::Core;
use super::DATABASE_FILE;
use super::LOGICAL_MODE_MASK;
use super::OBJECTS_DIRECTORY;
use super::STAGING_DIRECTORY;
use super::State;
use super::acquire_state_lock;
use super::clean_staging_directory;
use super::ensure_private_directory;
use super::normalize_non_root_path;
use super::normalize_path;
use super::prepare_lower_directory;
use super::prepare_state_directory;
use crate::DirectoryEntry;
use crate::EntryMetadata;
use crate::LowerLease;
use crate::ShareChange;
use crate::ShareFsError;
use crate::ShareFsResult;

/// One live-lower, copy-on-write share.
///
/// Operations are serialized around namespace transitions. Regular-file data
/// is stored outside `SQLite` and remains accessible through ordinary file
/// descriptors. Callers can therefore later place the state root in a
/// snapshot-capable subvolume without changing the filesystem semantics.
pub struct ShareFileSystem {
    core: Mutex<Core>,
    gate: Arc<OperationGate>,
}

impl ShareFileSystem {
    /// Opens or initializes one private upper over an existing lower directory.
    ///
    /// Both paths must be absolute. The lower and state trees may not overlap.
    /// Existing durable state is validated and unreachable staging objects are
    /// collected before the filesystem is returned.
    ///
    /// # Errors
    ///
    /// Returns an error when either root is unsafe, the roots overlap, durable
    /// state is corrupt, or initialization fails.
    #[tracing::instrument(
        name = "tascarrel_sharefs.open",
        level = "info",
        skip_all,
        fields(lower = %lower.as_ref().display(), state = %state.as_ref().display()),
        err
    )]
    pub fn open(lower: impl AsRef<Path>, state: impl AsRef<Path>) -> ShareFsResult<Self> {
        let lower = prepare_lower_directory(lower.as_ref())?;
        let state_root = prepare_state_directory(state.as_ref())?;
        if lower.starts_with(&state_root) || state_root.starts_with(&lower) {
            return Err(Report::new(ShareFsError::OverlappingDirectories));
        }
        let state_lock = acquire_state_lock(&state_root)?;
        let objects = state_root.join(OBJECTS_DIRECTORY);
        let staging = state_root.join(STAGING_DIRECTORY);
        ensure_private_directory(&objects)?;
        ensure_private_directory(&staging)?;
        clean_staging_directory(&staging)?;
        let database = state_root.join(DATABASE_FILE);
        let namespace = State::open(&database)?;
        let mut core = Core {
            lower,
            state_root,
            objects,
            staging,
            database,
            _state_lock: state_lock,
            namespace,
        };
        core.collect_garbage()?;
        core.validate_objects()?;
        Ok(Self {
            core: Mutex::new(core),
            gate: Arc::new(OperationGate::default()),
        })
    }

    /// Returns metadata for one merged path.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, absent, unsupported, or
    /// cannot be inspected.
    #[tracing::instrument(
        name = "tascarrel_sharefs.metadata",
        level = "trace",
        skip_all,
        fields(path = %path.as_ref().display()),
        err
    )]
    pub fn metadata(&self, path: impl AsRef<Path>) -> ShareFsResult<EntryMetadata> {
        let path = normalize_path(path.as_ref())?;
        self.lock()?.metadata(&path)
    }

    /// Enumerates one merged directory.
    ///
    /// Each call consults the current lower directory. Upper entries shadow
    /// equal lower names and whiteouts remove them. Results are sorted by raw
    /// Unix filename bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, absent, not a directory, or
    /// contains an unsupported lower entry.
    #[tracing::instrument(
        name = "tascarrel_sharefs.read_directory",
        level = "trace",
        skip_all,
        fields(path = %path.as_ref().display()),
        err
    )]
    pub fn read_directory(&self, path: impl AsRef<Path>) -> ShareFsResult<Vec<DirectoryEntry>> {
        let path = normalize_path(path.as_ref())?;
        self.lock()?.read_directory(&path)
    }

    /// Reads the complete contents of one merged regular file.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, absent, not a regular file,
    /// or cannot be read.
    #[tracing::instrument(
        name = "tascarrel_sharefs.read_file",
        level = "trace",
        skip_all,
        fields(path = %path.as_ref().display()),
        err
    )]
    pub fn read_file(&self, path: impl AsRef<Path>) -> ShareFsResult<Vec<u8>> {
        let path = normalize_non_root_path(path.as_ref())?;
        self.lock()?.read_file(&path)
    }

    /// Replaces the complete contents of an existing merged regular file.
    ///
    /// The lower file is hashed to create an approval lease but is not copied
    /// into a second base object.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, absent, not a regular file,
    /// changes while being captured, or cannot be written.
    #[tracing::instrument(
        name = "tascarrel_sharefs.write_file",
        level = "debug",
        skip_all,
        fields(path = %path.as_ref().display(), bytes = contents.len()),
        err
    )]
    pub fn write_file(&self, path: impl AsRef<Path>, contents: &[u8]) -> ShareFsResult<()> {
        let path = normalize_non_root_path(path.as_ref())?;
        self.lock()?.write_file(&path, contents)
    }

    /// Writes bytes at one offset in an existing merged regular file.
    ///
    /// A lower-only file is copied up on the first partial write while its
    /// original content is hashed in the same pass.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, absent, not a regular file,
    /// changes while being captured, or cannot be written.
    #[tracing::instrument(
        name = "tascarrel_sharefs.write_at",
        level = "debug",
        skip_all,
        fields(path = %path.as_ref().display(), offset, bytes = contents.len()),
        err
    )]
    pub fn write_at(
        &self,
        path: impl AsRef<Path>,
        offset: u64,
        contents: &[u8],
    ) -> ShareFsResult<()> {
        let path = normalize_non_root_path(path.as_ref())?;
        self.lock()?.write_at(&path, offset, contents)
    }

    /// Changes the length of one merged regular file.
    ///
    /// A lower-only file is copied up before its length changes.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, absent, not a regular file,
    /// changes while being captured, or cannot be truncated.
    #[tracing::instrument(
        name = "tascarrel_sharefs.set_file_length",
        level = "debug",
        skip_all,
        fields(path = %path.as_ref().display(), length),
        err
    )]
    pub fn set_file_length(&self, path: impl AsRef<Path>, length: u64) -> ShareFsResult<()> {
        let path = normalize_non_root_path(path.as_ref())?;
        self.lock()?.set_file_length(&path, length)
    }

    /// Creates an empty regular file with logical Unix permissions.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, already exists, has no
    /// directory parent, or cannot be persisted.
    #[tracing::instrument(
        name = "tascarrel_sharefs.create_file",
        level = "debug",
        skip_all,
        fields(path = %path.as_ref().display()),
        err
    )]
    pub fn create_file(&self, path: impl AsRef<Path>, mode: u32) -> ShareFsResult<()> {
        let path = normalize_non_root_path(path.as_ref())?;
        self.lock()?.create_file(&path, mode & LOGICAL_MODE_MASK)
    }

    /// Creates a dynamically merged directory with logical Unix permissions.
    ///
    /// Recreating a previously deleted lower directory produces an opaque
    /// directory so removed lower children cannot reappear.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, already exists, has no
    /// directory parent, or cannot be persisted.
    #[tracing::instrument(
        name = "tascarrel_sharefs.create_directory",
        level = "debug",
        skip_all,
        fields(path = %path.as_ref().display()),
        err
    )]
    pub fn create_directory(&self, path: impl AsRef<Path>, mode: u32) -> ShareFsResult<()> {
        let path = normalize_non_root_path(path.as_ref())?;
        self.lock()?
            .create_directory(&path, mode & LOGICAL_MODE_MASK)
    }

    /// Creates one symbolic link without following its target.
    ///
    /// # Errors
    ///
    /// Returns an error when the link path is unsafe, already exists, has no
    /// directory parent, or cannot be persisted.
    #[tracing::instrument(
        name = "tascarrel_sharefs.create_symlink",
        level = "debug",
        skip_all,
        fields(path = %path.as_ref().display()),
        err
    )]
    pub fn create_symlink(
        &self,
        path: impl AsRef<Path>,
        target: impl AsRef<Path>,
    ) -> ShareFsResult<()> {
        let path = normalize_non_root_path(path.as_ref())?;
        let target = target.as_ref();
        if target.as_os_str().as_bytes().contains(&0) {
            return Err(Report::new(ShareFsError::InvalidPath {
                path: target.to_owned(),
            }));
        }
        self.lock()?.create_symlink(&path, target)
    }

    /// Reads one symbolic-link target without resolving it.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, absent, not a symbolic link,
    /// or cannot be inspected.
    #[tracing::instrument(
        name = "tascarrel_sharefs.read_link",
        level = "trace",
        skip_all,
        fields(path = %path.as_ref().display()),
        err
    )]
    pub fn read_link(&self, path: impl AsRef<Path>) -> ShareFsResult<PathBuf> {
        let path = normalize_non_root_path(path.as_ref())?;
        self.lock()?.read_link(&path)
    }

    /// Removes one file, symbolic link, or empty directory from the merged
    /// view.
    ///
    /// Lower-backed entries become whiteouts. Entries originally created in
    /// the upper are removed without leaving a net change.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, absent, a non-empty
    /// directory, changes while being captured, or cannot be persisted.
    #[tracing::instrument(
        name = "tascarrel_sharefs.remove",
        level = "debug",
        skip_all,
        fields(path = %path.as_ref().display()),
        err
    )]
    pub fn remove(&self, path: impl AsRef<Path>) -> ShareFsResult<()> {
        let path = normalize_non_root_path(path.as_ref())?;
        self.lock()?.remove(&path)
    }

    /// Renames one merged entry.
    ///
    /// Regular files, symbolic links, and upper-created directories are
    /// supported. Renaming a directory that still dynamically merges a lower
    /// directory is intentionally rejected.
    ///
    /// # Errors
    ///
    /// Returns an error when either path is unsafe, the source is absent, the
    /// destination is incompatible, a directory is non-empty, or durable
    /// state cannot be updated atomically.
    #[tracing::instrument(
        name = "tascarrel_sharefs.rename",
        level = "debug",
        skip_all,
        fields(source = %source.as_ref().display(), destination = %destination.as_ref().display()),
        err
    )]
    pub fn rename(
        &self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> ShareFsResult<()> {
        let source = normalize_non_root_path(source.as_ref())?;
        let destination = normalize_non_root_path(destination.as_ref())?;
        self.lock()?.rename(&source, &destination)
    }

    /// Changes logical Unix permissions for one merged entry.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, absent, a symbolic link, or
    /// cannot be copied up and updated.
    #[tracing::instrument(
        name = "tascarrel_sharefs.set_mode",
        level = "debug",
        skip_all,
        fields(path = %path.as_ref().display(), mode),
        err
    )]
    pub fn set_mode(&self, path: impl AsRef<Path>, mode: u32) -> ShareFsResult<()> {
        let path = normalize_non_root_path(path.as_ref())?;
        self.lock()?.set_mode(&path, mode & LOGICAL_MODE_MASK)
    }

    /// Returns the canonical net changes retained by the upper state.
    ///
    /// The current lower filesystem is not consulted for base content. Base
    /// values come from the lease captured when each path was first
    /// overridden.
    ///
    /// # Errors
    ///
    /// Returns an error when durable state or a proposed object cannot be
    /// inspected.
    #[tracing::instrument(name = "tascarrel_sharefs.changes", level = "debug", skip_all, err)]
    pub fn changes(&self) -> ShareFsResult<Vec<ShareChange>> {
        self.lock()?.changes()
    }

    /// Compares the current lower entry with a previously captured lease.
    ///
    /// Matching identity, size, mode, mtime, and ctime take the cheap path.
    /// When that fingerprint differs, regular files and symbolic links are
    /// hashed so a metadata-only change or identical replacement does not
    /// create a false conflict. Directory fingerprint changes remain
    /// conservatively conflicting because directory leases do not hash child
    /// names.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is unsafe, has an unsupported current
    /// type, changes during fallback hashing, or cannot be inspected.
    #[tracing::instrument(
        name = "tascarrel_sharefs.lower_matches_lease",
        level = "debug",
        skip_all,
        fields(path = %path.as_ref().display()),
        err
    )]
    pub fn lower_matches_lease(
        &self,
        path: impl AsRef<Path>,
        lease: &LowerLease,
    ) -> ShareFsResult<bool> {
        let path = normalize_non_root_path(path.as_ref())?;
        self.lock()?.lower_matches_lease(&path, lease)
    }

    /// Flushes upper objects and checkpoints the namespace database.
    ///
    /// # Errors
    ///
    /// Returns an error when object or database synchronization fails.
    #[tracing::instrument(name = "tascarrel_sharefs.sync", level = "debug", skip_all, err)]
    pub fn sync(&self) -> ShareFsResult<()> {
        self.lock()?.sync()
    }

    /// Freezes ordinary filesystem operations at a consistent boundary.
    ///
    /// The returned handle owns the freeze and can be retained across an
    /// approval round trip without holding a Rust mutex guard. Existing
    /// operations finish before this method returns; new operations block
    /// until the handle is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when the synchronization gate is poisoned.
    #[tracing::instrument(name = "tascarrel_sharefs.freeze", level = "debug", skip_all, err)]
    pub fn freeze(self: &Arc<Self>) -> ShareFsResult<FrozenShareFileSystem> {
        let freeze = self.gate.freeze()?;
        Ok(FrozenShareFileSystem {
            filesystem: Arc::clone(self),
            _freeze: freeze,
        })
    }

    fn lock(&self) -> ShareFsResult<CoreGuard<'_>> {
        let operation = self.gate.enter()?;
        let core = self
            .core
            .lock()
            .map_err(|_| Report::new(ShareFsError::Poisoned))?;
        Ok(CoreGuard {
            core,
            _operation: operation,
        })
    }

    fn lock_frozen(&self) -> ShareFsResult<MutexGuard<'_, Core>> {
        self.core
            .lock()
            .map_err(|_| Report::new(ShareFsError::Poisoned))
    }
}

/// Exclusive `ShareFS` view used to capture or commit one approval revision.
pub struct FrozenShareFileSystem {
    filesystem: Arc<ShareFileSystem>,
    _freeze: FreezeGuard,
}

impl FrozenShareFileSystem {
    /// Flushes state and returns its canonical change set.
    ///
    /// # Errors
    ///
    /// Returns an error when state synchronization or enumeration fails.
    pub fn snapshot(&self) -> ShareFsResult<Vec<ShareChange>> {
        let mut core = self.filesystem.lock_frozen()?;
        core.sync()?;
        core.changes()
    }

    /// Clears the applied upper state and synchronizes the empty namespace.
    ///
    /// # Errors
    ///
    /// Returns an error when the namespace cannot be reset or synchronized.
    pub fn clear(&self) -> ShareFsResult<()> {
        let mut core = self.filesystem.lock_frozen()?;
        core.namespace.clear()?;
        core.collect_garbage()?;
        core.sync()
    }
}

impl std::fmt::Debug for FrozenShareFileSystem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrozenShareFileSystem")
            .field("filesystem", &self.filesystem)
            .finish_non_exhaustive()
    }
}

struct CoreGuard<'a> {
    core: MutexGuard<'a, Core>,
    _operation: OperationGuard,
}

impl std::ops::Deref for CoreGuard<'_> {
    type Target = Core;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl std::ops::DerefMut for CoreGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.core
    }
}

#[derive(Default)]
struct OperationGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    active: usize,
    frozen: bool,
}

impl OperationGate {
    fn enter(self: &Arc<Self>) -> ShareFsResult<OperationGuard> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Report::new(ShareFsError::Poisoned))?;
        while state.frozen {
            state = self
                .changed
                .wait(state)
                .map_err(|_| Report::new(ShareFsError::Poisoned))?;
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| Report::new(ShareFsError::Poisoned))?;
        Ok(OperationGuard {
            gate: Arc::clone(self),
        })
    }

    fn freeze(self: &Arc<Self>) -> ShareFsResult<FreezeGuard> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Report::new(ShareFsError::Poisoned))?;
        while state.frozen {
            state = self
                .changed
                .wait(state)
                .map_err(|_| Report::new(ShareFsError::Poisoned))?;
        }
        state.frozen = true;
        while state.active != 0 {
            state = self
                .changed
                .wait(state)
                .map_err(|_| Report::new(ShareFsError::Poisoned))?;
        }
        Ok(FreezeGuard {
            gate: Arc::clone(self),
        })
    }
}

struct OperationGuard {
    gate: Arc<OperationGate>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let Ok(mut state) = self.gate.state.lock() else {
            tracing::error!("could not release a poisoned ShareFS operation gate");
            return;
        };
        state.active = state.active.saturating_sub(1);
        if state.active == 0 {
            self.gate.changed.notify_all();
        }
    }
}

struct FreezeGuard {
    gate: Arc<OperationGate>,
}

impl Drop for FreezeGuard {
    fn drop(&mut self) {
        let Ok(mut state) = self.gate.state.lock() else {
            tracing::error!("could not release a poisoned ShareFS freeze gate");
            return;
        };
        state.frozen = false;
        self.gate.changed.notify_all();
    }
}

impl std::fmt::Debug for ShareFileSystem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.core.lock() {
            Ok(core) => formatter
                .debug_struct("ShareFileSystem")
                .field("lower", &core.lower)
                .field("state_root", &core.state_root)
                .finish_non_exhaustive(),
            Err(_) => formatter
                .debug_struct("ShareFileSystem")
                .field("state", &"<poisoned>")
                .finish_non_exhaustive(),
        }
    }
}
