//! Host server lifecycle and workspace-service initialization.
//!
//! [`run_with_startup`] binds the bootstrap HTTP interface before delegating
//! host checks and payload preparation, then publishes initialized host
//! services and supervises them until shutdown.

use std::env;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use clap::ValueEnum;
use reportify::Report;
use tascarrel_api::parse_size_bytes;
use tascarrel_api::types::host::ServerIssue;
use tascarrel_api::types::host::ServerStartupPhase;
use tascarrel_protocol::WorkspaceName;
use tascarrel_vm::Acceleration;
use tascarrel_vm::Architecture;
use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;
use tracing::info;
use tracing::warn;
use tracing_subscriber::EnvFilter;

use crate::Broker;
use crate::ExternalWorkspaceConfig;
use crate::HostState;
use crate::ManagedWorkspaceConfig;
use crate::TascarrelHome;
use crate::WorkspaceMode;
use crate::WorkspaceService;
use crate::WorkspaceServiceConfig;
use crate::bind_control_socket;
use crate::control_plane::HostControlService;
use crate::remove_control_socket;
use crate::server_config::ServerConfig;
use crate::services::auth::AuthService;
use crate::services::auth::AuthServiceConfig;
use crate::services::config::ConfigService;
use crate::services::config::ConfigServiceConfig;
use crate::services::host_operations::HostOperationService;
use crate::services::host_operations::HostOperationServiceConfig;
use crate::services::network::NetworkService;
use crate::services::network::NetworkServiceConfig;
use crate::services::repositories::RepositoryService;
use crate::services::repositories::RepositoryServiceConfig;
use crate::services::secrets::SecretsService;
use crate::services::secrets::SecretsServiceConfig;
use crate::services::workspaces::create_private_directory;
use crate::services::workspaces::lock_file;
use crate::startup::StartupFailure;
use crate::startup::StartupReporter;

const MINIMUM_STATE_DISK_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_STATE_DISK_SIZE: &str = "1T";
const DEFAULT_MAX_WORKSPACES: usize = 8;
const DEFAULT_WEB_ADDRESS: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 8_272);

/// Direct-boot assets extracted from the embedded Tascarrel payload.
#[derive(Clone, Debug)]
pub struct GuestPayload {
    pub image: PathBuf,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub kernel_append: String,
    pub ui: PathBuf,
}

/// Options and non-fatal diagnostics produced by one startup preparation.
#[derive(Clone, Debug)]
pub struct StartupPreparation {
    /// Daemon options resolved by the preparation callback.
    pub options: DaemonOptions,
    /// Non-fatal host diagnostics to retain after the server becomes ready.
    pub warnings: Vec<ServerIssue>,
}

impl StartupPreparation {
    /// Creates a preparation without non-fatal host diagnostics.
    #[must_use]
    pub fn new(options: DaemonOptions) -> Self {
        Self {
            options,
            warnings: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, clap::Args)]
pub struct DaemonOptions {
    /// Host Git used for bare mirrors, upstream synchronization, and pushes.
    #[arg(long, env = "TASCARREL_GIT", default_value = "git")]
    git: PathBuf,
    /// Host SOPS executable used by configured workspace secret providers.
    #[arg(long, env = "TASCARREL_SOPS", default_value = "sops")]
    sops: PathBuf,
    /// Workspace assigned to --guest-socket in external development mode.
    #[arg(long, requires = "guest_socket")]
    workspace: Option<WorkspaceName>,

    /// Unified socket used by all local Tascarrel clients.
    #[arg(long, env = "TASCARREL_SOCKET")]
    socket: Option<PathBuf>,

