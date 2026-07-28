//! Guest daemon process composition and transport lifecycle.
//!
//! The binary validates workspace inputs, constructs the guest-owned feature
//! services, and serves workspace-scoped control-plane and Git connections.

use std::collections::BTreeMap;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context as TaskContext;
use std::task::Poll;
use std::task::ready;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use async_trait::async_trait;
use clap::Parser;
use nix::unistd::Uid;
use nix::unistd::User;
use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use tascarrel_api::types::guest::GuestInstanceId;
use tascarrel_guest::CODE_EDITOR_CACHE_NAME;
use tascarrel_guest::CODE_EDITOR_PROFILE_PATH;
use tascarrel_guest::ChangesService;
use tascarrel_guest::ChangesServiceConfig;
use tascarrel_guest::ChatService;
use tascarrel_guest::CodeService;
use tascarrel_guest::CodeServiceConfig;
use tascarrel_guest::DEFAULT_DEVICE_PATH;
use tascarrel_guest::Executor;
use tascarrel_guest::FilesService;
use tascarrel_guest::FilesServiceError;
use tascarrel_guest::GuestControlService;
use tascarrel_guest::GuestNetworkService;
use tascarrel_guest::GuestNetworkServiceConfig;
use tascarrel_guest::GuestRepositoryManager;
use tascarrel_guest::GuestService;
use tascarrel_guest::GuestServiceConfig;
use tascarrel_guest::GuestServices;
use tascarrel_guest::GuestState;
use tascarrel_guest::GuestStorage;
use tascarrel_guest::GuestStorageConfig;
use tascarrel_guest::ImageBuilderConfig;
use tascarrel_guest::ImageInputRefresh;
use tascarrel_guest::ImageService;
use tascarrel_guest::ImageServiceConfig;
use tascarrel_guest::NetworkManager;
use tascarrel_guest::PodControlConnection;
use tascarrel_guest::PodInitStep;
use tascarrel_guest::PodPolicy;
use tascarrel_guest::PodPrograms;
use tascarrel_guest::PodServiceError;
use tascarrel_guest::PodShare;
use tascarrel_guest::ProcessSupervisor;
use tascarrel_guest::ProcessSupervisorConfig;
use tascarrel_guest::RepositoryConfigProvider;
use tascarrel_guest::RepositoryConfigSnapshot;
use tascarrel_guest::UsbGuest;
use tascarrel_guest::WorkspaceCaConfig;
use tascarrel_guest::WorkspaceConfig;
use tascarrel_mux::Channel;
use tascarrel_mux::Config as MuxConfig;
use tascarrel_mux::Incoming;
use tascarrel_mux::IncomingRequest;
use tascarrel_mux::MuxHandle;
use tascarrel_mux::Role as MuxRole;
use tascarrel_mux::connect as connect_mux;
use tascarrel_protocol::ChatAttachmentReadRequest;
use tascarrel_protocol::ChatAttachmentReadResponse;
use tascarrel_protocol::ChatAttachmentUploadRequest;
use tascarrel_protocol::ChatAttachmentUploadResponse;
use tascarrel_protocol::ErrorCode;
use tascarrel_protocol::Framed;
use tascarrel_protocol::GuestControlIdentity;
use tascarrel_protocol::MAX_WORKSPACE_ENVIRONMENT_FRAME_LEN;
use tascarrel_protocol::MUX_CA_HOST_ENDPOINT;
use tascarrel_protocol::MUX_CHAT_ATTACHMENT_READ_ENDPOINT;
use tascarrel_protocol::MUX_CHAT_ATTACHMENT_UPLOAD_ENDPOINT;
use tascarrel_protocol::MUX_CONTROL_PLANE_ENDPOINT;
use tascarrel_protocol::MUX_PUBLISH_GUEST_ENDPOINT;
use tascarrel_protocol::MUX_WORKSPACE_ENVIRONMENT_HOST_ENDPOINT;
use tascarrel_protocol::MUX_WORKSPACE_FILE_READ_ENDPOINT;
use tascarrel_protocol::MUX_WORKSPACE_HOST_ENDPOINT;
use tascarrel_protocol::Pod;
use tascarrel_protocol::PodId;
use tascarrel_protocol::PublishedPortConnect;
use tascarrel_protocol::PublishedPortConnectResponse;
use tascarrel_protocol::RemoteError;
use tascarrel_protocol::WorkspaceEnvironmentResponse;
use tascarrel_protocol::WorkspaceFileReadRequest;
use tascarrel_protocol::WorkspaceFileReadResponse;
use tascarrel_protocol::workspace_snapshot;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadBuf;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixListener;
use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tokio::time::timeout;
use tracing::debug;
use tracing::info;
use tracing::warn;

/// Failure while accepting or dispatching one pod-private mux connection.
#[derive(Debug, Error)]
enum PodControlListenerError {
    /// The pod listener could not accept its next connection.
    #[error("failed to accept a pod control connection")]
    Accept,
    /// The pod multiplexer could not be configured or driven.
    #[error("pod control multiplexer failed")]
    Multiplexer,
    /// The task driving the pod multiplexer failed.
    #[error("pod control multiplexer task failed")]
    MultiplexerTask,
    /// A logical pod channel could not be accepted or rejected.
    #[error("pod logical channel operation failed")]
    LogicalChannel,
    /// Guestd could not resolve the listener's authenticated identity.
    #[error("failed to resolve the pod control identity")]
    Identity,
}

const DEFAULT_WORKSPACE_INPUT_TRANSFER_TIMEOUT_SECONDS: u64 = 120;

#[derive(Debug, Parser)]
#[command(name = "tascarrel-guest", about = "Tascarrel guest pod daemon")]
struct Args {
    /// Host-assigned identity of this guest daemon incarnation.
    #[arg(long, env = "TASCARREL_GUEST_INSTANCE_ID")]
    guest_instance_id: GuestInstanceId,

