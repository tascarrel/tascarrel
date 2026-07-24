//! Persistent Btrfs-backed images, workspace seeds, and pod filesystems.

use std::ffi::OsString;
use std::fs;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tracing::warn;

use super::CommandRunner;
use super::ImageId;
use super::PodId;
use super::ProcessCommandRunner;
use super::manifest::ImageManifest;
use super::manifest::PodManifest;
use super::manifest::StoreManifest;
use super::manifest::TransactionManifest;
use super::manifest::TransactionOperation;
use super::runtime::ID_MAP_SIZE;

/// On-disk format understood by this crate.
pub const STORE_FORMAT_VERSION: u32 = 5;

const STORE_MANIFEST: &str = "store.json";
const IMAGES_DIRECTORY: &str = "images";
const IMAGE_STAGING_DIRECTORY: &str = "image-staging";
const IMAGE_PUBLISHING_DIRECTORY: &str = "image-publishing";
const POD_MANIFESTS_DIRECTORY: &str = "pods";
const POD_ROOTS_DIRECTORY: &str = "pod-roots";
const POD_WORKSPACES_DIRECTORY: &str = "pod-workspaces";
const POD_DOCKER_DIRECTORY: &str = "pod-docker";
const POD_TEMPORARIES_DIRECTORY: &str = "pod-temporaries";
const CACHES_DIRECTORY: &str = "caches";
const GOLDEN_WORKSPACE: &str = "golden-workspace";
const SETUP_SEEDS_DIRECTORY: &str = "setup-seeds";
const IMAGE_SEED_MANIFEST: &str = "workspace-seed.json";
const SELECTED_IMAGE_MANIFEST: &str = "selected-image.json";
const TRANSACTIONS_DIRECTORY: &str = "transactions";
const TRASH_DIRECTORY: &str = "trash";
const IMAGE_ROOT_DIRECTORY: &str = "root";
const IMAGE_MANIFEST: &str = "manifest.json";
const MANIFEST_SIZE_LIMIT: u64 = 64 * 1024;
const COMMAND_DIAGNOSTIC_LIMIT: usize = 4096;
const MAX_IMAGE_ENVIRONMENT_ENTRIES: usize = 4096;
const MAX_IMAGE_ADDITIONAL_GIDS: usize = 64;
// Flow attribution uses the same user string and admits at most 128 bytes.
// Keep image publication from accepting metadata which egress would reject.
const MAX_IMAGE_USER_NAME_BYTES: usize = 128;
const MAX_IMAGE_WORKING_DIRECTORY_BYTES: usize = 4096;
// Leave ample room in ImageManifest for the digest, version, timestamp, and
// JSON framing. Validation uses the serialized size too, so escape expansion
// can never produce a manifest which the durable reader rejects.
const MAX_IMAGE_CONFIG_BYTES: usize = 60 * 1024;
const POD_TEMPORARY_QUOTA_BYTES: u64 = 128 * 1024 * 1024 * 1024;

static NEXT_UNIQUE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Deserialize, Serialize)]
struct SetupSeedManifest {
    image: ImageId,
    context: ImageId,
    generation: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct SelectedImageManifest {
    image: ImageId,
}

#[derive(Clone, Copy)]
enum PodWorkspaceBasis {
    ImageSeed,
    GoldenWorkspace,
}

/// A failure while inspecting or mutating pod storage.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The configured store root is absent or is not a real directory.
    #[error("store root is not a real directory: {0}")]
    InvalidRoot(PathBuf),
    /// The Btrfs executable must not be resolved through a mutable `PATH`.
    #[error("btrfs command path must be absolute: {0}")]
    RelativeBtrfsProgram(PathBuf),
    /// A new store may only adopt an empty filesystem root.
    #[error("uninitialized store root is not empty: {0}")]
    NonEmptyUninitializedStore(PathBuf),
    /// A managed path was replaced by an unexpected file type or symlink.
    #[error("managed storage path is unsafe: {0}")]
    UnsafePath(PathBuf),
    /// A filesystem operation failed.
    #[error("could not {operation} {path}: {source}")]
    Io {
        /// Description of the attempted operation.
        operation: &'static str,
        /// Affected path.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },
    /// A manifest could not be encoded or decoded.
    #[error("invalid storage manifest {path}: {source}")]
    Manifest {
        /// Manifest path.
        path: PathBuf,
        /// JSON encoding or decoding failure.
        #[source]
        source: serde_json::Error,
    },
    /// A manifest would exceed the bounded durable reader's input limit.
    #[error("storage manifest {path} exceeds the {limit}-byte size limit")]
    ManifestTooLarge {
        /// Manifest path.
        path: PathBuf,
        /// Maximum serialized size, including the trailing newline.
        limit: u64,
    },
    /// A valid manifest belongs to an unsupported store version.
    #[error(
        "manifest {path} uses format {actual}, but this build supports only format {expected}; wipe the workspace VM state"
    )]
    UnsupportedFormat {
        /// Manifest path.
        path: PathBuf,
        /// Version found in the manifest.
        actual: u32,
        /// Version supported by this crate.
        expected: u32,
    },
    /// Manifest content disagreed with its durable location or transaction.
    #[error("corrupt storage manifest {path}: {message}")]
    CorruptManifest {
        /// Manifest path.
        path: PathBuf,
        /// Validation failure.
        message: String,
    },
    /// The external Btrfs tool could not be started.
    #[error("could not start {program} while attempting to {operation}: {source}")]
    CommandStart {
        /// Logical Btrfs operation.
        operation: &'static str,
        /// Configured executable.
        program: PathBuf,
        /// Process creation failure.
        #[source]
        source: io::Error,
    },
    /// The Btrfs tool rejected an operation.
    #[error("btrfs operation `{operation}` failed: {detail}")]
    CommandFailed {
        /// Logical Btrfs operation.
        operation: &'static str,
        /// Bounded command diagnostic.
        detail: String,
    },
    /// An image generation already exists.
    #[error("image generation {0} already exists")]
    ImageExists(ImageId),
    /// An image generation was not found.
    #[error("image generation {0} does not exist")]
    ImageNotFound(ImageId),
    /// A pod already exists or has unrecovered storage.
    #[error("pod {0} already exists or has an incomplete transaction")]
    PodExists(PodId),
    /// A pod was not found.
    #[error("pod {0} does not exist")]
    PodNotFound(PodId),
    /// A pod still pins the requested image generation.
    #[error("image generation {image} is still used by pod {pod}")]
    ImageInUse {
        /// Pinned image.
        image: ImageId,
        /// Referencing pod.
        pod: PodId,
    },
    /// The workspace currently selects the requested image generation.
    #[error("image generation {0} is selected for new pods")]
    ImageSelected(ImageId),
    /// An image staging handle came from another store or is stale.
    #[error("image staging handle is stale or belongs to another store")]
    InvalidStaging,
    /// The store operation mutex was poisoned by a panic.
    #[error("pod storage operation lock is poisoned")]
    LockPoisoned,
    /// OCI image metadata cannot be represented safely in a pod process.
    #[error("invalid image configuration: {0}")]
    InvalidImageConfig(String),
    /// A workspace cache name was not a safe single path component.
    #[error("invalid workspace cache name: {0}")]
    InvalidCacheName(String),
    /// Both an operation and its immediate rollback failed. Recovery can retry
    /// from the durable transaction manifest.
    #[error("{operation} failed: {cause}; rollback also failed: {rollback}")]
    RollbackFailed {
        /// High-level operation being rolled back.
        operation: &'static str,
        /// Original error.
        cause: String,
        /// Rollback error.
        rollback: String,
    },
}

/// A populated-but-unpublished image subvolume.
///
/// Callers may write the image root through [`Self::path`] and then pass the
/// handle to [`BtrfsStore::publish_image`]. Dropping a handle deliberately
/// leaves its transaction for startup recovery; use
/// [`BtrfsStore::discard_image`] for eager cleanup.
#[derive(Debug)]
pub struct ImageStaging {
    store_root: PathBuf,
    transaction_id: String,
    path: PathBuf,
}

impl ImageStaging {
    /// Returns the writable staging subvolume.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Numeric execution identity resolved from an OCI image's configured user.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageUser {
    name: String,
    uid: u32,
    gid: u32,
    additional_gids: Vec<u32>,
}

impl Default for ImageUser {
    fn default() -> Self {
        Self {
            name: "root".to_owned(),
            uid: 0,
            gid: 0,
            additional_gids: Vec::new(),
        }
    }
}

impl ImageUser {
    /// Constructs a resolved image user within Tascarrel's user-namespace map.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe display name, an ID outside the map, or
    /// an excessive number of supplementary groups.
    pub fn new<I>(
        name: impl Into<String>,
        uid: u32,
        gid: u32,
        additional_gids: I,
    ) -> Result<Self, StoreError>
    where
        I: IntoIterator<Item = u32>,
    {
        let mut additional_gids = additional_gids.into_iter().collect::<Vec<_>>();
        additional_gids.sort_unstable();
        additional_gids.dedup();
        additional_gids.retain(|additional| *additional != gid);
        let user = Self {
            name: name.into(),
            uid,
            gid,
            additional_gids,
        };
        user.validate()?;
        Ok(user)
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.name.is_empty()
            || self.name.len() > MAX_IMAGE_USER_NAME_BYTES
            || self.name.chars().any(char::is_control)
            || self.name.contains('\0')
            || self.name.contains(':')
        {
            return Err(StoreError::InvalidImageConfig(
                "image user name is empty, unsafe, or too long".to_owned(),
            ));
        }
        if self.uid >= ID_MAP_SIZE || self.gid >= ID_MAP_SIZE {
            return Err(StoreError::InvalidImageConfig(
                "image user UID or GID is outside the pod user-namespace map".to_owned(),
            ));
        }
        if self.additional_gids.len() > MAX_IMAGE_ADDITIONAL_GIDS
            || self
                .additional_gids
                .iter()
                .any(|gid| *gid >= ID_MAP_SIZE || *gid == self.gid)
            || self
                .additional_gids
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(StoreError::InvalidImageConfig(
                "image supplementary groups are invalid or excessive".to_owned(),
            ));
        }
        Ok(())
    }

    /// Returns the configured account name retained for display and process
    /// environment metadata. Numeric IDs remain authoritative for execution.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the UID inside the pod user namespace.
    #[must_use]
    pub const fn uid(&self) -> u32 {
        self.uid
    }

    /// Returns the primary GID inside the pod user namespace.
    #[must_use]
    pub const fn gid(&self) -> u32 {
        self.gid
    }

    /// Returns supplementary GIDs inside the pod user namespace.
    #[must_use]
    pub fn additional_gids(&self) -> &[u32] {
        &self.additional_gids
    }
}

/// OCI image defaults retained when Tascarrel replaces the image entrypoint.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageConfig {
    environment: Vec<String>,
    user: ImageUser,
    working_directory: String,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            environment: Vec::new(),
            user: ImageUser::default(),
            working_directory: "/workspace".to_owned(),
        }
    }
}

impl ImageConfig {
    /// Validates OCI `process.env` entries for durable storage and execution.
    ///
    /// Every entry must have the `name=value` shape accepted by Linux process
    /// environments. Duplicate names are retained in OCI order; the runtime
    /// applies the last value when combining image and Tascarrel defaults.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed entries, NUL bytes, or excessive
    /// metadata.
    pub fn new<I, S>(environment: I) -> Result<Self, StoreError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::for_process(environment, ImageUser::default(), "/workspace")
    }

    /// Constructs all retained OCI process defaults after the image user has
    /// been resolved to numeric IDs by the image unpacker.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid environment, identity, or working-directory
    /// metadata.
    pub fn for_process<I, S>(
        environment: I,
        user: ImageUser,
        working_directory: impl Into<String>,
    ) -> Result<Self, StoreError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let config = Self {
            environment: environment.into_iter().map(Into::into).collect(),
            user,
            working_directory: working_directory.into(),
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), StoreError> {
        self.user.validate()?;
        let working_directory = Path::new(&self.working_directory);
        if self.working_directory.len() > MAX_IMAGE_WORKING_DIRECTORY_BYTES
            || self.working_directory.contains('\0')
            || !working_directory.is_absolute()
            || working_directory
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(StoreError::InvalidImageConfig(
                "image working directory must be a bounded, normalized absolute path".to_owned(),
            ));
        }
        if self.environment.len() > MAX_IMAGE_ENVIRONMENT_ENTRIES {
            return Err(StoreError::InvalidImageConfig(format!(
                "environment has more than {MAX_IMAGE_ENVIRONMENT_ENTRIES} entries"
            )));
        }
        let mut bytes = 0usize;
        for entry in &self.environment {
            bytes = bytes.checked_add(entry.len()).ok_or_else(|| {
                StoreError::InvalidImageConfig("environment size overflowed".to_owned())
            })?;
            let Some((name, _)) = entry.split_once('=') else {
                return Err(StoreError::InvalidImageConfig(
                    "environment entry does not contain '='".to_owned(),
                ));
            };
            if name.is_empty() || entry.contains('\0') {
                return Err(StoreError::InvalidImageConfig(
                    "environment entry has an empty name or NUL byte".to_owned(),
                ));
            }
        }
        if bytes > MAX_IMAGE_CONFIG_BYTES {
            return Err(StoreError::InvalidImageConfig(format!(
                "environment exceeds {MAX_IMAGE_CONFIG_BYTES} bytes"
            )));
        }
        let serialized = serde_json::to_vec(self).map_err(|error| {
            StoreError::InvalidImageConfig(format!("environment cannot be encoded: {error}"))
        })?;
        if serialized.len() > MAX_IMAGE_CONFIG_BYTES {
            return Err(StoreError::InvalidImageConfig(format!(
                "serialized environment exceeds {MAX_IMAGE_CONFIG_BYTES} bytes"
            )));
        }
        Ok(())
    }

    /// Returns the OCI `name=value` entries in image order.
    #[must_use]
    pub fn environment(&self) -> &[String] {
        &self.environment
    }

    /// Returns the image's resolved execution identity.
    #[must_use]
    pub const fn user(&self) -> &ImageUser {
        &self.user
    }

    /// Returns the image's default working directory inside the pod.
    #[must_use]
    pub fn working_directory(&self) -> &str {
        &self.working_directory
    }
}