    /// Read-only directory containing local guest, pod, and Tasci binaries.
    #[arg(
        long,
        env = "TASCARREL_LOCAL_BINARIES",
        conflicts_with = "guest_socket"
    )]
    local_binaries: Option<PathBuf>,

    /// Immutable EROFS NixOS store image used by every managed workspace VM.
    #[arg(long, env = "TASCARREL_IMAGE", conflicts_with = "guest_socket")]
    image: Option<PathBuf>,

    /// Connect one named workspace to an already-running virtio-serial socket.
    #[arg(
        long,
        env = "TASCARREL_GUEST_SOCKET",
        conflicts_with = "image",
        requires = "workspace"
    )]
    guest_socket: Option<PathBuf>,

    /// Linux kernel used to boot managed workspace VMs directly.
    #[arg(long, env = "TASCARREL_KERNEL", conflicts_with = "guest_socket")]
    kernel: Option<PathBuf>,

    /// Linux initrd paired with --kernel.
    #[arg(long, env = "TASCARREL_INITRD", requires = "kernel")]
    initrd: Option<PathBuf>,

    /// Kernel command line paired with --kernel.
    #[arg(long, env = "TASCARREL_KERNEL_APPEND", requires = "kernel")]
    kernel_append: Option<String>,

    /// Guest architecture.
    #[arg(long, value_enum)]
    architecture: Option<ArchitectureArg>,

    /// QEMU system executable override.
    #[arg(long, env = "TASCARREL_QEMU")]
    qemu: Option<PathBuf>,

    /// QEMU acceleration policy.
    #[arg(long, value_enum, default_value = "auto")]
    acceleration: AccelerationArg,

    /// Default memory in MiB for workspaces without [vm].memory.
    #[arg(long)]
    memory: Option<u32>,

    /// Default virtual CPUs for workspaces without [vm].cores.
    #[arg(long)]
    cpus: Option<u16>,

    /// Total seconds allowed for QEMU and the guest daemon to become ready.
    #[arg(long, default_value_t = 120)]
    startup_timeout: u64,

    /// Seconds allowed for each QEMU process to stop before it is killed.
    #[arg(long, default_value_t = 10)]
    shutdown_timeout: u64,

    /// DNS resolver used for guest requests to the virtual DNS address.
    #[arg(long, env = "TASCARREL_DNS_RESOLVER")]
    dns_resolver: Option<SocketAddr>,

    /// Outer host reached by static workspace host-port mappings.
    #[arg(long, env = "TASCARREL_HOST_PORT_HOST", default_value = "127.0.0.1")]
    host_port_host: String,

    /// Maximum number of workspace VMs this host daemon may own.
    #[arg(long, default_value_t = DEFAULT_MAX_WORKSPACES)]
    max_workspaces: usize,

    /// Address for the browser UI and its local API. Omitted to disable HTTP.
    #[arg(long, env = "TASCARREL_WEB_ADDRESS")]
    web_address: Option<SocketAddr>,

    /// Extracted Vite build served directly by the HTTP server.
    #[arg(long, env = "TASCARREL_UI_DIR", requires = "web_address")]
    ui_dir: Option<PathBuf>,

    /// Default minimum virtual size for persistent state disks.
    #[arg(
        long = "state-disk-size",
        env = "TASCARREL_STATE_DISK_SIZE",
        default_value = DEFAULT_STATE_DISK_SIZE
    )]
    state_disk_size: String,

    /// Discard this workspace's persistent state disk on its first lazy start.
    #[arg(long = "reset-state-workspace", value_name = "WORKSPACE")]
    reset_state_workspaces: Vec<WorkspaceName>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ArchitectureArg {
    #[value(name = "x86_64", alias = "amd64")]
    X86_64,
    #[value(name = "aarch64", alias = "arm64")]
    Aarch64,
}