    /// Dedicated unprivileged VM account used for harness credential
    /// operations.
    #[arg(
        long,
        env = "TASCARREL_GUEST_HARNESS_USER",
        default_value = "tascarrel-harness"
    )]
    harness_user: String,

    /// Read-only code-server distribution embedded in the Tascarrel guest
    /// image.
    #[arg(long, env = "TASCARREL_GUEST_CODE_SERVER")]
    code_server: PathBuf,

    /// Immutable Git used to build repository checkout generations.
    #[arg(
        long,
        env = "TASCARREL_GUEST_GIT",
        default_value = "/run/current-system/sw/bin/git"
    )]
    git: PathBuf,
    /// Virtio-serial character device to open.
    #[arg(long, env = "TASCARREL_GUEST_DEVICE", default_value = DEFAULT_DEVICE_PATH)]
    device: PathBuf,

    /// Listen on a Unix socket instead of opening the virtio device
    /// (development).
    #[arg(long, env = "TASCARREL_GUEST_UNIX_SOCKET")]
    unix_socket: Option<PathBuf>,

    /// Maximum time to receive one host-published workspace input snapshot.
    #[arg(
        long,
        env = "TASCARREL_GUEST_WORKSPACE_INPUT_TRANSFER_TIMEOUT_SECONDS",
        default_value_t = DEFAULT_WORKSPACE_INPUT_TRANSFER_TIMEOUT_SECONDS
    )]
    workspace_input_transfer_timeout_seconds: u64,

    #[arg(
        long,
        env = "TASCARREL_GUEST_SETSID",
        default_value = "/run/current-system/sw/bin/setsid"
    )]
    setsid: PathBuf,

    /// Immutable setup shell injected into every image, including `FROM
    /// scratch`.
    #[arg(long, env = "TASCARREL_GUEST_LOGIN_SHELL")]
    login_shell: PathBuf,

    /// Immutable default interactive shell used when a pod does not configure
    /// `SHELL`.
    #[arg(long, env = "TASCARREL_GUEST_TERMINAL_SHELL")]
    terminal_shell: PathBuf,

    /// Immutable Tascarrel pod PID 1.
    #[arg(long, env = "TASCARREL_GUEST_PODD")]
    podd: PathBuf,

    /// Immutable Tascarrel client injected into every pod.
    #[arg(long, env = "TASCARREL_GUEST_PODCTL")]
    podctl: PathBuf,

    /// Immutable Tasci harness injected into every pod.
    #[arg(long, env = "TASCARREL_GUEST_TASCI")]
    tasci: PathBuf,

    /// Immutable nested Docker daemon and client.
    #[arg(long, env = "TASCARREL_GUEST_DOCKERD")]
    dockerd: PathBuf,

    #[arg(long, env = "TASCARREL_GUEST_DOCKER")]
    docker: PathBuf,

    /// Immutable Podman client injected when the Podman feature is enabled.
    #[arg(long, env = "TASCARREL_GUEST_PODMAN")]
    podman: PathBuf,

    /// Immutable subordinate-UID helper injected with Podman.
    #[arg(long, env = "TASCARREL_GUEST_NEWUIDMAP")]
    newuidmap: PathBuf,

    /// Immutable subordinate-GID helper injected with Podman.
    #[arg(long, env = "TASCARREL_GUEST_NEWGIDMAP")]
    newgidmap: PathBuf,

    /// Immutable Nix client injected when the pod Nix daemon is enabled.
    #[arg(long, env = "TASCARREL_GUEST_NIX")]
    nix: PathBuf,

    /// Persistent Btrfs mount for pod storage, state, and image-build scratch.
    #[arg(
        long,
        env = "TASCARREL_GUEST_STATE_DIR",
        default_value = "/var/lib/tascarrel"
    )]
    state_directory: PathBuf,

    /// Ephemeral root for OCI bundles and runc state.
    #[arg(
        long,
        env = "TASCARREL_GUEST_RUNTIME_DIR",
        default_value = "/run/tascarrel"
    )]
    runtime_dir: PathBuf,

    /// Read-only Dockerfile context exported by the host.
    #[arg(
        long,
        env = "TASCARREL_GUEST_IMAGE_DIR",
        default_value = "/run/tascarrel/image"
    )]
    image_dir: PathBuf,

    /// Read-only workspace policy exported by the host.
    #[arg(
        long,
        env = "TASCARREL_GUEST_WORKSPACE_CONFIG",
        default_value = "/run/tascarrel/config/config.toml"
    )]
    workspace_config: PathBuf,

    #[arg(long, env = "TASCARREL_GUEST_NIX_STORE", default_value = "/nix/store")]
    nix_store: PathBuf,

    /// Physical directory containing the pod Nix daemon socket.
    #[arg(
        long,
        env = "TASCARREL_GUEST_POD_NIX_DAEMON_SOCKET_DIR",
        default_value = "/nix/var/nix/daemon-socket"
    )]
    pod_nix_daemon_socket_dir: PathBuf,

    #[arg(
        long,
        env = "TASCARREL_GUEST_BTRFS",
        default_value = "/run/current-system/sw/bin/btrfs"
    )]
    btrfs: PathBuf,

    #[arg(
        long,
        env = "TASCARREL_GUEST_RUNC",
        default_value = "/run/current-system/sw/bin/runc"
    )]
    runc: PathBuf,

    #[arg(
        long,
        env = "TASCARREL_GUEST_MOUNT",
        default_value = "/run/current-system/sw/bin/mount"
    )]
    mount: PathBuf,

    #[arg(
        long,
        env = "TASCARREL_GUEST_UMOUNT",
        default_value = "/run/current-system/sw/bin/umount"
    )]
    umount: PathBuf,

    #[arg(
        long,
        env = "TASCARREL_GUEST_UNSHARE",
        default_value = "/run/current-system/sw/bin/unshare"
    )]
    unshare: PathBuf,

    #[arg(
        long,
        env = "TASCARREL_GUEST_NSENTER",
        default_value = "/run/current-system/sw/bin/nsenter"
    )]
    nsenter: PathBuf,

    #[arg(
        long,
        env = "TASCARREL_GUEST_BUILDKITD",
        default_value = "/run/current-system/sw/bin/buildkitd"
    )]
    buildkitd: PathBuf,

    #[arg(
        long,
        env = "TASCARREL_GUEST_BUILDCTL",
        default_value = "/run/current-system/sw/bin/buildctl"
    )]
    buildctl: PathBuf,

    #[arg(
        long,
        env = "TASCARREL_GUEST_UMOCI",
        default_value = "/run/current-system/sw/bin/umoci"
    )]
    umoci: PathBuf,

    #[arg(
        long,
        env = "TASCARREL_GUEST_TAR",
        default_value = "/run/current-system/sw/bin/tar"
    )]
    tar: PathBuf,

    #[arg(
        long,
        env = "TASCARREL_GUEST_CP",
        default_value = "/run/current-system/sw/bin/cp"
    )]
    cp: PathBuf,

    /// iproute2 executable used to create the synthetic default route.
    #[arg(
        long,
        env = "TASCARREL_GUEST_IP",
        default_value = "/run/current-system/sw/bin/ip"
    )]
    ip: PathBuf,

    /// nftables executable used to install per-principal network-service
    /// redirects.
    #[arg(
        long,
        env = "TASCARREL_GUEST_NFT",
        default_value = "/run/current-system/sw/bin/nft"
    )]
    nft: PathBuf,
}

struct WorkspaceInput {
    root: PathBuf,
    network_service: Arc<GuestNetworkService>,
    refresh_runtime_input: bool,
    published_generation_revision: Mutex<u64>,
    refresh_operation: Mutex<()>,
    transfer_timeout: Duration,
}

impl WorkspaceInput {
    fn current(&self) -> PathBuf {
        self.root.join("current")
    }

    /// Refreshes the published input while retaining a complete cached
    /// generation when the host snapshot is temporarily unavailable.
    async fn refresh_or_retain_cached(&self) -> Result<PathBuf, RemoteError> {
        match self.refresh().await {
            Ok(generation) => Ok(generation),
            Err(error) => {
                let current = self.current();
                let Ok(generation) = self.cached_generation().await else {
                    return Err(error);
                };
                warn!(
                    %error,
                    input = %current.display(),
                    "workspace input refresh failed; continuing with the last published input"
                );
                Ok(generation)
            }
        }
    }

    /// Coalesces concurrent callers around one atomic snapshot publication.
    async fn refresh(&self) -> Result<PathBuf, RemoteError> {
        let published_generation_revision_before_wait =
            *self.published_generation_revision.lock().await;
        let _operation = self.refresh_operation.lock().await;
        if *self.published_generation_revision.lock().await
            > published_generation_revision_before_wait
        {
            return self.cached_generation().await;
        }
        let temporary = self
            .root
            .join(format!(".workspace-input-{}.tar", uuid::Uuid::new_v4()));
        let snapshot_transfer =
            match timeout(self.transfer_timeout, self.download_snapshot(&temporary)).await {
                Ok(result) => result,
                Err(_) => Err(image_provider_error(format!(
                    "timed out receiving workspace input snapshot after {:?}",
                    self.transfer_timeout
                ))),
            };
        if let Err(error) = snapshot_transfer {
            self.remove_temporary_snapshot(&temporary).await;
            return Err(error);
        }

        let root = self.root.clone();
        let archive = temporary.clone();
        let published_generation = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
            let generation = workspace_snapshot::publish_snapshot(&archive, &root)?;
            make_workspace_runtime_inputs_readable(&generation.join("overlay"))?;
            make_workspace_runtime_inputs_readable(&generation.join("hooks"))?;
            make_workspace_runtime_inputs_readable(&generation.join("agents"))?;
            Ok(generation)
        })
        .await
        .map_err(|error| {
            image_provider_error(format!("workspace publication task failed: {error}"))
        })?
        .map_err(image_provider_error);
        self.remove_temporary_snapshot(&temporary).await;
        let generation = published_generation?;
        let mut published_generation_revision = self.published_generation_revision.lock().await;
        *published_generation_revision = published_generation_revision.saturating_add(1);
        Ok(generation)
    }

    /// Streams one host snapshot into a new private archive.
    async fn download_snapshot(&self, temporary: &Path) -> Result<(), RemoteError> {
        let mut channel = self
            .network_service
            .open_channel(MUX_WORKSPACE_HOST_ENDPOINT)
            .await?;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options
            .open(temporary)
            .await
            .map_err(image_provider_error)?;
        let copied = tokio::io::copy(
            &mut (&mut channel).take(workspace_snapshot::MAX_ARCHIVE_BYTES + 1),
            &mut file,
        )
        .await
        .map_err(image_provider_error)?;
        channel.shutdown().await.map_err(image_provider_error)?;
        file.sync_all().await.map_err(image_provider_error)?;
        drop(file);
        if copied > workspace_snapshot::MAX_ARCHIVE_BYTES {
            return Err(image_provider_error(
                "workspace input snapshot is too large",
            ));
        }
        Ok(())
    }

    /// Resolves `current` only when it names one complete direct child
    /// generation.
    async fn cached_generation(&self) -> Result<PathBuf, RemoteError> {
        let current = self.current();
        if !current.join("config.toml").is_file() || !current.join("image").is_dir() {
            return Err(image_provider_error("cached workspace input is incomplete"));
        }
        let generation = tokio::fs::canonicalize(&current)
            .await
            .map_err(image_provider_error)?;
        let root = tokio::fs::canonicalize(&self.root)
            .await
            .map_err(image_provider_error)?;
        if generation.parent() != Some(root.as_path()) {
            return Err(image_provider_error(
                "cached workspace input escaped its storage root",
            ));
        }
        Ok(generation)
    }

    async fn remove_temporary_snapshot(&self, temporary: &Path) {
        if let Err(error) = tokio::fs::remove_file(temporary).await
            && error.kind() != io::ErrorKind::NotFound
        {
            warn!(path = %temporary.display(), %error, "could not remove workspace input snapshot");
        }
    }
}