/// A published immutable image generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageGeneration {
    id: ImageId,
    root: PathBuf,
    config: ImageConfig,
    created_at_unix_ms: u64,
}

impl ImageGeneration {
    /// Returns the content digest identifying this generation.
    #[must_use]
    pub const fn id(&self) -> &ImageId {
        &self.id
    }

    /// Returns the read-only Btrfs subvolume containing the image root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns defaults retained from the OCI image configuration.
    #[must_use]
    pub const fn config(&self) -> &ImageConfig {
        &self.config
    }

    /// Returns the generation publication time.
    #[must_use]
    pub const fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }
}

/// The four independent writable subvolumes owned by one pod.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PodStorage {
    id: PodId,
    image: ImageId,
    root: PathBuf,
    workspace: PathBuf,
    docker: PathBuf,
    temporary: PathBuf,
    image_config: ImageConfig,
    created_at_unix_ms: u64,
}

impl PodStorage {
    /// Returns the pod identifier.
    #[must_use]
    pub const fn id(&self) -> &PodId {
        &self.id
    }

    /// Returns the immutable image generation pinned by this pod.
    #[must_use]
    pub const fn image(&self) -> &ImageId {
        &self.image
    }

    /// Returns the writable snapshot used as the pod root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the pod's fresh workspace subvolume.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Returns the pod's fresh Docker data subvolume.
    #[must_use]
    pub fn docker(&self) -> &Path {
        &self.docker
    }

    /// Returns the pod's quota-limited ephemeral temporary subvolume.
    #[must_use]
    pub fn temporary(&self) -> &Path {
        &self.temporary
    }

    /// Returns defaults inherited from the pod's pinned image generation.
    #[must_use]
    pub const fn image_config(&self) -> &ImageConfig {
        &self.image_config
    }

    /// Returns the pod storage creation time.
    #[must_use]
    pub const fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }
}

/// Transactional Btrfs image and pod storage.
pub struct BtrfsStore<R = ProcessCommandRunner> {
    root: PathBuf,
    btrfs_program: PathBuf,
    runner: Arc<R>,
    operation: Mutex<()>,
}

impl<R> std::fmt::Debug for BtrfsStore<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BtrfsStore")
            .field("root", &self.root)
            .field("btrfs_program", &self.btrfs_program)
            .finish_non_exhaustive()
    }
}

impl BtrfsStore<ProcessCommandRunner> {
    /// Opens or initializes a store using the real Btrfs command-line tool.
    ///
    /// # Errors
    ///
    /// Returns an error when paths are unsafe, the root is not a Btrfs
    /// filesystem, a manifest is incompatible, or recovery fails.
    pub fn open(
        root: impl AsRef<Path>,
        btrfs_program: impl Into<PathBuf>,
    ) -> Result<Self, StoreError> {
        Self::with_runner(root, btrfs_program, ProcessCommandRunner)
    }
}