impl From<ArchitectureArg> for Architecture {
    fn from(value: ArchitectureArg) -> Self {
        match value {
            ArchitectureArg::X86_64 => Self::X86_64,
            ArchitectureArg::Aarch64 => Self::Aarch64,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AccelerationArg {
    Auto,
    Kvm,
    Hvf,
    Tcg,
}

impl From<AccelerationArg> for Acceleration {
    fn from(value: AccelerationArg) -> Self {
        match value {
            AccelerationArg::Auto => Self::Auto,
            AccelerationArg::Kvm => Self::Kvm,
            AccelerationArg::Hvf => Self::Hvf,
            AccelerationArg::Tcg => Self::Tcg,
        }
    }
}

/// Runs the Git remote helper when the current executable was invoked through
/// the private `git-remote-tascarrel` symlink.
///
/// # Errors
///
/// Returns an error if the helper protocol or its host connection fails.
pub fn run_git_remote_helper_if_invoked() -> Result<bool> {
    if std::env::args_os()
        .next()
        .as_deref()
        .and_then(|argument| Path::new(argument).file_name())
        .is_some_and(|name| name == "git-remote-tascarrel")
    {
        run_git_remote_helper().context("run Git remote helper")?;
        return Ok(true);
    }
    Ok(false)
}

fn run_git_remote_helper() -> Result<()> {
    let socket = env::var_os("TASCARREL_GIT_SOCKET")
        .ok_or_else(|| anyhow!("TASCARREL_GIT_SOCKET is unset"))?;
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

fn init_tracing() {
    if let Err(error) = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("tascarrel_host=info")),
        )
        .with_writer(io::stderr)
        .try_init()
    {
        eprintln!("failed to initialize Tascarrel host tracing: {error}");
    }
}

/// Runs the per-user workspace service until it receives a termination
/// signal or its broker fails.
///
/// # Errors
///
/// Returns an error for invalid configuration, unsafe state paths, dependency
/// failures, socket errors, or an unexpected broker shutdown.
pub async fn run(args: DaemonOptions) -> Result<()> {
    let mut signals = ShutdownSignals::new()?;
    run_until_shutdown(args, signals.wait()).await
}

/// Runs the workspace service until an explicit shutdown future resolves.
///
/// # Errors
///
/// Returns an error for invalid configuration, unsafe state paths, dependency
/// failures, socket errors, or an unexpected broker or web-server shutdown.
#[tracing::instrument(name = "tascarrel_host.daemon.run", level = "info", skip_all, err)]
pub async fn run_until_shutdown(
    args: DaemonOptions,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<()> {
    run_with_startup_until_shutdown(
        args,
        |options, _reporter| async move { Ok(StartupPreparation::new(options)) },
        shutdown,
    )
    .await
}

/// Runs the server with an observable, retryable preparation before host
/// services initialize.
///
/// # Errors
///
/// Returns an error when the bootstrap listener, runtime lock, control plane,
/// or a long-running host service fails.
pub async fn run_with_startup<P, F>(args: DaemonOptions, prepare: P) -> Result<()>
where
    P: Fn(DaemonOptions, StartupReporter) -> F + Send + Sync,
    F: Future<Output = std::result::Result<StartupPreparation, Report<StartupFailure>>> + Send,
{
    let mut signals = ShutdownSignals::new()?;
    run_with_startup_until_shutdown(args, prepare, signals.wait()).await
}

/// Runs a server preparation with an explicit shutdown future.
///
/// # Errors
///
/// Returns an error when bootstrap infrastructure or a ready host service
/// stops unexpectedly.
#[tracing::instrument(
    name = "tascarrel_host.daemon.run_with_startup",
    level = "info",
    skip_all,
    err
)]
#[allow(clippy::too_many_lines)] // Keeping lifecycle selects together makes shutdown ordering explicit.
pub async fn run_with_startup_until_shutdown<P, F>(
    args: DaemonOptions,
    prepare: P,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<()>
where
    P: Fn(DaemonOptions, StartupReporter) -> F + Send + Sync,
    F: Future<Output = std::result::Result<StartupPreparation, Report<StartupFailure>>> + Send,
{
    init_tracing();
    let tascarrel_home = TascarrelHome::discover().map_err(|error| anyhow!(error.to_string()))?;
    let runtime_dir = tascarrel_home.runtime();
    create_private_directory(&runtime_dir).with_context(|| {
        format!(
            "failed to prepare host runtime directory {}",
            runtime_dir.display()
        )
    })?;
    let _host_lock = lock_file(
        &runtime_dir.join("host.lock"),
        "Tascarrel host workspace service",
    )?;
    let server_config = ServerConfig::load(tascarrel_home.server_config())
        .map_err(|error| anyhow!(error.to_string()))
        .context("load host server configuration")?;
    let reporter = StartupReporter::new();
    let mut auth_config = AuthServiceConfig::new(tascarrel_home.state().join("auth"));
    auth_config.secret_file = server_config
        .authentication_secret_file()
        .map(Path::to_owned);
    let auth = AuthService::open(auth_config)
        .await
        .map_err(|error| anyhow!(error.to_string()))
        .context("start host authentication service")?;
    let control_socket = args
        .socket
        .clone()
        .unwrap_or_else(|| tascarrel_home.control_socket());
    if !control_socket.is_absolute() {
        bail!(
            "control socket must be absolute: {}",
            control_socket.display()
        );
    }
    let _socket_cleanup = SocketCleanup(control_socket.clone());
    let listener = bind_control_socket(&control_socket).with_context(|| {
        format!(
            "failed to bind unified host socket {}",
            control_socket.display()
        )
    })?;
    let (control_ready, control_state) = tokio::sync::watch::channel(None);
    let broker_auth = auth.clone();
    let broker = async move {
        Broker::new(listener, broker_auth, control_state)
            .run()
            .await
            .context("local host control plane stopped")
    };
    let web_address = args.web_address.map(validate_web_address).transpose()?;
    let (web_server, mut web_control) = match web_address {
        Some(address) => {
            let (server, control) = crate::web::bind(
                address,
                reporter.clone(),
                auth.clone(),
                server_config.public_origin().map(str::to_owned),
            )
            .await
            .context("bind Tascarrel bootstrap web server")?;
            (Some(server), Some(control))
        }
        None => (None, None),
    };
    let web_enabled = web_server.is_some();
    let web = async move {
        match web_server {
            Some(server) => server.serve().await.context("Tascarrel web server stopped"),
            None => std::future::pending::<Result<()>>().await,
        }
    };
    tokio::pin!(broker, web, shutdown);

    let initial_options = args;
    let (initialized, warnings) = loop {
        reporter.starting(
            ServerStartupPhase::CheckingHost,
            "Checking required host capabilities",
        );
        let startup = prepare(initial_options.clone(), reporter.clone());
        tokio::pin!(startup);
        let preparation = tokio::select! {
            () = &mut shutdown => {
                info!("host workspace service shutdown requested during startup");
                return Ok(());
            }
            result = &mut broker => return result,
            result = &mut web => return result,
            result = &mut startup => result,
        };
        let preparation = match preparation {
            Ok(preparation) => preparation,
            Err(failure) => {
                reporter.failed(failure.error());
                if !web_enabled {
                    bail!("{failure}");
                }
                let Some(control) = web_control.as_mut() else {
                    bail!("Tascarrel web startup control is unavailable");
                };
                tokio::select! {
                    () = &mut shutdown => return Ok(()),
                    result = &mut broker => return result,
                    result = &mut web => return result,
                    () = control.retry_requested() => {}
                }
                continue;
            }
        };

        reporter.starting(
            ServerStartupPhase::InitializingServices,
            "Initializing Tascarrel services",
        );
        match Initialized::new(preparation.options, auth.clone(), &server_config) {
            Ok(initialized) => break (initialized, preparation.warnings),
            Err(error) => {
                let failure = StartupFailure::retryable(
                    "service-initialization-failed",
                    "Tascarrel services could not be initialized",
                    format!("{error:#}"),
                );
                reporter.failed(&failure);
                if !web_enabled {
                    return Err(error);
                }
                let Some(control) = web_control.as_mut() else {
                    return Err(error);
                };
                tokio::select! {
                    () = &mut shutdown => return Ok(()),
                    result = &mut broker => return result,
                    result = &mut web => return result,
                    () = control.retry_requested() => {}
                }
            }
        }
    };

    let Initialized {
        prepared,
        repository_service,
        state,
        host_control,
    } = initialized;
    control_ready.send_replace(Some(host_control.clone()));
    if let Some(control) = &web_control {
        control.make_ready(
            state.clone(),
            host_control.clone(),
            prepared.ui_root.clone(),
        );
    }
    reporter.ready(warnings);
    let workspace_requests = serve_workspace_requests(state.clone());
    info!(
        socket = %prepared.socket.display(),
        "host workspace service ready"
    );
    let repository_refreshes = repository_service.run_background_refreshes();
    tokio::pin!(repository_refreshes, workspace_requests);
    let result = tokio::select! {
        () = &mut shutdown => {
            info!("host workspace service shutdown requested");
            Ok(())
        }
        result = &mut broker => result,
        result = &mut web => result,
        () = &mut repository_refreshes => unreachable!("repository refresh service returned"),
        result = &mut workspace_requests => result,
    };

    state.network().shutdown().await;
    state.workspaces().shutdown().await;
    result
}

/// Dispatches workspace-owned private channels to the services that handle
/// them.
async fn serve_workspace_requests(state: HostState) -> Result<()> {
    let network_requests = state
        .workspaces()
        .take_network_requests()
        .map_err(|error| anyhow!(error.to_string()))
        .context("attach workspace network request stream")?;
    let environment_requests = state
        .workspaces()
        .take_environment_requests()
        .map_err(|error| anyhow!(error.to_string()))
        .context("attach workspace environment request stream")?;
    let host_operation_input_requests = state
        .workspaces()
        .take_host_operation_input_requests()
        .map_err(|error| anyhow!(error.to_string()))
        .context("attach workspace host operation input request stream")?;
    let network = state
        .network()
        .serve_workspace_requests(network_requests, state.secrets().clone());
    let environment = state
        .secrets()
        .serve_workspace_requests(environment_requests, state.config().clone());
    let host_operation_inputs = state
        .host_operations()
        .serve_workspace_requests(host_operation_input_requests);
    tokio::pin!(network, environment, host_operation_inputs);
    tokio::select! {
        () = &mut network => bail!("workspace network request stream closed"),
        () = &mut environment => bail!("workspace environment request stream closed"),
        () = &mut host_operation_inputs => {
            bail!("workspace host operation input request stream closed")
        },
    }
}

impl DaemonOptions {
    /// Constructs the managed daemon configuration used by app mode.
    #[must_use]
    pub fn for_payload(
        guest: GuestPayload,
        qemu: PathBuf,
        git: PathBuf,
        sops: PathBuf,
        socket: Option<PathBuf>,
    ) -> Self {
        Self {
            git,
            sops,
            workspace: None,
            socket,
            local_binaries: None,
            image: Some(guest.image),
            guest_socket: None,
            kernel: Some(guest.kernel),
            initrd: Some(guest.initrd),
            kernel_append: Some(guest.kernel_append),
            architecture: None,
            qemu: Some(qemu),
            acceleration: AccelerationArg::Auto,
            memory: None,
            cpus: None,
            startup_timeout: 120,
            shutdown_timeout: 10,
            dns_resolver: None,
            host_port_host: "127.0.0.1".to_owned(),
            max_workspaces: DEFAULT_MAX_WORKSPACES,
            web_address: Some(DEFAULT_WEB_ADDRESS),
            ui_dir: Some(guest.ui),
            state_disk_size: DEFAULT_STATE_DISK_SIZE.to_owned(),
            reset_state_workspaces: Vec::new(),
        }
    }

    /// Uses extracted payload assets for options not explicitly overridden.
    #[must_use]
    pub fn with_payload_defaults(mut self, guest: GuestPayload) -> Self {
        if self.guest_socket.is_none() {
            self.image.get_or_insert(guest.image);
            self.kernel.get_or_insert(guest.kernel);
            self.initrd.get_or_insert(guest.initrd);
            self.kernel_append.get_or_insert(guest.kernel_append);
        }
        self.web_address.get_or_insert(DEFAULT_WEB_ADDRESS);
        self.ui_dir.get_or_insert(guest.ui);
        self
    }

    /// Enables the standard loopback web address unless explicitly configured.
    #[must_use]
    pub fn with_default_web_address(mut self) -> Self {
        self.web_address.get_or_insert(DEFAULT_WEB_ADDRESS);
        self
    }

    /// Returns the configured web address.
    #[must_use]
    pub const fn web_address(&self) -> Option<SocketAddr> {
        self.web_address
    }

    /// Overrides the loopback web address used by an installed distribution.
    #[must_use]
    pub fn with_web_address(mut self, address: SocketAddr) -> Self {
        self.web_address = Some(address);
        self
    }
}

struct Initialized {
    prepared: Prepared,
    repository_service: RepositoryService,
    state: HostState,
    host_control: HostControlService,
}

impl Initialized {
    /// Builds the host services that become available only after preparation.
    fn new(args: DaemonOptions, auth: AuthService, server_config: &ServerConfig) -> Result<Self> {
        let prepared = Prepared::new(&args)?;
        let repository_service = RepositoryService::new(RepositoryServiceConfig::new(
            prepared.workspace_service.git.clone(),
            &prepared.workspaces_dir,
            &prepared.repository_cache_dir,
        ))
        .map_err(|error| anyhow!(error.to_string()))
        .context("start host repository service")?;
        let workspace_service = WorkspaceService::new(prepared.workspace_service.clone())?;
        let config_service =
            ConfigService::open(ConfigServiceConfig::new(&prepared.workspaces_dir))
                .map_err(|error| anyhow!(error.to_string()))
                .context("start host configuration service")?;
        let host_operation_service = HostOperationService::open(HostOperationServiceConfig::new(
            &prepared.host_operations_dir,
            &prepared.workspace_service.git,
        ))
        .map_err(|error| anyhow!(error.to_string()))
        .context("start host operation service")?;
        let network_service = NetworkService::new(NetworkServiceConfig {
            dns_resolver: args.dns_resolver,
            host_port_host: args.host_port_host,
            hostname_suffix: server_config.route_hostname_suffix().to_owned(),
            ..NetworkServiceConfig::default()
        })
        .map_err(|error| anyhow!(error.to_string()))
        .context("start host network service")?;
        let secrets_service = SecretsService::new(SecretsServiceConfig::new(
            &prepared.workspaces_dir,
            &prepared.sops,
        ))
        .map_err(|error| anyhow!(error.to_string()))
        .context("start host secrets service")?;
        let state = HostState::new(
            auth,
            workspace_service,
            config_service,
            host_operation_service,
            network_service,
            repository_service.clone(),
            secrets_service,
        );
        let host_control = HostControlService::new(state.clone());
        Ok(Self {
            prepared,
            repository_service,
            state,
            host_control,
        })
    }
}

struct Prepared {
    socket: PathBuf,
    workspaces_dir: PathBuf,
    repository_cache_dir: PathBuf,
    host_operations_dir: PathBuf,
    sops: PathBuf,
    workspace_service: WorkspaceServiceConfig,
    ui_root: Option<PathBuf>,
}

impl Prepared {
    #[allow(clippy::too_many_lines)]
    fn new(args: &DaemonOptions) -> Result<Self> {
        if args.startup_timeout == 0 || args.shutdown_timeout == 0 {
            bail!("startup and shutdown timeouts must be greater than zero");
        }
        if args.max_workspaces == 0 {
            bail!("--max-workspaces must be greater than zero");
        }
        let state_disk_size = parse_size_bytes(&args.state_disk_size)
            .map_err(|error| anyhow!(error.to_string()))
            .context("parse --state-disk-size")?;
        if state_disk_size < MINIMUM_STATE_DISK_BYTES {
            bail!("state disk size must describe at least 256 MiB");
        }
        let tascarrel_home =
            TascarrelHome::discover().map_err(|error| anyhow!(error.to_string()))?;
        let runtime_dir = tascarrel_home.runtime();
        let socket = args
            .socket
            .clone()
            .unwrap_or_else(|| tascarrel_home.control_socket());
        if !socket.is_absolute() {
            bail!("control socket must be absolute: {}", socket.display());
        }
        let workspaces_dir = tascarrel_home.workspaces();
        let state_dir = tascarrel_home.state();
        fs::create_dir_all(&workspaces_dir).with_context(|| {
            format!(
                "failed to prepare workspace configuration root {}",
                workspaces_dir.display()
            )
        })?;
        create_private_directory(&state_dir)
            .with_context(|| format!("failed to prepare state root {}", state_dir.display()))?;
        let git = resolve_executable(&args.git, "Git")?;
        let sops = host_executable_path(&args.sops)?;
        let repository_cache_dir = state_dir.join("repos");
        let host_operations_dir = state_dir.join("host-operations");

        let mode = if let Some(guest_socket) = args.guest_socket.clone() {
            let workspace = args
                .workspace
                .clone()
                .ok_or_else(|| anyhow!("--workspace is required when using --guest-socket"))?;
            if !args.reset_state_workspaces.is_empty() {
                bail!("workspace state reset cannot be used with --guest-socket");
            }
            WorkspaceMode::External(ExternalWorkspaceConfig {
                workspace_root: workspaces_dir.join(workspace.as_str()),
                workspace_state: state_dir.join("workspaces").join(workspace.as_str()),
                workspace,
                guest_socket,
            })
        } else {
            if args.workspace.is_some() {
                bail!("--workspace is only valid with --guest-socket");
            }
            let image = args
                .image
                .clone()
                .ok_or_else(|| anyhow!("--image is required in managed workspace service mode"))?;
            let (kernel, initrd, kernel_append) = managed_boot(args)?;
            let memory_mib = args.memory.map_or_else(default_vm_memory_mib, Ok)?;
            let vcpu_count = args.cpus.map_or_else(default_vm_cpu_count, Ok)?;
            let local_binaries = args
                .local_binaries
                .clone()
                .map(validate_local_binaries)
                .transpose()?;
            let user_home =
                TascarrelHome::discover_user_home().map_err(|error| anyhow!(error.to_string()))?;
            WorkspaceMode::Managed(ManagedWorkspaceConfig {
                image,
                kernel,
                initrd,
                kernel_append,
                user_home,
                workspaces_dir: workspaces_dir.clone(),
                state_dir,
                local_binaries,
                architecture: args.architecture.map(Architecture::from),
                qemu: args.qemu.clone(),
                acceleration: args.acceleration.into(),
                memory_mib,
                vcpu_count,
                shutdown_timeout: Duration::from_secs(args.shutdown_timeout),
                data_disk_size: state_disk_size,
                reset_data_workspaces: args.reset_state_workspaces.iter().cloned().collect(),
            })
        };

        let ui_root = args.ui_dir.clone().map(validate_ui_root).transpose()?;

        Ok(Self {
            socket,
            workspaces_dir,
            repository_cache_dir,
            host_operations_dir,
            sops,
            workspace_service: WorkspaceServiceConfig {
                runtime_dir,
                git,
                startup_timeout: Duration::from_secs(args.startup_timeout),
                max_workspaces: args.max_workspaces,
                network_request_queue_capacity:
                    WorkspaceServiceConfig::DEFAULT_NETWORK_REQUEST_QUEUE_CAPACITY,
                mux_initial_byte_window: WorkspaceServiceConfig::DEFAULT_MUX_INITIAL_BYTE_WINDOW,
                mux_max_channels: WorkspaceServiceConfig::DEFAULT_MUX_MAX_CHANNELS,
                mux_service_handshake_timeout:
                    WorkspaceServiceConfig::DEFAULT_MUX_SERVICE_HANDSHAKE_TIMEOUT,
                max_concurrent_mux_services:
                    WorkspaceServiceConfig::DEFAULT_MAX_CONCURRENT_MUX_SERVICES,
                mode,
            },
            ui_root,
        })
    }
}

fn validate_web_address(address: SocketAddr) -> Result<SocketAddr> {
    if !address.ip().is_loopback() {
        bail!("web interface address must be loopback-only: {address}");
    }
    Ok(address)
}

fn validate_ui_root(path: PathBuf) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("UI directory must be absolute: {}", path.display());
    }
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("inspect UI directory {}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("UI directory must be a real directory: {}", path.display());
    }
    let index = path.join("index.html");
    let metadata = fs::symlink_metadata(&index)
        .with_context(|| format!("inspect UI entry point {}", index.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("UI entry point must be a regular file: {}", index.display());
    }
    Ok(path)
}

fn managed_boot(args: &DaemonOptions) -> Result<(PathBuf, PathBuf, String)> {
    let kernel = args
        .kernel
        .clone()
        .ok_or_else(|| anyhow!("--kernel is required in managed workspace service mode"))?;
    let initrd = args
        .initrd
        .clone()
        .ok_or_else(|| anyhow!("--initrd is required in managed workspace service mode"))?;
    let kernel_append = args
        .kernel_append
        .clone()
        .ok_or_else(|| anyhow!("--kernel-append is required in managed workspace service mode"))?;
    Ok((kernel, initrd, kernel_append))
}

fn resolve_executable(program: &Path, label: &str) -> Result<PathBuf> {
    if program.is_absolute() {
        return executable(program.to_owned()).ok_or_else(|| {
            anyhow!(
                "{label} executable is not an executable file: {}",
                program.display()
            )
        });
    }
    if program.components().count() != 1 {
        bail!(
            "{label} executable must be absolute or a program name resolved through PATH: {}",
            program.display()
        );
    }
    let search_path = env::var_os("PATH").ok_or_else(|| {
        anyhow!(
            "PATH is unset; use --git or TASCARREL_GIT to provide an absolute {label} executable"
        )
    })?;
    env::split_paths(&search_path)
        .map(|directory| directory.join(program))
        .find_map(executable)
        .ok_or_else(|| {
            anyhow!(
                "could not find {label} executable `{}` in PATH",
                program.display()
            )
        })
}

/// Anchors an optional host executable override before provider commands change
/// their working directory. A bare name remains eligible for PATH lookup when
/// the provider is used.
fn host_executable_path(program: &Path) -> Result<PathBuf> {
    if program.as_os_str().is_empty() {
        bail!("SOPS executable must not be empty");
    }
    if program.is_absolute() || program.components().count() == 1 {
        return Ok(program.to_owned());
    }
    env::current_dir()
        .map(|directory| directory.join(program))
        .context("resolve relative SOPS executable path")
}

fn executable(path: PathBuf) -> Option<PathBuf> {
    let metadata = fs::metadata(&path).ok()?;
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .then(|| fs::canonicalize(&path).unwrap_or(path))
}

fn default_vm_cpu_count() -> Result<u16> {
    let count = std::thread::available_parallelism()
        .context("detect host CPU count")?
        .get();
    u16::try_from(count).context("host CPU count exceeds the QEMU vCPU limit")
}

fn default_vm_memory_mib() -> Result<u32> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    memory_mib_from_total_bytes(system.total_memory())
}

