//! Guest-owned durable storage and its on-disk layout.
//!
//! [`GuestStorage`] is the single entry point for persistent guest state. It
//! initializes the versioned filesystem layout, opens the shared `SQLite`
//! database, and recovers the Btrfs store before feature services are
//! constructed. Typed namespace handles keep path derivation inside this
//! module while the database remains shared for cross-feature transactions.

//! # On-Disk Layout
//!
//! The guest has two state roots. Durable state lives on the workspace VM's
//! Btrfs data disk at `/var/lib/tascarrel`. Reconstructable runtime state lives
//! on the mount-private tmpfs at `/run/tascarrel`. Both roots can be overridden
//! through guestd arguments or environment variables, but the paths below are
//! the NixOS guest-module defaults.
//!
//! Guestd opens durable state only through `GuestStorage`. That boundary checks
//! the storage format version, owns all persistent path derivation, opens the
//! one shared `SQLite` database, and recovers the Btrfs image/pod store before
//! feature services start. Feature services receive the database, Btrfs store,
//! or typed namespace handles from it instead of constructing persistent paths
//! independently. The guest is a trusted appliance, so startup does not police
//! mount topology or exact file ownership beyond creating its namespaces with
//! their intended modes.
//!
//! ## Persistent State
//!
//! ```text
//! /var/lib/tascarrel/
//! ├── format.json
//! ├── database/
//! │   ├── state.sqlite3
//! │   ├── state.sqlite3-wal?
//! │   └── state.sqlite3-shm?
//! ├── input/
//! │   ├── current -> generation-<sha256>
//! │   ├── generation-<sha256>/
//! │   │   ├── config.toml
//! │   │   ├── .env
//! │   │   ├── image/
//! │   │   ├── overlay/
//! │   │   ├── hooks/
//! │   │   │   ├── setup/
//! │   │   │   └── init/
//! │   │   └── agents/
//! │   │       ├── AGENTS.md
//! │   │       ├── CLAUDE.md -> AGENTS.md
//! │   │       └── skills/
//! │   └── .workspace-input-<uuid>.tar?
//! ├── store/
//! │   ├── store.json
//! │   ├── selected-image.json?
//! │   ├── setup-seed.json?
//! │   ├── images/
//! │   │   └── <oci-digest>/
//! │   │       ├── manifest.json
//! │   │       └── root/
//! │   ├── image-staging/
//! │   │   └── <transaction>/
//! │   ├── image-publishing/
//! │   │   └── <transaction>/
//! │   │       ├── manifest.json
//! │   │       └── root/
//! │   ├── pods/
//! │   │   └── <pod-id>.json
//! │   ├── pod-roots/
//! │   │   └── <pod-id>/
//! │   ├── pod-workspaces/
//! │   │   └── <pod-id>/
//! │   ├── pod-docker/
//! │   │   └── <pod-id>/
//! │   ├── caches/
//! │   │   └── <cache-name>/
//! │   ├── workspace-seed/?
//! │   ├── setup-seeds/
//! │   │   └── <generation>/
//! │   │       ├── root/
//! │   │       └── workspace/
//! │   ├── transactions/
//! │   │   └── <transaction>.json
//! │   └── trash/
//! │       └── <transaction>/
//! ├── repositories/
//! │   ├── current? -> generations/<generation>
//! │   └── generations/
//! │       └── <generation>/
//! ├── chat/
//! │   ├── harnesses/
//! │   │   ├── codex/
//! │   │   │   ├── <version>/
//! │   │   │   └── current -> <version>
//! │   │   └── claude-code/
//! │   │       ├── <version>/
//! │   │       └── current -> <version>
//! │   ├── harness-codex/
//! │   ├── harness-claude-code/
//! │   ├── attachments/
//! │   │   ├── attachments/
//! │   │   │   └── <attachment-id>/
//! │   │   │       ├── content
//! │   │   │       └── metadata.json
//! │   │   ├── chats/
//! │   │   │   └── <chat-id>/
//! │   │   │       └── <attachment-id>
//! │   │   └── staging/
//! │   └── pricing/
//! │       └── models-dev.json?
//! ├── network/
//! │   └── public/
//! │       └── ca.pem?
//! ├── scratch/
//! │   └── image-builds/
//! │       └── tascarrel-image-build-<random>/
//! │           ├── buildkit/
//! │           ├── buildkitd.toml
//! │           ├── buildkitd.sock
//! │           ├── image.oci.tar
//! │           ├── oci-layout/
//! │           └── bundle/
//! └── nix-store/?
//!     └── nix/
//!         ├── store/
//!         └── var/nix/
//!             ├── db/
//!             ├── profiles/
//!             ├── temproots/
//!             ├── gc.lock
//!             ├── gcroots/tascarrel/runtime
//!             ├── gcroots/tascarrel/pods/
//!             │   └── <pod-id>/
//!             │       ├── state/profiles/
//!             │       └── roots/
//!             └── tascarrel-gc-root-trash/
//!                 └── <pod-id>/
//! ```
//!
//! The `format.json` file contains the top-level storage format version. This
//! implementation uses format 1. A nonempty unversioned root is rejected, so
//! the former layout is not migrated or adopted. The only path allowed to
//! precede initial publication of `format.json` is `nix-store`, because the
//! NixOS module initializes that Btrfs subvolume before starting guestd.
//! Unsupported versions fail closed; additional top-level entries are left
//! alone so future features can add namespaces without changing the format
//! merely to reserve a name.
//!
//! The database at `database/state.sqlite3` is shared by all guestd concerns.
//! It contains one ordered migration ledger plus durable pod lifecycle records,
//! pod identity-slot allocations, image build records, chat summaries, chat
//! turns, timeline entries, attachment metadata, and harness resumption
//! cursors. One database preserves cross-concern transactions and one migration
//! order. `SQLite` creates the WAL and shared-memory sidecars in the same
//! private directory.
//!
//! The `input` namespace contains content-addressed snapshots received from
//! hostd. The `current` symlink is atomically replaced and always points to a
//! complete generation. The temporary archive exists only while a new snapshot
//! is received or after an interrupted receive.
//!
//! The `store` namespace uses Tascarrel Btrfs store format 5. Published image
//! roots are read-only subvolumes. Each pod owns independent writable root,
//! workspace, and nested-Docker subvolumes. Cache entries are workspace-level
//! subvolumes shared into pods through idmapped mounts. Transaction, staging,
//! publishing, and trash directories make Btrfs mutations recoverable after
//! interruption. Publication waits only for the Btrfs transaction containing
//! the new metadata; it does not issue a full shared-filesystem sync. Pod and
//! image resource locks keep unrelated mutations independent.
//!
//! Repository checkout generations are read-only subvolumes below
//! `repositories`. The `store/workspace-seed` path is a writable snapshot of
//! the current checkout generation. A successful image setup can publish a
//! read-only root/workspace pair below `store/setup-seeds`; new pods snapshot
//! it while its image and input context still match. Executable links and Unix
//! sockets are not persisted with repository generations.
//!
//! The `chat` namespace contains harness installations, provider-owned
//! credential and session state, uploaded prompt attachments,
//! attachment-to-chat indexes, and cached model pricing. Harness installations
//! and attachment content are exposed to pods through separate read-only
//! mounts. Provider state is owned by the dedicated `tascarrel-harness` account
//! and can contain authentication material.
//!
//! The `network/public` namespace contains only public material derived from
//! host-owned network policy. Its `ca.pem` file lets running pods accept HTTPS
//! secret-injection rules added through a policy reload; the corresponding
//! private key never enters the guest.
//!
//! The `scratch/image-builds` namespace is persistent because `BuildKit` state,
//! OCI archives, and unpacking scratch can exceed the tmpfs runtime budget. Its
//! per-build directories are rebuildable and removed after a build or during
//! startup recovery.
//!
//! The `nix-store` namespace is a Btrfs subvolume initialized by the NixOS
//! guest module and used as a separate persistent Nix store for pods. The
//! module owns the normal Nix metadata and runtime closure root. Guestd owns
//! one direct-root child per Nix-enabled pod and atomically withdraws that
//! child through `tascarrel-gc-root-trash` during pod removal.
//!
//! Entries suffixed with `?` are conditional. Other collection directories can
//! be empty until their feature is used.
//!
//! ## Ownership and Permissions
//!
//! The persistent root and namespaces that UID-dropped processes must traverse
//! are searchable without being listable (`0711`) until a feature deliberately
//! exposes a narrower directory. The `database` and `scratch` namespaces are
//! root-only (`0700`), which also protects the `SQLite` database and its WAL
//! sidecars. The public network tree and published repository views are `0755`.
//!
//! Provider credential directories and files are owned by the dedicated harness
//! UID and use `0700` and `0600`. Shared cache subvolumes are writable through
//! idmapped mounts; pods in one workspace can therefore poison one another's
//! shared caches, which is within the documented pod trust model.
//!
//! Pods never mount `/var/lib/tascarrel` as a tree. Guestd exposes individual
//! root, workspace, Docker, cache, repository, harness, attachment, and
//! certificate mounts. Searchability of a guest ancestor does not expose
//! unrelated state in a pod mount namespace.
//!
//! ## Transient Runtime State
//!
//! ```text
//! /run/tascarrel/
//! ├── pod-nix-daemon/
//! │   └── socket
//! ├── local-binaries/?
//! ├── runc/
//! │   └── <pod-id>/...
//! ├── repos/
//! │   ├── state/                 # bind mount of persistent repositories/
//! │   ├── current -> state/current
//! │   ├── git-remote-tascarrel -> <guestd executable>
//! │   ├── tascarrel-git-receive-pack -> <guestd executable>
//! │   ├── push.sock
//! │   └── git-<uuid>.sock?
//! └── pods/
//!     └── <pod-id>/
//!         ├── bundle/
//!         │   ├── config.json
//!         │   ├── userns
//!         │   ├── mountns
//!         │   ├── resolv.conf
//!         │   ├── hosts
//!         │   ├── subuid?
//!         │   ├── subgid?
//!         │   ├── usb-devices.json
//!         │   ├── runc-create.log
//!         │   └── startup.log
//!         └── mounts/
//!             ├── rootfs/
//!             ├── workspace/
//!             ├── docker/
//!             └── share-<name>/
//! ```
//!
//! The repository view combines durable generations with boot-scoped helpers
//! and sockets. Guestd detaches any stale view, recursively bind-mounts the
//! persistent repository namespace at `state`, and atomically republishes the
//! helper links on every start. Pods receive the complete view through a
//! read-only recursive bind. This prevents guest-image upgrades from leaving a
//! persistent helper link to a removed Nix-store path.
//!
//! The `pods` tree contains OCI bundles, pinned namespace handles, bounded
//! startup logs, and mountpoints for durable pod storage and shares. The `runc`
//! tree is managed by runc. Per-clone Git sockets exist only for the lifetime
//! of their operation. The optional `local-binaries` path is a development-only
//! shared mount.
//!
//! The entire `/run/tascarrel` tree is discarded on reboot. Guestd reconstructs
//! pod runtime state from `SQLite`, the Btrfs store, and per-pod Nix roots.
//! Process state and process/image log buffers remain in memory.
//!
//! ## Other Guest-Owned State
//!
//! Some transient state cannot live below either primary root:
//!
//! - `/run/netns/tascarrel-build` pins the named image-build network namespace.
//! - `/dev/.tascarrel-usb` contains the curated devtmpfs device source exposed
//!   to pods.
//! - Pod veths, transparent network listeners, nftables rules, cgroups, and
//!   bind mounts live in kernel state and are reconciled by guestd.
//!
//! Workspace-input generations, repository generations, setup-seed generations,
//! configured caches, installed harness versions, archived chat history, and
//! attachments currently have no reachability garbage collector or quota.
//! Content addressing and Btrfs snapshots reduce duplication but do not bound
//! exclusive growth. A future collector must preserve current publications and
//! anything referenced by active pod, image, setup, or chat records while
//! retiring unreachable generations under an explicit policy.

