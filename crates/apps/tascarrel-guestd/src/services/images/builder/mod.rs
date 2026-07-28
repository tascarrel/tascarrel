//! Deterministic, transactional Dockerfile image builds.
//!
//! A build runs in a fresh private `BuildKit` state directory and only
//! publishes its root filesystem after both the OCI output and the unpacked
//! filesystem have passed conservative path/type checks. The caller is
//! responsible for exposing the workspace image directory read-only and for
//! supplying `BuildKit` with guest egress.

mod accounts;
mod context;
mod executor;
mod filesystem;
mod oci;

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use accounts::normalize_image_user;
#[cfg(test)]
use accounts::normalize_image_user_with;
use accounts::normalized_image_id;
use context::hash_context;
use context::same_metadata;
#[cfg(test)]
use executor::BoundedLog;
#[cfg(test)]
use executor::BuildDaemon;
use executor::BuildExecutor;
use executor::BuildOutput;
use executor::DaemonGuard;
#[cfg(test)]
use executor::ExecutionOutput;
use executor::ProcessBuildExecutor;
#[cfg(test)]
use executor::join_log_readers;
#[cfg(test)]
use executor::spawn_log_reader;
use filesystem::TreePolicy;
use filesystem::create_private_directory;
use filesystem::ensure_empty_real_directory;
use filesystem::read_bounded_metadata;
use filesystem::real_directory;
use filesystem::real_regular_file;
use filesystem::safe_component;
use filesystem::validate_tree;
use filesystem::validate_umoci_bundle;
use filesystem::write_private_file;
use nix::errno::Errno;
use nix::sys::signal::Signal;
use nix::sys::signal::kill;
use nix::unistd::Gid;
use nix::unistd::Pid;
use nix::unistd::Uid;
use nix::unistd::fchown;
#[cfg(test)]
use oci::LayerCompression;
#[cfg(test)]
use oci::configured_user_from_oci_image;
use oci::image_config_from_umoci_bundle;
use oci::image_from_oci_layout;
#[cfg(test)]
use oci::layer_compression;
#[cfg(test)]
use oci::normalize_archive_path;
#[cfg(test)]
use oci::sha256_blob_path;
#[cfg(test)]
use oci::validate_layer_blob;
#[cfg(test)]
use oci::validate_layer_tar;
use oci::validate_oci_archive;
#[cfg(test)]
use oci::validate_oci_layer_ownership;
use oci::validate_oci_layout;
use rustix::fs::Mode;
use rustix::fs::OFlags;
use rustix::fs::open;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use tempfile::Builder as TempDirBuilder;
use tempfile::TempDir;
use thiserror::Error;

use crate::runtime::pod::BtrfsStore;
use crate::runtime::pod::CommandRunner;
use crate::runtime::pod::ID_MAP_SIZE;
use crate::runtime::pod::ImageConfig;
use crate::runtime::pod::ImageGeneration;
use crate::runtime::pod::ImageId;
use crate::runtime::pod::ImageUser;
use crate::runtime::pod::StoreError;

const HASH_DOMAIN: &[u8] = b"tascarrel-dockerfile-context-v1\0";
const DOCKERFILE: &str = "Dockerfile";
const BUILD_DIRECTORY_PREFIX: &str = "tascarrel-image-build-";
const BUILD_DIRECTORY_RANDOM_LEN: usize = 12;
const OCI_EXPORT_SUFFIX: &str = ",name=tascarrel:latest";
const OCI_EXPORT_TAG_SUFFIX: &str = ":latest";
const READ_BUFFER_SIZE: usize = 64 * 1024;
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(20);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_CONFIGURED_TIMEOUT: Duration = Duration::from_hours(24);
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 100;
const MAX_SYMLINK_TARGET_BYTES: usize = 4096;
const MAX_OCI_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ACCOUNT_DATABASE_BYTES: u64 = 4 * 1024 * 1024;
const DEVELOPMENT_USER_ID: u32 = 1000;
const DEVELOPMENT_USER_NAME: &str = "develop";
const DEVELOPMENT_USER_HOME: &str = "/home/develop";
const NORMALIZED_IMAGE_ALGORITHM: &str = "tascarrel-user-v2";
const NORMALIZED_IMAGE_HASH_DOMAIN: &[u8] = b"tascarrel-image-user-v2\0";
const OCI_IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_IMAGE_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";
const OCI_LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar";
const OCI_NONDISTRIBUTABLE_LAYER_MEDIA_TYPE: &str =
    "application/vnd.oci.image.layer.nondistributable.v1.tar";

/// Resource and filesystem limits applied to one image build.
#[derive(Clone, Debug)]
pub struct ImageBuildLimits {
    /// Maximum number of files and directories in the input context.
    pub max_context_entries: u64,
    /// Maximum aggregate regular-file bytes in the input context.
    pub max_context_bytes: u64,
    /// Maximum depth below the input context root.
    pub max_context_depth: usize,
    /// Maximum size of `BuildKit`'s OCI archive.
    pub max_oci_archive_bytes: u64,
    /// Maximum number of filesystem entries in either build output tree.
    pub max_output_entries: u64,
    /// Maximum aggregate regular-file bytes in either build output tree.
    pub max_output_bytes: u64,
    /// Maximum depth below an output tree root.
    pub max_output_depth: usize,
    /// Maximum diagnostic bytes retained from each child process.
    pub diagnostic_bytes: usize,
    /// Maximum number of concurrent `BuildKit` build steps.
    pub buildkit_parallelism: u16,
    /// `BuildKit`'s private garbage-collection storage target, in MiB.
    pub buildkit_keep_storage_mib: u32,
}

impl Default for ImageBuildLimits {
    fn default() -> Self {
        Self {
            max_context_entries: 100_000,
            max_context_bytes: 8 * 1024 * 1024 * 1024,
            max_context_depth: 128,
            max_oci_archive_bytes: 16 * 1024 * 1024 * 1024,
            max_output_entries: 1_000_000,
            max_output_bytes: 32 * 1024 * 1024 * 1024,
            max_output_depth: 256,
            diagnostic_bytes: 16 * 1024,
            buildkit_parallelism: 4,
            buildkit_keep_storage_mib: 2048,
        }
    }
}

/// Absolute tool paths and timeouts for guest-side image builds.
#[derive(Clone, Debug)]
pub struct ImageBuilderConfig {
    /// Absolute path to `buildkitd`.
    pub buildkitd: PathBuf,
    /// Absolute path to util-linux's `nsenter` command. Joining only the
    /// network namespace preserves the guest cgroup mount needed by runc.
    pub nsenter: PathBuf,
    /// Optional named network namespace shared by `buildkitd`, its workers,
    /// and `buildctl`. The client and daemon still communicate over their
    /// private filesystem-backed Unix socket.
    pub network_namespace: Option<String>,
    /// Absolute path to `buildctl`.
    pub buildctl: PathBuf,
    /// Absolute path to `umoci`.
    pub umoci: PathBuf,
    /// Absolute path to GNU `tar`.
    pub tar: PathBuf,
    /// Absolute path to GNU `cp`.
    pub cp: PathBuf,
    /// Existing, real directory under which private build directories live.
    pub temporary_root: PathBuf,
    /// Maximum time to wait for `BuildKit`'s private Unix socket.
    pub daemon_startup_timeout: Duration,
    /// Maximum time allowed for the Dockerfile build itself.
    pub build_timeout: Duration,
    /// Maximum time allowed for each unpack/copy command.
    pub helper_timeout: Duration,
    /// Grace period for terminating `BuildKit` before it is killed.
    pub daemon_shutdown_timeout: Duration,
    /// Resource and filesystem bounds.
    pub limits: ImageBuildLimits,
}

impl ImageBuilderConfig {
    /// Creates a configuration with conservative production defaults.
    #[must_use]
    pub fn new(
        buildkitd: impl Into<PathBuf>,
        buildctl: impl Into<PathBuf>,
        umoci: impl Into<PathBuf>,
        tar: impl Into<PathBuf>,
        cp: impl Into<PathBuf>,
    ) -> Self {
        Self {
            buildkitd: buildkitd.into(),
            nsenter: PathBuf::from("/run/current-system/sw/bin/nsenter"),
            network_namespace: None,
            buildctl: buildctl.into(),
            umoci: umoci.into(),
            tar: tar.into(),
            cp: cp.into(),
            temporary_root: PathBuf::from("/tmp"),
            daemon_startup_timeout: Duration::from_secs(20),
            build_timeout: Duration::from_mins(30),
            helper_timeout: Duration::from_mins(5),
            daemon_shutdown_timeout: Duration::from_secs(10),
            limits: ImageBuildLimits::default(),
        }
    }
}

/// Result of resolving a Dockerfile context to an immutable image generation.
#[derive(Clone, Debug)]
pub struct ImageBuildOutcome {
    generation: ImageGeneration,
    reused: bool,
}

#[derive(Debug)]
struct BuiltImage {
    id: ImageId,
    config: ImageConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PasswdAccount {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GroupAccount {
    name: String,
    gid: u32,
    members: Vec<String>,
}

#[derive(Deserialize)]
struct OciIndex {
    manifests: Vec<OciDescriptor>,
}

#[derive(Deserialize)]
struct OciDescriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct OciManifest {
    config: OciLayerDescriptor,
    layers: Vec<OciLayerDescriptor>,
}

#[derive(Deserialize)]
struct OciLayerDescriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
}

#[derive(Deserialize)]
struct UmociConfiguration {
    process: UmociProcess,
}

#[derive(Deserialize)]
struct UmociProcess {
    #[serde(default)]
    env: Vec<String>,
    cwd: String,
    user: UmociUser,
}

#[derive(Deserialize)]
struct UmociUser {
    uid: u32,
    gid: u32,
    #[serde(default, rename = "additionalGids")]
    additional_gids: Vec<u32>,
}

#[derive(Deserialize)]
struct OciImageConfiguration {
    #[serde(default)]
    config: Option<OciImageDefaults>,
}

#[derive(Default, Deserialize)]
struct OciImageDefaults {
    #[serde(default, rename = "User")]
    user: Option<String>,
}

struct ValidatedOciImage {
    id: ImageId,
    configured_user: String,
}

impl ImageBuildOutcome {
    /// The immutable generation selected or built for the context.
    #[must_use]
    pub const fn generation(&self) -> &ImageGeneration {
        &self.generation
    }

    /// Whether an already-published generation was reused.
    #[must_use]
    pub const fn reused(&self) -> bool {
        self.reused
    }

