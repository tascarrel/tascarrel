//! Initialization and typed paths for the guest persistent-state filesystem.

use std::ffi::OsStr;
use std::fs;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use reportify::ErrorExt as _;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use super::GUEST_STORAGE_FORMAT_VERSION;
use super::GuestStorageError;

const FORMAT_MANIFEST: &str = "format.json";
const DATABASE_DIRECTORY: &str = "database";
const DATABASE_FILE: &str = "state.sqlite3";
const INPUT_DIRECTORY: &str = "input";
const STORE_DIRECTORY: &str = "store";
const REPOSITORIES_DIRECTORY: &str = "repositories";
const CHAT_DIRECTORY: &str = "chat";
const NETWORK_DIRECTORY: &str = "network";
const NETWORK_PUBLIC_DIRECTORY: &str = "public";
const NIX_STORE_DIRECTORY: &str = "nix-store";
const SCRATCH_DIRECTORY: &str = "scratch";
const IMAGE_BUILDS_DIRECTORY: &str = "image-builds";

/// Host-published, content-addressed workspace inputs.
#[derive(Clone, Debug)]
pub struct InputStorage {
    root: PathBuf,
}

impl InputStorage {
    /// Returns the root consumed by workspace snapshot publication.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the atomically published current input generation.
    #[must_use]
    pub fn current(&self) -> PathBuf {
        self.root.join("current")
    }
}

/// Persistent immutable repository checkout generations.
#[derive(Clone, Debug)]
pub struct RepositoryStorage {
    root: PathBuf,
}

impl RepositoryStorage {
    /// Returns the repository-manager namespace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Persistent chat artifacts, harness installations, and provider state.
#[derive(Clone, Debug)]
pub struct ChatStorage {
    root: PathBuf,
}

impl ChatStorage {
    /// Returns the namespace root used by the chat feature.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns immutable harness installations exposed to pods.
    #[must_use]
    pub fn harnesses(&self) -> PathBuf {
        self.root.join("harnesses")
    }

    /// Returns the attachment tree exposed read-only to pods.
    #[must_use]
    pub fn attachment_binding_source(&self) -> PathBuf {
        self.root.join("attachments").join("attachments")
    }

    /// Returns the provider-owned Codex state directory.
    #[must_use]
    pub fn codex_state(&self) -> PathBuf {
        self.root.join("harness-codex")
    }

    /// Returns the provider-owned Claude Code state directory.
    #[must_use]
    pub fn claude_code_state(&self) -> PathBuf {
        self.root.join("harness-claude-code")
    }
}

/// Public certificate material derived from host-owned network policy.
#[derive(Clone, Debug)]
pub struct NetworkStorage {
    public: PathBuf,
}

impl NetworkStorage {
    /// Returns the directory mounted read-only into pods.
    #[must_use]
    pub fn public(&self) -> &Path {
        &self.public
    }

    /// Returns the durable public workspace CA certificate path.
    #[must_use]
    pub fn authority_certificate(&self) -> PathBuf {
        self.public.join("ca.pem")
    }
}

/// Persistent pod Nix store managed by the guest NixOS module.
#[derive(Clone, Debug)]
pub struct NixStoreStorage {
    root: PathBuf,
}

impl NixStoreStorage {
    /// Returns the physical store served to Nix-enabled pods.
    #[must_use]
    pub fn store(&self) -> PathBuf {
        self.root.join("nix/store")
    }

    /// Returns the guestd-owned direct-root directory for pods.
    #[must_use]
    pub fn pod_gc_roots(&self) -> PathBuf {
        self.root.join("nix/var/nix/gcroots/tascarrel/pods")
    }

    /// Returns the same-filesystem staging directory for withdrawn roots.
    #[must_use]
    pub fn gc_root_trash(&self) -> PathBuf {
        self.root.join("nix/var/nix/tascarrel-gc-root-trash")
    }
}

/// Rebuildable large scratch data kept off the tmpfs runtime filesystem.
#[derive(Clone, Debug)]
pub struct ScratchStorage {
    image_builds: PathBuf,
}

impl ScratchStorage {
    /// Returns the same-filesystem image-build scratch root.
    #[must_use]
    pub fn image_builds(&self) -> &Path {
        &self.image_builds
    }
}

/// Concrete paths for every persistent namespace.
#[derive(Debug)]
pub(crate) struct StorageLayout {
    root: PathBuf,
    database: PathBuf,
    input: InputStorage,
    store_root: PathBuf,
    repositories: RepositoryStorage,
    chats: ChatStorage,
    network: NetworkStorage,
    nix_store: NixStoreStorage,
    scratch: ScratchStorage,
}

impl StorageLayout {
    /// Initializes a new-format root or validates an existing one.
    pub(crate) fn initialize(root: &Path) -> Result<Self, Report<GuestStorageError>> {
        if !root.is_absolute() {
            return Err(GuestStorageError::InvalidConfiguration
                .report()
                .message("persistent state root must be absolute")
                .field_display("root", root.display()));
        }
        let root = fs::canonicalize(root)
            .map_err(|error| initialization_io("resolve persistent state root", root, error))?;
        if !root.is_dir() {
            return Err(incompatible(
                "persistent state root is not a directory",
                &root,
            ));
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o711))
            .map_err(|error| initialization_io("secure persistent state root", &root, error))?;
        initialize_manifest(&root)?;