#[async_trait]
impl ImageInputRefresh for WorkspaceInput {
    async fn refresh_image_input(
        &self,
    ) -> Result<Option<PathBuf>, reportify::Report<tascarrel_guest::ImageServiceError>> {
        if !self.refresh_runtime_input {
            return Ok(None);
        }
        self.refresh_or_retain_cached()
            .await
            .map(|generation| Some(generation.join("image")))
            .map_err(|error| {
                reportify::Report::new(tascarrel_guest::ImageServiceError::Internal(format!(
                    "failed to refresh workspace image input: {error}"
                )))
            })
    }
}

#[async_trait]
impl RepositoryConfigProvider for WorkspaceInput {
    async fn repository_config(&self) -> Result<RepositoryConfigSnapshot, RemoteError> {
        let generation = self.refresh_or_retain_cached().await?;
        WorkspaceConfig::load(&generation.join("config.toml"))
            .map(|workspace| RepositoryConfigSnapshot {
                repositories: workspace.repos,
                image_definition_directory: Some(generation.join("image")),
                workspace_overlay_directory: Some(generation.join("overlay")),
            })
            .map_err(image_provider_error)
    }
}

fn make_workspace_runtime_inputs_readable(root: &Path) -> io::Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            let mode = metadata.permissions().mode() | 0o005;
            fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        } else if metadata.is_file() {
            let mode = metadata.permissions().mode() | 0o004;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

fn parse_environment_file(bytes: &[u8]) -> Result<std::collections::BTreeMap<String, String>> {
    const MAX_BYTES: usize = 64 * 1024;
    const MAX_ENTRIES: usize = 128;
    if bytes.len() > MAX_BYTES {
        bail!(".env exceeds {MAX_BYTES} bytes");
    }
    let text = std::str::from_utf8(bytes).context(".env is not UTF-8")?;
    let mut environment = std::collections::BTreeMap::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (name, raw_value) = line
            .split_once('=')
            .with_context(|| format!(".env line {} lacks '='", index + 1))?;
        let name = name.trim();
        let mut bytes = name.bytes();
        if !bytes
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
            || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        {
            bail!(".env line {} has an invalid name", index + 1);
        }
        let value = raw_value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('\"') && value.ends_with('\"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if value.contains('\0') {
            bail!(".env line {} contains NUL", index + 1);
        }
        if environment
            .insert(name.to_owned(), value.to_owned())
            .is_some()
        {
            bail!(".env defines {name:?} more than once");
        }
        if environment.len() > MAX_ENTRIES {
            bail!(".env has more than {MAX_ENTRIES} entries");
        }
    }
    Ok(environment)
}

fn image_provider_error(error: impl std::fmt::Display) -> RemoteError {
    const MAX_CHARS: usize = 2048;
    const HEAD_CHARS: usize = 512;

    let detail = error.to_string();
    let count = detail.chars().count();
    let detail = if count <= MAX_CHARS {
        detail
    } else {
        let head = detail.chars().take(HEAD_CHARS).collect::<String>();
        let tail = detail
            .chars()
            .skip(count - (MAX_CHARS - HEAD_CHARS))
            .collect::<String>();
        format!("{head}\n... diagnostic truncated ...\n{tail}")
    };
    RemoteError::new(ErrorCode::ExecutionFailed, detail)
}