mod database;
mod layout;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use database::Database;
pub use database::DatabaseError;
pub use layout::ChatStorage;
pub use layout::InputStorage;
pub use layout::NetworkStorage;
pub use layout::NixStoreStorage;
pub use layout::RepositoryStorage;
pub use layout::ScratchStorage;
use layout::StorageLayout;
use reportify::ErrorExt as _;
use reportify::Report;
use thiserror::Error;

use crate::runtime::pod::BtrfsStore;

/// The fully initialized persistence boundary for one workspace guest.
pub struct GuestStorage {
    layout: StorageLayout,
    database: Database,
    store: Arc<BtrfsStore>,
}

impl GuestStorage {
    /// Initializes and opens every guest-owned durable storage component.
    ///
    /// # Errors
    ///
    /// Returns [`GuestStorageError`] when the root is incompatible, the
    /// database cannot be migrated, or the Btrfs store cannot recover.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(root = %config.root.display())
    )]
    pub async fn open(config: GuestStorageConfig) -> Result<Self, Report<GuestStorageError>> {
        let layout = StorageLayout::initialize(&config.root)?;
        let database = Database::open(layout.database_path())
            .await
            .map_err(|report| report.escalate(GuestStorageError::Database))?;

        let store_root = layout.store_root().to_owned();
        let btrfs_program = config.btrfs_program;
        let btrfs_operation_timeout = config.btrfs_operation_timeout;
        let store = tokio::task::spawn_blocking(move || {
            BtrfsStore::open_with_timeout(store_root, btrfs_program, btrfs_operation_timeout)
        })
        .await
        .map_err(|error| {
            GuestStorageError::Store
                .report()
                .message(format!("store initialization task failed: {error}"))
        })?
        .map_err(|error| GuestStorageError::Store.report().message(error.to_string()))?;

        Ok(Self {
            layout,
            database,
            store: Arc::new(store),
        })
    }

    /// Returns the canonical persistent state root.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.layout.root()
    }

    /// Returns the shared database used by feature state repositories.
    #[must_use]
    pub const fn database(&self) -> &Database {
        &self.database
    }

    /// Returns the host-published workspace-input namespace.
    #[must_use]
    pub fn input(&self) -> &InputStorage {
        self.layout.input()
    }

    /// Returns the shared Btrfs image, pod, setup-seed, and cache store.
    #[must_use]
    pub fn store(&self) -> Arc<BtrfsStore> {
        Arc::clone(&self.store)
    }

    /// Returns the immutable repository-generation namespace.
    #[must_use]
    pub fn repositories(&self) -> &RepositoryStorage {
        self.layout.repositories()
    }

    /// Returns the chat artifact and provider-state namespace.
    #[must_use]
    pub fn chats(&self) -> &ChatStorage {
        self.layout.chats()
    }

    /// Returns the public network-material namespace.
    #[must_use]
    pub fn network(&self) -> &NetworkStorage {
        self.layout.network()
    }

    /// Returns the externally managed persistent pod Nix store location.
    #[must_use]
    pub fn nix_store(&self) -> &NixStoreStorage {
        self.layout.nix_store()
    }

    /// Returns the rebuildable persistent scratch namespace.
    #[must_use]
    pub fn scratch(&self) -> &ScratchStorage {
        self.layout.scratch()
    }
}