    /// Consumes the outcome and returns its generation.
    #[must_use]
    pub fn into_generation(self) -> ImageGeneration {
        self.generation
    }
}

/// A rejected image context, command, or transactional image build.
#[derive(Debug, Error)]
pub enum ImageBuildError {
    /// A configured executable or temporary root is not an absolute,
    /// normalized path.
    #[error("invalid configured {name} path {path}: {reason}")]
    InvalidConfiguredPath {
        /// Configuration field name.
        name: &'static str,
        /// Rejected path.
        path: PathBuf,
        /// Rejection reason.
        reason: &'static str,
    },
    /// One of the configured limits cannot safely bound a build.
    #[error("invalid image build limit: {0}")]
    InvalidLimit(&'static str),
    /// The image context is missing, mutable while read, or contains an unsafe
    /// entry.
    #[error("unsafe image context {path}: {reason}")]
    UnsafeContext {
        /// Affected context path.
        path: PathBuf,
        /// Rejection reason.
        reason: &'static str,
    },
    /// The context exceeded a configured size, count, or depth bound.
    #[error("image context limit exceeded at {path}: {limit}")]
    ContextLimit {
        /// Entry being inspected when the limit was exceeded.
        path: PathBuf,
        /// Limit description.
        limit: &'static str,
    },
    /// Build output contains an unsafe path or file type.
    #[error("unsafe {kind} output {path}: {reason}")]
    UnsafeOutput {
        /// Which output was being checked.
        kind: &'static str,
        /// Rejected path.
        path: PathBuf,
        /// Rejection reason.
        reason: &'static str,
    },
    /// Build output exceeded a configured size, count, or depth bound.
    #[error("{kind} output limit exceeded at {path}: {limit}")]
    OutputLimit {
        /// Which output was being checked.
        kind: &'static str,
        /// Entry being inspected when the limit was exceeded.
        path: PathBuf,
        /// Limit description.
        limit: &'static str,
    },
    /// A filesystem operation failed.
    #[error("could not {operation} {path}: {source}")]
    Io {
        /// Logical operation.
        operation: &'static str,
        /// Affected path.
        path: PathBuf,
        /// Underlying failure.
        #[source]
        source: io::Error,
    },
    /// An external command could not be started or waited for.
    #[error("could not run {operation} with {program}: {source}")]
    CommandIo {
        /// Logical command operation.
        operation: &'static str,
        /// Absolute executable path.
        program: PathBuf,
        /// Underlying process failure.
        #[source]
        source: io::Error,
    },
    /// An external command exceeded its deadline.
    #[error("{operation} timed out after {timeout:?}: {diagnostic}")]
    CommandTimedOut {
        /// Logical command operation.
        operation: &'static str,
        /// Configured deadline.
        timeout: Duration,
        /// Bounded command diagnostic.
        diagnostic: String,
    },
    /// An external command exited unsuccessfully.
    #[error("{operation} failed: {diagnostic}")]
    CommandFailed {
        /// Logical command operation.
        operation: &'static str,
        /// Bounded command diagnostic.
        diagnostic: String,
    },
    /// `BuildKit` exited before its private socket was ready.
    #[error("BuildKit exited before becoming ready: {diagnostic}")]
    DaemonExited {
        /// Bounded daemon diagnostic.
        diagnostic: String,
    },
    /// `BuildKit` did not create its private socket before the deadline.
    #[error("BuildKit did not become ready after {timeout:?}: {diagnostic}")]
    DaemonStartupTimedOut {
        /// Configured deadline.
        timeout: Duration,
        /// Bounded daemon diagnostic.
        diagnostic: String,
    },
    /// The read-only context changed between hashing and publication.
    #[error("image context changed while the build was in progress")]
    ContextChanged,
    /// OCI process defaults cannot be represented safely by the pod runtime.
    #[error("invalid OCI image configuration: {0}")]
    InvalidImageConfig(String),
    /// A store operation failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The build and cleanup both failed. Durable store recovery can retry a
    /// staging cleanup which could not complete immediately.
    #[error("image build failed: {cause}; cleanup also failed: {cleanup}")]
    CleanupFailed {
        /// Original failure.
        cause: String,
        /// Cleanup failure.
        cleanup: String,
    },
}

/// Removes private per-build directories left by an interrupted guest daemon.
///
/// Only directory names which exactly match the name shape generated by this
/// module are removed. Files, symlinks, and unrelated directory names are left
/// untouched.
///
/// # Errors
///
/// Returns an error when `root` is not a real directory, it cannot be read, or
/// a matching stale directory cannot be removed durably.
pub fn cleanup_stale_image_build_directories(
    root: impl AsRef<Path>,
) -> Result<usize, ImageBuildError> {
    let root = root.as_ref();
    let metadata = fs::symlink_metadata(root).map_err(|source| ImageBuildError::Io {
        operation: "inspect image-build temporary root",
        path: root.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ImageBuildError::InvalidConfiguredPath {
            name: "temporary root",
            path: root.to_path_buf(),
            reason: "must be an existing real directory",
        });
    }

    let entries = fs::read_dir(root).map_err(|source| ImageBuildError::Io {
        operation: "read image-build temporary root",
        path: root.to_path_buf(),
        source,
    })?;
    let mut removed = 0usize;
    for entry in entries {
        let entry = entry.map_err(|source| ImageBuildError::Io {
            operation: "read image-build temporary root entry",
            path: root.to_path_buf(),
            source,
        })?;
        if !is_image_build_directory_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| ImageBuildError::Io {
            operation: "inspect stale image-build directory",
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        fs::remove_dir_all(&path).map_err(|source| ImageBuildError::Io {
            operation: "remove stale image-build directory",
            path,
            source,
        })?;
        removed += 1;
    }
    if removed != 0 {
        File::open(root)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| ImageBuildError::Io {
                operation: "sync image-build temporary root",
                path: root.to_path_buf(),
                source,
            })?;
    }
    Ok(removed)
}

fn is_image_build_directory_name(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    let Some(suffix) = bytes.strip_prefix(BUILD_DIRECTORY_PREFIX.as_bytes()) else {
        return false;
    };
    suffix.len() == BUILD_DIRECTORY_RANDOM_LEN && suffix.iter().all(u8::is_ascii_alphanumeric)
}

/// Guest-side Dockerfile-to-Btrfs image builder.
pub struct ImageBuilder {
    config: ImageBuilderConfig,
    executor: Arc<dyn BuildExecutor>,
}

impl std::fmt::Debug for ImageBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageBuilder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ImageBuilder {
    /// Validates a production builder configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for relative/non-normalized tool paths, an unsafe
    /// temporary root, zero timeouts, or ineffective limits.
    pub fn new(config: ImageBuilderConfig) -> Result<Self, ImageBuildError> {
        let diagnostic_limit = config.limits.diagnostic_bytes;
        Self::with_executor(config, Arc::new(ProcessBuildExecutor { diagnostic_limit }))
    }

    fn with_executor(
        config: ImageBuilderConfig,
        executor: Arc<dyn BuildExecutor>,
    ) -> Result<Self, ImageBuildError> {
        validate_config(&config)?;
        Ok(Self { config, executor })
    }

    /// Computes a stable fingerprint for a safe input context.
    ///
    /// The digest includes relative path bytes, entry type, portable mode
    /// bits, file size, and file content. It intentionally excludes mtimes,
    /// ownership, and directory enumeration order.
    ///
    /// # Errors
    ///
    /// Returns an error when `context` or its root `Dockerfile` is unsafe,
    /// changes while read, contains links/special files, crosses filesystems,
    /// or exceeds configured limits.
    pub fn context_digest(&self, context: impl AsRef<Path>) -> Result<ImageId, ImageBuildError> {
        hash_context(context.as_ref(), &self.config.limits).map(|snapshot| snapshot.image)
    }

    /// Reuses or transactionally builds and publishes an image generation.
    ///
    /// The input directory must be mounted read-only by the caller. It must
    /// contain a regular root-level `Dockerfile`. `BuildKit` network access is
    /// deliberately not configured here; guest egress supplies it.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe context/output data, process failures,
    /// deadlines, concurrent publication, or Btrfs store failures. An
    /// unpublished staging subvolume is discarded on every ordinary error.
    pub fn build<R: CommandRunner>(
        &self,
        store: &BtrfsStore<R>,
        context: impl AsRef<Path>,
    ) -> Result<ImageBuildOutcome, ImageBuildError> {
        self.build_inner(store, context.as_ref(), None)
    }

    /// Reuses or builds an image while streaming BuildKit/build-client output.
    ///
    /// The observer can be called concurrently for standard output and
    /// standard error. It must return promptly so the child process cannot be
    /// blocked by its output pipes.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`ImageBuilder::build`].
    pub(crate) fn build_with_output<R, F>(
        &self,
        store: &BtrfsStore<R>,
        context: impl AsRef<Path>,
        output: F,
    ) -> Result<ImageBuildOutcome, ImageBuildError>
    where
        R: CommandRunner,
        F: Fn(&[u8]) + Send + Sync + 'static,
    {
        self.build_inner(store, context.as_ref(), Some(Arc::new(output)))
    }