impl<R: CommandRunner> BtrfsStore<R> {
    /// Opens or initializes a store with an injected command runner.
    ///
    /// Recovery is completed before this function returns, so callers never
    /// observe a half-created pod or half-published image from an earlier
    /// process.
    ///
    /// # Errors
    ///
    /// Returns an error when paths are unsafe, the root is not a Btrfs
    /// filesystem, a manifest is incompatible, or recovery fails.
    pub fn with_runner(
        root: impl AsRef<Path>,
        btrfs_program: impl Into<PathBuf>,
        runner: R,
    ) -> Result<Self, StoreError> {
        let requested_root = root.as_ref();
        let metadata = fs::symlink_metadata(requested_root)
            .map_err(|source| io_error("inspect", requested_root, source))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StoreError::InvalidRoot(requested_root.to_path_buf()));
        }
        let root = fs::canonicalize(requested_root)
            .map_err(|source| io_error("canonicalize", requested_root, source))?;
        let btrfs_program = btrfs_program.into();
        if !btrfs_program.is_absolute() {
            return Err(StoreError::RelativeBtrfsProgram(btrfs_program));
        }
        let store = Self {
            root,
            btrfs_program,
            runner: Arc::new(runner),
            operation: Mutex::new(()),
        };
        store.run_btrfs(
            "verify store filesystem",
            &[
                OsString::from("filesystem"),
                OsString::from("usage"),
                OsString::from("--raw"),
                store.root.as_os_str().to_owned(),
            ],
        )?;
        store.ensure_quotas_enabled()?;
        store.initialize_layout()?;
        store.recover()?;
        Ok(store)
    }

    /// Returns the canonical store root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Makes the pod-workspace container searchable but not listable so a
    /// service dropped to one pod's mapped UID can reach that pod's validated
    /// workspace path.
    ///
    /// Individual workspace subvolumes retain their own ownership and modes.
    ///
    /// # Errors
    ///
    /// Returns an error if the managed directory is unsafe or cannot be
    /// secured.
    pub fn enable_pod_workspace_traversal(&self) -> Result<(), StoreError> {
        let path = self.root.join(POD_WORKSPACES_DIRECTORY);
        if !path_state(&path)?.is_some_and(|metadata| metadata.is_dir()) {
            return Err(StoreError::UnsafePath(path));
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o711))
            .map_err(|source| io_error("make pod workspaces searchable", &path, source))
    }

    /// Makes the immutable image-seed container searchable but not listable
    /// so Git running as an image user can inspect a validated seed path.
    ///
    /// Seed subvolumes remain read-only and retain their own ownership and
    /// modes.
    ///
    /// # Errors
    ///
    /// Returns an error if the managed directory is unsafe or cannot be
    /// secured.
    pub fn enable_image_seed_traversal(&self) -> Result<(), StoreError> {
        let path = self.root.join(SETUP_SEEDS_DIRECTORY);
        if !path_state(&path)?.is_some_and(|metadata| metadata.is_dir()) {
            return Err(StoreError::UnsafePath(path));
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o711))
            .map_err(|source| io_error("make image seeds searchable", &path, source))
    }

    /// Creates a writable Btrfs subvolume for a prospective image generation.
    ///
    /// The returned directory is intentionally unpublished and may be
    /// populated by an OCI unpacker. A store reopen discards staging handles
    /// which were never published.
    ///
    /// # Errors
    ///
    /// Returns an error when Btrfs cannot create the staging subvolume or the
    /// store cannot durably record its transaction. The final content digest
    /// is deliberately supplied only to [`Self::publish_image`], after an OCI
    /// builder has produced and verified its output.
    pub fn begin_image(&self) -> Result<ImageStaging, StoreError> {
        let _operation = self.lock()?;
        let transaction_id = unique_id();
        let transaction = TransactionManifest {
            format_version: STORE_FORMAT_VERSION,
            transaction_id: transaction_id.clone(),
            operation: TransactionOperation::StageImage,
        };
        self.write_transaction(&transaction)?;
        let path = self.staging_path(&transaction_id);
        if let Err(error) = self.create_subvolume(&path, "create image staging subvolume") {
            let rollback = self
                .cleanup_image_publication(&transaction_id)
                .and_then(|()| self.sync_and_finish_transaction(&transaction_id));
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback) => Err(rollback_failed("begin image", &error, &rollback)),
            };
        }
        Ok(ImageStaging {
            store_root: self.root.clone(),
            transaction_id,
            path,
        })
    }

    /// Makes a populated image staging subvolume immutable and atomically
    /// publishes it as an image generation.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale handle, a conflicting image, a failed
    /// Btrfs operation, or a durable manifest failure. An incomplete operation
    /// remains recoverable on the next store open.
    #[allow(clippy::needless_pass_by_value)] // Consuming the staging token prevents publication reuse.
    pub fn publish_image(
        &self,
        staging: ImageStaging,
        image: ImageId,
        config: ImageConfig,
    ) -> Result<ImageGeneration, StoreError> {
        let _operation = self.lock()?;
        self.validate_staging(&staging)?;
        if path_state(&self.image_directory(&image))?.is_some()
            || self.read_transactions()?.iter().any(|(_, transaction)| {
                transaction.transaction_id != staging.transaction_id
                    && transaction.operation.references_image(&image)
            })
        {
            let error = StoreError::ImageExists(image);
            let rollback = self
                .cleanup_image_publication(&staging.transaction_id)
                .and_then(|()| self.sync_and_finish_transaction(&staging.transaction_id));
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback) => Err(rollback_failed("publish image", &error, &rollback)),
            };
        }

        self.write_transaction(&TransactionManifest {
            format_version: STORE_FORMAT_VERSION,
            transaction_id: staging.transaction_id.clone(),
            operation: TransactionOperation::PublishImage {
                image: image.clone(),
            },
        })?;
        let result = self.publish_image_inner(&staging, &image, config);
        match result {
            Ok(generation) => {
                self.finish_transaction(&staging.transaction_id)?;
                Ok(generation)
            }
            Err(error) => {
                // Once the final directory is visible, publication is
                // committed. Keep the transaction so recovery can only remove
                // its bookkeeping; never roll a visible generation back.
                if self.image_unlocked(&image).is_ok() {
                    return Err(error);
                }
                let rollback = self.cleanup_image_publication(&staging.transaction_id);
                match rollback
                    .and_then(|()| self.sync_and_finish_transaction(&staging.transaction_id))
                {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(rollback_failed("publish image", &error, &rollback)),
                }
            }
        }
    }

    /// Eagerly deletes an unpublished image staging subvolume.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale handle or failed cleanup. Failed cleanup is
    /// retained as a durable transaction for recovery.
    #[allow(clippy::needless_pass_by_value)] // Consuming the token prevents reuse after cleanup.
    pub fn discard_image(&self, staging: ImageStaging) -> Result<(), StoreError> {
        let _operation = self.lock()?;
        self.validate_staging(&staging)?;
        self.cleanup_image_publication(&staging.transaction_id)?;
        self.sync_and_finish_transaction(&staging.transaction_id)
    }

    /// Looks up an immutable image generation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ImageNotFound`] when absent, or an integrity error
    /// when its durable manifest and paths disagree.
    pub fn image(&self, image: &ImageId) -> Result<ImageGeneration, StoreError> {
        let _operation = self.lock()?;
        self.image_unlocked(image)
    }

    /// Lists all published image generations in digest order.
    ///
    /// # Errors
    ///
    /// Returns an error if any published generation is malformed.
    pub fn list_images(&self) -> Result<Vec<ImageGeneration>, StoreError> {
        let _operation = self.lock()?;
        self.list_images_unlocked()
    }

    /// Returns the image selected for ordinary pod creation.
    ///
    /// The selection survives guest daemon and workspace VM restarts. It is
    /// changed only by an explicit image build boundary.
    ///
    /// # Errors
    ///
    /// Returns an error if the selection manifest or referenced image is
    /// invalid.
    pub fn selected_image(&self) -> Result<Option<ImageId>, StoreError> {
        let _operation = self.lock()?;
        self.selected_image_unlocked()
    }

    /// Selects an existing immutable image for ordinary pod creation.
    ///
    /// # Errors
    ///
    /// Returns an error if the image does not exist or the selection cannot be
    /// persisted.
    pub fn select_image(&self, image: &ImageId) -> Result<(), StoreError> {
        let _operation = self.lock()?;
        self.image_unlocked(image)?;
        write_json_atomic(
            &self.root.join(SELECTED_IMAGE_MANIFEST),
            &SelectedImageManifest {
                image: image.clone(),
            },
        )
    }

    /// Clears the selected image so the next explicit resolution rebuilds it.
    ///
    /// # Errors
    ///
    /// Returns an error if the durable selection cannot be removed.
    pub fn clear_selected_image(&self) -> Result<(), StoreError> {
        let _operation = self.lock()?;
        remove_file_durable(&self.root.join(SELECTED_IMAGE_MANIFEST))
    }

    /// Creates all writable storage for a pod.
    ///
    /// The root is a writable snapshot of `image`; the workspace and Docker
    /// data roots are new independent sibling subvolumes. Only a fresh
    /// workspace subvolume's root inode is assigned to the image user. Future
    /// seeded workspace snapshots must preserve their existing ownership and
    /// must not be rewritten recursively at pod startup.
    ///
    /// # Errors
    ///
    /// Returns an error when the image is absent, the pod already has storage,
    /// or any transaction step fails. Completed rollback removes the durable
    /// transaction; incomplete rollback is retried on store reopen.
    pub fn create_pod(&self, pod: PodId, image: &ImageId) -> Result<PodStorage, StoreError> {
        self.create_pod_from_basis(pod, image, PodWorkspaceBasis::ImageSeed)
    }

    /// Creates the hidden pod used to prepare an image from its immutable OCI
    /// root and the current golden workspace.
    ///
    /// Existing image seeds are deliberately ignored so rebuilding an image
    /// never layers setup changes from an earlier build onto the new setup
    /// execution.
    ///
    /// # Errors
    ///
    /// Returns an error when the image is absent, the pod already has storage,
    /// or any transaction step fails.
    pub fn create_setup_pod(&self, pod: PodId, image: &ImageId) -> Result<PodStorage, StoreError> {
        self.create_pod_from_basis(pod, image, PodWorkspaceBasis::GoldenWorkspace)
    }

    fn create_pod_from_basis(
        &self,
        pod: PodId,
        image: &ImageId,
        basis: PodWorkspaceBasis,
    ) -> Result<PodStorage, StoreError> {
        let _operation = self.lock()?;
        let generation = self.image_unlocked(image)?;
        if self.pod_storage_exists(&pod)? || self.transaction_references_pod(&pod)? {
            return Err(StoreError::PodExists(pod));
        }

        let transaction_id = unique_id();
        let transaction = TransactionManifest {
            format_version: STORE_FORMAT_VERSION,
            transaction_id: transaction_id.clone(),
            operation: TransactionOperation::CreatePod {
                pod: pod.clone(),
                image: image.clone(),
            },
        };
        self.write_transaction(&transaction)?;
        let created_at_unix_ms = now_unix_ms();
        let result = self.create_pod_inner(
            &pod,
            image,
            generation.root(),
            generation.config(),
            created_at_unix_ms,
            basis,
        );
        match result {
            Ok(storage) => {
                self.finish_transaction(&transaction_id)?;
                Ok(storage)
            }
            Err(error) => {
                if self.pod_unlocked(&pod).is_ok() {
                    return Err(error);
                }
                let rollback = self.cleanup_pod_subvolumes(&pod);
                match rollback.and_then(|()| self.sync_and_finish_transaction(&transaction_id)) {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(rollback_failed("create pod", &error, &rollback)),
                }
            }
        }
    }

    /// Looks up a pod's persistent storage.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::PodNotFound`] when absent, or an integrity error
    /// when its durable manifest and paths disagree.
    pub fn pod(&self, pod: &PodId) -> Result<PodStorage, StoreError> {
        let _operation = self.lock()?;
        self.pod_unlocked(pod)
    }

    /// Lists all pods in identifier order.
    ///
    /// # Errors
    ///
    /// Returns an error if any pod manifest or subvolume is malformed.
    pub fn list_pods(&self) -> Result<Vec<PodStorage>, StoreError> {
        let _operation = self.lock()?;
        self.list_pods_unlocked()
    }

    /// Replaces a pod's temporary subvolume before a fresh runtime start.
    ///
    /// The persistent root, workspace, and Docker data remain untouched. A
    /// missing temporary subvolume left by an interrupted reset is recreated
    /// on the next attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the pod is absent or Btrfs cannot replace, limit,
    /// or synchronize the temporary subvolume.
    pub fn reset_pod_temporary(&self, pod: &PodId) -> Result<PodStorage, StoreError> {
        let _operation = self.lock()?;
        self.pod_unlocked(pod)?;
        let temporary = self.pod_temporary_path(pod);
        self.delete_subvolume_if_exists(&temporary, "delete pod temporary subvolume")?;
        self.create_pod_temporary(&temporary)?;
        self.sync_filesystem()?;
        self.pod_unlocked(pod)
    }

    /// Returns a persistent workspace-level writable cache, creating its
    /// Btrfs subvolume when first declared.
    ///
    /// Cache removal from workspace configuration deliberately leaves the
    /// subvolume intact so temporarily disabling a cache cannot destroy data.
    /// The new root starts mode 0777: each pod receives a different idmapped
    /// view, while files created inside retain their ordinary container IDs.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe name, replaced path, or failed Btrfs
    /// operation.
    pub fn ensure_cache(&self, name: &str) -> Result<PathBuf, StoreError> {
        let _operation = self.lock()?;
        PodId::new(name).map_err(|error| StoreError::InvalidCacheName(error.to_string()))?;
        let path = self.root.join(CACHES_DIRECTORY).join(name);
        match path_state(&path)? {
            None => {
                self.create_subvolume(&path, "create workspace cache subvolume")?;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o777))
                    .map_err(|source| io_error("set workspace cache permissions", &path, source))?;
                self.sync_filesystem()?;
            }
            Some(metadata) if metadata.is_dir() => {}
            Some(_) => return Err(StoreError::UnsafePath(path)),
        }
        Ok(path)
    }

    /// Replaces the golden workspace with a writable snapshot of a reconciled
    /// repository tree. Image setup is serialized with this publication and
    /// never observes a partially reconciled workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the source is unsafe or snapshot publication fails.
    pub fn publish_golden_workspace(&self, source: &Path) -> Result<PathBuf, StoreError> {
        let _operation = self.lock()?;
        let source = fs::canonicalize(source)
            .map_err(|error| io_error("canonicalize golden workspace source", source, error))?;
        if !path_state(&source)?.is_some_and(|metadata| metadata.is_dir()) {
            return Err(StoreError::UnsafePath(source));
        }
        let seed = self.root.join(GOLDEN_WORKSPACE);
        let staging_name = format!(".golden-workspace-{}", unique_id());
        let staging = self.root.join(&staging_name);
        self.snapshot_subvolume(&source, &staging)?;
        if path_state(&seed)?.is_some() {
            let directory = File::open(&self.root)
                .map_err(|source| io_error("open store root", &self.root, source))?;
            if let Err(source) = nix::fcntl::renameat2(
                &directory,
                staging_name.as_str(),
                &directory,
                GOLDEN_WORKSPACE,
                nix::fcntl::RenameFlags::RENAME_EXCHANGE,
            ) {
                let error = io_error(
                    "atomically publish golden workspace",
                    &seed,
                    std::io::Error::from(source),
                );
                return match self
                    .delete_subvolume_if_exists(&staging, "discard golden workspace staging")
                {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(rollback_failed(
                        "publish golden workspace",
                        &error,
                        &cleanup,
                    )),
                };
            }
            self.sync_filesystem()?;
            self.delete_subvolume_if_exists(&staging, "retire previous golden workspace")?;
        } else if let Err(source) = fs::rename(&staging, &seed) {
            let error = io_error("publish initial golden workspace", &seed, source);
            return match self
                .delete_subvolume_if_exists(&staging, "discard golden workspace staging")
            {
                Ok(()) => Err(error),
                Err(cleanup) => Err(rollback_failed(
                    "publish golden workspace",
                    &error,
                    &cleanup,
                )),
            };
        }
        self.sync_filesystem()?;
        Ok(seed)
    }

    /// Returns the published golden workspace, when one has been reconciled.
    ///
    /// # Errors
    ///
    /// Returns an error when the managed path was replaced with an unsafe
    /// filesystem object.
    pub fn golden_workspace(&self) -> Result<Option<PathBuf>, StoreError> {
        let _operation = self.lock()?;
        let path = self.root.join(GOLDEN_WORKSPACE);
        match path_state(&path)? {
            None => Ok(None),
            Some(metadata) if metadata.is_dir() => Ok(Some(path)),
            Some(_) => Err(StoreError::UnsafePath(path)),
        }
    }

    /// Publishes immutable snapshots of a successfully prepared pod as the
    /// root and workspace basis for subsequently created pods.
    ///
    /// # Errors
    ///
    /// Returns an error if the pod is absent or snapshots cannot be published.
    pub fn publish_setup_seed(&self, pod: &PodId, context: &ImageId) -> Result<String, StoreError> {
        let _operation = self.lock()?;
        let storage = self.pod_unlocked(pod)?;
        let previous = self
            .setup_seed_for_image_with_manifest(storage.image())?
            .map(|(manifest, _, _)| manifest.generation);
        let generation = unique_id();
        let directory = self.root.join(SETUP_SEEDS_DIRECTORY).join(&generation);
        fs::create_dir(&directory)
            .map_err(|source| io_error("create setup seed generation", &directory, source))?;
        let root = directory.join("root");
        let workspace = directory.join("workspace");
        let result = (|| {
            self.snapshot_subvolume(storage.root(), &root)?;
            self.snapshot_subvolume(storage.workspace(), &workspace)?;
            self.set_subvolume_read_only(&root)?;
            self.set_subvolume_read_only(&workspace)?;
            self.sync_filesystem()?;
            write_json_atomic(
                &self.image_seed_manifest_path(storage.image()),
                &SetupSeedManifest {
                    image: storage.image().clone(),
                    context: context.clone(),
                    generation: generation.clone(),
                },
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            return match self.delete_seed_generation(&directory) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(rollback_failed("publish setup seed", &error, &cleanup)),
            };
        }
        if let Some(previous) = previous {
            let previous = self.root.join(SETUP_SEEDS_DIRECTORY).join(previous);
            if let Err(error) = self.delete_seed_generation(&previous) {
                warn!(path = %previous.display(), %error, "could not retire previous image seed");
            }
        }
        Ok(generation)
    }

    /// Atomically replaces one image's canonical workspace seed while
    /// retaining the root filesystem produced by its setup run.
    ///
    /// The source is expected to be a private staging subvolume. Publication
    /// snapshots it into a new immutable seed generation, then switches the
    /// image-local manifest in one durable rename.
    ///
    /// # Errors
    ///
    /// Returns an error when the image has no completed setup seed, the source
    /// is unsafe, or snapshot publication fails.
    pub fn publish_image_workspace_seed(
        &self,
        image: &ImageId,
        source: &Path,
    ) -> Result<String, StoreError> {
        let _operation = self.lock()?;
        let (manifest, prepared_root, _) = self
            .setup_seed_for_image_with_manifest(image)?
            .ok_or_else(|| StoreError::CorruptManifest {
                path: self.image_seed_manifest_path(image),
                message: "image has no completed workspace seed".to_owned(),
            })?;
        let source = fs::canonicalize(source)
            .map_err(|error| io_error("canonicalize image workspace seed source", source, error))?;
        if !path_state(&source)?.is_some_and(|metadata| metadata.is_dir()) {
            return Err(StoreError::UnsafePath(source));
        }
        let generation = unique_id();
        let directory = self.root.join(SETUP_SEEDS_DIRECTORY).join(&generation);
        fs::create_dir(&directory)
            .map_err(|source| io_error("create image seed generation", &directory, source))?;
        let root = directory.join("root");
        let workspace = directory.join("workspace");
        let result = (|| {
            self.snapshot_subvolume(&prepared_root, &root)?;
            self.snapshot_subvolume(&source, &workspace)?;
            self.set_subvolume_read_only(&root)?;
            self.set_subvolume_read_only(&workspace)?;
            self.sync_filesystem()?;
            write_json_atomic(
                &self.image_seed_manifest_path(image),
                &SetupSeedManifest {
                    image: image.clone(),
                    context: manifest.context,
                    generation: generation.clone(),
                },
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            return match self.delete_seed_generation(&directory) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(rollback_failed(
                    "publish image workspace seed",
                    &error,
                    &cleanup,
                )),
            };
        }
        let previous = self
            .root
            .join(SETUP_SEEDS_DIRECTORY)
            .join(manifest.generation);
        if let Err(error) = self.delete_seed_generation(&previous) {
            warn!(path = %previous.display(), %error, "could not retire previous image seed");
        }
        Ok(generation)
    }

    /// Returns one image's immutable canonical workspace seed.
    ///
    /// # Errors
    ///
    /// Returns an error when the image seed manifest or retained snapshots are
    /// unsafe or corrupt.
    pub fn image_workspace_seed(&self, image: &ImageId) -> Result<Option<PathBuf>, StoreError> {
        let _operation = self.lock()?;
        self.setup_seed_for_image(image)
            .map(|seed| seed.map(|(_, workspace)| workspace))
    }

    /// Returns the image generation pinned by the currently published setup
    /// seed.
    ///
    /// # Errors
    ///
    /// Returns an error if the setup manifest is unsafe or corrupt.
    pub fn setup_seed_image(&self, context: &ImageId) -> Result<Option<ImageId>, StoreError> {
        let _operation = self.lock()?;
        for image in self.list_images_unlocked()? {
            let manifest_path = self.image_seed_manifest_path(image.id());
            if path_state(&manifest_path)?.is_none() {
                continue;
            }
            let manifest: SetupSeedManifest = read_json(&manifest_path)?;
            if manifest.context == *context {
                return Ok(Some(manifest.image));
            }
        }
        Ok(None)
    }

    /// Durably hides a pod and deletes all four of its subvolumes.
    ///
    /// Deletion is roll-forward: after its manifest is removed, a failure
    /// leaves a transaction which startup recovery completes.
    ///
    /// # Errors
    ///
    /// Returns an error when the pod is absent or cleanup cannot complete.
    pub fn destroy_pod(&self, pod: &PodId) -> Result<(), StoreError> {
        let _operation = self.lock()?;
        self.pod_unlocked(pod)?;
        let transaction_id = unique_id();
        self.write_transaction(&TransactionManifest {
            format_version: STORE_FORMAT_VERSION,
            transaction_id: transaction_id.clone(),
            operation: TransactionOperation::DeletePod { pod: pod.clone() },
        })?;
        self.continue_delete_pod(pod)?;
        self.finish_transaction(&transaction_id)
    }

    /// Deletes an unreferenced image generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the image is absent, is pinned by a pod or
    /// incomplete pod transaction, or cannot be deleted.
    pub fn remove_image(&self, image: &ImageId) -> Result<(), StoreError> {
        let _operation = self.lock()?;
        self.image_unlocked(image)?;
        if self.selected_image_unlocked()?.as_ref() == Some(image) {
            return Err(StoreError::ImageSelected(image.clone()));
        }
        if let Some(pod) = self.image_reference(image)? {
            return Err(StoreError::ImageInUse {
                image: image.clone(),
                pod,
            });
        }
        let transaction_id = unique_id();
        self.write_transaction(&TransactionManifest {
            format_version: STORE_FORMAT_VERSION,
            transaction_id: transaction_id.clone(),
            operation: TransactionOperation::DeleteImage {
                image: image.clone(),
            },
        })?;
        self.continue_delete_image(&transaction_id, image)?;
        self.finish_transaction(&transaction_id)
    }

    /// Replays or rolls back every durable incomplete transaction.
    ///
    /// `with_runner` invokes this automatically. Calling it explicitly is
    /// useful after resolving a transient storage error without restarting the
    /// daemon. It must not run while an image staging directory is actively
    /// being populated.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt transaction state or failed cleanup.
    pub fn recover(&self) -> Result<(), StoreError> {
        let _operation = self.lock()?;
        cleanup_atomic_temps(&self.root.join(POD_MANIFESTS_DIRECTORY))?;
        cleanup_atomic_temps(&self.root.join(TRANSACTIONS_DIRECTORY))?;
        for (transaction_id, transaction) in self.read_transactions()? {
            match transaction.operation {
                TransactionOperation::StageImage => {
                    self.cleanup_image_publication(&transaction_id)?;
                }
                TransactionOperation::PublishImage { image } => match self.image_unlocked(&image) {
                    Ok(_) => self.cleanup_image_publication(&transaction_id)?,
                    Err(StoreError::ImageNotFound(_)) => {
                        self.cleanup_image_publication(&transaction_id)?;
                    }
                    Err(error) => return Err(error),
                },
                TransactionOperation::CreatePod { pod, image } => match self.pod_unlocked(&pod) {
                    Ok(storage) if storage.image() == &image => {}
                    Ok(_) => {
                        return Err(StoreError::CorruptManifest {
                            path: self.pod_manifest_path(&pod),
                            message: "committed pod pins a different image than its transaction"
                                .to_owned(),
                        });
                    }
                    Err(StoreError::PodNotFound(_)) => {
                        self.cleanup_pod_subvolumes(&pod)?;
                    }
                    Err(error) => return Err(error),
                },
                TransactionOperation::DeletePod { pod } => {
                    self.continue_delete_pod(&pod)?;
                }
                TransactionOperation::DeleteImage { image } => {
                    if let Some(pod) =
                        self.image_reference_excluding_transaction(&image, &transaction_id)?
                    {
                        return Err(StoreError::ImageInUse { image, pod });
                    }
                    self.continue_delete_image(&transaction_id, &image)?;
                }
            }
            self.sync_and_finish_transaction(&transaction_id)?;
        }
        Ok(())
    }

    fn image_directory(&self, image: &ImageId) -> PathBuf {
        self.root.join(IMAGES_DIRECTORY).join(image.as_str())
    }

    fn image_manifest_path(&self, image: &ImageId) -> PathBuf {
        self.image_directory(image).join(IMAGE_MANIFEST)
    }

    fn image_seed_manifest_path(&self, image: &ImageId) -> PathBuf {
        self.image_directory(image).join(IMAGE_SEED_MANIFEST)
    }

    fn staging_path(&self, transaction_id: &str) -> PathBuf {
        self.root.join(IMAGE_STAGING_DIRECTORY).join(transaction_id)
    }

    fn publishing_path(&self, transaction_id: &str) -> PathBuf {
        self.root
            .join(IMAGE_PUBLISHING_DIRECTORY)
            .join(transaction_id)
    }

    fn transaction_path(&self, transaction_id: &str) -> PathBuf {
        self.root
            .join(TRANSACTIONS_DIRECTORY)
            .join(format!("{transaction_id}.json"))
    }

    fn pod_manifest_path(&self, pod: &PodId) -> PathBuf {
        self.root
            .join(POD_MANIFESTS_DIRECTORY)
            .join(format!("{}.json", pod.as_str()))
    }

    fn pod_root_path(&self, pod: &PodId) -> PathBuf {
        self.root.join(POD_ROOTS_DIRECTORY).join(pod.as_str())
    }

    fn pod_workspace_path(&self, pod: &PodId) -> PathBuf {
        self.root.join(POD_WORKSPACES_DIRECTORY).join(pod.as_str())
    }

    fn pod_docker_path(&self, pod: &PodId) -> PathBuf {
        self.root.join(POD_DOCKER_DIRECTORY).join(pod.as_str())
    }

    fn pod_temporary_path(&self, pod: &PodId) -> PathBuf {
        self.root.join(POD_TEMPORARIES_DIRECTORY).join(pod.as_str())
    }

    fn trash_path(&self, transaction_id: &str) -> PathBuf {
        self.root.join(TRASH_DIRECTORY).join(transaction_id)
    }

    fn write_transaction(&self, transaction: &TransactionManifest) -> Result<(), StoreError> {
        if !valid_transaction_id(&transaction.transaction_id) {
            return Err(StoreError::CorruptManifest {
                path: self.transaction_path(&transaction.transaction_id),
                message: "invalid transaction ID".to_owned(),
            });
        }
        write_json_atomic(
            &self.transaction_path(&transaction.transaction_id),
            transaction,
        )
    }

    fn finish_transaction(&self, transaction_id: &str) -> Result<(), StoreError> {
        remove_file_durable(&self.transaction_path(transaction_id))
    }

    fn sync_and_finish_transaction(&self, transaction_id: &str) -> Result<(), StoreError> {
        self.sync_filesystem()?;
        self.finish_transaction(transaction_id)
    }

    fn read_transactions(&self) -> Result<Vec<(String, TransactionManifest)>, StoreError> {
        let directory = self.root.join(TRANSACTIONS_DIRECTORY);
        let mut transactions = Vec::new();
        for entry in
            fs::read_dir(&directory).map_err(|source| io_error("read", &directory, source))?
        {
            let entry = entry.map_err(|source| io_error("read", &directory, source))?;
            let path = entry.path();
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                return Err(StoreError::UnsafePath(path));
            };
            if file_name.starts_with('.') && file_name.contains(".tmp-") {
                continue;
            }
            let Some(transaction_id) = file_name.strip_suffix(".json") else {
                return Err(StoreError::UnsafePath(path));
            };
            if !valid_transaction_id(transaction_id) {
                return Err(StoreError::UnsafePath(path));
            }
            let transaction: TransactionManifest = read_json(&path)?;
            check_format(&path, transaction.format_version)?;
            if transaction.transaction_id != transaction_id {
                return Err(StoreError::CorruptManifest {
                    path,
                    message: "transaction ID does not match its file name".to_owned(),
                });
            }
            transactions.push((transaction_id.to_owned(), transaction));
        }
        transactions.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(transactions)
    }

    fn validate_staging(&self, staging: &ImageStaging) -> Result<(), StoreError> {
        if staging.store_root != self.root
            || staging.path != self.staging_path(&staging.transaction_id)
        {
            return Err(StoreError::InvalidStaging);
        }
        let transaction_path = self.transaction_path(&staging.transaction_id);
        let transaction: TransactionManifest =
            read_json(&transaction_path).map_err(|_| StoreError::InvalidStaging)?;
        check_format(&transaction_path, transaction.format_version)?;
        if transaction.transaction_id != staging.transaction_id
            || !matches!(transaction.operation, TransactionOperation::StageImage)
        {
            return Err(StoreError::InvalidStaging);
        }
        let metadata = path_state(&staging.path)?.ok_or(StoreError::InvalidStaging)?;
        if !metadata.is_dir() {
            return Err(StoreError::InvalidStaging);
        }
        Ok(())
    }

    fn publish_image_inner(
        &self,
        staging: &ImageStaging,
        image: &ImageId,
        config: ImageConfig,
    ) -> Result<ImageGeneration, StoreError> {
        let publishing = self.publishing_path(&staging.transaction_id);
        ensure_new_directory(&publishing)?;
        let publishing_root = publishing.join(IMAGE_ROOT_DIRECTORY);
        fs::rename(&staging.path, &publishing_root)
            .map_err(|source| io_error("move image staging subvolume", &staging.path, source))?;
        sync_directory(
            staging
                .path
                .parent()
                .ok_or_else(|| StoreError::UnsafePath(staging.path.clone()))?,
        )?;
        sync_directory(&publishing)?;

        let created_at_unix_ms = now_unix_ms();
        let manifest_path = publishing.join(IMAGE_MANIFEST);
        let manifest = ImageManifest {
            format_version: STORE_FORMAT_VERSION,
            id: image.clone(),
            config: config.clone(),
            created_at_unix_ms,
        };
        write_json_atomic(&manifest_path, &manifest)?;
        // Publication must never make an image visible which a subsequent
        // open cannot decode under the same bounded-reader policy.
        let persisted: ImageManifest = read_json(&manifest_path)?;
        if persisted != manifest {
            return Err(StoreError::CorruptManifest {
                path: manifest_path,
                message: "newly written image manifest did not round-trip".to_owned(),
            });
        }
        // A read-only Btrfs subvolume cannot itself be renamed. Move the
        // writable staging subvolume into its private publication directory
        // first, then freeze it before the parent directory becomes visible.
        self.run_btrfs(
            "make image generation read-only",
            &[
                OsString::from("property"),
                OsString::from("set"),
                publishing_root.as_os_str().to_owned(),
                OsString::from("ro"),
                OsString::from("true"),
            ],
        )?;
        self.sync_filesystem()?;

        let final_path = self.image_directory(image);
        fs::rename(&publishing, &final_path)
            .map_err(|source| io_error("publish image generation", &final_path, source))?;
        sync_directory(&self.root.join(IMAGES_DIRECTORY))?;
        sync_directory(&self.root.join(IMAGE_PUBLISHING_DIRECTORY))?;
        self.sync_filesystem()?;
        Ok(ImageGeneration {
            id: image.clone(),
            root: final_path.join(IMAGE_ROOT_DIRECTORY),
            config,
            created_at_unix_ms,
        })
    }

    fn cleanup_image_publication(&self, transaction_id: &str) -> Result<(), StoreError> {
        let staging = self.staging_path(transaction_id);
        self.delete_subvolume_if_exists(&staging, "delete image staging subvolume")?;
        let publishing = self.publishing_path(transaction_id);
        if let Some(metadata) = path_state(&publishing)? {
            if !metadata.is_dir() {
                return Err(StoreError::UnsafePath(publishing));
            }
            self.delete_subvolume_if_exists(
                &publishing.join(IMAGE_ROOT_DIRECTORY),
                "delete unpublished image subvolume",
            )?;
            remove_file_durable(&publishing.join(IMAGE_MANIFEST))?;
            remove_empty_directory(&publishing)?;
        }
        Ok(())
    }

    fn image_unlocked(&self, image: &ImageId) -> Result<ImageGeneration, StoreError> {
        let directory = self.image_directory(image);
        let Some(metadata) = path_state(&directory)? else {
            return Err(StoreError::ImageNotFound(image.clone()));
        };
        if !metadata.is_dir() {
            return Err(StoreError::UnsafePath(directory));
        }
        let manifest_path = self.image_manifest_path(image);
        let manifest: ImageManifest = read_json(&manifest_path)?;
        check_format(&manifest_path, manifest.format_version)?;
        manifest.config.validate()?;
        if &manifest.id != image {
            return Err(StoreError::CorruptManifest {
                path: manifest_path,
                message: "image digest does not match its directory".to_owned(),
            });
        }
        let root = directory.join(IMAGE_ROOT_DIRECTORY);
        let root_metadata = path_state(&root)?.ok_or_else(|| StoreError::CorruptManifest {
            path: self.image_manifest_path(image),
            message: "image root subvolume is missing".to_owned(),
        })?;
        if !root_metadata.is_dir() {
            return Err(StoreError::UnsafePath(root));
        }
        Ok(ImageGeneration {
            id: manifest.id,
            root,
            config: manifest.config,
            created_at_unix_ms: manifest.created_at_unix_ms,
        })
    }

    fn selected_image_unlocked(&self) -> Result<Option<ImageId>, StoreError> {
        let manifest_path = self.root.join(SELECTED_IMAGE_MANIFEST);
        if path_state(&manifest_path)?.is_none() {
            return Ok(None);
        }
        let manifest: SelectedImageManifest = read_json(&manifest_path)?;
        self.image_unlocked(&manifest.image)?;
        Ok(Some(manifest.image))
    }

    fn list_images_unlocked(&self) -> Result<Vec<ImageGeneration>, StoreError> {
        let directory = self.root.join(IMAGES_DIRECTORY);
        let mut images = Vec::new();
        for entry in
            fs::read_dir(&directory).map_err(|source| io_error("read", &directory, source))?
        {
            let entry = entry.map_err(|source| io_error("read", &directory, source))?;
            let path = entry.path();
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| StoreError::UnsafePath(path.clone()))?
                .to_owned();
            let image = ImageId::new(name).map_err(|error| StoreError::CorruptManifest {
                path,
                message: error.to_string(),
            })?;
            images.push(self.image_unlocked(&image)?);
        }
        images.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(images)
    }

    fn create_pod_inner(
        &self,
        pod: &PodId,
        image: &ImageId,
        image_root: &Path,
        image_config: &ImageConfig,
        created_at_unix_ms: u64,
        basis: PodWorkspaceBasis,
    ) -> Result<PodStorage, StoreError> {
        let setup = match basis {
            PodWorkspaceBasis::ImageSeed => self.setup_seed_for_image(image)?,
            PodWorkspaceBasis::GoldenWorkspace => None,
        };
        let root = self.pod_root_path(pod);
        self.snapshot_subvolume(
            setup
                .as_ref()
                .map_or(image_root, |(root, _)| root.as_path()),
            &root,
        )?;
        let workspace = self.pod_workspace_path(pod);
        if let Some((_, setup_workspace)) = setup {
            self.snapshot_subvolume(&setup_workspace, &workspace)?;
        } else {
            let seed = self.root.join(GOLDEN_WORKSPACE);
            let seed_matches_user = path_state(&seed)?.is_some_and(|metadata| {
                metadata.is_dir()
                    && metadata.uid() == image_config.user().uid()
                    && metadata.gid() == image_config.user().gid()
            });
            if seed_matches_user {
                self.snapshot_subvolume(&seed, &workspace)?;
            } else {
                self.create_subvolume(&workspace, "create pod workspace subvolume")?;
                let user = image_config.user();
                if (user.uid(), user.gid()) != (0, 0) {
                    set_directory_owner(&workspace, user.uid(), user.gid())?;
                }
            }
        }
        let docker = self.pod_docker_path(pod);
        self.create_subvolume(&docker, "create pod Docker subvolume")?;
        let temporary = self.pod_temporary_path(pod);
        self.create_pod_temporary(&temporary)?;
        self.sync_filesystem()?;
        write_json_atomic(
            &self.pod_manifest_path(pod),
            &PodManifest {
                format_version: STORE_FORMAT_VERSION,
                id: pod.clone(),
                image: image.clone(),
                created_at_unix_ms,
            },
        )?;
        Ok(PodStorage {
            id: pod.clone(),
            image: image.clone(),
            root,
            workspace,
            docker,
            temporary,
            image_config: image_config.clone(),
            created_at_unix_ms,
        })
    }

    fn setup_seed_for_image(
        &self,
        image: &ImageId,
    ) -> Result<Option<(PathBuf, PathBuf)>, StoreError> {
        self.setup_seed_for_image_with_manifest(image)
            .map(|seed| seed.map(|(_, root, workspace)| (root, workspace)))
    }

    fn setup_seed_for_image_with_manifest(
        &self,
        image: &ImageId,
    ) -> Result<Option<(SetupSeedManifest, PathBuf, PathBuf)>, StoreError> {
        let manifest_path = self.image_seed_manifest_path(image);
        if path_state(&manifest_path)?.is_none() {
            return Ok(None);
        }
        let manifest: SetupSeedManifest = read_json(&manifest_path)?;
        if &manifest.image != image {
            return Ok(None);
        }
        let directory = self
            .root
            .join(SETUP_SEEDS_DIRECTORY)
            .join(&manifest.generation);
        let root = directory.join("root");
        let workspace = directory.join("workspace");
        if !path_state(&root)?.is_some_and(|metadata| metadata.is_dir())
            || !path_state(&workspace)?.is_some_and(|metadata| metadata.is_dir())
        {
            return Err(StoreError::CorruptManifest {
                path: manifest_path,
                message: "published setup seed is incomplete".to_owned(),
            });
        }
        Ok(Some((manifest, root, workspace)))
    }

    fn pod_unlocked(&self, pod: &PodId) -> Result<PodStorage, StoreError> {
        let manifest_path = self.pod_manifest_path(pod);
        if path_state(&manifest_path)?.is_none() {
            return Err(StoreError::PodNotFound(pod.clone()));
        }
        let manifest: PodManifest = read_json(&manifest_path)?;
        check_format(&manifest_path, manifest.format_version)?;
        if &manifest.id != pod {
            return Err(StoreError::CorruptManifest {
                path: manifest_path,
                message: "pod ID does not match its file name".to_owned(),
            });
        }
        let image = self.image_unlocked(&manifest.image)?;
        let root = Self::require_subvolume_path(self.pod_root_path(pod), &manifest_path)?;
        let workspace = Self::require_subvolume_path(self.pod_workspace_path(pod), &manifest_path)?;
        let docker = Self::require_subvolume_path(self.pod_docker_path(pod), &manifest_path)?;
        let temporary_path = self.pod_temporary_path(pod);
        let temporary = match path_state(&temporary_path)? {
            Some(metadata) if metadata.is_dir() => temporary_path,
            Some(_) => return Err(StoreError::UnsafePath(temporary_path)),
            None => {
                self.create_pod_temporary(&temporary_path)?;
                self.sync_filesystem()?;
                temporary_path
            }
        };
        Ok(PodStorage {
            id: manifest.id,
            image: manifest.image,
            root,
            workspace,
            docker,
            temporary,
            image_config: image.config,
            created_at_unix_ms: manifest.created_at_unix_ms,
        })
    }

    fn list_pods_unlocked(&self) -> Result<Vec<PodStorage>, StoreError> {
        let directory = self.root.join(POD_MANIFESTS_DIRECTORY);
        let mut pods = Vec::new();
        for entry in
            fs::read_dir(&directory).map_err(|source| io_error("read", &directory, source))?
        {
            let entry = entry.map_err(|source| io_error("read", &directory, source))?;
            let path = entry.path();
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| StoreError::UnsafePath(path.clone()))?
                .to_owned();
            if name.starts_with('.') && name.contains(".tmp-") {
                continue;
            }
            let Some(id) = name.strip_suffix(".json") else {
                return Err(StoreError::UnsafePath(path));
            };
            let pod = PodId::new(id).map_err(|error| StoreError::CorruptManifest {
                path,
                message: error.to_string(),
            })?;
            pods.push(self.pod_unlocked(&pod)?);
        }
        pods.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(pods)
    }

    fn require_subvolume_path(path: PathBuf, manifest_path: &Path) -> Result<PathBuf, StoreError> {
        let metadata = path_state(&path)?.ok_or_else(|| StoreError::CorruptManifest {
            path: manifest_path.to_path_buf(),
            message: format!("required subvolume {} is missing", path.display()),
        })?;
        if metadata.is_dir() {
            Ok(path)
        } else {
            Err(StoreError::UnsafePath(path))
        }
    }

    fn pod_storage_exists(&self, pod: &PodId) -> Result<bool, StoreError> {
        for path in [
            self.pod_manifest_path(pod),
            self.pod_root_path(pod),
            self.pod_workspace_path(pod),
            self.pod_docker_path(pod),
            self.pod_temporary_path(pod),
        ] {
            if path_state(&path)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn transaction_references_pod(&self, pod: &PodId) -> Result<bool, StoreError> {
        Ok(self
            .read_transactions()?
            .iter()
            .any(|(_, transaction)| transaction.operation.references_pod(pod)))
    }

    fn cleanup_pod_subvolumes(&self, pod: &PodId) -> Result<(), StoreError> {
        let mut first_error = None;
        for (path, operation) in [
            (
                self.pod_temporary_path(pod),
                "delete pod temporary subvolume",
            ),
            (self.pod_docker_path(pod), "delete pod Docker subvolume"),
            (
                self.pod_workspace_path(pod),
                "delete pod workspace subvolume",
            ),
            (self.pod_root_path(pod), "delete pod root subvolume"),
        ] {
            if let Err(error) = self.delete_subvolume_if_exists(&path, operation) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn continue_delete_pod(&self, pod: &PodId) -> Result<(), StoreError> {
        remove_file_durable(&self.pod_manifest_path(pod))?;
        self.cleanup_pod_subvolumes(pod)?;
        self.sync_filesystem()
    }

    fn image_reference(&self, image: &ImageId) -> Result<Option<PodId>, StoreError> {
        self.image_reference_excluding_transaction(image, "")
    }

    fn image_reference_excluding_transaction(
        &self,
        image: &ImageId,
        excluded_transaction: &str,
    ) -> Result<Option<PodId>, StoreError> {
        if let Some(storage) = self
            .list_pods_unlocked()?
            .into_iter()
            .find(|storage| storage.image() == image)
        {
            return Ok(Some(storage.id));
        }
        for (transaction_id, transaction) in self.read_transactions()? {
            if transaction_id == excluded_transaction {
                continue;
            }
            if let TransactionOperation::CreatePod {
                pod,
                image: transaction_image,
            } = transaction.operation
                && &transaction_image == image
            {
                return Ok(Some(pod));
            }
        }
        Ok(None)
    }

    fn continue_delete_image(
        &self,
        transaction_id: &str,
        image: &ImageId,
    ) -> Result<(), StoreError> {
        let final_path = self.image_directory(image);
        let trash = self.trash_path(transaction_id);
        if path_state(&trash)?.is_none()
            && let Some(metadata) = path_state(&final_path)?
        {
            if !metadata.is_dir() {
                return Err(StoreError::UnsafePath(final_path));
            }
            fs::rename(&final_path, &trash)
                .map_err(|source| io_error("hide image generation", &final_path, source))?;
            sync_directory(&self.root.join(IMAGES_DIRECTORY))?;
            sync_directory(&self.root.join(TRASH_DIRECTORY))?;
        }
        self.delete_image_tree(&trash)?;
        self.sync_filesystem()
    }

    fn delete_image_tree(&self, directory: &Path) -> Result<(), StoreError> {
        let Some(metadata) = path_state(directory)? else {
            return Ok(());
        };
        if !metadata.is_dir() {
            return Err(StoreError::UnsafePath(directory.to_path_buf()));
        }
        let seed_manifest_path = directory.join(IMAGE_SEED_MANIFEST);
        if path_state(&seed_manifest_path)?.is_some() {
            let manifest: SetupSeedManifest = read_json(&seed_manifest_path)?;
            let seed = self
                .root
                .join(SETUP_SEEDS_DIRECTORY)
                .join(manifest.generation);
            self.delete_seed_generation(&seed)?;
            remove_file_durable(&seed_manifest_path)?;
        }
        self.delete_subvolume_if_exists(
            &directory.join(IMAGE_ROOT_DIRECTORY),
            "delete image generation subvolume",
        )?;
        remove_file_durable(&directory.join(IMAGE_MANIFEST))?;
        remove_empty_directory(directory)
    }

    fn delete_seed_generation(&self, directory: &Path) -> Result<(), StoreError> {
        let Some(metadata) = path_state(directory)? else {
            return Ok(());
        };
        if !metadata.is_dir() {
            return Err(StoreError::UnsafePath(directory.to_path_buf()));
        }
        self.delete_subvolume_if_exists(
            &directory.join("workspace"),
            "delete image workspace seed",
        )?;
        self.delete_subvolume_if_exists(&directory.join("root"), "delete image root seed")?;
        remove_empty_directory(directory)
    }

    fn create_subvolume(&self, path: &Path, operation: &'static str) -> Result<(), StoreError> {
        if path_state(path)?.is_some() {
            return Err(StoreError::UnsafePath(path.to_path_buf()));
        }
        self.run_btrfs(
            operation,
            &[
                OsString::from("subvolume"),
                OsString::from("create"),
                path.as_os_str().to_owned(),
            ],
        )?;
        let metadata = path_state(path)?.ok_or_else(|| StoreError::CorruptManifest {
            path: path.to_path_buf(),
            message: "btrfs reported success without creating a subvolume".to_owned(),
        })?;
        if metadata.is_dir() {
            Ok(())
        } else {
            Err(StoreError::UnsafePath(path.to_path_buf()))
        }
    }

    fn create_pod_temporary(&self, path: &Path) -> Result<(), StoreError> {
        self.create_subvolume(path, "create pod temporary subvolume")?;
        let configured = (|| {
            self.run_btrfs(
                "limit pod temporary subvolume",
                &[
                    OsString::from("qgroup"),
                    OsString::from("limit"),
                    OsString::from(POD_TEMPORARY_QUOTA_BYTES.to_string()),
                    path.as_os_str().to_owned(),
                ],
            )?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o1777))
                .map_err(|source| io_error("set pod temporary permissions", path, source))
        })();
        match configured {
            Ok(()) => Ok(()),
            Err(error) => {
                match self.delete_subvolume_if_exists(path, "roll back pod temporary subvolume") {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(rollback_failed(
                        "create pod temporary subvolume",
                        &error,
                        &rollback,
                    )),
                }
            }
        }
    }

    fn snapshot_subvolume(&self, source: &Path, destination: &Path) -> Result<(), StoreError> {
        if path_state(destination)?.is_some() {
            return Err(StoreError::UnsafePath(destination.to_path_buf()));
        }
        self.run_btrfs(
            "snapshot pod root subvolume",
            &[
                OsString::from("subvolume"),
                OsString::from("snapshot"),
                source.as_os_str().to_owned(),
                destination.as_os_str().to_owned(),
            ],
        )?;
        let metadata = path_state(destination)?.ok_or_else(|| StoreError::CorruptManifest {
            path: destination.to_path_buf(),
            message: "btrfs reported success without creating a snapshot".to_owned(),
        })?;
        if metadata.is_dir() {
            Ok(())
        } else {
            Err(StoreError::UnsafePath(destination.to_path_buf()))
        }
    }

    fn set_subvolume_read_only(&self, path: &Path) -> Result<(), StoreError> {
        self.run_btrfs(
            "mark setup seed read-only",
            &[
                OsString::from("property"),
                OsString::from("set"),
                path.as_os_str().to_owned(),
                OsString::from("ro"),
                OsString::from("true"),
            ],
        )
    }

    fn delete_subvolume_if_exists(
        &self,
        path: &Path,
        operation: &'static str,
    ) -> Result<(), StoreError> {
        let Some(metadata) = path_state(path)? else {
            return Ok(());
        };
        if !metadata.is_dir() {
            return Err(StoreError::UnsafePath(path.to_path_buf()));
        }
        self.run_btrfs(
            operation,
            &[
                OsString::from("subvolume"),
                OsString::from("delete"),
                OsString::from("--recursive"),
                path.as_os_str().to_owned(),
            ],
        )?;
        if path_state(path)?.is_some() {
            return Err(StoreError::CorruptManifest {
                path: path.to_path_buf(),
                message: "btrfs reported success without deleting the subvolume".to_owned(),
            });
        }
        Ok(())
    }

    fn sync_filesystem(&self) -> Result<(), StoreError> {
        self.run_btrfs(
            "sync storage filesystem",
            &[
                OsString::from("filesystem"),
                OsString::from("sync"),
                self.root.as_os_str().to_owned(),
            ],
        )
    }
}

impl TransactionOperation {
    fn references_image(&self, expected: &ImageId) -> bool {
        match self {
            Self::PublishImage { image }
            | Self::CreatePod { image, .. }
            | Self::DeleteImage { image } => image == expected,
            Self::StageImage | Self::DeletePod { .. } => false,
        }
    }

    fn references_pod(&self, expected: &PodId) -> bool {
        match self {
            Self::CreatePod { pod, .. } | Self::DeletePod { pod } => pod == expected,
            Self::StageImage | Self::PublishImage { .. } | Self::DeleteImage { .. } => false,
        }
    }
}

fn valid_transaction_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

fn rollback_failed(
    operation: &'static str,
    cause: &StoreError,
    rollback: &StoreError,
) -> StoreError {
    StoreError::RollbackFailed {
        operation,
        cause: cause.to_string(),
        rollback: rollback.to_string(),
    }
}

fn ensure_new_directory(path: &Path) -> Result<(), StoreError> {
    if path_state(path)?.is_some() {
        return Err(StoreError::UnsafePath(path.to_path_buf()));
    }
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
        .create(path)
        .map_err(|source| io_error("create directory", path, source))?;
    sync_parent(path)
}

fn remove_empty_directory(path: &Path) -> Result<(), StoreError> {
    match path_state(path)? {
        None => Ok(()),
        Some(metadata) if metadata.is_dir() => {
            fs::remove_dir(path).map_err(|source| io_error("remove directory", path, source))?;
            sync_parent(path)
        }
        Some(_) => Err(StoreError::UnsafePath(path.to_path_buf())),
    }
}

fn cleanup_atomic_temps(directory: &Path) -> Result<(), StoreError> {
    let mut changed = false;
    for entry in fs::read_dir(directory).map_err(|source| io_error("read", directory, source))? {
        let entry = entry.map_err(|source| io_error("read", directory, source))?;
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') && name.to_string_lossy().contains(".tmp-") {
            let metadata =
                fs::symlink_metadata(&path).map_err(|source| io_error("inspect", &path, source))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(StoreError::UnsafePath(path));
            }
            fs::remove_file(&path).map_err(|source| io_error("remove", &path, source))?;
            changed = true;
        }
    }
    if changed {
        sync_directory(directory)?;
    }
    Ok(())
}