#[tokio::main]
#[allow(clippy::too_many_lines)] // Startup keeps the ordered fail-closed resource wiring visible.
async fn main() -> Result<()> {
    let invoked_as = std::env::args_os()
        .next()
        .as_deref()
        .and_then(|argument| Path::new(argument).file_name())
        .map(ToOwned::to_owned);
    if let Some("git-remote-tascarrel") = invoked_as.as_deref().and_then(|name| name.to_str()) {
        run_git_remote_helper().context("run Git remote helper")?;
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tascarrel_guest=info".into()),
        )
        .init();
    let args = Args::parse();
    if !Uid::effective().is_root() {
        bail!("tascarrel-guest must run as root so it can manage the pod runtime");
    }
    if args.workspace_input_transfer_timeout_seconds == 0 {
        bail!("workspace input transfer timeout must be greater than zero");
    }
    let network_service = GuestNetworkService::new(GuestNetworkServiceConfig {
        ip: args.ip.clone(),
        nft: args.nft.clone(),
        ..GuestNetworkServiceConfig::default()
    })
    .map_err(|error| anyhow!(error.to_string()))
    .context("create guest network service")?;
    network_service.initialize().await?;

    let storage = GuestStorage::open(GuestStorageConfig::new(&args.state_directory, &args.btrfs))
        .await
        .map_err(|error| anyhow!("{error}"))
        .context("open guest storage")?;
    let guest = GuestService::start(
        args.guest_instance_id,
        GuestServiceConfig::new(storage.root()),
    )
    .map_err(|error| anyhow!("{error}"))
    .context("failed to start guest information and metrics service")?;
    let database = storage.database().clone();
    let harness_user = User::from_name(&args.harness_user)
        .context("resolve dedicated harness user")?
        .ok_or_else(|| {
            anyhow!(
                "dedicated harness user {:?} does not exist",
                args.harness_user
            )
        })?;
    if harness_user.uid.is_root() {
        bail!("dedicated harness user must not be root");
    }
    let chats = ChatService::open(
        database.clone(),
        storage.chats(),
        harness_user.uid.as_raw(),
        harness_user.gid.as_raw(),
        args.tasci.clone(),
    )
    .await
    .map_err(|error| anyhow!("{error}"))
    .context("open workspace chat service")?;
    let input = Arc::new(WorkspaceInput {
        root: storage.input().root().to_owned(),
        network_service: Arc::clone(&network_service),
        refresh_runtime_input: args.unix_socket.is_none(),
        published_generation_revision: Mutex::new(0),
        refresh_operation: Mutex::new(()),
        transfer_timeout: Duration::from_secs(args.workspace_input_transfer_timeout_seconds),
    });
    let mut initial_session = if args.unix_socket.is_none() {
        let device = open_device(&args.device).await;
        let session = MuxSession::connect(Box::new(device))?;
        network_service.attach_mux(session.handle.clone());
        input.refresh_or_retain_cached().await.map_err(|error| {
            anyhow!(error)
                .context("refresh workspace input without a durable cached workspace generation")
        })?;
        Some(session)
    } else {
        None
    };
    let (workspace_config_path, image_dir) = if initial_session.is_some() {
        (
            input.current().join("config.toml"),
            input.current().join("image"),
        )
    } else {
        (args.workspace_config.clone(), args.image_dir.clone())
    };
    let (workspace, workspace_diagnostic) = WorkspaceConfig::load_degraded(&workspace_config_path);
    if let Some(message) = workspace_diagnostic {
        warn!(%message, config = %workspace_config_path.display(), "using safe workspace defaults");
    }
    let uses_host_secrets = workspace
        .env
        .values()
        .any(|value| value.contains("${secrets."));
    let workspace_environment = if let Some(session) = initial_session.as_ref() {
        match fetch_workspace_environment(&session.handle).await {
            Ok(environment) => environment,
            Err(error) if !uses_host_secrets => {
                warn!(%error, "host-resolved workspace environment unavailable; using config values");
                workspace.env.clone()
            }
            Err(error) => {
                return Err(error).context("resolve workspace secrets in the startup environment");
            }
        }
    } else if uses_host_secrets {
        bail!("workspace environment uses secrets but no host transport is available");
    } else {
        workspace.env.clone()
    };
    info!(
        rootless_containers = workspace.rootless_containers(),
        nested_containers = workspace.nested_containers(),
        virtualization = workspace.features.virtualization,
        docker = workspace.features.docker,
        podman = workspace.features.podman,
        nix_daemon = workspace.nix.daemon,
        caches = workspace.caches.len(),
        config = %workspace_config_path.display(),
        "loaded workspace policy"
    );
    let system_principal = Pod {
        id: PodId("workspace-system".into()),
        name: "workspace system services".into(),
        title: None,
        user: "root".into(),
        uid: 0,
        gid: 0,
        created_at_unix_ms: 0,
        health: tascarrel_protocol::Health::healthy(),
    };
    let system_mapping = network_service
        .activate_guest(&system_principal)
        .await
        .map_err(|error| anyhow!(error))
        .context("activate workspace system network service")?;
    let harness_principal = Pod {
        id: PodId("chat-harness".into()),
        name: "workspace chat harnesses".into(),
        title: None,
        user: harness_user.name.clone(),
        uid: harness_user.uid.as_raw(),
        gid: harness_user.gid.as_raw(),
        created_at_unix_ms: 0,
        health: tascarrel_protocol::Health::healthy(),
    };
    let harness_mapping = network_service
        .activate_system(&harness_principal)
        .await
        .map_err(|error| anyhow!(error))
        .context("activate workspace chat harness network service")?;
    let service_network = [
        (system_principal, system_mapping),
        (harness_principal, harness_mapping),
    ];
    chats.start_eager_installation();

    let pod_runtime = args.runtime_dir.join("pods");
    let runc_root = args.runtime_dir.join("runc");
    let build_root = storage.scratch().image_builds().to_owned();
    ensure_private_directory(&runc_root)?;
    // runc and the UID-dropped repository Git process resolve paths owned by
    // guestd. Search-only access to these ancestors permits that traversal
    // without making their contents listable; pods never receive the guest
    // host's data tree in their mount namespaces.
    for directory in [&args.runtime_dir, &pod_runtime] {
        ensure_searchable_directory(directory)?;
    }
    let store = storage.store();
    let mut image_service_builder = ImageBuilderConfig::new(
        &args.buildkitd,
        &args.buildctl,
        &args.umoci,
        &args.tar,
        &args.cp,
    );
    image_service_builder.temporary_root = build_root.clone();
    let mut image_service_config =
        ImageServiceConfig::new(&image_dir, &args.ip, &args.nsenter, image_service_builder);
    image_service_config.setup_scripts = workspace
        .setup
        .steps
        .iter()
        .map(|step| step.script.clone())
        .collect();
    let images = ImageService::open(image_service_config, database.clone(), Arc::clone(&store))
        .await
        .map_err(|error| anyhow!("{error}"))
        .context("open workspace image service")?;
    let mut shares = workspace
        .caches
        .iter()
        .map(|cache| {
            let source = store
                .ensure_cache(&cache.name)
                .with_context(|| format!("provision workspace cache {:?}", cache.name))?;
            PodShare::new(&cache.name, source, &cache.path)
                .with_context(|| format!("configure workspace cache {:?}", cache.name))
        })
        .collect::<Result<Vec<_>>>()?;
    let code_editor_profile = store
        .ensure_cache(CODE_EDITOR_CACHE_NAME)
        .context("provision shared Code editor profile")?;
    shares.push(
        PodShare::new(
            CODE_EDITOR_CACHE_NAME,
            code_editor_profile,
            CODE_EDITOR_PROFILE_PATH,
        )
        .context("configure shared Code editor profile")?,
    );
    shares.push(PodShare::agent_harnesses(storage.chats().harnesses())?);
    shares.push(PodShare::chat_attachments(
        storage.chats().attachment_binding_source(),
    )?);
    shares.push(PodShare::new(
        "chat-codex-credentials",
        storage.chats().codex_state(),
        "/opt/tascarrel/chat/harness-codex",
    )?);
    shares.push(PodShare::new(
        "chat-claude-code-credentials",
        storage.chats().claude_code_state(),
        "/opt/tascarrel/chat/harness-claude-code",
    )?);
    let code_server = fs::canonicalize(&args.code_server).with_context(|| {
        format!(
            "resolve embedded code-server distribution {}",
            args.code_server.display()
        )
    })?;
    if !code_server.is_dir() {
        bail!(
            "embedded code-server distribution must be a directory: {}",
            code_server.display()
        );
    }
    shares.push(PodShare::code_server(code_server)?);
    let ca_path = if workspace.network.secret_injection.is_empty() {
        None
    } else {
        // Pod recovery happens before the virtio-serial transport is served.
        // Keep the public certificate in durable workspace state so restored
        // pods never depend on a transport that guestd has not opened yet.
        let directory = storage.network().public().to_owned();
        shares.push(PodShare::workspace_authority(&directory)?);
        Some(storage.network().authority_certificate())
    };
    let network = NetworkManager::new(&args.ip, &args.nsenter).context("configure pod network")?;
    let workspace_overlay = Some(
        workspace_config_path
            .parent()
            .expect("workspace config has an absolute parent")
            .join("overlay"),
    );
    let repository_config: Option<Arc<dyn RepositoryConfigProvider>> = initial_session
        .is_some()
        .then(|| input.clone() as Arc<dyn RepositoryConfigProvider>);
    let repository_manager = if workspace.repos.is_empty() && workspace_overlay.is_none() {
        None
    } else {
        let manager = GuestRepositoryManager::new(
            workspace.repos.clone(),
            Arc::clone(&store),
            Arc::clone(&network_service),
            storage.repositories(),
            args.runtime_dir.join("repos"),
            args.git.clone(),
            args.btrfs.clone(),
            args.cp.clone(),
            workspace_overlay,
        )?;
        Some(manager)
    };

    let programs = PodPrograms::new(
        &args.nix_store,
        &args.podd,
        &args.podctl,
        &args.tasci,
        &args.login_shell,
        &args.terminal_shell,
        &args.dockerd,
        &args.docker,
        &args.podman,
        &args.newuidmap,
        &args.newgidmap,
        &args.nix,
    )
    .context("validate immutable pod programs")?;
    if let Some(parent) = workspace_config_path.parent() {
        let hooks = parent.join("hooks");
        if hooks.is_dir() {
            shares.push(PodShare::workspace_hooks(fs::canonicalize(hooks)?)?);
        }
        let agents = parent.join("agents");
        if agents.is_dir() {
            shares.push(PodShare::workspace_agents(fs::canonicalize(&agents)?)?);
            let skills = agents.join("skills");
            if skills.is_dir() {
                shares.push(PodShare::workspace_agent_skills(fs::canonicalize(skills)?)?);
            }
        }
    }
    let mut runtime_config = tascarrel_guest::RuncConfig::new(pod_runtime, runc_root, programs);
    runtime_config.runc = args.runc.clone();
    runtime_config.mount = args.mount.clone();
    runtime_config.umount = args.umount.clone();
    runtime_config.unshare = args.unshare.clone();
    runtime_config.nsenter = args.nsenter.clone();
    runtime_config.ip = args.ip.clone();
    runtime_config.pod_nix_store = storage.nix_store().store();
    runtime_config.nix_daemon_socket_dir = args.pod_nix_daemon_socket_dir.clone();
    runtime_config.nix_gc_root_dir = storage.nix_store().pod_gc_roots();
    runtime_config.nix_gc_root_trash_dir = storage.nix_store().gc_root_trash();
    runtime_config.workspace_ca = ca_path.clone().map(WorkspaceCaConfig::new);
    runtime_config.policy = PodPolicy::default()
        .with_docker_daemon(workspace.features.docker)
        .with_podman(workspace.features.podman)
        .with_virtualization(workspace.features.virtualization)
        .with_nix_daemon(workspace.nix.daemon);
    runtime_config.environment = workspace_environment;
    if let Some(parent) = workspace_config_path.parent() {
        let environment_path = parent.join(".env");
        let environment = match fs::read(&environment_path) {
            Ok(bytes) => parse_environment_file(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", environment_path.display()));
            }
        };
        runtime_config.environment.extend(environment);
    }
    if ca_path.is_some() {
        for name in [
            "SSL_CERT_FILE",
            "CURL_CA_BUNDLE",
            "REQUESTS_CA_BUNDLE",
            "GIT_SSL_CAINFO",
        ] {
            runtime_config.environment.insert(
                name.to_owned(),
                "/etc/ssl/certs/ca-certificates.crt".to_owned(),
            );
        }
        runtime_config.environment.insert(
            "NODE_EXTRA_CA_CERTS".to_owned(),
            "/run/tascarrel/https-ca/ca.pem".to_owned(),
        );
    }
    runtime_config.shares = shares;
    // `/dev` is recreated on every boot. Durable pod recovery may mount the
    // curated USB source before discovery has any nodes to populate it.
    UsbGuest::prepare_source().context("prepare curated USB device source")?;
    let mut pod_service_config = tascarrel_guest::PodServiceConfig::new(runtime_config);
    pod_service_config.init_steps = workspace
        .init
        .steps
        .iter()
        .map(|step| PodInitStep::new(step.script.clone(), step.wait))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| anyhow!(error.to_string()))
        .context("configure pod initialization steps")?;
    let pods = tascarrel_guest::PodService::open(
        pod_service_config,
        database.clone(),
        Arc::clone(&store),
        network,
    )
    .await
    .map_err(|error| anyhow!("{error}"))?;
    let usb = UsbGuest::new()?;
    let usb_task = tokio::spawn(usb.run(pods.clone()));
    let executor = Executor::new(args.setsid);
    let process_supervisor = ProcessSupervisor::new(executor, ProcessSupervisorConfig::default());
    let code_config = CodeServiceConfig {
        extensions: workspace.editors.code.extensions.clone(),
        ..CodeServiceConfig::default()
    };
    let code = CodeService::new(code_config)
        .map_err(|error| anyhow!(error.to_string()))
        .context("start workspace Code editor service")?;
    let changes = ChangesService::new(ChangesServiceConfig::new(args.git.clone()))
        .map_err(|error| anyhow!(error.to_string()))
        .context("start workspace changes service")?;
    changes
        .ensure_tracking(
            pods.clone(),
            repository_manager.clone(),
            repository_config.clone(),
        )
        .await;
    let files = FilesService::new();
    let image_input: Arc<dyn ImageInputRefresh> = input;
    let control_plane = GuestControlService::new(
        GuestState::new(
            GuestServices {
                chats: chats.clone(),
                changes,
                code: code.clone(),
                files,
                guest,
                images: images.clone(),
                network: Arc::clone(&network_service),
                pods: pods.clone(),
                processes: process_supervisor.clone(),
            },
            Arc::clone(&image_input),
        )
        .with_repositories(repository_manager.clone(), repository_config.clone()),
    );
    let control = ControlServices {
        chats: chats.clone(),
        control_plane,
        files,
        pods: pods.clone(),
    };
    let pod_control_connections = pods
        .take_control_connections()
        .map_err(|error| anyhow!(error.to_string()))?;
    let pod_control_task = tokio::spawn(run_pod_control_listeners(
        pod_control_connections,
        control.control_plane.clone(),
        repository_manager.clone(),
        pods.clone(),
    ));

    let transport = async {
        if let Some(session) = initial_session.take() {
            if let Err(error) =
                serve_mux_session(&control, &network_service, session, ca_path.as_deref()).await
            {
                warn!(error = ?error, "initial control connection ended with an error");
            }
            run_device(&control, &network_service, &args.device, ca_path.as_deref()).await
        } else if let Some(socket) = args.unix_socket.as_deref() {
            run_unix_socket(&control, &network_service, socket, ca_path.as_deref()).await
        } else {
            run_device(&control, &network_service, &args.device, ca_path.as_deref()).await
        }
    };
    tokio::pin!(transport);
    tokio::select! {
        result = &mut transport => result?,
        () = shutdown_signal() => info!("received shutdown signal"),
    }

    usb_task.abort();
    if let Err(error) = usb_task.await
        && !error.is_cancelled()
    {
        warn!(%error, "USB discovery task failed during shutdown");
    }
    pod_control_task.abort();
    if let Err(error) = pod_control_task.await
        && !error.is_cancelled()
    {
        warn!(%error, "pod control listener task failed during shutdown");
    }

    code.shutdown().await;
    if let Err(error) = chats.shutdown().await {
        warn!(%error, "could not shut down workspace chats cleanly");
    }
    network_service.detach_mux();
    pods.shutdown(&network_service).await;
    for (principal, mapping) in service_network.iter().rev() {
        if let Err(error) = network_service.deactivate(principal, mapping).await {
            warn!(%error, service = %principal.id, "could not stop workspace network binding during shutdown");
        }
    }
    Ok(())
}