    fn build_inner<R: CommandRunner>(
        &self,
        store: &BtrfsStore<R>,
        context: &Path,
        output: Option<Arc<BuildOutput>>,
    ) -> Result<ImageBuildOutcome, ImageBuildError> {
        let snapshot = hash_context(context, &self.config.limits)?;
        // A context fingerprint cannot identify a build: mutable base-image
        // tags, the selected platform, and builder changes can all produce a
        // different OCI result from identical Dockerfile bytes. Always resolve
        // the build, then deduplicate by its verified OCI manifest digest.
        let staging = store.begin_image()?;

        let build_result = self.populate_staging(&snapshot.root, staging.path(), output);
        let build_result = match build_result {
            Ok(built) => match hash_context(&snapshot.root, &self.config.limits) {
                Ok(after) if after.image == snapshot.image => Ok(built),
                Ok(_) => Err(ImageBuildError::ContextChanged),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };

        let built = match build_result {
            Ok(built) => built,
            Err(cause) => {
                return match store.discard_image(staging) {
                    Ok(()) => Err(cause),
                    Err(cleanup) => Err(ImageBuildError::CleanupFailed {
                        cause: cause.to_string(),
                        cleanup: cleanup.to_string(),
                    }),
                };
            }
        };
        match store.publish_image(staging, built.id.clone(), built.config) {
            Ok(generation) => Ok(ImageBuildOutcome {
                generation,
                reused: false,
            }),
            Err(StoreError::ImageExists(_)) => Ok(ImageBuildOutcome {
                generation: store.image(&built.id)?,
                reused: true,
            }),
            Err(error) => Err(error.into()),
        }
    }

    fn populate_staging(
        &self,
        context: &Path,
        staging: &Path,
        output: Option<Arc<BuildOutput>>,
    ) -> Result<BuiltImage, ImageBuildError> {
        ensure_empty_real_directory(staging, "image staging")?;
        let temporary = self.create_temporary_directory()?;
        let result = self.populate_staging_in(context, staging, temporary.path(), output);
        let cleanup = temporary.close().map_err(|source| ImageBuildError::Io {
            operation: "remove private build directory",
            path: self.config.temporary_root.clone(),
            source,
        });
        match (result, cleanup) {
            (Ok(built), Ok(())) => Ok(built),
            (Err(cause), Ok(())) => Err(cause),
            (Ok(_), Err(cleanup)) => Err(cleanup),
            (Err(cause), Err(cleanup)) => Err(ImageBuildError::CleanupFailed {
                cause: cause.to_string(),
                cleanup: cleanup.to_string(),
            }),
        }
    }

    fn create_temporary_directory(&self) -> Result<TempDir, ImageBuildError> {
        let metadata = fs::symlink_metadata(&self.config.temporary_root).map_err(|source| {
            ImageBuildError::Io {
                operation: "reinspect temporary root",
                path: self.config.temporary_root.clone(),
                source,
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ImageBuildError::InvalidConfiguredPath {
                name: "temporary root",
                path: self.config.temporary_root.clone(),
                reason: "was replaced after builder initialization",
            });
        }
        TempDirBuilder::new()
            .prefix(BUILD_DIRECTORY_PREFIX)
            .rand_bytes(BUILD_DIRECTORY_RANDOM_LEN)
            .tempdir_in(&self.config.temporary_root)
            .map_err(|source| ImageBuildError::Io {
                operation: "create private build directory",
                path: self.config.temporary_root.clone(),
                source,
            })
    }

    #[allow(clippy::too_many_lines)] // A linear pipeline keeps cleanup boundaries explicit.
    fn populate_staging_in(
        &self,
        context: &Path,
        staging: &Path,
        temporary: &Path,
        output: Option<Arc<BuildOutput>>,
    ) -> Result<BuiltImage, ImageBuildError> {
        let buildkit_root = temporary.join("buildkit");
        let buildkit_config = temporary.join("buildkitd.toml");
        let socket = temporary.join("buildkitd.sock");
        let archive = temporary.join("image.oci.tar");
        let layout = temporary.join("oci-layout");
        let bundle = temporary.join("bundle");
        create_private_directory(&buildkit_root)?;
        create_private_directory(&layout)?;
        write_private_file(
            &buildkit_config,
            b"# Private per-build Tascarrel configuration.\n",
        )?;
        validate_socket_path(&socket)?;

        let address = prefixed_os_string("unix://", &socket);
        let daemon_arguments = vec![
            OsString::from("--config"),
            buildkit_config.as_os_str().to_owned(),
            OsString::from("--addr"),
            address.clone(),
            OsString::from("--root"),
            buildkit_root.as_os_str().to_owned(),
            OsString::from("--containerd-worker=false"),
            OsString::from("--oci-worker=true"),
            // `host` is relative to buildkitd's namespace. The workspace image
            // provider launches the daemon in a dedicated netns, so pulls and
            // every RUN user share one veth-attributed egress identity.
            OsString::from("--oci-worker-net=host"),
            OsString::from("--oci-worker-gc=true"),
            OsString::from(format!(
                "--oci-worker-gc-keepstorage={0},{0},{0}",
                self.config.limits.buildkit_keep_storage_mib
            )),
            OsString::from(format!(
                "--oci-max-parallelism={}",
                self.config.limits.buildkit_parallelism
            )),
        ];
        let (daemon_program, daemon_arguments) =
            self.command_in_network_namespace(&self.config.buildkitd, daemon_arguments);
        let daemon = self
            .executor
            .spawn_daemon(&daemon_program, &daemon_arguments, output.clone())
            .map_err(|source| ImageBuildError::CommandIo {
                operation: "start BuildKit",
                program: daemon_program,
                source,
            })?;
        let mut daemon = DaemonGuard::new(daemon, self.config.daemon_shutdown_timeout);
        self.wait_for_daemon(&mut daemon, &socket)?;

        let oci_output = comma_attribute("type=oci,dest=", &archive, OCI_EXPORT_SUFFIX)?;
        let context_local = prefixed_os_string("context=", context);
        let dockerfile_local = prefixed_os_string("dockerfile=", context);
        let build_arguments = vec![
            OsString::from("--debug"),
            // BuildKit 0.31 applies buildctl's five-second connection timeout
            // while resolving its client. Disable that deadline: the process
            // executor independently enforces build_timeout and daemon socket
            // readiness is separately bounded.
            OsString::from("--timeout"),
            OsString::from("0"),
            OsString::from("--addr"),
            address,
            OsString::from("build"),
            OsString::from("--frontend"),
            OsString::from("dockerfile.v0"),
            OsString::from("--local"),
            context_local,
            OsString::from("--local"),
            dockerfile_local,
            OsString::from("--opt"),
            OsString::from("filename=Dockerfile"),
            OsString::from("--output"),
            oci_output,
        ];
        let (build_program, build_arguments) =
            self.command_in_network_namespace(&self.config.buildctl, build_arguments);
        if let Err(error) = self.run_checked(
            "build Dockerfile",
            &build_program,
            &build_arguments,
            self.config.build_timeout,
            output,
        ) {
            return Err(add_daemon_diagnostic(
                error,
                &bounded_bytes(&daemon.diagnostics(), self.config.limits.diagnostic_bytes),
            ));
        }
        daemon
            .shutdown()
            .map_err(|source| ImageBuildError::CommandIo {
                operation: "stop BuildKit",
                program: self.config.buildkitd.clone(),
                source,
            })?;

        validate_oci_archive(&archive, &self.config.limits)?;
        let tar_arguments = vec![
            OsString::from("--extract"),
            OsString::from("--file"),
            archive.as_os_str().to_owned(),
            OsString::from("--directory"),
            layout.as_os_str().to_owned(),
            OsString::from("--no-same-owner"),
            OsString::from("--no-same-permissions"),
            OsString::from("--no-overwrite-dir"),
            OsString::from("--delay-directory-restore"),
        ];
        self.run_checked(
            "extract OCI layout",
            &self.config.tar,
            &tar_arguments,
            self.config.helper_timeout,
            None,
        )?;
        validate_oci_layout(&layout, &self.config.limits)?;
        let image = image_from_oci_layout(&layout, &self.config.limits)?;

        let image_reference = suffixed_os_string(&layout, OCI_EXPORT_TAG_SUFFIX);
        let identity_map = OsString::from(format!("0:0:{ID_MAP_SIZE}"));
        let umoci_arguments = vec![
            OsString::from("unpack"),
            OsString::from("--uid-map"),
            identity_map.clone(),
            OsString::from("--gid-map"),
            identity_map,
            OsString::from("--image"),
            image_reference,
            bundle.as_os_str().to_owned(),
        ];
        self.run_checked(
            "unpack OCI image",
            &self.config.umoci,
            &umoci_arguments,
            self.config.helper_timeout,
            None,
        )?;
        let rootfs = validate_umoci_bundle(&bundle, &self.config.limits)?;
        let image_config = image_config_from_umoci_bundle(&bundle, &image.configured_user)?;

        let copy_source = rootfs.join(".");
        let copy_arguments = vec![
            OsString::from("--archive"),
            OsString::from("--no-dereference"),
            OsString::from("--one-file-system"),
            OsString::from("--"),
            copy_source.as_os_str().to_owned(),
            staging.as_os_str().to_owned(),
        ];
        self.run_checked(
            "copy image root filesystem",
            &self.config.cp,
            &copy_arguments,
            self.config.helper_timeout,
            None,
        )?;
        validate_tree(
            staging,
            TreePolicy {
                kind: "staged root filesystem",
                allow_symlinks: true,
                allow_hardlinks: true,
                require_mapped_ownership: true,
            },
            &self.config.limits,
        )?;
        let (image_config, normalized) = normalize_image_user(staging, image_config)?;
        validate_tree(
            staging,
            TreePolicy {
                kind: "normalized root filesystem",
                allow_symlinks: true,
                allow_hardlinks: true,
                require_mapped_ownership: true,
            },
            &self.config.limits,
        )?;
        Ok(BuiltImage {
            id: if normalized {
                normalized_image_id(&image.id)?
            } else {
                image.id
            },
            config: image_config,
        })
    }

    fn command_in_network_namespace(
        &self,
        program: &Path,
        arguments: Vec<OsString>,
    ) -> (PathBuf, Vec<OsString>) {
        let Some(namespace) = &self.config.network_namespace else {
            return (program.to_path_buf(), arguments);
        };
        let namespace_path = format!("--net=/run/netns/{namespace}");
        let mut wrapped = Vec::with_capacity(arguments.len() + 3);
        wrapped.extend([
            OsString::from(namespace_path),
            OsString::from("--"),
            program.as_os_str().to_owned(),
        ]);
        wrapped.extend(arguments);
        (self.config.nsenter.clone(), wrapped)
    }

    fn wait_for_daemon(
        &self,
        daemon: &mut DaemonGuard,
        socket: &Path,
    ) -> Result<(), ImageBuildError> {
        let deadline = Instant::now() + self.config.daemon_startup_timeout;
        loop {
            match fs::symlink_metadata(socket) {
                Ok(metadata) if metadata.file_type().is_socket() => return Ok(()),
                Ok(_) => {
                    return Err(ImageBuildError::UnsafeOutput {
                        kind: "BuildKit socket",
                        path: socket.to_path_buf(),
                        reason: "path is not a Unix socket",
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(ImageBuildError::Io {
                        operation: "inspect BuildKit socket",
                        path: socket.to_path_buf(),
                        source,
                    });
                }
            }
            if daemon
                .try_wait()
                .map_err(|source| ImageBuildError::CommandIo {
                    operation: "wait for BuildKit",
                    program: self.config.buildkitd.clone(),
                    source,
                })?
                .is_some()
            {
                return Err(ImageBuildError::DaemonExited {
                    diagnostic: diagnostic_or_default(&bounded_bytes(
                        &daemon.diagnostics(),
                        self.config.limits.diagnostic_bytes,
                    )),
                });
            }
            if Instant::now() >= deadline {
                return Err(ImageBuildError::DaemonStartupTimedOut {
                    timeout: self.config.daemon_startup_timeout,
                    diagnostic: diagnostic_or_default(&bounded_bytes(
                        &daemon.diagnostics(),
                        self.config.limits.diagnostic_bytes,
                    )),
                });
            }
            thread::sleep(SOCKET_POLL_INTERVAL);
        }
    }

    fn run_checked(
        &self,
        operation: &'static str,
        program: &Path,
        arguments: &[OsString],
        timeout: Duration,
        output: Option<Arc<BuildOutput>>,
    ) -> Result<(), ImageBuildError> {
        let output = self
            .executor
            .run(program, arguments, timeout, output)
            .map_err(|source| ImageBuildError::CommandIo {
                operation,
                program: program.to_path_buf(),
                source,
            })?;
        let diagnostic = diagnostic_or_default(&bounded_bytes(
            &output.diagnostic,
            self.config.limits.diagnostic_bytes,
        ));
        if output.timed_out {
            Err(ImageBuildError::CommandTimedOut {
                operation,
                timeout,
                diagnostic,
            })
        } else if !output.success {
            Err(ImageBuildError::CommandFailed {
                operation,
                diagnostic,
            })
        } else {
            Ok(())
        }
    }
}

fn validate_config(config: &ImageBuilderConfig) -> Result<(), ImageBuildError> {
    for (name, path) in [
        ("buildkitd", &config.buildkitd),
        ("nsenter", &config.nsenter),
        ("buildctl", &config.buildctl),
        ("umoci", &config.umoci),
        ("tar", &config.tar),
        ("cp", &config.cp),
        ("temporary root", &config.temporary_root),
    ] {
        validate_absolute_normal_path(name, path)?;
    }
    if let Some(namespace) = &config.network_namespace
        && (namespace.is_empty()
            || namespace.len() > 63
            || !namespace
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    {
        return Err(ImageBuildError::InvalidLimit(
            "network namespace name must contain only ASCII letters, digits, '_' or '-'",
        ));
    }
    let metadata =
        fs::symlink_metadata(&config.temporary_root).map_err(|source| ImageBuildError::Io {
            operation: "inspect temporary root",
            path: config.temporary_root.clone(),
            source,
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ImageBuildError::InvalidConfiguredPath {
            name: "temporary root",
            path: config.temporary_root.clone(),
            reason: "must be an existing real directory",
        });
    }
    if config
        .temporary_root
        .as_os_str()
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b',' | b'\n' | b'\r'))
    {
        return Err(ImageBuildError::InvalidConfiguredPath {
            name: "temporary root",
            path: config.temporary_root.clone(),
            reason: "must not contain comma or newline bytes used by BuildKit attributes",
        });
    }
    if config.daemon_startup_timeout.is_zero()
        || config.build_timeout.is_zero()
        || config.helper_timeout.is_zero()
        || config.daemon_shutdown_timeout.is_zero()
    {
        return Err(ImageBuildError::InvalidLimit("timeouts must be non-zero"));
    }
    if config.daemon_startup_timeout > MAX_CONFIGURED_TIMEOUT
        || config.build_timeout > MAX_CONFIGURED_TIMEOUT
        || config.helper_timeout > MAX_CONFIGURED_TIMEOUT
        || config.daemon_shutdown_timeout > MAX_CONFIGURED_TIMEOUT
    {
        return Err(ImageBuildError::InvalidLimit(
            "timeouts must not exceed 24 hours",
        ));
    }
    let limits = &config.limits;
    if limits.max_context_entries == 0
        || limits.max_context_bytes == 0
        || limits.max_context_depth == 0
        || limits.max_oci_archive_bytes == 0
        || limits.max_output_entries == 0
        || limits.max_output_bytes == 0
        || limits.max_output_depth == 0
        || limits.diagnostic_bytes == 0
        || limits.buildkit_parallelism == 0
        || limits.buildkit_keep_storage_mib == 0
    {
        return Err(ImageBuildError::InvalidLimit(
            "count, byte, depth, diagnostic, and BuildKit bounds must be non-zero",
        ));
    }
    Ok(())
}

fn validate_absolute_normal_path(name: &'static str, path: &Path) -> Result<(), ImageBuildError> {
    if !path.is_absolute() {
        return Err(ImageBuildError::InvalidConfiguredPath {
            name,
            path: path.to_path_buf(),
            reason: "must be absolute",
        });
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(ImageBuildError::InvalidConfiguredPath {
            name,
            path: path.to_path_buf(),
            reason: "must not contain `.` or `..` components",
        });
    }
    Ok(())
}

fn validate_socket_path(path: &Path) -> Result<(), ImageBuildError> {
    if path.as_os_str().as_bytes().len() > MAX_UNIX_SOCKET_PATH_BYTES {
        return Err(ImageBuildError::UnsafeOutput {
            kind: "BuildKit socket",
            path: path.to_path_buf(),
            reason: "Unix socket path is too long",
        });
    }
    Ok(())
}

fn prefixed_os_string(prefix: &str, path: &Path) -> OsString {
    let mut value = OsString::from(prefix);
    value.push(path);
    value
}

fn suffixed_os_string(path: &Path, suffix: &str) -> OsString {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    value
}

fn comma_attribute(prefix: &str, path: &Path, suffix: &str) -> Result<OsString, ImageBuildError> {
    if path
        .as_os_str()
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b',' | b'\n' | b'\r'))
    {
        return Err(ImageBuildError::UnsafeOutput {
            kind: "BuildKit output path",
            path: path.to_path_buf(),
            reason: "path cannot be represented in a BuildKit CSV attribute",
        });
    }
    let mut value = OsString::from(prefix);
    value.push(path);
    value.push(suffix);
    Ok(value)
}

fn add_daemon_diagnostic(error: ImageBuildError, daemon: &[u8]) -> ImageBuildError {
    if daemon.is_empty() {
        return error;
    }
    match error {
        ImageBuildError::CommandFailed {
            operation,
            diagnostic,
        } => ImageBuildError::CommandFailed {
            operation,
            diagnostic: format!("{diagnostic}; BuildKit: {}", diagnostic_or_default(daemon)),
        },
        ImageBuildError::CommandTimedOut {
            operation,
            timeout,
            diagnostic,
        } => ImageBuildError::CommandTimedOut {
            operation,
            timeout,
            diagnostic: format!("{diagnostic}; BuildKit: {}", diagnostic_or_default(daemon)),
        },
        other => other,
    }
}

fn diagnostic_or_default(bytes: &[u8]) -> String {
    let value = String::from_utf8_lossy(bytes).trim().to_owned();
    if value.is_empty() {
        "command exited without a diagnostic".to_owned()
    } else {
        value
    }
}

fn bounded_bytes(bytes: &[u8], limit: usize) -> Vec<u8> {
    bytes[bytes.len().saturating_sub(limit)..].to_vec()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    use tempfile::TempDir;

    use super::*;
    use crate::runtime::pod::CommandOutput;

    #[derive(Clone, Default)]
    struct FakeBtrfsRunner;

    impl CommandRunner for FakeBtrfsRunner {
        fn run(&self, _program: &Path, arguments: &[OsString]) -> io::Result<CommandOutput> {
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
                    Ok(CommandOutput::success())
                }
                [subvolume, create, path]
                    if subvolume == Path::new("subvolume") && create == Path::new("create") =>
                {
                    fs::create_dir(path)?;
                    Ok(CommandOutput::success())
                }
                [subvolume, delete, recursive, path]
                    if subvolume == Path::new("subvolume") && delete == Path::new("delete") =>
                {
                    assert_eq!(recursive, Path::new("--recursive"));
                    fs::remove_dir_all(path)?;
                    Ok(CommandOutput::success())
                }
                [property, set, _, read_only, value]
                    if property == Path::new("property")
                        && set == Path::new("set")
                        && read_only == Path::new("ro")
                        && value == Path::new("true") =>
                {
                    Ok(CommandOutput::success())
                }
                _ => Ok(CommandOutput::failure("unsupported fake Btrfs command")),
            }
        }
    }