fn memory_mib_from_total_bytes(total: u64) -> Result<u32> {
    const MIB: u64 = 1024 * 1024;

    let memory_mib = total / 3 / MIB;
    if memory_mib == 0 {
        bail!("total host memory is too small to assign one third to a VM");
    }
    Ok(u32::try_from(memory_mib).unwrap_or(u32::MAX))
}

#[tracing::instrument(
    name = "tascarrel_host.local_binaries.validate",
    level = "debug",
    fields(path = %path.display()),
    err
)]
fn validate_local_binaries(path: PathBuf) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!(
            "local binary directory must be absolute: {}",
            path.display()
        );
    }
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("inspect local binary directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "local binary directory must be a real directory: {}",
            path.display()
        );
    }
    for name in ["tascarrel-guest", "tascarrel-podd", "podctl", "tasci-exec"] {
        let binary = path.join(name);
        let metadata = fs::symlink_metadata(&binary)
            .with_context(|| format!("inspect local guest binary {}", binary.display()))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.permissions().mode() & 0o111 == 0
        {
            bail!(
                "local guest binary must be a real executable file: {}",
                binary.display()
            );
        }
    }
    Ok(path)
}

struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn wait(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}

struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if let Err(error) = remove_control_socket(&self.0) {
            warn!(path = %self.0.display(), %error, "failed to remove host control socket");
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(flatten)]
        options: DaemonOptions,
    }

    fn parse(
        arguments: impl IntoIterator<Item = impl Into<std::ffi::OsString> + Clone>,
    ) -> Result<DaemonOptions, clap::Error> {
        TestCli::try_parse_from(arguments).map(|cli| cli.options)
    }

    #[test]
    fn external_mode_requires_one_explicit_workspace() {
        assert!(parse(["tascarrel-host", "--guest-socket", "/tmp/guest.sock",]).is_err());
        let args = parse([
            "tascarrel-host",
            "--guest-socket",
            "/tmp/guest.sock",
            "--workspace",
            "demo",
        ])
        .unwrap();
        assert_eq!(args.workspace.unwrap().as_str(), "demo");
    }

    #[test]
    fn portable_git_default_is_resolved_instead_of_assuming_a_host_layout() {
        let args = parse(["tascarrel-host"]).unwrap();
        assert_eq!(args.git, Path::new("git"));
        let executable = std::env::current_exe().unwrap();
        assert_eq!(resolve_executable(&executable, "test").unwrap(), executable);
        assert!(resolve_executable(Path::new("relative/tool"), "test").is_err());
    }

    #[test]
    fn managed_mode_accepts_one_unified_socket_override() {
        let args = parse([
            "tascarrel-host",
            "--image",
            "/tmp/system.erofs",
            "--kernel",
            "/tmp/kernel",
            "--initrd",
            "/tmp/initrd",
            "--kernel-append",
            "init=/nix/store/example/init",
            "--socket",
            "/tmp/control.sock",
        ]);
        assert!(args.is_ok());
    }

    /// Verifies embedded assets fill defaults without replacing host overrides.
    #[test]
    fn payload_assets_preserve_explicit_host_options() {
        let options = parse([
            "tascarrel-host",
            "--image",
            "/override/system.erofs",
            "--web-address",
            "127.0.0.1:18080",
        ])
        .unwrap()
        .with_payload_defaults(GuestPayload {
            image: PathBuf::from("/payload/system.erofs"),
            kernel: PathBuf::from("/payload/kernel"),
            initrd: PathBuf::from("/payload/initrd"),
            kernel_append: "init=/payload/init".to_owned(),
            ui: PathBuf::from("/payload/ui"),
        });

        assert_eq!(options.image.unwrap(), Path::new("/override/system.erofs"));
        assert_eq!(options.kernel.unwrap(), Path::new("/payload/kernel"));
        assert_eq!(options.initrd.unwrap(), Path::new("/payload/initrd"));
        assert_eq!(options.kernel_append.unwrap(), "init=/payload/init");
        assert_eq!(options.ui_dir.unwrap(), Path::new("/payload/ui"));
        assert_eq!(
            options.web_address.unwrap(),
            "127.0.0.1:18080".parse().unwrap()
        );
    }

    #[test]
    fn web_interface_is_loopback_only() {
        assert!(validate_web_address("127.0.0.1:8272".parse().unwrap()).is_ok());
        assert!(validate_web_address("[::1]:8272".parse().unwrap()).is_ok());
        assert!(validate_web_address("0.0.0.0:8272".parse().unwrap()).is_err());
    }

    #[test]
    fn default_vm_memory_is_one_third_of_total_memory() {
        assert_eq!(
            memory_mib_from_total_bytes(24 * 1024 * 1024 * 1024).unwrap(),
            8192
        );
        assert!(memory_mib_from_total_bytes(2 * 1024 * 1024).is_err());
    }

    /// Verifies development binary shares contain only real executable files.
    #[test]
    fn local_binary_share_requires_executable_guest_and_podd_files() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["tascarrel-guest", "tascarrel-podd", "podctl", "tasci-exec"] {
            let binary = directory.path().join(name);
            fs::write(&binary, name).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(validate_local_binaries(directory.path().to_owned()).is_ok());

        fs::set_permissions(
            directory.path().join("tascarrel-podd"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(validate_local_binaries(directory.path().to_owned()).is_err());
    }
}