fn run_git_remote_helper() -> Result<()> {
    let socket = std::env::var_os("TASCARREL_GIT_SOCKET")
        .ok_or_else(|| anyhow!("TASCARREL_GIT_SOCKET is unset for the internal Git helper"))?;
    let input = File::open("/dev/stdin").context("open Git helper stdin")?;
    let output = OpenOptions::new()
        .write(true)
        .open("/dev/stdout")
        .context("open Git helper stdout")?;
    tascarrel_git::run_remote_helper(input, output, |service| {
        if service != tascarrel_git::RemoteService::UploadPack {
            return Err(reportify::Report::new(
                tascarrel_git::GitError::UnsupportedService {
                    service: "git-receive-pack".to_owned(),
                },
            ));
        }
        let stream = UnixStream::connect(Path::new(&socket)).map_err(git_helper_io)?;
        let reader = stream.try_clone().map_err(git_helper_io)?;
        Ok((reader, stream))
    })
    .map_err(|report| anyhow!(report.to_string()))
}

fn git_helper_io(source: io::Error) -> reportify::Report<tascarrel_git::GitError> {
    reportify::Report::new(tascarrel_git::GitError::Io {
        action: "connect the Tascarrel Git helper",
        source,
    })
}

#[derive(Clone)]
struct ControlServices {
    chats: ChatService,
    control_plane: GuestControlService,
    files: FilesService,
    pods: tascarrel_guest::PodService,
}

async fn run_device(
    control: &ControlServices,
    network_service: &Arc<GuestNetworkService>,
    path: &Path,
    ca_path: Option<&Path>,
) -> Result<()> {
    loop {
        let device = open_device(path).await;
        info!(path = %path.display(), "opened Tascarrel control device");
        if let Err(error) = serve_mux_connection(control, network_service, device, ca_path).await {
            warn!(error = ?error, "control connection ended with an error");
        } else {
            info!("control connection closed; waiting for reconnect");
        }
        sleep(Duration::from_secs(1)).await;
    }
}

async fn open_device(path: &Path) -> AsyncDevice<File> {
    loop {
        match AsyncDevice::open(path) {
            Ok(device) => return device,
            Err(error) => warn!(path = %path.display(), %error, "could not open control device"),
        }
        sleep(Duration::from_secs(1)).await;
    }
}

/// Reactor-backed I/O for a nonblocking character device.
///
/// `tokio::fs::File` delegates reads to the blocking pool. A pending read from
/// a connected virtio console cannot be canceled, so dropping the Tokio runtime
/// would wait for it forever during service shutdown. `AsyncFd` keeps both
/// halves in the reactor and makes dropping a pending transport immediate.
struct AsyncDevice<T: AsRawFd> {
    inner: AsyncFd<T>,
}

impl AsyncDevice<File> {
    fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
            .open(path)?;
        Self::from_nonblocking(file)
    }
}

impl<T: AsRawFd> AsyncDevice<T> {
    fn from_nonblocking(inner: T) -> io::Result<Self> {
        AsyncFd::new(inner).map(|inner| Self { inner })
    }
}

impl<T> AsyncRead for AsyncDevice<T>
where
    T: AsRawFd + Read + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            let mut readiness = ready!(this.inner.poll_read_ready_mut(context))?;
            match readiness.try_io(|inner| inner.get_mut().read(buffer.initialize_unfilled())) {
                Ok(Ok(count)) => {
                    buffer.advance(count);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_would_block) => {}
            }
        }
    }
}