        let database = ensure_directory(&root.join(DATABASE_DIRECTORY), 0o700)?;
        let input = InputStorage {
            root: ensure_directory(&root.join(INPUT_DIRECTORY), 0o711)?,
        };
        let store_root = ensure_directory(&root.join(STORE_DIRECTORY), 0o711)?;
        let repositories = RepositoryStorage {
            root: ensure_directory(&root.join(REPOSITORIES_DIRECTORY), 0o711)?,
        };
        let chats = ChatStorage {
            root: ensure_directory(&root.join(CHAT_DIRECTORY), 0o711)?,
        };
        let network_root = ensure_directory(&root.join(NETWORK_DIRECTORY), 0o755)?;
        let network = NetworkStorage {
            public: ensure_directory(&network_root.join(NETWORK_PUBLIC_DIRECTORY), 0o755)?,
        };
        let scratch_root = ensure_directory(&root.join(SCRATCH_DIRECTORY), 0o700)?;
        let scratch = ScratchStorage {
            image_builds: ensure_directory(&scratch_root.join(IMAGE_BUILDS_DIRECTORY), 0o700)?,
        };
        let nix_store_path = root.join(NIX_STORE_DIRECTORY);

        Ok(Self {
            root,
            database,
            input,
            store_root,
            repositories,
            chats,
            network,
            nix_store: NixStoreStorage {
                root: nix_store_path,
            },
            scratch,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn database_path(&self) -> PathBuf {
        self.database.join(DATABASE_FILE)
    }

    pub(crate) fn input(&self) -> &InputStorage {
        &self.input
    }

    pub(crate) fn store_root(&self) -> &Path {
        &self.store_root
    }

    pub(crate) fn repositories(&self) -> &RepositoryStorage {
        &self.repositories
    }

    pub(crate) fn chats(&self) -> &ChatStorage {
        &self.chats
    }

    pub(crate) fn network(&self) -> &NetworkStorage {
        &self.network
    }

    pub(crate) fn nix_store(&self) -> &NixStoreStorage {
        &self.nix_store
    }

    pub(crate) fn scratch(&self) -> &ScratchStorage {
        &self.scratch
    }
}

#[derive(Deserialize, Serialize)]
struct StorageManifest {
    format_version: u32,
}

/// Creates or validates the root format marker before any managed directories.
fn initialize_manifest(root: &Path) -> Result<(), Report<GuestStorageError>> {
    let path = root.join(FORMAT_MANIFEST);
    match fs::symlink_metadata(&path) {
        Ok(_) => validate_manifest(&path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            cleanup_manifest_temps(root)?;
            require_initializable_root(root)?;
            write_manifest(&path)?;
            validate_manifest(&path)
        }
        Err(error) => Err(initialization_io("inspect storage manifest", &path, error)),
    }
}

/// Allows the NixOS-owned store to exist before guestd claims a fresh root.
fn require_initializable_root(root: &Path) -> Result<(), Report<GuestStorageError>> {
    for entry in fs::read_dir(root)
        .map_err(|error| initialization_io("read persistent state root", root, error))?
    {
        let entry =
            entry.map_err(|error| initialization_io("read persistent state root", root, error))?;
        if entry.file_name() != OsStr::new(NIX_STORE_DIRECTORY) {
            return Err(incompatible(
                "unformatted persistent state root is not empty; old layouts are unsupported",
                root,
            ));
        }
    }
    Ok(())
}

/// Reads and checks the format manifest.
fn validate_manifest(path: &Path) -> Result<(), Report<GuestStorageError>> {
    let bytes =
        fs::read(path).map_err(|error| initialization_io("read storage manifest", path, error))?;
    let manifest: StorageManifest = serde_json::from_slice(&bytes).map_err(|error| {
        GuestStorageError::IncompatibleLayout
            .report()
            .message(error.to_string())
            .field_display("path", path.display())
    })?;
    if manifest.format_version != GUEST_STORAGE_FORMAT_VERSION {
        return Err(GuestStorageError::IncompatibleLayout
            .report()
            .message("persistent state uses an unsupported layout version")
            .field("actual_version", manifest.format_version)
            .field("expected_version", GUEST_STORAGE_FORMAT_VERSION)
            .field_display("path", path.display()));
    }
    Ok(())
}

/// Publishes the initial format marker atomically and durably.
fn write_manifest(path: &Path) -> Result<(), Report<GuestStorageError>> {
    let parent = path.parent().ok_or_else(|| {
        GuestStorageError::Initialization
            .report()
            .message("storage manifest has no parent directory")
            .field_display("path", path.display())
    })?;
    let temporary = parent.join(format!(".{FORMAT_MANIFEST}.tmp-{}", Uuid::new_v4()));
    let result = (|| -> io::Result<()> {
        let mut bytes = serde_json::to_vec(&StorageManifest {
            format_version: GUEST_STORAGE_FORMAT_VERSION,
        })
        .map_err(io::Error::other)?;
        bytes.push(b'\n');
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()
    })();
    if let Err(error) = result {
        if let Err(cleanup_error) = fs::remove_file(&temporary)
            && cleanup_error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %temporary.display(),
                %cleanup_error,
                "could not remove failed storage-manifest temporary file"
            );
        }
        return Err(initialization_io("publish storage manifest", path, error));
    }
    Ok(())
}

/// Removes safe temporary files left by interrupted initial publication.
fn cleanup_manifest_temps(root: &Path) -> Result<(), Report<GuestStorageError>> {
    let prefix = format!(".{FORMAT_MANIFEST}.tmp-");
    for entry in fs::read_dir(root)
        .map_err(|error| initialization_io("read persistent state root", root, error))?
    {
        let entry =
            entry.map_err(|error| initialization_io("read persistent state root", root, error))?;
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        fs::remove_file(&path).map_err(|error| {
            initialization_io("remove storage manifest temporary", &path, error)
        })?;
    }
    Ok(())
}

/// Creates one daemon-owned directory on the state filesystem.
fn ensure_directory(path: &Path, mode: u32) -> Result<PathBuf, Report<GuestStorageError>> {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(mode);
    builder
        .create(path)
        .map_err(|error| initialization_io("create storage directory", path, error))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| initialization_io("secure storage directory", path, error))?;
    Ok(path.to_owned())
}