impl<R: CommandRunner> BtrfsStore<R> {
    fn lock(&self) -> Result<MutexGuard<'_, ()>, StoreError> {
        self.operation.lock().map_err(|_| StoreError::LockPoisoned)
    }

    fn initialize_layout(&self) -> Result<(), StoreError> {
        let manifest_path = self.root.join(STORE_MANIFEST);
        if path_state(&manifest_path)?.is_none() {
            cleanup_initialization_temps(&self.root)?;
            if fs::read_dir(&self.root)
                .map_err(|source| io_error("read", &self.root, source))?
                .next()
                .is_some()
            {
                return Err(StoreError::NonEmptyUninitializedStore(self.root.clone()));
            }
            write_json_atomic(
                &manifest_path,
                &StoreManifest {
                    format_version: STORE_FORMAT_VERSION,
                },
            )?;
        }
        let manifest: StoreManifest = read_json(&manifest_path)?;
        check_format(&manifest_path, manifest.format_version)?;

        for name in [
            IMAGES_DIRECTORY,
            IMAGE_STAGING_DIRECTORY,
            IMAGE_PUBLISHING_DIRECTORY,
            POD_MANIFESTS_DIRECTORY,
            POD_ROOTS_DIRECTORY,
            POD_WORKSPACES_DIRECTORY,
            POD_DOCKER_DIRECTORY,
            POD_TEMPORARIES_DIRECTORY,
            CACHES_DIRECTORY,
            SETUP_SEEDS_DIRECTORY,
            TRANSACTIONS_DIRECTORY,
            TRASH_DIRECTORY,
        ] {
            ensure_managed_directory(&self.root.join(name))?;
        }
        Ok(())
    }

    fn ensure_quotas_enabled(&self) -> Result<(), StoreError> {
        let operation = "inspect storage quotas";
        let output = self
            .runner
            .run(
                &self.btrfs_program,
                &[
                    OsString::from("qgroup"),
                    OsString::from("show"),
                    OsString::from("--raw"),
                    self.root.as_os_str().to_owned(),
                ],
            )
            .map_err(|source| StoreError::CommandStart {
                operation,
                program: self.btrfs_program.clone(),
                source,
            })?;
        if output.success {
            return Ok(());
        }
        self.run_btrfs(
            "enable simple storage quotas",
            &[
                OsString::from("quota"),
                OsString::from("enable"),
                OsString::from("--simple"),
                self.root.as_os_str().to_owned(),
            ],
        )
    }

    fn run_btrfs(&self, operation: &'static str, arguments: &[OsString]) -> Result<(), StoreError> {
        let output = self
            .runner
            .run(&self.btrfs_program, arguments)
            .map_err(|source| StoreError::CommandStart {
                operation,
                program: self.btrfs_program.clone(),
                source,
            })?;
        if output.success {
            return Ok(());
        }
        let detail: String = String::from_utf8_lossy(&output.stderr)
            .trim()
            .chars()
            .take(COMMAND_DIAGNOSTIC_LIMIT)
            .collect();
        Err(StoreError::CommandFailed {
            operation,
            detail: if detail.is_empty() {
                "command exited unsuccessfully".to_owned()
            } else {
                detail
            },
        })
    }
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> StoreError {
    StoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn set_directory_owner(path: &Path, uid: u32, gid: u32) -> Result<(), StoreError> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| io_error("open fresh pod workspace", path, source))?;
    nix::unistd::fchown(
        &directory,
        Some(nix::unistd::Uid::from_raw(uid)),
        Some(nix::unistd::Gid::from_raw(gid)),
    )
    .map_err(|source| {
        io_error(
            "set fresh pod workspace ownership",
            path,
            io::Error::from_raw_os_error(source as i32),
        )
    })
}