    #[derive(Clone, Default)]
    struct FakeExecutor {
        state: Arc<Mutex<FakeExecutorState>>,
    }

    #[derive(Default)]
    struct FakeExecutorState {
        commands: Vec<(PathBuf, Vec<OsString>)>,
        failures: VecDeque<OsString>,
        daemon_starts: usize,
        daemon_shutdowns: usize,
        output_variant: String,
    }

    impl FakeExecutor {
        fn fail_once(&self, program_name: impl Into<OsString>) {
            self.state
                .lock()
                .unwrap()
                .failures
                .push_back(program_name.into());
        }

        fn commands(&self) -> Vec<(PathBuf, Vec<OsString>)> {
            self.state.lock().unwrap().commands.clone()
        }

        fn daemon_counts(&self) -> (usize, usize) {
            let state = self.state.lock().unwrap();
            (state.daemon_starts, state.daemon_shutdowns)
        }

        fn set_output_variant(&self, variant: impl Into<String>) {
            self.state.lock().unwrap().output_variant = variant.into();
        }
    }

    impl BuildExecutor for FakeExecutor {
        fn spawn_daemon(
            &self,
            program: &Path,
            arguments: &[OsString],
            output: Option<Arc<BuildOutput>>,
        ) -> io::Result<Box<dyn BuildDaemon>> {
            let address = argument_after(arguments, "--addr")?
                .to_string_lossy()
                .strip_prefix("unix://")
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid address"))?
                .to_owned();
            let listener = UnixListener::bind(&address)?;
            let mut state = self.state.lock().unwrap();
            state.daemon_starts += 1;
            state
                .commands
                .push((program.to_path_buf(), arguments.to_vec()));
            drop(state);
            if let Some(output) = output {
                output(b"BuildKit daemon\n");
            }
            Ok(Box::new(FakeDaemon {
                listener: Some(listener),
                state: Arc::clone(&self.state),
            }))
        }