impl<T> AsyncWrite for AsyncDevice<T>
where
    T: AsRawFd + Write + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        loop {
            let mut readiness = ready!(this.inner.poll_write_ready_mut(context))?;
            match readiness.try_io(|inner| inner.get_mut().write(buffer)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => {}
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

async fn run_unix_socket(
    control: &ControlServices,
    network_service: &Arc<GuestNetworkService>,
    path: &Path,
    ca_path: Option<&Path>,
) -> Result<()> {
    if let Ok(metadata) = tokio::fs::symlink_metadata(path).await {
        if !metadata.file_type().is_socket() {
            bail!("refusing to replace non-socket path {}", path.display());
        }
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("remove stale socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind development socket {}", path.display()))?;
    info!(path = %path.display(), "listening on development Unix socket");
    loop {
        let (stream, _) = listener.accept().await.context("accept Unix connection")?;
        if let Err(error) = serve_mux_connection(control, network_service, stream, ca_path).await {
            warn!(error = ?error, "development client connection ended with an error");
        }
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    ensure_owned_directory(path, 0o700, "private state")
}

fn ensure_searchable_directory(path: &Path) -> Result<()> {
    ensure_owned_directory(path, 0o711, "searchable runtime")
}

fn ensure_owned_directory(path: &Path, mode: u32, purpose: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!("{purpose} path is not a real directory: {}", path.display());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(mode);
            builder
                .create(path)
                .with_context(|| format!("create {purpose} directory {}", path.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect {purpose} directory {}", path.display()));
        }
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {purpose} directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{purpose} path is not a real directory: {}", path.display());
    }
    if metadata.uid() != Uid::effective().as_raw() {
        bail!(
            "{purpose} path is not owned by the guest daemon: {}",
            path.display()
        );
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("secure {purpose} directory {}", path.display()))
}

async fn serve_mux_connection<T>(
    control: &ControlServices,
    network_service: &Arc<GuestNetworkService>,
    io: T,
    ca_path: Option<&Path>,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let session = MuxSession::connect(io)?;
    serve_mux_session(control, network_service, session, ca_path).await
}

/// Maintains the authenticated listener owned by each running pod.
#[tracing::instrument(level = "debug", skip_all)]
async fn run_pod_control_listeners(
    mut listeners: tokio::sync::mpsc::Receiver<PodControlConnection>,
    control_plane: GuestControlService,
    repositories: Option<Arc<GuestRepositoryManager>>,
    pods: tascarrel_guest::PodService,
) {
    let mut active =
        BTreeMap::<tascarrel_api::types::pods::PodId, tokio::task::JoinHandle<()>>::new();
    while let Some(connection) = listeners.recv().await {
        if let Some(previous) = active.remove(&connection.pod_id) {
            previous.abort();
            if let Err(error) = previous.await
                && !error.is_cancelled()
            {
                warn!(pod_id = %connection.pod_id.0, %error, "replaced pod listener task failed");
            }
        }
        let pod_id = connection.pod_id.clone();
        let task_pod_id = pod_id.clone();
        let control_plane = control_plane.clone();
        let repositories = repositories.clone();
        let pods = pods.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = serve_pod_listener(
                connection.listener,
                task_pod_id,
                control_plane,
                repositories,
                pods,
            )
            .await
            {
                warn!(pod_id = %pod_id.0, %error, "pod control listener stopped");
            }
        });
        active.insert(connection.pod_id, task);
        let finished = active
            .iter()
            .filter_map(|(pod_id, task)| task.is_finished().then_some(pod_id.clone()))
            .collect::<Vec<_>>();
        for pod_id in finished {
            if let Some(task) = active.remove(&pod_id)
                && let Err(error) = task.await
            {
                warn!(pod_id = %pod_id.0, %error, "pod listener task failed");
            }
        }
    }
    for (pod_id, task) in active {
        task.abort();
        if let Err(error) = task.await
            && !error.is_cancelled()
        {
            warn!(pod_id = %pod_id.0, %error, "pod listener task failed during shutdown");
        }
    }
}

/// Accepts multiplexed connections from one authenticated pod listener.
#[tracing::instrument(level = "debug", skip_all, fields(pod_id = %pod_id.0), err)]
async fn serve_pod_listener(
    listener: UnixListener,
    pod_id: tascarrel_api::types::pods::PodId,
    control_plane: GuestControlService,
    repositories: Option<Arc<GuestRepositoryManager>>,
    pods: tascarrel_guest::PodService,
) -> std::result::Result<(), Report<PodControlListenerError>> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.escalate(PodControlListenerError::Accept)?;
                let control_plane = control_plane.clone();
                let repositories = repositories.clone();
                let pods = pods.clone();
                let connection_pod_id = pod_id.clone();
                connections.spawn(async move {
                    if let Err(error) = serve_pod_mux_connection(
                        stream,
                        connection_pod_id,
                        control_plane,
                        repositories,
                        pods,
                    ).await {
                        warn!(%error, "pod multiplex connection ended with an error");
                    }
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    warn!(%error, "pod multiplex connection task failed");
                }
            }
        }
    }
}

/// Dispatches control and Git channels opened on one pod connection.
#[tracing::instrument(level = "debug", skip_all, fields(pod_id = %pod_id.0), err)]
async fn serve_pod_mux_connection(
    stream: tokio::net::UnixStream,
    pod_id: tascarrel_api::types::pods::PodId,
    control_plane: GuestControlService,
    repositories: Option<Arc<GuestRepositoryManager>>,
    pods: tascarrel_guest::PodService,
) -> std::result::Result<(), Report<PodControlListenerError>> {
    let mut session = MuxSession::connect(stream).map_err(|error| {
        PodControlListenerError::Multiplexer
            .report()
            .message(error.to_string())
    })?;
    let mut channels = JoinSet::new();
    loop {
        tokio::select! {
            result = &mut session.driver => {
                return pod_mux_driver_result(result);
            }
            request = session.incoming.recv() => {
                let Some(request) = request else {
                    return pod_mux_driver_result((&mut session.driver).await);
                };
                if request.endpoint() == MUX_CONTROL_PLANE_ENDPOINT {
                    let channel = request
                        .accept()
                        .map_err(|error| error.escalate(PodControlListenerError::LogicalChannel))?;
                    let identity = control_plane.pod_identity(pod_id.clone())
                        .map_err(|error| error.escalate(PodControlListenerError::Identity))?;
                    let control_plane = control_plane.clone();
                    let control_pod_id = pod_id.clone();
                    channels.spawn(async move {
                        let mut framed = Framed::new(channel);
                        if let Err(error) = framed.write(&identity).await {
                            warn!(%error, "could not send pod control identity");
                            return;
                        }
                        if let Err(error) = control_plane
                            .serve_pod_connection(framed.into_inner(), control_pod_id)
                            .await
                        {
                            warn!(%error, "pod control-plane channel ended with an error");
                        }
                    });
                } else if request.endpoint() == tascarrel_protocol::MUX_POD_GIT_ENDPOINT {
                    let Some(repositories) = repositories.clone() else {
                        request.reject(b"repository service unavailable")
                            .map_err(|error| error.escalate(PodControlListenerError::LogicalChannel))?;
                        continue;
                    };
                    let channel = request
                        .accept()
                        .map_err(|error| error.escalate(PodControlListenerError::LogicalChannel))?;
                    let git_pod_id = pod_id.clone();
                    let pods = pods.clone();
                    channels.spawn(async move {
                        if let Err(error) = repositories
                            .serve_pod_git(channel, git_pod_id, &pods)
                            .await
                        {
                            warn!(%error, "pod Git channel ended with an error");
                        }
                    });
                } else {
                    request.reject(b"unknown pod endpoint")
                        .map_err(|error| error.escalate(PodControlListenerError::LogicalChannel))?;
                }
            }
            Some(result) = channels.join_next(), if !channels.is_empty() => {
                if let Err(error) = result {
                    warn!(%error, "pod logical channel task failed");
                }
            }
        }
    }
}

/// Classifies the terminal result of one pod-private multiplexer connection.
fn pod_mux_driver_result(
    result: std::result::Result<tascarrel_mux::Result<()>, tokio::task::JoinError>,
) -> std::result::Result<(), Report<PodControlListenerError>> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) if error.error() == &tascarrel_mux::Error::ConnectionClosed => Ok(()),
        Ok(Err(error)) => Err(error.escalate(PodControlListenerError::Multiplexer)),
        Err(error) => Err(error.escalate(PodControlListenerError::MultiplexerTask)),
    }
}

struct MuxSession {
    driver: tokio::task::JoinHandle<tascarrel_mux::Result<()>>,
    handle: MuxHandle,
    incoming: Incoming,
}