/// Concrete configuration required to open [`GuestStorage`].
#[derive(Clone, Debug)]
pub struct GuestStorageConfig {
    root: PathBuf,
    btrfs_program: PathBuf,
    btrfs_operation_timeout: Duration,
}

impl GuestStorageConfig {
    /// Creates storage configuration for one persistent root.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, btrfs_program: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            btrfs_program: btrfs_program.into(),
            btrfs_operation_timeout: Duration::from_mins(2),
        }
    }

    /// Sets the availability deadline for one Btrfs operation.
    #[must_use]
    pub const fn with_btrfs_operation_timeout(mut self, timeout: Duration) -> Self {
        self.btrfs_operation_timeout = timeout;
        self
    }
}

/// Caller-relevant failures while opening the guest persistence boundary.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum GuestStorageError {
    /// The configured storage root is invalid.
    #[error("guest storage configuration is invalid")]
    InvalidConfiguration,
    /// Existing state does not match the supported layout.
    #[error("guest storage layout is incompatible with this binary")]
    IncompatibleLayout,
    /// The filesystem layout could not be initialized.
    #[error("failed to initialize the guest storage layout")]
    Initialization,
    /// The shared database could not be opened or migrated.
    #[error("failed to open the guest storage database")]
    Database,
    /// The Btrfs store could not be opened or recovered.
    #[error("failed to open the guest Btrfs store")]
    Store,
}

/// On-disk layout version supported by this guest daemon.
const GUEST_STORAGE_FORMAT_VERSION: u32 = 1;