        fn run(
            &self,
            program: &Path,
            arguments: &[OsString],
            _timeout: Duration,
            output_observer: Option<Arc<BuildOutput>>,
        ) -> io::Result<ExecutionOutput> {
            let should_fail = {
                let mut state = self.state.lock().unwrap();
                state
                    .commands
                    .push((program.to_path_buf(), arguments.to_vec()));
                let matches = state.failures.front().is_some_and(|expected| {
                    program.file_name().is_some_and(|actual| actual == expected)
                });
                if matches {
                    state.failures.pop_front();
                }
                matches
            };
            if should_fail {
                return Ok(ExecutionOutput {
                    success: false,
                    timed_out: false,
                    diagnostic: vec![b'x'; 64 * 1024],
                });
            }

            match program.file_name().and_then(OsStr::to_str) {
                Some("buildctl") => {
                    if let Some(output_observer) = output_observer {
                        output_observer(b"BuildKit progress\n");
                    }
                    let output = argument_after(arguments, "--output")?.to_string_lossy();
                    let archive = output
                        .strip_prefix("type=oci,dest=")
                        .and_then(|value| value.strip_suffix(OCI_EXPORT_SUFFIX))
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "invalid OCI output")
                        })?;
                    let variant = self.state.lock().unwrap().output_variant.clone();
                    write_test_oci_archive(Path::new(archive), &variant)?;
                }
                Some("tar") => {
                    let archive = PathBuf::from(argument_after(arguments, "--file")?);
                    let directory = PathBuf::from(argument_after(arguments, "--directory")?);
                    tar::Archive::new(File::open(archive)?).unpack(directory)?;
                }
                Some("umoci") => {
                    let variant = self.state.lock().unwrap().output_variant.clone();
                    let bundle = PathBuf::from(arguments.last().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "missing bundle")
                    })?);
                    fs::create_dir(&bundle)?;
                    fs::write(
                        bundle.join("config.json"),
                        br#"{"process":{"user":{"uid":1000,"gid":1000,"additionalGids":[999]},"env":["PATH=/image/bin","HOME=/home/develop","SHELL=/bin/bash","IMAGE_DEFAULT=from-dockerfile","DUP=first","DUP=last"],"cwd":"/workspace"}}"#,
                    )?;
                    let rootfs = bundle.join("rootfs");
                    fs::create_dir(&rootfs)?;
                    fs::create_dir(rootfs.join("etc"))?;
                    fs::write(rootfs.join("etc/image-marker"), b"built")?;
                    symlink("/proc/mounts", rootfs.join("etc/mtab"))?;
                    if variant == "unsafe-unpacked-inode" {
                        nix::unistd::mkfifo(
                            &rootfs.join("unsafe-fifo"),
                            nix::sys::stat::Mode::from_bits_truncate(0o600),
                        )?;
                    }
                }
                Some("cp") => {
                    let variant = self.state.lock().unwrap().output_variant.clone();
                    let destination = PathBuf::from(arguments.last().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "missing destination")
                    })?);
                    let source = PathBuf::from(
                        arguments
                            .get(arguments.len().saturating_sub(2))
                            .ok_or_else(|| {
                                io::Error::new(io::ErrorKind::InvalidInput, "missing source")
                            })?,
                    );
                    copy_contents(&source, &destination)?;
                    if variant == "unsafe-staged-inode" {
                        nix::unistd::mkfifo(
                            &destination.join("unsafe-fifo"),
                            nix::sys::stat::Mode::from_bits_truncate(0o600),
                        )?;
                    }
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "unknown fake build command",
                    ));
                }
            }
            Ok(ExecutionOutput {
                success: true,
                timed_out: false,
                diagnostic: Vec::new(),
            })
        }
    }

    struct FakeDaemon {
        listener: Option<UnixListener>,
        state: Arc<Mutex<FakeExecutorState>>,
    }

    impl BuildDaemon for FakeDaemon {
        fn try_wait(&mut self) -> io::Result<Option<bool>> {
            Ok(None)
        }

        fn shutdown(&mut self, _timeout: Duration) -> io::Result<()> {
            if self.listener.take().is_some() {
                self.state.lock().unwrap().daemon_shutdowns += 1;
            }
            Ok(())
        }

        fn diagnostics(&self) -> Vec<u8> {
            b"fake daemon diagnostic".to_vec()
        }
    }

    fn argument_after<'a>(arguments: &'a [OsString], flag: &str) -> io::Result<&'a OsStr> {
        arguments
            .windows(2)
            .find(|pair| pair[0] == OsStr::new(flag))
            .map(|pair| pair[1].as_os_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing argument"))
    }

    fn write_test_oci_archive(path: &Path, variant: &str) -> io::Result<()> {
        let file = File::create(path)?;
        let mut archive = tar::Builder::new(file);
        append_tar_file(
            &mut archive,
            "oci-layout",
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )?;
        let layer = write_test_layer(if variant == "unmapped-owner" {
            u64::from(ID_MAP_SIZE)
        } else {
            0
        })?;
        let layer_digest = format!("sha256:{:x}", Sha256::digest(&layer));
        let image_config = br#"{"config":{"User":"develop"}}"#;
        let image_config_digest = format!("sha256:{:x}", Sha256::digest(image_config));
        let manifest = format!(
            r#"{{"schemaVersion":2,"variant":"{variant}","config":{{"mediaType":"{OCI_IMAGE_CONFIG_MEDIA_TYPE}","digest":"{image_config_digest}","size":{}}},"layers":[{{"mediaType":"{OCI_LAYER_MEDIA_TYPE}","digest":"{layer_digest}","size":{}}}]}}"#,
            image_config.len(),
            layer.len()
        );
        let digest = format!("sha256:{:x}", Sha256::digest(manifest.as_bytes()));
        let index = format!(
            r#"{{"schemaVersion":2,"manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{digest}","size":{},"annotations":{{"org.opencontainers.image.ref.name":"tascarrel:latest"}}}}]}}"#,
            manifest.len()
        );
        append_tar_file(&mut archive, "index.json", index.as_bytes())?;
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        archive.append_data(&mut header, "blobs", io::empty())?;
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        archive.append_data(&mut header, "blobs/sha256", io::empty())?;
        append_tar_file(
            &mut archive,
            &format!("blobs/sha256/{}", digest.trim_start_matches("sha256:")),
            manifest.as_bytes(),
        )?;
        append_tar_file(
            &mut archive,
            &format!(
                "blobs/sha256/{}",
                image_config_digest.trim_start_matches("sha256:")
            ),
            image_config,
        )?;
        append_tar_file(
            &mut archive,
            &format!(
                "blobs/sha256/{}",
                layer_digest.trim_start_matches("sha256:")
            ),
            &layer,
        )?;
        archive.finish()
    }

    fn write_test_layer(uid: u64) -> io::Result<Vec<u8>> {
        write_test_layer_with_ids(uid, 0)
    }

    fn write_test_layer_with_ids(uid: u64, gid: u64) -> io::Result<Vec<u8>> {
        let mut archive = tar::Builder::new(Vec::new());
        append_test_layer_entry(&mut archive, "image-marker", uid, gid)?;
        archive.into_inner()
    }

    fn write_test_base256_layer(uid: u64, gid: u64) -> io::Result<Vec<u8>> {
        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_size(0);
        write_base256_number(&mut header.as_mut_bytes()[108..116], uid);
        write_base256_number(&mut header.as_mut_bytes()[116..124], gid);
        header.set_cksum();
        archive.append_data(&mut header, "image-marker", io::empty())?;
        archive.into_inner()
    }

    fn write_base256_number(field: &mut [u8], value: u64) {
        field.fill(0);
        let bytes = value.to_be_bytes();
        field.copy_from_slice(&bytes[bytes.len() - field.len()..]);
        field[0] |= 0x80;
    }

    fn write_two_entry_test_layer(second_uid: u64) -> io::Result<Vec<u8>> {
        let mut archive = tar::Builder::new(Vec::new());
        append_test_layer_entry(&mut archive, "first", 0, 0)?;
        append_test_layer_entry(&mut archive, "second", second_uid, 0)?;
        archive.into_inner()
    }

    fn append_test_layer_entry<W: Write>(
        archive: &mut tar::Builder<W>,
        path: &str,
        uid: u64,
        gid: u64,
    ) -> io::Result<()> {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_size(0);
        header.set_cksum();
        archive.append_data(&mut header, path, io::empty())
    }

    fn write_test_pax_layer(
        ownership: &[(&str, &[u8])],
        raw_user_id: u64,
        raw_group_id: u64,
    ) -> io::Result<Vec<u8>> {
        let mut archive = tar::Builder::new(Vec::new());
        archive.append_pax_extensions(ownership.iter().copied())?;
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_uid(raw_user_id);
        header.set_gid(raw_group_id);
        header.set_size(0);
        header.set_cksum();
        archive.append_data(&mut header, "image-marker", io::empty())?;
        archive.into_inner()
    }

    fn encode_test_layer(layer: &[u8], compression: LayerCompression) -> io::Result<Vec<u8>> {
        match compression {
            LayerCompression::None => Ok(layer.to_vec()),
            LayerCompression::Gzip => {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(layer)?;
                encoder.finish()
            }
            LayerCompression::Zstd => zstd::stream::encode_all(Cursor::new(layer), 0),
        }
    }

    fn write_test_layer_layout(
        layer: &[u8],
        media_type: &str,
    ) -> io::Result<(TempDir, OciLayerDescriptor)> {
        let layout = TempDir::new()?;
        let digest = format!("sha256:{:x}", Sha256::digest(layer));
        let hexadecimal = digest.strip_prefix("sha256:").unwrap();
        let blob_directory = layout.path().join("blobs/sha256");
        fs::create_dir_all(&blob_directory)?;
        fs::write(blob_directory.join(hexadecimal), layer)?;
        Ok((
            layout,
            OciLayerDescriptor {
                media_type: media_type.to_owned(),
                digest,
                size: u64::try_from(layer.len()).unwrap(),
            },
        ))
    }

    fn unpack_test_oci_layout(root: &Path) -> io::Result<()> {
        let archive = root.join("image.tar");
        write_test_oci_archive(&archive, "manifest-validation")?;
        let file = File::open(&archive)?;
        let layout = root.join("layout");
        fs::create_dir(&layout)?;
        tar::Archive::new(file).unpack(&layout)
    }

    fn append_tar_file<W: Write>(
        archive: &mut tar::Builder<W>,
        path: &str,
        contents: &[u8],
    ) -> io::Result<()> {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(u64::try_from(contents.len()).unwrap());
        header.set_cksum();
        archive.append_data(&mut header, path, Cursor::new(contents))
    }

    fn copy_contents(source: &Path, destination: &Path) -> io::Result<()> {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            let metadata = fs::symlink_metadata(&source_path)?;
            if metadata.is_dir() {
                fs::create_dir(&destination_path)?;
                copy_contents(&source_path, &destination_path)?;
            } else if metadata.is_file() {
                fs::copy(&source_path, &destination_path)?;
            } else if metadata.file_type().is_symlink() {
                symlink(fs::read_link(&source_path)?, &destination_path)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsupported test file type",
                ));
            }
        }
        Ok(())
    }

    fn test_config(temporary_root: &Path) -> ImageBuilderConfig {
        let mut config = ImageBuilderConfig::new(
            "/fake/buildkitd",
            "/fake/buildctl",
            "/fake/umoci",
            "/fake/tar",
            "/fake/cp",
        );
        config.temporary_root = temporary_root.to_path_buf();
        config.daemon_startup_timeout = Duration::from_secs(1);
        config.build_timeout = Duration::from_secs(1);
        config.helper_timeout = Duration::from_secs(1);
        config.daemon_shutdown_timeout = Duration::from_secs(1);
        config.limits.diagnostic_bytes = 128;
        config
    }

    fn test_builder(
        temporary_root: &Path,
        executor: &FakeExecutor,
    ) -> Result<ImageBuilder, ImageBuildError> {
        ImageBuilder::with_executor(test_config(temporary_root), Arc::new(executor.clone()))
    }

    fn write_context(root: &Path) {
        fs::write(root.join("Dockerfile"), b"FROM scratch\nCOPY . /src\n").unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("nested/value"), b"same content").unwrap();
    }

    fn directory_is_empty(path: &Path) -> bool {
        fs::read_dir(path).unwrap().next().is_none()
    }

    #[test]
    fn stale_build_cleanup_removes_only_generated_directories() {
        let temporary = TempDir::new().unwrap();
        let executor = FakeExecutor::default();
        let stale = test_builder(temporary.path(), &executor)
            .unwrap()
            .create_temporary_directory()
            .unwrap()
            .keep();
        assert!(is_image_build_directory_name(
            stale.file_name().expect("temporary directory has a name")
        ));
        fs::create_dir(stale.join("nested")).unwrap();
        fs::write(stale.join("nested/content"), b"stale").unwrap();

        let unrelated = temporary.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        let similarly_named = temporary
            .path()
            .join(format!("{BUILD_DIRECTORY_PREFIX}not-a-generated-name"));
        fs::create_dir(&similarly_named).unwrap();
        let matching_file = temporary.path().join(format!(
            "{BUILD_DIRECTORY_PREFIX}{}",
            "B".repeat(BUILD_DIRECTORY_RANDOM_LEN)
        ));
        fs::write(&matching_file, b"not a directory").unwrap();
        let matching_symlink = temporary.path().join(format!(
            "{BUILD_DIRECTORY_PREFIX}{}",
            "C".repeat(BUILD_DIRECTORY_RANDOM_LEN)
        ));
        symlink(&unrelated, &matching_symlink).unwrap();

        assert_eq!(
            cleanup_stale_image_build_directories(temporary.path()).unwrap(),
            1
        );
        assert!(!stale.exists());
        assert!(unrelated.is_dir());
        assert!(similarly_named.is_dir());
        assert!(matching_file.is_file());
        assert!(
            fs::symlink_metadata(matching_symlink)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn context_digest_is_deterministic_and_covers_modes_and_contents() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        write_context(first.path());
        fs::create_dir(second.path().join("nested")).unwrap();
        fs::write(second.path().join("nested/value"), b"same content").unwrap();
        fs::write(
            second.path().join("Dockerfile"),
            b"FROM scratch\nCOPY . /src\n",
        )
        .unwrap();

        let temporary = TempDir::new().unwrap();
        let builder = ImageBuilder::new(test_config(temporary.path())).unwrap();
        let first_id = builder.context_digest(first.path()).unwrap();
        let second_id = builder.context_digest(second.path()).unwrap();
        assert_eq!(first_id, second_id);

        fs::write(second.path().join("nested/value"), b"changed").unwrap();
        assert_ne!(first_id, builder.context_digest(second.path()).unwrap());
        fs::write(second.path().join("nested/value"), b"same content").unwrap();
        let mut permissions = fs::metadata(second.path().join("nested/value"))
            .unwrap()
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(second.path().join("nested/value"), permissions).unwrap();
        assert_ne!(first_id, builder.context_digest(second.path()).unwrap());
    }

    #[test]
    fn context_rejects_links_special_files_hardlinks_and_missing_dockerfile() {
        let temporary = TempDir::new().unwrap();
        let builder = ImageBuilder::new(test_config(temporary.path())).unwrap();

        let missing = TempDir::new().unwrap();
        assert!(matches!(
            builder.context_digest(missing.path()),
            Err(ImageBuildError::UnsafeContext { .. })
        ));

        let linked = TempDir::new().unwrap();
        fs::write(linked.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
        symlink("Dockerfile", linked.path().join("alias")).unwrap();
        assert!(matches!(
            builder.context_digest(linked.path()),
            Err(ImageBuildError::UnsafeContext { .. })
        ));

        let hardlinked = TempDir::new().unwrap();
        fs::write(hardlinked.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
        fs::hard_link(
            hardlinked.path().join("Dockerfile"),
            hardlinked.path().join("copy"),
        )
        .unwrap();
        assert!(matches!(
            builder.context_digest(hardlinked.path()),
            Err(ImageBuildError::UnsafeContext { .. })
        ));

        let special = TempDir::new().unwrap();
        fs::write(special.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
        let _listener = UnixListener::bind(special.path().join("socket")).unwrap();
        assert!(matches!(
            builder.context_digest(special.path()),
            Err(ImageBuildError::UnsafeContext { .. })
        ));
    }

    #[test]
    fn configuration_and_archive_paths_are_not_interpreted() {
        let temporary = TempDir::new().unwrap();
        let mut relative = test_config(temporary.path());
        relative.buildctl = PathBuf::from("buildctl");
        assert!(matches!(
            ImageBuilder::new(relative),
            Err(ImageBuildError::InvalidConfiguredPath { .. })
        ));

        let mut traversing = test_config(temporary.path());
        traversing.cp = PathBuf::from("/fake/../cp");
        assert!(matches!(
            ImageBuilder::new(traversing),
            Err(ImageBuildError::InvalidConfiguredPath { .. })
        ));
        let comma_parent = TempDir::new().unwrap();
        let comma_root = comma_parent.path().join("bad,root");
        fs::create_dir(&comma_root).unwrap();
        assert!(matches!(
            ImageBuilder::new(test_config(&comma_root)),
            Err(ImageBuildError::InvalidConfiguredPath { .. })
        ));
        assert_eq!(
            normalize_archive_path(b"./blobs/sha256/value").unwrap(),
            b"blobs/sha256/value"
        );
        for unsafe_path in [b"../escape".as_slice(), b"/absolute", b"a/../../b", b"."] {
            assert!(normalize_archive_path(unsafe_path).is_none());
        }

        let mut unsafe_namespace = test_config(temporary.path());
        unsafe_namespace.network_namespace = Some("../host".into());
        assert!(matches!(
            ImageBuilder::new(unsafe_namespace),
            Err(ImageBuildError::InvalidLimit(_))
        ));
    }

    /// A root image adopts the complete account metadata for UID 1000.
    #[test]
    fn root_image_uses_the_existing_uid_1000_account_regardless_of_name() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("etc")).unwrap();
        fs::write(
            root.path().join("etc/passwd"),
            b"root:x:0:0:root:/root:/bin/sh\nnode:x:1000:2000:Node:/home/node:/bin/sh\n",
        )
        .unwrap();
        fs::write(
            root.path().join("etc/group"),
            b"root:x:0:\nnode:x:2000:\ndocker:x:999:node\n",
        )
        .unwrap();
        let config = ImageConfig::for_process(
            ["PATH=/usr/bin", "HOME=/root", "USER=root", "LOGNAME=root"],
            ImageUser::default(),
            "/workspace",
        )
        .unwrap();

        let (normalized, changed) =
            normalize_image_user_with(root.path(), config, |home, uid, gid| {
                assert_eq!(home, root.path().join("home/node"));
                assert_eq!((uid, gid), (1000, 2000));
                Ok(())
            })
            .unwrap();

        assert!(changed);
        assert_eq!(normalized.user().name(), "node");
        assert_eq!(normalized.user().uid(), 1000);
        assert_eq!(normalized.user().gid(), 2000);
        assert_eq!(normalized.user().additional_gids(), [999]);
        assert_eq!(
            normalized.environment(),
            [
                "PATH=/usr/bin",
                "HOME=/home/node",
                "USER=node",
                "LOGNAME=node"
            ]
        );
        assert!(root.path().join("home/node").is_dir());
    }

    /// A root image without a development account receives `develop:1000`.
    #[test]
    fn root_image_creates_develop_when_uid_1000_is_unused() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("etc")).unwrap();
        fs::write(
            root.path().join("etc/passwd"),
            b"root:x:0:0:root:/root:/bin/sh\n",
        )
        .unwrap();
        let config = ImageConfig::for_process(
            ["PATH=/usr/bin", "HOME=/root"],
            ImageUser::default(),
            "/workspace",
        )
        .unwrap();

        let (normalized, changed) =
            normalize_image_user_with(root.path(), config, |_home, uid, gid| {
                assert_eq!((uid, gid), (1000, 1000));
                Ok(())
            })
            .unwrap();

        assert!(changed);
        assert_eq!(normalized.user().name(), "develop");
        assert_eq!(
            (normalized.user().uid(), normalized.user().gid()),
            (1000, 1000)
        );
        assert!(
            fs::read_to_string(root.path().join("etc/passwd"))
                .unwrap()
                .contains("develop:x:1000:1000:Tascarrel development user:/home/develop:/bin/sh\n")
        );
        assert_eq!(
            fs::read_to_string(root.path().join("etc/group")).unwrap(),
            "develop:x:1000:\n"
        );
        assert!(root.path().join("home/develop").is_dir());
    }

    /// Explicit non-root OCI users bypass Tascarrel account normalization.
    #[test]
    fn non_root_image_user_is_preserved() {
        let root = TempDir::new().unwrap();
        let user = ImageUser::new("custom", 1234, 4321, [999]).unwrap();
        let config = ImageConfig::for_process(["HOME=/custom"], user, "/src").unwrap();

        let (unchanged, normalized) = normalize_image_user_with(
            root.path(),
            config.clone(),
            |_home, _uid, _gid| unreachable!(),
        )
        .unwrap();

        assert!(normalized);
        assert_eq!(unchanged, config);
        for name in ["subuid", "subgid"] {
            assert_eq!(
                fs::read_to_string(root.path().join("etc").join(name)).unwrap(),
                "custom:65536:65536\n"
            );
        }
    }

    /// Explicit numeric image users join an existing Docker group through the
    /// account resolved by UID without changing OCI identity.
    #[test]
    fn non_root_image_user_joins_an_existing_docker_group() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("etc")).unwrap();
        fs::write(
            root.path().join("etc/passwd"),
            b"root:x:0:0:root:/root:/bin/sh\ncustom:x:1234:4321:Custom:/custom:/bin/sh\n",
        )
        .unwrap();
        fs::write(
            root.path().join("etc/group"),
            b"root:x:0:\ndocker:x:999:\ncustom:x:4321:\n",
        )
        .unwrap();
        let user = ImageUser::new("1234", 1234, 4321, []).unwrap();
        let config = ImageConfig::for_process(["HOME=/custom"], user, "/src").unwrap();

        let (normalized, changed) =
            normalize_image_user_with(root.path(), config, |_home, _uid, _gid| unreachable!())
                .unwrap();

        assert!(changed);
        assert_eq!(normalized.user().name(), "1234");
        assert_eq!(
            (normalized.user().uid(), normalized.user().gid()),
            (1234, 4321)
        );
        assert_eq!(normalized.user().additional_gids(), [999]);
        assert!(
            fs::read_to_string(root.path().join("etc/group"))
                .unwrap()
                .contains("docker:x:999:custom\n")
        );
    }

    /// Normalized derivatives cannot collide with their OCI source digest.
    #[test]
    fn normalized_image_ids_are_stable_and_versioned() {
        let source = ImageId::new(format!("sha256:{}", "a".repeat(64))).unwrap();

        let first = normalized_image_id(&source).unwrap();
        let second = normalized_image_id(&source).unwrap();

        assert_eq!(first, second);
        assert_ne!(first, source);
        assert!(first.as_str().starts_with("tascarrel-user-v2:"));
    }

    #[test]
    fn buildkit_client_and_daemon_can_share_a_named_network_namespace() {
        let temporary = TempDir::new().unwrap();
        let mut config = test_config(temporary.path());
        config.nsenter = PathBuf::from("/fake/nsenter");
        config.network_namespace = Some("tascarrel-build".into());
        let builder = ImageBuilder::new(config).unwrap();
        let (program, arguments) = builder.command_in_network_namespace(
            Path::new("/fake/buildkitd"),
            vec![
                OsString::from("--oci-worker-net=host"),
                OsString::from("--root=/private"),
            ],
        );
        assert_eq!(program, Path::new("/fake/nsenter"));
        assert_eq!(
            arguments,
            [
                "--net=/run/netns/tascarrel-build",
                "--",
                "/fake/buildkitd",
                "--oci-worker-net=host",
                "--root=/private",
            ]
        );
        let (program, arguments) = builder.command_in_network_namespace(
            Path::new("/fake/buildctl"),
            vec![OsString::from("build")],
        );
        assert_eq!(program, Path::new("/fake/nsenter"));
        assert_eq!(
            arguments,
            [
                "--net=/run/netns/tascarrel-build",
                "--",
                "/fake/buildctl",
                "build",
            ]
        );
    }

    #[test]
    fn build_ids_follow_oci_output_and_reuse_only_identical_results() {
        let store_root = TempDir::new().unwrap();
        let store =
            BtrfsStore::with_runner(store_root.path(), "/fake/btrfs", FakeBtrfsRunner).unwrap();
        let context_parent = TempDir::new().unwrap();
        let context = context_parent.path().join("image;not-a-command");
        fs::create_dir(&context).unwrap();
        write_context(&context);
        let temporary = TempDir::new().unwrap();
        let executor = FakeExecutor::default();
        let builder = test_builder(temporary.path(), &executor).unwrap();

        let first = builder.build(&store, &context).unwrap();
        assert!(!first.reused());
        assert_eq!(
            fs::read(first.generation().root().join("etc/image-marker")).unwrap(),
            b"built"
        );
        assert_eq!(
            fs::read_link(first.generation().root().join("etc/mtab")).unwrap(),
            Path::new("/proc/mounts")
        );
        for name in ["subuid", "subgid"] {
            assert_eq!(
                fs::read(first.generation().root().join("etc").join(name)).unwrap(),
                b"develop:65536:65536\n"
            );
        }
        assert!(directory_is_empty(temporary.path()));
        assert_eq!(executor.daemon_counts(), (1, 1));
        assert_eq!(
            first.generation().config().environment(),
            [
                "PATH=/image/bin",
                "HOME=/home/develop",
                "SHELL=/bin/bash",
                "IMAGE_DEFAULT=from-dockerfile",
                "DUP=first",
                "DUP=last"
            ]
        );
        assert_eq!(first.generation().config().user().name(), "develop");
        assert_eq!(first.generation().config().user().uid(), 1000);
        assert_eq!(first.generation().config().user().gid(), 1000);
        assert_eq!(first.generation().config().user().additional_gids(), [999]);
        assert_eq!(
            first.generation().config().working_directory(),
            "/workspace"
        );

        let commands = executor.commands();
        assert_eq!(commands.len(), 5);
        assert_eq!(commands[0].0, Path::new("/fake/buildkitd"));
        assert!(
            commands[0]
                .1
                .iter()
                .any(|argument| argument == "--oci-worker-net=host")
        );
        assert_eq!(commands[1].0, Path::new("/fake/buildctl"));
        assert_eq!(
            &commands[1].1[..3],
            [
                OsString::from("--debug"),
                OsString::from("--timeout"),
                OsString::from("0")
            ]
        );
        assert!(commands[1].1.iter().any(|argument| {
            argument
                .as_bytes()
                .ends_with(context.as_os_str().as_bytes())
                && argument.as_bytes().starts_with(b"context=")
        }));
        assert_eq!(commands[3].0, Path::new("/fake/umoci"));
        assert!(commands[3].1.iter().any(|argument| {
            argument
                .as_bytes()
                .ends_with(OCI_EXPORT_TAG_SUFFIX.as_bytes())
        }));

        let second = builder.build(&store, &context).unwrap();
        assert!(second.reused());
        assert_eq!(second.generation().id(), first.generation().id());
        assert_eq!(executor.commands().len(), commands.len() * 2);
        assert_eq!(executor.daemon_counts(), (2, 2));

        executor.set_output_variant("updated-base-image");
        let third = builder.build(&store, &context).unwrap();
        assert!(!third.reused());
        assert_ne!(third.generation().id(), first.generation().id());
        assert_eq!(store.list_images().unwrap().len(), 2);
        assert_eq!(executor.commands().len(), commands.len() * 3);
        assert_eq!(executor.daemon_counts(), (3, 3));
    }

    /// Verifies the streaming build path forwards actual build client output.
    #[test]
    fn build_streams_buildctl_output() {
        let store_root = TempDir::new().unwrap();
        let store =
            BtrfsStore::with_runner(store_root.path(), "/fake/btrfs", FakeBtrfsRunner).unwrap();
        let context = TempDir::new().unwrap();
        write_context(context.path());
        let temporary = TempDir::new().unwrap();
        let executor = FakeExecutor::default();
        let builder = test_builder(temporary.path(), &executor).unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&output);

        builder
            .build_with_output(&store, context.path(), move |bytes| {
                observed.lock().unwrap().extend_from_slice(bytes);
            })
            .unwrap();

        assert_eq!(
            *output.lock().unwrap(),
            b"BuildKit daemon\nBuildKit progress\n"
        );
    }

    /// Output pipe failures and reader panics reach the build operation.
    #[test]
    fn build_output_reader_failures_are_propagated() {
        struct BrokenReader;

        impl Read for BrokenReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture failure"))
            }
        }

        let reader = spawn_log_reader(BrokenReader, Arc::new(BoundedLog::new(128)), None);
        let error = join_log_readers([reader]).expect_err("read failure is returned");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);

        let reader = thread::spawn(|| -> io::Result<()> { panic!("fixture panic") });
        let error = join_log_readers([reader]).expect_err("reader panic is returned");
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn command_failures_bound_diagnostics_and_remove_all_staging() {
        let context = TempDir::new().unwrap();
        write_context(context.path());
        for failed_program in ["buildctl", "tar", "umoci", "cp"] {
            let store_root = TempDir::new().unwrap();
            let store =
                BtrfsStore::with_runner(store_root.path(), "/fake/btrfs", FakeBtrfsRunner).unwrap();
            let temporary = TempDir::new().unwrap();
            let executor = FakeExecutor::default();
            executor.fail_once(failed_program);
            let builder = test_builder(temporary.path(), &executor).unwrap();

            let error = builder.build(&store, context.path()).unwrap_err();
            assert!(
                matches!(error, ImageBuildError::CommandFailed { .. }),
                "{failed_program} returned {error:?}"
            );
            assert!(error.to_string().len() < 512);
            assert!(store.list_images().unwrap().is_empty());
            assert!(directory_is_empty(&store_root.path().join("image-staging")));
            assert!(directory_is_empty(&store_root.path().join("transactions")));
            assert!(directory_is_empty(temporary.path()));
            assert_eq!(executor.daemon_counts(), (1, 1));
        }
    }

    #[test]
    fn mapped_layer_ownership_accepts_the_inclusive_boundary_in_supported_encodings() {
        let maximum = u64::from(ID_MAP_SIZE - 1);
        let maximum_text = maximum.to_string();
        let raw = write_test_layer_with_ids(maximum, maximum).unwrap();
        let base256 = write_test_base256_layer(maximum, maximum).unwrap();
        let pax = write_test_pax_layer(
            &[
                ("uid", maximum_text.as_bytes()),
                ("gid", maximum_text.as_bytes()),
            ],
            u64::from(ID_MAP_SIZE),
            u64::from(ID_MAP_SIZE),
        )
        .unwrap();
        let empty_pax_fallback =
            write_test_pax_layer(&[("uid", b""), ("gid", b"")], maximum, maximum).unwrap();
        let gzip = encode_test_layer(&pax, LayerCompression::Gzip).unwrap();
        let zstd = encode_test_layer(&raw, LayerCompression::Zstd).unwrap();

        for (layer, media_type) in [
            (raw, OCI_LAYER_MEDIA_TYPE.to_owned()),
            (base256, OCI_LAYER_MEDIA_TYPE.to_owned()),
            (empty_pax_fallback, OCI_LAYER_MEDIA_TYPE.to_owned()),
            (gzip, format!("{OCI_LAYER_MEDIA_TYPE}+gzip")),
            (zstd, format!("{OCI_LAYER_MEDIA_TYPE}+zstd")),
        ] {
            let (layout, descriptor) = write_test_layer_layout(&layer, &media_type).unwrap();
            validate_oci_layer_ownership(
                layout.path(),
                &[descriptor],
                &ImageBuildLimits::default(),
            )
            .unwrap();
        }

        assert!(matches!(
            layer_compression(OCI_NONDISTRIBUTABLE_LAYER_MEDIA_TYPE),
            Some(LayerCompression::None)
        ));
        assert!(matches!(
            layer_compression(&format!("{OCI_NONDISTRIBUTABLE_LAYER_MEDIA_TYPE}+gzip")),
            Some(LayerCompression::Gzip)
        ));
        assert!(matches!(
            layer_compression(&format!("{OCI_NONDISTRIBUTABLE_LAYER_MEDIA_TYPE}+zstd")),
            Some(LayerCompression::Zstd)
        ));
    }

    #[test]
    fn layer_ownership_rejects_uid_gid_and_pax_values_at_the_exclusive_boundary() {
        let outside = u64::from(ID_MAP_SIZE);
        for layer in [
            write_test_layer_with_ids(outside, 0).unwrap(),
            write_test_layer_with_ids(0, outside).unwrap(),
            write_test_base256_layer(outside, 0).unwrap(),
            write_test_base256_layer(0, u64::from(u32::MAX)).unwrap(),
        ] {
            let error = validate_layer_tar(
                Box::new(Cursor::new(layer)),
                Path::new("test-layer"),
                &ImageBuildLimits::default(),
                &mut 0,
                &mut 0,
            )
            .unwrap_err();
            assert!(matches!(error, ImageBuildError::UnsafeOutput { .. }));
            assert!(
                error
                    .to_string()
                    .contains("outside the pod user-namespace map")
            );
        }

        let outside_text = outside.to_string();
        let u32_sentinel = u32::MAX.to_string();
        let u32_wrap = (u64::from(u32::MAX) + 1).to_string();
        for ownership in [
            vec![("uid", outside_text.as_bytes())],
            vec![("gid", outside_text.as_bytes())],
            vec![("uid", b"-1".as_slice())],
            vec![("gid", b"not-a-number".as_slice())],
            vec![("uid", u32_sentinel.as_bytes())],
            vec![("gid", u32_wrap.as_bytes())],
            vec![("uid", b"18446744073709551616".as_slice())],
        ] {
            let layer = write_test_pax_layer(&ownership, 0, 0).unwrap();
            assert!(matches!(
                validate_layer_tar(
                    Box::new(Cursor::new(layer)),
                    Path::new("test-layer"),
                    &ImageBuildLimits::default(),
                    &mut 0,
                    &mut 0,
                ),
                Err(ImageBuildError::UnsafeOutput { .. })
            ));
        }
    }

    #[test]
    fn duplicate_pax_ownership_is_rejected_instead_of_choosing_a_parser_winner() {
        let outside = u64::from(ID_MAP_SIZE).to_string();
        // Rust's tar reader applies the first value while Go's archive/tar
        // collapses records into a map where the last value wins. Accepting
        // either interpretation here would let validation and umoci disagree.
        for key in ["uid", "gid"] {
            let layer =
                write_test_pax_layer(&[(key, b"0"), (key, outside.as_bytes())], 0, 0).unwrap();
            let error = validate_layer_tar(
                Box::new(Cursor::new(layer)),
                Path::new("test-layer"),
                &ImageBuildLimits::default(),
                &mut 0,
                &mut 0,
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("duplicate PAX ownership metadata")
            );
        }
    }

    #[test]
    fn multi_member_gzip_cannot_hide_an_unmapped_later_tar_entry() {
        let raw = write_two_entry_test_layer(u64::from(ID_MAP_SIZE)).unwrap();
        // Both entries are empty, so the second header starts at one tar block.
        let mut encoded = encode_test_layer(&raw[..512], LayerCompression::Gzip).unwrap();
        encoded.extend(encode_test_layer(&raw[512..], LayerCompression::Gzip).unwrap());
        let media_type = format!("{OCI_LAYER_MEDIA_TYPE}+gzip");
        let (layout, descriptor) = write_test_layer_layout(&encoded, &media_type).unwrap();
        assert!(matches!(
            validate_oci_layer_ownership(
                layout.path(),
                &[descriptor],
                &ImageBuildLimits::default(),
            ),
            Err(ImageBuildError::UnsafeOutput { .. })
        ));
    }

    #[test]
    fn layer_descriptors_bind_media_type_size_digest_and_content() {
        let layer = write_test_layer(0).unwrap();
        let (layout, mut descriptor) =
            write_test_layer_layout(&layer, OCI_LAYER_MEDIA_TYPE).unwrap();
        let blob = sha256_blob_path(
            layout.path(),
            &descriptor.digest,
            "test layer",
            layout.path(),
            "invalid test digest",
        )
        .unwrap();

        descriptor.size += 1;
        assert!(matches!(
            validate_layer_blob(&blob, &descriptor),
            Err(ImageBuildError::UnsafeOutput { .. })
        ));
        descriptor.size -= 1;
        descriptor.digest = format!("sha256:{}", "0".repeat(64));
        assert!(matches!(
            validate_layer_blob(&blob, &descriptor),
            Err(ImageBuildError::UnsafeOutput { .. })
        ));

        descriptor.digest = format!("sha256:{:x}", Sha256::digest(&layer));
        descriptor.media_type = "application/vnd.docker.image.rootfs.diff.tar.gzip".to_owned();
        assert!(matches!(
            validate_oci_layer_ownership(
                layout.path(),
                &[descriptor],
                &ImageBuildLimits::default(),
            ),
            Err(ImageBuildError::UnsafeOutput { .. })
        ));
    }

    #[test]
    fn manifest_descriptor_requires_the_exact_oci_media_type_and_size() {
        for mutation in ["media-type", "size"] {
            let temporary = TempDir::new().unwrap();
            unpack_test_oci_layout(temporary.path()).unwrap();
            let layout = temporary.path().join("layout");
            let index_path = layout.join("index.json");
            let mut index: serde_json::Value =
                serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
            let descriptor = &mut index["manifests"][0];
            match mutation {
                "media-type" => {
                    descriptor["mediaType"] = serde_json::Value::String(
                        "application/vnd.docker.distribution.manifest.v2+json".to_owned(),
                    );
                }
                "size" => {
                    descriptor["size"] =
                        serde_json::Value::from(descriptor["size"].as_u64().unwrap() + 1);
                }
                _ => unreachable!(),
            }
            fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
            assert!(matches!(
                image_from_oci_layout(&layout, &ImageBuildLimits::default()),
                Err(ImageBuildError::UnsafeOutput { .. })
            ));
        }
    }

    #[test]
    fn image_config_descriptor_binds_user_media_type_size_digest_and_content() {
        for mutation in ["media-type", "size", "digest", "content"] {
            let temporary = TempDir::new().unwrap();
            unpack_test_oci_layout(temporary.path()).unwrap();
            let layout = temporary.path().join("layout");
            let index: OciIndex = serde_json::from_slice(
                &read_bounded_metadata(&layout.join("index.json"), "OCI index").unwrap(),
            )
            .unwrap();
            let manifest_path = sha256_blob_path(
                &layout,
                &index.manifests[0].digest,
                "OCI index",
                &layout.join("index.json"),
                "invalid manifest digest",
            )
            .unwrap();
            let mut manifest: OciManifest = serde_json::from_slice(
                &read_bounded_metadata(&manifest_path, "OCI image manifest").unwrap(),
            )
            .unwrap();
            match mutation {
                "media-type" => manifest.config.media_type = "application/json".to_owned(),
                "size" => manifest.config.size += 1,
                "digest" => manifest.config.digest = format!("sha256:{}", "0".repeat(64)),
                "content" => {
                    let path = sha256_blob_path(
                        &layout,
                        &manifest.config.digest,
                        "OCI image manifest",
                        &manifest_path,
                        "invalid config digest",
                    )
                    .unwrap();
                    let mut contents = fs::read(&path).unwrap();
                    contents[0] ^= 1;
                    fs::write(path, contents).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                configured_user_from_oci_image(&layout, &manifest.config, &manifest_path).is_err(),
                "accepted mutated image config {mutation}"
            );
        }

        let temporary = TempDir::new().unwrap();
        unpack_test_oci_layout(temporary.path()).unwrap();
        let image = image_from_oci_layout(
            &temporary.path().join("layout"),
            &ImageBuildLimits::default(),
        )
        .unwrap();
        assert_eq!(image.configured_user, "develop");
    }

    #[test]
    fn ownership_and_inode_validation_fail_closed_and_cleanup_staging() {
        let context = TempDir::new().unwrap();
        write_context(context.path());
        for variant in [
            "unmapped-owner",
            "unsafe-unpacked-inode",
            "unsafe-staged-inode",
        ] {
            let store_root = TempDir::new().unwrap();
            let store =
                BtrfsStore::with_runner(store_root.path(), "/fake/btrfs", FakeBtrfsRunner).unwrap();
            let temporary = TempDir::new().unwrap();
            let executor = FakeExecutor::default();
            executor.set_output_variant(variant);
            let builder = test_builder(temporary.path(), &executor).unwrap();

            let error = builder.build(&store, context.path()).unwrap_err();
            assert!(
                matches!(error, ImageBuildError::UnsafeOutput { .. }),
                "{variant} returned {error:?}"
            );
            assert!(store.list_images().unwrap().is_empty());
            assert!(directory_is_empty(&store_root.path().join("image-staging")));
            assert!(directory_is_empty(&store_root.path().join("transactions")));
            assert!(directory_is_empty(temporary.path()));
            assert_eq!(executor.daemon_counts(), (1, 1));
        }
    }

    #[test]
    fn archive_validation_rejects_links_before_external_tar_runs() {
        let temporary = TempDir::new().unwrap();
        let archive_path = temporary.path().join("unsafe.tar");
        let file = File::create(&archive_path).unwrap();
        let mut archive = tar::Builder::new(file);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        header.set_link_name("../../outside").unwrap();
        header.set_cksum();
        archive
            .append_data(&mut header, "link", io::empty())
            .unwrap();
        archive.finish().unwrap();
        assert!(matches!(
            validate_oci_archive(&archive_path, &ImageBuildLimits::default()),
            Err(ImageBuildError::UnsafeOutput { .. })
        ));
    }

    #[test]
    fn bounded_log_and_byte_guard_keep_only_the_tail() {
        let log = BoundedLog::new(5);
        log.push(b"abc");
        log.push(b"defg");
        assert_eq!(log.snapshot(), b"cdefg");
        assert_eq!(bounded_bytes(b"012345", 3), b"345");
    }
}