impl MuxSession {
    fn connect<T>(io: T) -> Result<Self>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let config = MuxConfig {
            initial_byte_window: 64 * 1024,
            max_channels: 512,
            ..MuxConfig::default()
        };
        let (driver, handle, incoming) = connect_mux(io, MuxRole::Server, config)
            .map_err(|error| anyhow!("{error}"))
            .context("configure guest multiplexer")?;
        Ok(Self {
            driver: tokio::spawn(driver.run()),
            handle,
            incoming,
        })
    }
}

async fn serve_mux_session(
    control: &ControlServices,
    network_service: &Arc<GuestNetworkService>,
    mut session: MuxSession,
    ca_path: Option<&Path>,
) -> Result<()> {
    let handle = session.handle.clone();
    let incoming = &mut session.incoming;
    if let Some(path) = ca_path {
        let fetch = fetch_workspace_ca(&handle, path);
        tokio::pin!(fetch);
        tokio::select! {
            result = &mut session.driver => {
                return result
                    .context("guest multiplex driver task failed")?
                    .map_err(|error| anyhow!("{error}"))
                    .context("guest multiplex transport stopped while fetching workspace CA");
            }
            result = &mut fetch => result?,
        }
    }
    network_service.attach_mux(handle);
    let mut connections = JoinSet::new();

    let result = loop {
        tokio::select! {
            result = &mut session.driver => {
                break match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error))
                        if error.error() == &tascarrel_mux::Error::ConnectionClosed => Ok(()),
                    Ok(Err(error)) => {
                        Err(anyhow!("{error}")).context("guest multiplex transport failed")
                    }
                    Err(error) => Err(error).context("guest multiplex driver task failed"),
                };
            }
            request = incoming.recv() => {
                let Some(request) = request else {
                    break Err(anyhow::anyhow!("guest multiplex request stream closed"));
                };
                accept_mux_request(request, control, &mut connections);
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    warn!(%error, "logical control connection task failed");
                }
            }
        }
    };
    network_service.detach_mux();
    result
}

/// Dispatches one incoming logical channel to its owning guest service.
fn accept_mux_request(
    request: IncomingRequest,
    control: &ControlServices,
    connections: &mut JoinSet<()>,
) {
    if request.endpoint() == MUX_PUBLISH_GUEST_ENDPOINT {
        match request.accept() {
            Ok(channel) => {
                let pods = control.pods.clone();
                connections.spawn(async move {
                    if let Err(error) = serve_published_connection(channel, &pods).await {
                        warn!(%error, "published-port connection ended with an error");
                    }
                });
            }
            Err(error) => warn!(%error, "could not accept published-port connection"),
        }
        return;
    }
    if request.endpoint() == MUX_CHAT_ATTACHMENT_UPLOAD_ENDPOINT {
        match request.accept() {
            Ok(channel) => {
                let chats = control.chats.clone();
                connections.spawn(async move {
                    if let Err(error) = serve_chat_attachment_upload(channel, &chats).await {
                        warn!(%error, "logical chat attachment upload ended with an error");
                    }
                });
            }
            Err(error) => warn!(%error, "could not accept chat attachment upload"),
        }
        return;
    }
    if request.endpoint() == MUX_CHAT_ATTACHMENT_READ_ENDPOINT {
        match request.accept() {
            Ok(channel) => {
                let chats = control.chats.clone();
                connections.spawn(async move {
                    if let Err(error) = serve_chat_attachment_read(channel, &chats).await {
                        warn!(%error, "logical chat attachment read ended with an error");
                    }
                });
            }
            Err(error) => warn!(%error, "could not accept chat attachment read"),
        }
        return;
    }
    if request.endpoint() == MUX_WORKSPACE_FILE_READ_ENDPOINT {
        match request.accept() {
            Ok(channel) => {
                let files = control.files;
                let pods = control.pods.clone();
                connections.spawn(async move {
                    if let Err(error) = serve_workspace_file_read(channel, files, &pods).await {
                        warn!(%error, "logical workspace file read ended with an error");
                    }
                });
            }
            Err(error) => warn!(%error, "could not accept workspace file read"),
        }
        return;
    }
    if request.endpoint() == MUX_CONTROL_PLANE_ENDPOINT {
        accept_guest_control_plane_request(request, &control.control_plane, connections);
        return;
    }
    if let Err(error) = request.reject(b"unknown endpoint") {
        warn!(%error, "could not reject an unknown logical endpoint");
    }
}

/// Resolves one host-requested pod port and changes the framed mux channel to
/// a raw byte relay after reporting the connection result.
async fn serve_published_connection(
    channel: Channel,
    pod_service: &tascarrel_guest::PodService,
) -> Result<()> {
    let mut framed = Framed::new(channel);
    let request = framed
        .read::<PublishedPortConnect>()
        .await
        .context("read published-port target")?
        .ok_or_else(|| anyhow!("host closed published-port handshake"))?;
    let mut podd_stream = match pod_service
        .connect_port(&request.pod_id, request.pod_port)
        .await
    {
        Ok(stream) => stream,
        Err(error) => {
            framed
                .write(&PublishedPortConnectResponse {
                    result: Err(published_port_error(error)),
                })
                .await
                .context("write rejected published-port result")?;
            return Ok(());
        }
    };
    framed
        .write(&PublishedPortConnectResponse { result: Ok(()) })
        .await
        .context("write accepted published-port result")?;
    let mut channel = framed.into_inner();
    tokio::io::copy_bidirectional(&mut channel, &mut podd_stream)
        .await
        .context("relay published-port connection")?;
    Ok(())
}

/// Maps an internal pod-service report to the published-port wire contract.
fn published_port_error(error: reportify::Report<PodServiceError>) -> RemoteError {
    let (code, message) = match error.into_error() {
        PodServiceError::InvalidRequest(message) => {
            let code = if message == "pod is not running" {
                ErrorCode::NotFound
            } else {
                ErrorCode::InvalidRequest
            };
            (code, message)
        }
        PodServiceError::Internal(message) => (ErrorCode::ExecutionFailed, message),
    };
    RemoteError::new(code, message)
}

/// Reads one metadata frame followed by raw attachment bytes and returns one
/// framed result on the same logical channel.
async fn serve_chat_attachment_upload(channel: Channel, chats: &ChatService) -> Result<()> {
    let mut framed = Framed::new(channel);
    let request = framed
        .read::<ChatAttachmentUploadRequest>()
        .await
        .context("read chat attachment upload metadata")?
        .ok_or_else(|| anyhow!("chat attachment upload closed before metadata"))?;
    let mut channel = framed.into_inner();
    let response = match chats
        .store_attachment(request.name, request.media_type, &mut channel)
        .await
    {
        Ok(attachment) => ChatAttachmentUploadResponse::Uploaded { attachment },
        Err(error) => ChatAttachmentUploadResponse::Rejected {
            code: error.code,
            message: error.message,
        },
    };
    let mut framed = Framed::new(channel);
    framed
        .write(&response)
        .await
        .context("write chat attachment upload result")?;
    let mut channel = framed.into_inner();
    channel
        .shutdown()
        .await
        .context("finish chat attachment upload result")?;
    // Retain the channel until the request ends so dropping it cannot reset the
    // buffered result.
    if let Err(error) = tokio::io::copy(&mut channel, &mut tokio::io::sink()).await {
        debug!(%error, "chat attachment uploader disconnected after receiving its result");
    }
    Ok(())
}

/// Returns one metadata frame followed by the immutable raw attachment bytes.
async fn serve_chat_attachment_read(channel: Channel, chats: &ChatService) -> Result<()> {
    let mut framed = Framed::new(channel);
    let request = framed
        .read::<ChatAttachmentReadRequest>()
        .await
        .context("read chat attachment request")?
        .ok_or_else(|| anyhow!("chat attachment read closed before its request"))?;
    match chats.open_attachment(&request.attachment_id).await {
        Ok((attachment, mut file)) => {
            framed
                .write(&ChatAttachmentReadResponse::Found { attachment })
                .await
                .context("write chat attachment response metadata")?;
            let mut channel = framed.into_inner();
            tokio::io::copy(&mut file, &mut channel)
                .await
                .context("stream chat attachment content")?;
            channel
                .shutdown()
                .await
                .context("finish chat attachment content")
        }
        Err(error) => framed
            .write(&ChatAttachmentReadResponse::Rejected {
                code: error.code,
                message: error.message,
            })
            .await
            .context("write rejected chat attachment response"),
    }
}