fn check_format(path: &Path, actual: u32) -> Result<(), StoreError> {
    if actual == STORE_FORMAT_VERSION {
        Ok(())
    } else {
        Err(StoreError::UnsupportedFormat {
            path: path.to_path_buf(),
            actual,
            expected: STORE_FORMAT_VERSION,
        })
    }
}

fn path_state(path: &Path) -> Result<Option<fs::Metadata>, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(StoreError::UnsafePath(path.to_path_buf()))
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error("inspect", path, source)),
    }
}

fn ensure_managed_directory(path: &Path) -> Result<(), StoreError> {
    match path_state(path)? {
        Some(metadata) if metadata.is_dir() => Ok(()),
        Some(_) => Err(StoreError::UnsafePath(path.to_path_buf())),
        None => {
            let mut builder = DirBuilder::new();
            builder.mode(0o700);
            builder
                .create(path)
                .map_err(|source| io_error("create directory", path, source))?;
            sync_parent(path)
        }
    }
}

fn cleanup_initialization_temps(root: &Path) -> Result<(), StoreError> {
    let prefix = format!(".{STORE_MANIFEST}.tmp-");
    for entry in fs::read_dir(root).map_err(|source| io_error("read", root, source))? {
        let entry = entry.map_err(|source| io_error("read", root, source))?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(&prefix) {
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|source| io_error("inspect", &path, source))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(StoreError::UnsafePath(path));
            }
            fs::remove_file(&path).map_err(|source| io_error("remove", &path, source))?;
        }
    }
    sync_directory(root)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, StoreError> {
    let Some(metadata) = path_state(path)? else {
        return Err(io_error(
            "open",
            path,
            io::Error::new(io::ErrorKind::NotFound, "manifest does not exist"),
        ));
    };
    if !metadata.is_file() || metadata.len() > MANIFEST_SIZE_LIMIT {
        return Err(StoreError::UnsafePath(path.to_path_buf()));
    }
    let file = File::open(path).map_err(|source| io_error("open", path, source))?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(MANIFEST_SIZE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error("read", path, source))?;
    serde_json::from_slice(&bytes).map_err(|source| StoreError::Manifest {
        path: path.to_path_buf(),
        source,
    })
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| StoreError::UnsafePath(path.to_path_buf()))?;
    let metadata =
        path_state(parent)?.ok_or_else(|| StoreError::UnsafePath(parent.to_path_buf()))?;
    if !metadata.is_dir() {
        return Err(StoreError::UnsafePath(parent.to_path_buf()));
    }
    if let Some(existing) = path_state(path)?
        && !existing.is_file()
    {
        return Err(StoreError::UnsafePath(path.to_path_buf()));
    }

    let mut encoded = serde_json::to_vec(value).map_err(|source| StoreError::Manifest {
        path: path.to_path_buf(),
        source,
    })?;
    encoded.push(b'\n');
    if u64::try_from(encoded.len()).map_or(true, |length| length > MANIFEST_SIZE_LIMIT) {
        return Err(StoreError::ManifestTooLarge {
            path: path.to_path_buf(),
            limit: MANIFEST_SIZE_LIMIT,
        });
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| StoreError::UnsafePath(path.to_path_buf()))?;
    let temporary = parent.join(format!(".{file_name}.tmp-{}", unique_id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|source| io_error("create", &temporary, source))?;
        file.write_all(&encoded)
            .map_err(|source| io_error("write", &temporary, source))?;
        file.sync_all()
            .map_err(|source| io_error("sync", &temporary, source))?;
        fs::rename(&temporary, path).map_err(|source| io_error("publish", path, source))?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn remove_file_durable(path: &Path) -> Result<(), StoreError> {
    match path_state(path)? {
        None => Ok(()),
        Some(metadata) if metadata.is_file() => {
            fs::remove_file(path).map_err(|source| io_error("remove", path, source))?;
            sync_parent(path)
        }
        Some(_) => Err(StoreError::UnsafePath(path.to_path_buf())),
    }
}

fn sync_parent(path: &Path) -> Result<(), StoreError> {
    path.parent().map_or(Ok(()), sync_directory)
}

fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error("sync directory", path, source))
}