/// Builds an incompatible-layout report for one managed path.
fn incompatible(message: &str, path: &Path) -> Report<GuestStorageError> {
    GuestStorageError::IncompatibleLayout
        .report()
        .message(message.to_owned())
        .field_display("path", path.display())
}

/// Builds an initialization report for a filesystem operation.
fn initialization_io(
    operation: &'static str,
    path: &Path,
    error: impl std::fmt::Display,
) -> Report<GuestStorageError> {
    GuestStorageError::Initialization
        .report()
        .message(error.to_string())
        .field("operation", operation)
        .field_display("path", path.display())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    /// Initializes every namespace with a private database and rebuildable
    /// scratch directory.
    #[test]
    fn initializes_the_versioned_layout() {
        let temporary = tempdir().unwrap();

        let layout = StorageLayout::initialize(temporary.path()).unwrap();

        assert_eq!(layout.root(), temporary.path());
        assert!(layout.database.is_dir());
        assert!(layout.input.root().is_dir());
        assert!(layout.store_root().is_dir());
        assert!(layout.repositories.root().is_dir());
        assert!(layout.chats.root().is_dir());
        assert!(layout.network.public().is_dir());
        assert!(layout.scratch.image_builds().is_dir());
        assert_eq!(
            fs::metadata(&layout.database).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let manifest: StorageManifest =
            serde_json::from_slice(&fs::read(temporary.path().join(FORMAT_MANIFEST)).unwrap())
                .unwrap();
        assert_eq!(manifest.format_version, GUEST_STORAGE_FORMAT_VERSION);
    }

    /// Accepts the NixOS-owned subvolume as the sole path created before the
    /// guest storage manifest.
    #[test]
    fn initializes_around_the_preexisting_nix_store() {
        let temporary = tempdir().unwrap();
        let nix_store = temporary.path().join(NIX_STORE_DIRECTORY);
        fs::create_dir(&nix_store).unwrap();
        fs::write(nix_store.join("probe"), b"preserved").unwrap();

        let layout = StorageLayout::initialize(temporary.path()).unwrap();

        assert_eq!(layout.nix_store.root, nix_store);
        assert_eq!(fs::read(nix_store.join("probe")).unwrap(), b"preserved");
    }

    /// Refuses to reinterpret the previous unversioned layout as the new
    /// storage format.
    #[test]
    fn rejects_unversioned_existing_state() {
        let temporary = tempdir().unwrap();
        fs::write(temporary.path().join("state.sqlite3"), []).unwrap();

        let error = StorageLayout::initialize(temporary.path()).unwrap_err();

        assert_eq!(error.error(), &GuestStorageError::IncompatibleLayout);
    }
}