/// Returns one result frame followed by raw bytes from a safely opened
/// workspace file.
async fn serve_workspace_file_read(
    channel: Channel,
    files: FilesService,
    pods: &tascarrel_guest::PodService,
) -> std::result::Result<(), Report<WorkspaceFileStreamError>> {
    let mut framed = Framed::new(channel);
    let request = framed
        .read::<WorkspaceFileReadRequest>()
        .await
        .map_err(|error| stream_failed("failed to read workspace file request", error))?
        .ok_or_else(|| stream_failed("workspace file read closed before its request", "EOF"))?;
    match files
        .open_file(&request.pod_id, request.path.as_str(), pods)
        .await
    {
        Ok(mut opened) => {
            framed
                .write(&WorkspaceFileReadResponse::Found { size: opened.size })
                .await
                .map_err(|error| stream_failed("failed to write workspace file response", error))?;
            let mut channel = framed.into_inner();
            tokio::io::copy(&mut opened.file, &mut channel)
                .await
                .map_err(|error| stream_failed("failed to stream workspace file", error))?;
            channel
                .shutdown()
                .await
                .map_err(|error| stream_failed("failed to finish workspace file stream", error))
        }
        Err(error) => {
            let code = match error.error() {
                FilesServiceError::InvalidRequest(_) => "invalid_path",
                FilesServiceError::Unavailable(_) => "unavailable",
                FilesServiceError::Internal(_) => "internal",
            };
            framed
                .write(&WorkspaceFileReadResponse::Rejected {
                    code: code.to_owned(),
                    message: error.to_string(),
                })
                .await
                .map_err(|error| {
                    stream_failed("failed to write rejected workspace file response", error)
                })
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("workspace file stream failed: {0}")]
struct WorkspaceFileStreamError(String);

fn stream_failed(
    message: impl Into<String>,
    source: impl std::fmt::Display,
) -> Report<WorkspaceFileStreamError> {
    WorkspaceFileStreamError(message.into())
        .report()
        .message(source.to_string())
}

/// Accepts the full-duplex guest control-plane channel.
fn accept_guest_control_plane_request(
    request: IncomingRequest,
    control_plane: &GuestControlService,
    connections: &mut JoinSet<()>,
) {
    match request.accept() {
        Ok(channel) => {
            let control_plane = control_plane.clone();
            connections.spawn(async move {
                let mut framed = Framed::new(channel);
                let identity = match framed.read::<GuestControlIdentity>().await {
                    Ok(Some(identity)) => identity,
                    Ok(None) => {
                        warn!("host closed guest control-plane identity handshake");
                        return;
                    }
                    Err(error) => {
                        warn!(%error, "could not read guest control-plane identity");
                        return;
                    }
                };
                if let Err(error) = control_plane
                    .serve_host_connection(framed.into_inner(), identity.workspace)
                    .await
                {
                    warn!(%error, "guest control-plane connection ended with an error");
                }
            });
        }
        Err(error) => warn!(%error, "could not accept guest control-plane connection"),
    }
}

async fn fetch_workspace_ca(handle: &MuxHandle, path: &Path) -> Result<()> {
    const MAX_CA_BYTES: u64 = 64 * 1024;
    let mut channel = handle
        .open(MUX_CA_HOST_ENDPOINT)
        .await
        .map_err(|error| anyhow!("{error}"))
        .context("open workspace CA channel")?;
    let mut pem = Vec::new();
    (&mut channel)
        .take(MAX_CA_BYTES + 1)
        .read_to_end(&mut pem)
        .await
        .context("read workspace CA")?;
    channel
        .shutdown()
        .await
        .context("close workspace CA channel")?;
    if pem.len() as u64 > MAX_CA_BYTES
        || !pem.starts_with(b"-----BEGIN CERTIFICATE-----\n")
        || !pem.ends_with(b"-----END CERTIFICATE-----\n")
    {
        bail!("host returned an invalid workspace CA certificate");
    }
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create temporary workspace CA {}", temporary.display()))?;
    if let Err(error) = file.write_all(&pem).and_then(|()| file.sync_all()) {
        if let Err(cleanup) = fs::remove_file(&temporary) {
            warn!(path = %temporary.display(), %cleanup, "could not remove incomplete workspace CA");
        }
        return Err(error).context("write workspace CA");
    }
    fs::rename(&temporary, path).with_context(|| format!("publish workspace CA {}", path.display()))
}

/// Fetches and validates the host-resolved startup environment.
async fn fetch_workspace_environment(handle: &MuxHandle) -> Result<BTreeMap<String, String>> {
    let channel = handle
        .open(MUX_WORKSPACE_ENVIRONMENT_HOST_ENDPOINT)
        .await
        .map_err(|error| anyhow!("{error}"))
        .context("open workspace environment channel")?;
    let mut framed = Framed::with_max_frame_len(channel, MAX_WORKSPACE_ENVIRONMENT_FRAME_LEN)
        .map_err(|error| anyhow!("{error}"))
        .context("configure workspace environment channel")?;
    let response = framed
        .read::<WorkspaceEnvironmentResponse>()
        .await
        .map_err(|error| anyhow!("{error}"))
        .context("read workspace environment")?
        .ok_or_else(|| {
            anyhow!("host closed the workspace environment channel without a response")
        })?;
    response
        .validate()
        .map_err(|error| anyhow!("{error}"))
        .context("validate workspace environment")?;
    let mut channel = framed.into_inner();
    channel
        .shutdown()
        .await
        .context("close workspace environment channel")?;
    response.result.map_err(|failure| anyhow!(failure.message))
}

async fn shutdown_signal() {
    let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                warn!(%error, "Ctrl-C handler failed");
            }
        }
        _ = terminate.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use nix::pty::openpty;
    use tokio::io::AsyncReadExt;

    use super::*;

    /// Verifies long diagnostics retain bounded head and tail context.
    #[test]
    fn image_provider_diagnostic_preserves_the_cause_and_tail() {
        let detail = format!("cause:{}:tail", "x".repeat(4096));
        let error = image_provider_error(&detail);
        assert!(error.message.starts_with("cause:"));
        assert!(error.message.contains("diagnostic truncated"));
        assert!(error.message.ends_with(":tail"));
    }

    /// Verifies dotenv parsing accepts supported syntax and rejects invalid
    /// or duplicate names.
    #[test]
    fn environment_file_is_strict_and_supports_common_dotenv_forms() {
        let environment = parse_environment_file(
            b"# comment\nPLAIN=value\nexport QUOTED=\"two words\"\nSINGLE='three'\n",
        )
        .unwrap();
        assert_eq!(environment["PLAIN"], "value");
        assert_eq!(environment["QUOTED"], "two words");
        assert_eq!(environment["SINGLE"], "three");
        assert!(parse_environment_file(b"BAD-NAME=x\n").is_err());
        assert!(parse_environment_file(b"DUP=x\nDUP=y\n").is_err());
    }

    /// Verifies transferred runtime inputs remain traversable and readable by
    /// UID-mapped pod processes.
    #[test]
    fn runtime_input_trees_are_readable_without_rewriting_ownership() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        fs::create_dir(&directory).unwrap();
        let file = directory.join("payload");
        fs::write(&file, b"payload").unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

        make_workspace_runtime_inputs_readable(&directory).unwrap();

        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o705
        );
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o604
        );
    }

    /// Verifies canceling an idle device read does not block Tokio runtime
    /// shutdown.
    #[test]
    fn pending_device_read_does_not_hold_runtime_shutdown() {
        // Keep the peer of this pollable character device open and idle. The
        // device read must remain pending until the future is canceled, just
        // like an attached host which sends no more virtio-serial bytes.
        let pty = openpty(None, None).expect("open test PTY");
        let path = PathBuf::from(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()));
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test runtime");
            runtime.block_on(async move {
                let mut device = AsyncDevice::open(&path).expect("open test character device");
                let mut byte = [0_u8; 1];
                let result =
                    tokio::time::timeout(Duration::from_millis(25), device.read_exact(&mut byte))
                        .await;
                assert!(result.is_err(), "idle character-device read completed");
            });
            // This is the regression boundary: a pending tokio::fs::File read
            // leaves an uncancelable blocking-pool syscall, and Runtime::drop
            // never reaches the notification while its peer remains open.
            drop(runtime);
            finished_tx.send(()).expect("notify test thread");
        });

        finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("runtime shutdown was held by the canceled device read");
        worker.join().expect("device test worker panicked");
        drop(pty);
    }
}