fn unique_id() -> String {
    let counter = NEXT_UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{:x}-{nanos:x}-{counter:x}", std::process::id())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::collections::VecDeque;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::symlink;
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::super::CommandOutput;
    use super::*;

    #[derive(Clone, Default)]
    struct FakeRunner {
        state: Arc<FakeRunnerState>,
    }

    #[derive(Default)]
    struct FakeRunnerState {
        commands: Mutex<Vec<String>>,
        failures: Mutex<VecDeque<String>>,
        read_only: Mutex<BTreeSet<PathBuf>>,
        quotas_enabled: Mutex<bool>,
    }

    impl FakeRunner {
        fn fail_once(&self, command_fragment: impl Into<String>) {
            self.state
                .failures
                .lock()
                .unwrap()
                .push_back(command_fragment.into());
        }

        fn commands(&self) -> Vec<String> {
            self.state.commands.lock().unwrap().clone()
        }

        fn is_read_only(&self, path: &Path) -> bool {
            self.state.read_only.lock().unwrap().contains(path)
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, _program: &Path, arguments: &[OsString]) -> io::Result<CommandOutput> {
            let rendered = arguments
                .iter()
                .map(|argument| argument.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            self.state.commands.lock().unwrap().push(rendered.clone());
            let should_fail = self
                .state
                .failures
                .lock()
                .unwrap()
                .front()
                .is_some_and(|fragment| rendered.contains(fragment));
            if should_fail {
                self.state.failures.lock().unwrap().pop_front();
                return Ok(CommandOutput::failure("injected Btrfs failure"));
            }

            let arguments = arguments.iter().map(PathBuf::from).collect::<Vec<_>>();
            match arguments.as_slice() {
                [filesystem, usage, raw, _]
                    if filesystem == Path::new("filesystem")
                        && usage == Path::new("usage")
                        && raw == Path::new("--raw") =>
                {
                    Ok(CommandOutput::success())
                }
                [filesystem, sync, _]
                    if filesystem == Path::new("filesystem") && sync == Path::new("sync") =>
                {
                    Ok(CommandOutput::success())
                }
                [qgroup, show, raw, _]
                    if qgroup == Path::new("qgroup")
                        && show == Path::new("show")
                        && raw == Path::new("--raw") =>
                {
                    if *self.state.quotas_enabled.lock().unwrap() {
                        Ok(CommandOutput::success())
                    } else {
                        Ok(CommandOutput::failure("quotas are not enabled"))
                    }
                }
                [quota, enable, simple, _]
                    if quota == Path::new("quota")
                        && enable == Path::new("enable")
                        && simple == Path::new("--simple") =>
                {
                    *self.state.quotas_enabled.lock().unwrap() = true;
                    Ok(CommandOutput::success())
                }
                [qgroup, limit, size, _]
                    if qgroup == Path::new("qgroup")
                        && limit == Path::new("limit")
                        && size == Path::new(&POD_TEMPORARY_QUOTA_BYTES.to_string())
                        && *self.state.quotas_enabled.lock().unwrap() =>
                {
                    Ok(CommandOutput::success())
                }
                [subvolume, create, path]
                    if subvolume == Path::new("subvolume") && create == Path::new("create") =>
                {
                    fs::create_dir(path)?;
                    Ok(CommandOutput::success())
                }
                [subvolume, snapshot, source, destination]
                    if subvolume == Path::new("subvolume") && snapshot == Path::new("snapshot") =>
                {
                    copy_tree(source, destination)?;
                    Ok(CommandOutput::success())
                }
                [subvolume, delete, recursive, path]
                    if subvolume == Path::new("subvolume") && delete == Path::new("delete") =>
                {
                    assert_eq!(recursive, Path::new("--recursive"));
                    fs::remove_dir_all(path)?;
                    self.state.read_only.lock().unwrap().remove(path);
                    Ok(CommandOutput::success())
                }
                [property, set, path, read_only, value]
                    if property == Path::new("property")
                        && set == Path::new("set")
                        && read_only == Path::new("ro")
                        && value == Path::new("true") =>
                {
                    self.state.read_only.lock().unwrap().insert(path.clone());
                    Ok(CommandOutput::success())
                }
                _ => Ok(CommandOutput::failure(format!(
                    "unsupported fake command: {rendered}"
                ))),
            }
        }
    }

    fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            let metadata = fs::symlink_metadata(&source_path)?;
            if metadata.is_dir() {
                copy_tree(&source_path, &destination_path)?;
            } else if metadata.is_file() {
                fs::copy(&source_path, &destination_path)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fake snapshots support only directories and files",
                ));
            }
        }
        Ok(())
    }

    fn image_id(byte: char) -> ImageId {
        ImageId::new(format!("sha256:{}", byte.to_string().repeat(64))).unwrap()
    }

    fn pod_id(value: &str) -> PodId {
        PodId::new(value).unwrap()
    }

    fn open_store(root: &Path, runner: &FakeRunner) -> BtrfsStore<FakeRunner> {
        BtrfsStore::with_runner(root, "/fake/btrfs", runner.clone()).unwrap()
    }

    fn publish_test_image(store: &BtrfsStore<FakeRunner>, image: ImageId) -> ImageGeneration {
        publish_test_image_with_config(store, image, ImageConfig::default())
    }

    fn publish_test_image_with_config(
        store: &BtrfsStore<FakeRunner>,
        image: ImageId,
        config: ImageConfig,
    ) -> ImageGeneration {
        let staging = store.begin_image().unwrap();
        fs::create_dir(staging.path().join("etc")).unwrap();
        fs::write(staging.path().join("etc/image-marker"), b"immutable").unwrap();
        store.publish_image(staging, image, config).unwrap()
    }

    fn directory_is_empty(path: &Path) -> bool {
        fs::read_dir(path).unwrap().next().is_none()
    }

    /// Verifies the store initializes and reopens only an empty real root.
    #[test]
    fn initializes_only_an_empty_real_root_and_reopens_it() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        assert_eq!(store.root(), temp.path().canonicalize().unwrap());
        assert!(temp.path().join(STORE_MANIFEST).is_file());
        assert!(runner.commands()[0].starts_with("filesystem usage --raw "));
        drop(store);
        open_store(temp.path(), &runner);

        let nonempty = TempDir::new().unwrap();
        fs::write(nonempty.path().join("unrelated"), b"keep").unwrap();
        let error =
            BtrfsStore::with_runner(nonempty.path(), "/fake/btrfs", runner.clone()).unwrap_err();
        assert!(matches!(error, StoreError::NonEmptyUninitializedStore(_)));
        assert_eq!(
            fs::read(nonempty.path().join("unrelated")).unwrap(),
            b"keep"
        );

        let parent = TempDir::new().unwrap();
        let target = parent.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = parent.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(matches!(
            BtrfsStore::with_runner(&link, "/fake/btrfs", runner),
            Err(StoreError::InvalidRoot(_))
        ));
    }

    /// Verifies the configured Btrfs executable is absolute.
    #[test]
    fn requires_an_absolute_btrfs_program() {
        let temp = TempDir::new().unwrap();
        let error =
            BtrfsStore::with_runner(temp.path(), "btrfs", FakeRunner::default()).unwrap_err();
        assert!(matches!(error, StoreError::RelativeBtrfsProgram(_)));
        assert!(directory_is_empty(temp.path()));
    }

    /// Verifies populated staging images publish atomically and read-only.
    #[test]
    fn publishes_a_populated_staging_subvolume_atomically_and_read_only() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('a');
        let staging = store.begin_image().unwrap();
        let staging_path = staging.path().to_path_buf();
        let publishing_root = store
            .publishing_path(&staging.transaction_id)
            .join(IMAGE_ROOT_DIRECTORY);
        fs::write(staging.path().join("marker"), b"rootfs").unwrap();

        let generation = store
            .publish_image(staging, image.clone(), ImageConfig::default())
            .unwrap();
        assert_eq!(generation.id(), &image);
        assert_eq!(
            fs::read(generation.root().join("marker")).unwrap(),
            b"rootfs"
        );
        assert!(!staging_path.exists());
        assert!(runner.is_read_only(&publishing_root));
        assert_eq!(store.image(&image).unwrap(), generation);
        assert_eq!(store.list_images().unwrap(), vec![generation]);
        let duplicate = store.begin_image().unwrap();
        assert!(matches!(
            store.publish_image(duplicate, image, ImageConfig::default()),
            Err(StoreError::ImageExists(_))
        ));
        assert!(directory_is_empty(
            &temp.path().join(TRANSACTIONS_DIRECTORY)
        ));
    }

    /// Verifies selected images persist and remain pinned until cleared.
    #[test]
    fn selected_image_survives_reopen_and_is_pinned_until_cleared() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('a');
        publish_test_image(&store, image.clone());

        assert_eq!(store.selected_image().unwrap(), None);
        store.select_image(&image).unwrap();
        assert_eq!(store.selected_image().unwrap(), Some(image.clone()));
        assert!(matches!(
            store.remove_image(&image),
            Err(StoreError::ImageSelected(selected)) if selected == image
        ));

        drop(store);
        let store = open_store(temp.path(), &runner);
        assert_eq!(store.selected_image().unwrap(), Some(image.clone()));
        store.clear_selected_image().unwrap();
        assert_eq!(store.selected_image().unwrap(), None);
        store.remove_image(&image).unwrap();
    }

    /// Verifies image environments remain validated across pod storage reopen.
    #[test]
    fn image_environment_is_validated_and_follows_pod_storage_across_reopen() {
        for invalid in ["NO_EQUALS", "=empty-name", "NUL=bad\0value"] {
            assert!(matches!(
                ImageConfig::new([invalid]),
                Err(StoreError::InvalidImageConfig(_))
            ));
        }
        // JSON escaping counts toward the durable manifest bound, not only
        // the unescaped process-environment bytes.
        assert!(matches!(
            ImageConfig::new([format!(
                "ESCAPED={}",
                "\\".repeat(MAX_IMAGE_CONFIG_BYTES / 2)
            )]),
            Err(StoreError::InvalidImageConfig(_))
        ));

        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('e');
        let metadata = fs::metadata(temp.path()).unwrap();
        let user = ImageUser::new("develop", metadata.uid(), metadata.gid(), [999]).unwrap();
        let config = ImageConfig::for_process(
            [
                "PATH=/image/bin",
                "HOME=/home/develop",
                "IMAGE_DEFAULT=from-dockerfile",
                "DUP=first",
                "DUP=last",
            ],
            user,
            "/workspace",
        )
        .unwrap();
        let generation = publish_test_image_with_config(&store, image.clone(), config.clone());
        assert_eq!(generation.config(), &config);
        let storage = store.create_pod(pod_id("env-pod"), &image).unwrap();
        assert_eq!(storage.image_config(), &config);
        let workspace = fs::metadata(storage.workspace()).unwrap();
        assert_eq!(
            (workspace.uid(), workspace.gid()),
            (metadata.uid(), metadata.gid())
        );
        drop(store);

        let reopened = open_store(temp.path(), &runner);
        assert_eq!(reopened.image(&image).unwrap().config(), &config);
        assert_eq!(
            reopened.pod(&pod_id("env-pod")).unwrap().image_config(),
            &config
        );
    }

    /// Verifies image users remain inside ID maps and group-count bounds.
    #[test]
    fn image_user_rejects_id_map_escape_and_excessive_groups() {
        assert!(matches!(
            ImageUser::new("x".repeat(MAX_IMAGE_USER_NAME_BYTES + 1), 0, 0, []),
            Err(StoreError::InvalidImageConfig(_))
        ));
        assert!(matches!(
            ImageUser::new("outside", ID_MAP_SIZE, 0, []),
            Err(StoreError::InvalidImageConfig(_))
        ));
        assert!(matches!(
            ImageUser::new("outside", 0, ID_MAP_SIZE, []),
            Err(StoreError::InvalidImageConfig(_))
        ));
        assert!(matches!(
            ImageUser::new("outside", 0, 0, [ID_MAP_SIZE]),
            Err(StoreError::InvalidImageConfig(_))
        ));
        assert!(matches!(
            ImageUser::new(
                "too-many-groups",
                0,
                0,
                1..=u32::try_from(MAX_IMAGE_ADDITIONAL_GIDS + 1).unwrap()
            ),
            Err(StoreError::InvalidImageConfig(_))
        ));
    }

    /// Verifies atomic manifest writes honor the reader's size limit.
    #[test]
    fn atomic_manifest_writer_enforces_the_reader_size_limit() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("oversized.json");
        let value = serde_json::json!({
            "payload": "x".repeat(usize::try_from(MANIFEST_SIZE_LIMIT).unwrap())
        });
        assert!(matches!(
            write_json_atomic(&path, &value),
            Err(StoreError::ManifestTooLarge { .. })
        ));
        assert!(!path.exists());
        assert!(directory_is_empty(temp.path()));
    }

    /// Verifies discarded or failed staging never becomes visible.
    #[test]
    fn discard_and_failed_publication_leave_no_visible_or_staged_image() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('9');

        let discarded = store.begin_image().unwrap();
        let discarded_path = discarded.path().to_path_buf();
        store.discard_image(discarded).unwrap();
        assert!(!discarded_path.exists());
        assert!(directory_is_empty(
            &temp.path().join(TRANSACTIONS_DIRECTORY)
        ));

        let failed = store.begin_image().unwrap();
        let failed_path = failed.path().to_path_buf();
        runner.fail_once("property set");
        assert!(
            store
                .publish_image(failed, image.clone(), ImageConfig::default())
                .is_err()
        );
        assert!(!failed_path.exists());
        assert!(matches!(
            store.image(&image),
            Err(StoreError::ImageNotFound(_))
        ));
        assert!(directory_is_empty(
            &temp.path().join(TRANSACTIONS_DIRECTORY)
        ));

        publish_test_image(&store, image);
    }

    /// Verifies pod roots snapshot images while mutable data uses fresh
    /// siblings.
    #[test]
    fn pod_root_is_a_snapshot_and_mutable_data_uses_fresh_siblings() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('b');
        let generation = publish_test_image(&store, image.clone());
        let pod = pod_id("pod-one");

        let storage = store.create_pod(pod.clone(), &image).unwrap();
        assert_eq!(storage.id(), &pod);
        assert_eq!(storage.image(), &image);
        assert_eq!(
            fs::read(storage.root().join("etc/image-marker")).unwrap(),
            b"immutable"
        );
        assert!(directory_is_empty(storage.workspace()));
        assert!(directory_is_empty(storage.docker()));
        assert!(directory_is_empty(storage.temporary()));
        assert_eq!(
            fs::metadata(storage.temporary())
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o1777
        );
        assert_ne!(storage.root().parent(), storage.workspace().parent());
        assert_ne!(storage.workspace().parent(), storage.docker().parent());
        assert_ne!(storage.docker().parent(), storage.temporary().parent());
        assert!(runner.commands().iter().any(|command| {
            command
                == &format!(
                    "qgroup limit {POD_TEMPORARY_QUOTA_BYTES} {}",
                    storage.temporary().display()
                )
        }));

        fs::write(storage.root().join("etc/image-marker"), b"pod mutation").unwrap();
        fs::write(storage.workspace().join("source"), b"workspace").unwrap();
        fs::write(storage.docker().join("layer"), b"docker").unwrap();
        fs::write(storage.temporary().join("transient"), b"temporary").unwrap();
        let reset = store.reset_pod_temporary(&pod).unwrap();
        assert!(directory_is_empty(reset.temporary()));
        store
            .delete_subvolume_if_exists(
                storage.temporary(),
                "simulate a pod from before temporary subvolumes",
            )
            .unwrap();
        assert!(!storage.temporary().exists());
        assert!(store.pod(&pod).unwrap().temporary().exists());
        assert_eq!(
            fs::read(generation.root().join("etc/image-marker")).unwrap(),
            b"immutable"
        );
        assert_eq!(store.pod(&pod).unwrap(), storage);
        assert_eq!(store.list_pods().unwrap(), vec![storage]);

        assert!(matches!(
            store.remove_image(&image),
            Err(StoreError::ImageInUse { .. })
        ));
        store.destroy_pod(&pod).unwrap();
        assert!(matches!(store.pod(&pod), Err(StoreError::PodNotFound(_))));
        store.remove_image(&image).unwrap();
        assert!(matches!(
            store.image(&image),
            Err(StoreError::ImageNotFound(_))
        ));
    }

    /// Verifies image seeds remain independent of golden workspace refreshes.
    #[test]
    fn image_seed_is_independent_from_golden_workspace_refreshes() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('a');
        publish_test_image(&store, image.clone());
        let setup = pod_id("setup-source");
        let storage = store.create_pod(setup.clone(), &image).unwrap();
        fs::write(storage.root().join("prepared-root"), b"yes").unwrap();
        fs::write(storage.workspace().join("prepared-workspace"), b"yes").unwrap();

        let context = image_id('f');
        let generation = store.publish_setup_seed(&setup, &context).unwrap();
        assert_eq!(
            store.setup_seed_image(&context).unwrap(),
            Some(image.clone())
        );
        assert_eq!(store.setup_seed_image(&image_id('e')).unwrap(), None);
        let seed = temp.path().join(SETUP_SEEDS_DIRECTORY).join(generation);
        assert!(runner.is_read_only(&seed.join("root")));
        assert!(runner.is_read_only(&seed.join("workspace")));
        store.destroy_pod(&setup).unwrap();

        let prepared = store.create_pod(pod_id("prepared"), &image).unwrap();
        assert_eq!(
            fs::read(prepared.root().join("prepared-root")).unwrap(),
            b"yes"
        );
        assert_eq!(
            fs::read(prepared.workspace().join("prepared-workspace")).unwrap(),
            b"yes"
        );

        let checkout = temp.path().join("checkout-refresh");
        store
            .create_subvolume(&checkout, "create test checkout")
            .unwrap();
        fs::write(checkout.join("refreshed"), b"yes").unwrap();
        store.publish_golden_workspace(&checkout).unwrap();
        assert_eq!(
            store.setup_seed_image(&context).unwrap(),
            Some(image.clone())
        );
        let refreshed = store.create_pod(pod_id("refreshed"), &image).unwrap();
        assert!(refreshed.root().join("prepared-root").exists());
        assert!(!refreshed.workspace().join("refreshed").exists());

        store
            .publish_image_workspace_seed(&image, &checkout)
            .unwrap();
        let updated = store.create_pod(pod_id("updated"), &image).unwrap();
        assert!(updated.root().join("prepared-root").exists());
        assert!(updated.workspace().join("refreshed").exists());
        assert!(!updated.workspace().join("prepared-workspace").exists());
    }

    /// Verifies setup and later workspace updates remain scoped to their
    /// canonical image while setup always starts from the golden workspace.
    #[test]
    fn images_own_independent_workspace_seeds() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let first = image_id('a');
        let second = image_id('b');

        let golden = temp.path().join("golden-source");
        store
            .create_subvolume(&golden, "create test golden workspace")
            .unwrap();
        fs::create_dir(golden.join("repository")).unwrap();
        fs::write(golden.join("repository/README.md"), b"base").unwrap();
        let golden_metadata = fs::metadata(&golden).unwrap();
        let image_config = ImageConfig {
            environment: Vec::new(),
            user: ImageUser::new(
                "test-user",
                golden_metadata.uid(),
                golden_metadata.gid(),
                [],
            )
            .unwrap(),
            working_directory: "/workspace".to_owned(),
        };
        publish_test_image_with_config(&store, first.clone(), image_config.clone());
        publish_test_image_with_config(&store, second.clone(), image_config);
        store.publish_golden_workspace(&golden).unwrap();

        let first_setup = pod_id("first-setup");
        let first_storage = store.create_setup_pod(first_setup.clone(), &first).unwrap();
        assert!(
            first_storage
                .workspace()
                .join("repository/README.md")
                .exists()
        );
        fs::write(first_storage.workspace().join("first"), b"yes").unwrap();
        store
            .publish_setup_seed(&first_setup, &image_id('c'))
            .unwrap();
        store.destroy_pod(&first_setup).unwrap();

        let second_setup = pod_id("second-setup");
        let second_storage = store
            .create_setup_pod(second_setup.clone(), &second)
            .unwrap();
        assert!(
            second_storage
                .workspace()
                .join("repository/README.md")
                .exists()
        );
        assert!(!second_storage.workspace().join("first").exists());
        fs::write(second_storage.workspace().join("second"), b"yes").unwrap();
        store
            .publish_setup_seed(&second_setup, &image_id('d'))
            .unwrap();
        store.destroy_pod(&second_setup).unwrap();

        let update = temp.path().join("first-update");
        store
            .create_subvolume(&update, "create first image update")
            .unwrap();
        fs::write(update.join("updated"), b"yes").unwrap();
        store.publish_image_workspace_seed(&first, &update).unwrap();

        let first_pod = store.create_pod(pod_id("first-pod"), &first).unwrap();
        assert!(first_pod.workspace().join("updated").exists());
        assert!(!first_pod.workspace().join("second").exists());
        let second_pod = store.create_pod(pod_id("second-pod"), &second).unwrap();
        assert!(second_pod.workspace().join("second").exists());
        assert!(!second_pod.workspace().join("updated").exists());
    }

    /// Verifies a failed workspace-seed snapshot cannot replace the image's
    /// previously published root/workspace pair.
    #[test]
    fn failed_image_workspace_update_keeps_the_previous_seed() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('a');
        publish_test_image(&store, image.clone());
        let setup = pod_id("setup");
        let setup_storage = store.create_setup_pod(setup.clone(), &image).unwrap();
        fs::write(setup_storage.workspace().join("original"), b"yes").unwrap();
        store.publish_setup_seed(&setup, &image_id('b')).unwrap();
        store.destroy_pod(&setup).unwrap();

        let update = temp.path().join("failed-update");
        store
            .create_subvolume(&update, "create failed update source")
            .unwrap();
        fs::write(update.join("replacement"), b"no").unwrap();
        runner.fail_once(update.display().to_string());
        assert!(store.publish_image_workspace_seed(&image, &update).is_err());

        let pod = store.create_pod(pod_id("after-failure"), &image).unwrap();
        assert!(pod.workspace().join("original").exists());
        assert!(!pod.workspace().join("replacement").exists());
    }

    /// Verifies workspace caches persist as idmapped mount sources.
    #[test]
    fn workspace_caches_are_persistent_idmap_sources() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let cache = store.ensure_cache("cargo-cache").unwrap();
        assert_eq!(
            cache,
            temp.path().join(CACHES_DIRECTORY).join("cargo-cache")
        );
        assert_eq!(
            fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o777
        );
        fs::write(cache.join("entry"), b"cached").unwrap();

        assert_eq!(store.ensure_cache("cargo-cache").unwrap(), cache);
        assert_eq!(fs::read(cache.join("entry")).unwrap(), b"cached");
        assert!(store.ensure_cache("../escape").is_err());
    }

    /// Verifies image users may traverse the seed container without gaining
    /// permission to enumerate it.
    #[test]
    fn image_seed_container_can_be_made_searchable() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let seeds = temp.path().join(SETUP_SEEDS_DIRECTORY);

        assert_eq!(
            fs::metadata(&seeds).unwrap().permissions().mode() & 0o777,
            0o700
        );
        store.enable_image_seed_traversal().unwrap();
        assert_eq!(
            fs::metadata(seeds).unwrap().permissions().mode() & 0o777,
            0o711
        );
    }

    /// Verifies failed pod creation rolls back every new subvolume.
    #[test]
    fn failed_pod_creation_rolls_back_every_created_subvolume() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('c');
        publish_test_image(&store, image.clone());
        let pod = pod_id("pod-rollback");
        runner.fail_once("pod-workspaces/pod-rollback");

        assert!(store.create_pod(pod.clone(), &image).is_err());
        assert!(!store.pod_root_path(&pod).exists());
        assert!(!store.pod_workspace_path(&pod).exists());
        assert!(!store.pod_docker_path(&pod).exists());
        assert!(!store.pod_temporary_path(&pod).exists());
        assert!(!store.pod_manifest_path(&pod).exists());
        assert!(directory_is_empty(
            &temp.path().join(TRANSACTIONS_DIRECTORY)
        ));
    }

    /// Verifies failed temporary quotas remove the unlimited subvolume.
    #[test]
    fn failed_temporary_quota_rolls_back_the_unlimited_subvolume() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('c');
        publish_test_image(&store, image.clone());
        let pod = pod_id("pod-quota-rollback");
        runner.fail_once("qgroup limit");

        assert!(store.create_pod(pod.clone(), &image).is_err());
        assert!(!store.pod_root_path(&pod).exists());
        assert!(!store.pod_workspace_path(&pod).exists());
        assert!(!store.pod_docker_path(&pod).exists());
        assert!(!store.pod_temporary_path(&pod).exists());
        assert!(!store.pod_manifest_path(&pod).exists());
    }

    /// Verifies startup resumes a failed creation rollback.
    #[test]
    fn startup_recovers_a_failed_creation_rollback() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('d');
        publish_test_image(&store, image.clone());
        let pod = pod_id("pod-recover-create");
        runner.fail_once("pod-workspaces/pod-recover-create");
        runner.fail_once("pod-roots/pod-recover-create");

        let error = store.create_pod(pod.clone(), &image).unwrap_err();
        assert!(matches!(error, StoreError::RollbackFailed { .. }));
        assert!(store.pod_root_path(&pod).exists());
        assert!(!directory_is_empty(
            &temp.path().join(TRANSACTIONS_DIRECTORY)
        ));
        drop(store);

        let recovered = open_store(temp.path(), &runner);
        assert!(!recovered.pod_root_path(&pod).exists());
        assert!(!recovered.pod_workspace_path(&pod).exists());
        assert!(!recovered.pod_docker_path(&pod).exists());
        assert!(!recovered.pod_temporary_path(&pod).exists());
        assert!(directory_is_empty(
            &temp.path().join(TRANSACTIONS_DIRECTORY)
        ));
    }

    /// Verifies startup completes an interrupted pod deletion.
    #[test]
    fn startup_rolls_forward_an_interrupted_pod_delete() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('e');
        publish_test_image(&store, image.clone());
        let pod = pod_id("pod-recover-delete");
        let storage = store.create_pod(pod.clone(), &image).unwrap();
        runner.fail_once("pod-docker/pod-recover-delete");

        assert!(store.destroy_pod(&pod).is_err());
        assert!(!store.pod_manifest_path(&pod).exists());
        assert!(storage.docker().exists());
        drop(store);

        let recovered = open_store(temp.path(), &runner);
        assert!(!storage.root().exists());
        assert!(!storage.workspace().exists());
        assert!(!storage.docker().exists());
        assert!(!storage.temporary().exists());
        assert!(matches!(
            recovered.pod(&pod),
            Err(StoreError::PodNotFound(_))
        ));
    }

    /// Verifies startup discards abandoned image staging.
    #[test]
    fn startup_discards_an_abandoned_image_staging_transaction() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('f');
        let staging = store.begin_image().unwrap();
        let path = staging.path().to_path_buf();
        fs::write(path.join("partial"), b"partial rootfs").unwrap();
        drop(staging);
        drop(store);

        let recovered = open_store(temp.path(), &runner);
        assert!(!path.exists());
        assert!(matches!(
            recovered.image(&image),
            Err(StoreError::ImageNotFound(_))
        ));
        assert!(directory_is_empty(
            &temp.path().join(TRANSACTIONS_DIRECTORY)
        ));
    }

    /// Verifies startup completes interrupted image removal.
    #[test]
    fn startup_finishes_an_interrupted_image_removal() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('1');
        publish_test_image(&store, image.clone());
        runner.fail_once("trash/");

        assert!(store.remove_image(&image).is_err());
        assert!(!store.image_directory(&image).exists());
        assert!(!directory_is_empty(&temp.path().join(TRASH_DIRECTORY)));
        drop(store);

        let recovered = open_store(temp.path(), &runner);
        assert!(matches!(
            recovered.image(&image),
            Err(StoreError::ImageNotFound(_))
        ));
        assert!(directory_is_empty(&temp.path().join(TRASH_DIRECTORY)));
        assert!(directory_is_empty(
            &temp.path().join(TRANSACTIONS_DIRECTORY)
        ));
    }

    /// Verifies committed images survive stale transaction metadata.
    #[test]
    fn a_committed_image_is_not_rolled_back_by_leftover_transaction_metadata() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('2');
        let generation = publish_test_image(&store, image.clone());
        let transaction_id = unique_id();
        store
            .write_transaction(&TransactionManifest {
                format_version: STORE_FORMAT_VERSION,
                transaction_id: transaction_id.clone(),
                operation: TransactionOperation::PublishImage {
                    image: image.clone(),
                },
            })
            .unwrap();
        drop(store);

        let recovered = open_store(temp.path(), &runner);
        assert_eq!(
            fs::read(generation.root().join("etc/image-marker")).unwrap(),
            b"immutable"
        );
        assert_eq!(recovered.image(&image).unwrap(), generation);
        assert!(!recovered.transaction_path(&transaction_id).exists());
    }

    /// Verifies incompatible or path-mismatched manifests fail closed.
    #[test]
    fn incompatible_and_path_mismatched_manifests_fail_closed() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        let image = image_id('3');
        publish_test_image(&store, image.clone());
        let manifest = store.image_manifest_path(&image);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        value["format_version"] = serde_json::json!(STORE_FORMAT_VERSION + 1);
        fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            store.image(&image),
            Err(StoreError::UnsupportedFormat { .. })
        ));

        value["format_version"] = serde_json::json!(STORE_FORMAT_VERSION);
        value["id"] = serde_json::json!(image_id('4').as_str());
        fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            store.image(&image),
            Err(StoreError::CorruptManifest { .. })
        ));
    }

    /// Verifies managed directory symlinks are never followed.
    #[test]
    fn managed_directory_symlinks_are_never_followed() {
        let temp = TempDir::new().unwrap();
        let runner = FakeRunner::default();
        let store = open_store(temp.path(), &runner);
        drop(store);
        let manifests = temp.path().join(POD_MANIFESTS_DIRECTORY);
        fs::remove_dir(&manifests).unwrap();
        let outside = TempDir::new().unwrap();
        symlink(outside.path(), &manifests).unwrap();

        assert!(matches!(
            BtrfsStore::with_runner(temp.path(), "/fake/btrfs", runner),
            Err(StoreError::UnsafePath(_))
        ));
        assert!(directory_is_empty(outside.path()));
    }
}
