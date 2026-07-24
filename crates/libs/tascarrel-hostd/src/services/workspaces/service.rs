//! Host-owned workspace inventory and VM lifecycle implementation.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{self};
use std::num::NonZeroUsize;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::parse_memory_mib;
use tascarrel_api::parse_size_bytes;
use tascarrel_api::types::guest::GuestInstanceId;
use tascarrel_api::types::store as store_api;
use tascarrel_api::types::workspaces as api;
use tascarrel_mux::Config as MuxConfig;
use tascarrel_mux::MuxHandle;
use tascarrel_mux::Role as MuxRole;
use tascarrel_mux::connect as connect_mux;
use tascarrel_protocol::ErrorCode;
use tascarrel_protocol::Framed;
use tascarrel_protocol::GuestControlIdentity;
use tascarrel_protocol::MUX_CONTROL_PLANE_ENDPOINT;
use tascarrel_protocol::RemoteError;
use tascarrel_protocol::WorkspaceName;
use tascarrel_protocol::control_plane::StreamTransport;
use tascarrel_protocol::control_plane::server::Connection as ControlPlaneConnection;
use tascarrel_protocol::control_plane::server::Peer as ControlPlanePeer;
use tascarrel_store::Store;
use tascarrel_vm::Acceleration;
use tascarrel_vm::Architecture;
use tascarrel_vm::SharedDirectory;
use tascarrel_vm::Vm;
use tascarrel_vm::VmConfig;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::sleep_until;
use tracing::Instrument;
use tracing::info;
use tracing::warn;

use super::mux::WorkspaceEnvironmentRequestSender;
use super::mux::WorkspaceEnvironmentRequests;
use super::mux::WorkspaceMuxHost;
use super::mux::WorkspaceMuxHostConfig;
use super::mux::WorkspaceMuxHostError;
use super::mux::WorkspaceNetworkRequestSender;
use super::mux::WorkspaceNetworkRequests;
use super::usb::DEFAULT_USB_RECONCILE_INTERVAL;
use super::usb::UsbDeviceRegistry;
use super::usb::UsbForwardError;
use super::usb::WorkspaceUsbForwards;
use super::usb::run_usb_inventory;
use super::watcher::WatchEvents;
use super::watcher::WatchMessage;
use crate::HostRepositoryManager;
use crate::WorkspaceAuthority;
use crate::control_plane::HostControlService;
use crate::services::config::DEFAULT_MAX_CONFIG_BYTES;
use crate::services::config::load_config_file;
use crate::services::network::NetworkPolicy;

const WORKSPACE_QUEUE_CAPACITY: usize = 64;
const ERROR_DETAIL_LIMIT: usize = 2048;
const MINIMUM_DATA_DISK_BYTES: u64 = 256 * 1024 * 1024;
const LOCAL_BINARIES_MOUNT_TAG: &str = "tascarrel-binaries";
const LOCAL_BINARIES_KERNEL_PARAMETER: &str = "tascarrel.local-binaries=1";
const GUEST_INSTANCE_KERNEL_PARAMETER: &str = "tascarrel.guest-instance-id";
const DEFAULT_WORKSPACE_CONFIG: &str = "[features]\ndocker = true\n";
const DEFAULT_WORKSPACE_DOCKERFILE: &str = r"FROM debian:stable-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git starship zsh \
    && rm -rf /var/lib/apt/lists/*

ENV SHELL=/usr/bin/zsh
ENV LANG=C.UTF-8
ENV LC_ALL=C.UTF-8
WORKDIR /workspace
";
const WORKSPACE_STORE_HISTORY_LIMIT: NonZeroUsize =
    NonZeroUsize::new(256).expect("the workspace store history limit is non-zero");
const VM_LOG_LINE_HISTORY_LIMIT: NonZeroUsize =
    NonZeroUsize::new(8192).expect("the VM log line history limit is non-zero");
const VM_LOG_BATCH_LIMIT: NonZeroUsize =
    NonZeroUsize::new(128).expect("the VM log batch limit is non-zero");
const VM_LOG_LINE_BYTE_LIMIT: NonZeroUsize =
    NonZeroUsize::new(16 * 1024).expect("the VM log line byte limit is non-zero");
const VM_LOG_COMPLETED_INSTANCE_HISTORY_LIMIT: usize = 256;
const WORKSPACE_WATCH_CHANNEL_CAPACITY: NonZeroUsize =
    NonZeroUsize::new(256).expect("the workspace watcher capacity is non-zero");
const WORKSPACE_WATCH_DEBOUNCE: Duration = Duration::from_millis(250);
const VM_FAILURE_LOG_GRACE_PERIOD: Duration = Duration::from_secs(10);

/// Global VM settings shared by every lazily started managed workspace.
#[derive(Clone, Debug)]
pub struct ManagedWorkspaceConfig {
    pub image: PathBuf,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub kernel_append: String,
    pub workspaces_dir: PathBuf,
    pub state_dir: PathBuf,
    pub local_binaries: Option<PathBuf>,
    pub architecture: Option<Architecture>,
    pub qemu: Option<PathBuf>,
    pub acceleration: Acceleration,
    pub memory_mib: u32,
    pub vcpu_count: u16,
    pub shutdown_timeout: Duration,
    pub data_disk_size: u64,
    pub reset_data_workspaces: HashSet<WorkspaceName>,
}

/// A single externally launched guest assigned to one explicit workspace.
#[derive(Clone, Debug)]
pub struct ExternalWorkspaceConfig {
    pub workspace: WorkspaceName,
    pub guest_socket: PathBuf,
    pub workspace_root: PathBuf,
    pub workspace_state: PathBuf,
}

/// Selects whether workspace requests launch managed QEMU VMs or reach one
/// externally supplied development guest.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // The managed configuration is the common mode and public API.
pub enum WorkspaceMode {
    Managed(ManagedWorkspaceConfig),
    External(ExternalWorkspaceConfig),
}

/// Configuration for the per-user workspace service.
#[derive(Clone, Debug)]
pub struct WorkspaceServiceConfig {
    pub runtime_dir: PathBuf,
    pub git: PathBuf,
    pub startup_timeout: Duration,
    pub max_workspaces: usize,
    /// Maximum number of accepted guest network channels awaiting hostd.
    pub network_request_queue_capacity: usize,
    /// Initial per-channel byte window for each workspace mux.
    pub mux_initial_byte_window: u32,
    /// Maximum number of concurrently open channels on each workspace mux.
    pub mux_max_channels: usize,
    /// Maximum time to drain a completed workspace service handshake.
    pub mux_service_handshake_timeout: Duration,
    /// Maximum number of non-network workspace services served concurrently.
    pub max_concurrent_mux_services: usize,
    pub mode: WorkspaceMode,
}

impl WorkspaceServiceConfig {
    /// Default number of accepted guest network channels that may await
    /// `NetworkService` dispatch.
    pub const DEFAULT_NETWORK_REQUEST_QUEUE_CAPACITY: usize = 512;
    /// Default initial per-channel byte window for a workspace mux.
    pub const DEFAULT_MUX_INITIAL_BYTE_WINDOW: u32 = 64 * 1024;
    /// Default maximum number of open channels on a workspace mux.
    pub const DEFAULT_MUX_MAX_CHANNELS: usize = 512;
    /// Default workspace service handshake drain timeout.
    pub const DEFAULT_MUX_SERVICE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
    /// Default maximum number of concurrently served non-network mux channels.
    pub const DEFAULT_MAX_CONCURRENT_MUX_SERVICES: usize = 512;

    /// Validates values which would otherwise create unbounded or ambiguous
    /// worker behavior.
    ///
    /// # Errors
    ///
    /// Returns an error for zero limits, relative private paths, or an invalid
    /// managed VM shutdown timeout.
    pub fn validate(&self) -> Result<()> {
        if self.startup_timeout.is_zero() {
            bail!("workspace startup timeout must be greater than zero");
        }
        if self.max_workspaces == 0 {
            bail!("maximum workspace count must be greater than zero");
        }
        if self.network_request_queue_capacity == 0 {
            bail!("network request queue capacity must be greater than zero");
        }
        if self.mux_initial_byte_window == 0
            || self.mux_max_channels == 0
            || self.mux_service_handshake_timeout.is_zero()
            || self.max_concurrent_mux_services == 0
        {
            bail!("workspace mux limits and timeouts must be greater than zero");
        }
        if !self.runtime_dir.is_absolute() {
            bail!(
                "runtime directory must be absolute: {}",
                self.runtime_dir.display()
            );
        }
        if !self.git.is_absolute() {
            bail!("Git executable must be absolute: {}", self.git.display());
        }
        if let WorkspaceMode::Managed(managed) = &self.mode {
            if !managed.image.is_absolute()
                || !managed.kernel.is_absolute()
                || !managed.initrd.is_absolute()
                || !managed.workspaces_dir.is_absolute()
                || !managed.state_dir.is_absolute()
                || managed
                    .local_binaries
                    .as_ref()
                    .is_some_and(|path| !path.is_absolute())
            {
                bail!(
                    "managed image, kernel, initrd, workspace, state, and local binary paths must be absolute"
                );
            }
            if managed.kernel_append.trim().is_empty() {
                bail!("managed direct-boot kernel command line must not be empty");
            }
            if managed
                .kernel_append
                .split_ascii_whitespace()
                .any(|argument| {
                    argument.split_once('=').map_or(argument, |(name, _)| name)
                        == GUEST_INSTANCE_KERNEL_PARAMETER
                })
            {
                bail!(
                    "managed direct-boot kernel command line must not set the host-owned {GUEST_INSTANCE_KERNEL_PARAMETER} parameter"
                );
            }
            if managed.shutdown_timeout.is_zero() {
                bail!("VM shutdown timeout must be greater than zero");
            }
            if managed.memory_mib == 0 || managed.vcpu_count == 0 {
                bail!("managed VM memory and virtual CPU counts must be greater than zero");
            }
            if managed.data_disk_size < MINIMUM_DATA_DISK_BYTES {
                bail!("managed VM state disks must be at least 256 MiB");
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
enum WorkspaceCommand {
    Connect {
        response: oneshot::Sender<Result<MuxHandle, RemoteError>>,
    },
    ControlPlane {
        host_control: HostControlService,
        response: oneshot::Sender<Result<ControlPlanePeer, RemoteError>>,
    },
    AttachUsb {
        device_id: String,
        response: oneshot::Sender<Result<(), RemoteError>>,
    },
    DetachUsb {
        device_id: String,
        response: oneshot::Sender<Result<(), RemoteError>>,
    },
    Stop {
        response: oneshot::Sender<Result<(), RemoteError>>,
    },
}

#[derive(Clone)]
pub struct WorkspaceService {
    inner: Arc<WorkspaceServiceInner>,
}

/// Observable lifecycle state of one configured workspace VM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceRuntimeState {
    Stopped,
    Starting(GuestInstanceId),
    Running(GuestInstanceId),
    Stopping(GuestInstanceId),
    Destroying,
    Failed {
        guest_instance_id: Option<GuestInstanceId>,
        message: String,
        failed_at: Timestamp,
    },
}

/// Resumable host-wide workspace list stream.
pub type WorkspaceListSubscription =
    tascarrel_store::Subscription<api::WorkspaceList, api::WorkspaceListMutation>;

/// Caller-relevant workspace service failure categories.
#[derive(Debug, Error)]
pub enum WorkspaceServiceError {
    /// The requested name, cursor, or lifecycle transition is invalid.
    #[error("invalid workspace request: {0}")]
    InvalidRequest(String),
    /// The requested lifecycle resource is temporarily unavailable.
    #[error("workspace service is unavailable: {0}")]
    Unavailable(String),
    /// Workspace infrastructure failed unexpectedly.
    #[error("workspace service failed: {0}")]
    Internal(String),
}

type WorkspaceStore = Store<api::WorkspaceList, api::WorkspaceListMutation>;

struct WorkspaceServiceInner {
    config: WorkspaceServiceConfig,
    workers: Mutex<HashMap<WorkspaceName, mpsc::Sender<WorkspaceCommand>>>,
    stopping_workspaces: StdMutex<HashSet<WorkspaceName>>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
    shutdown: watch::Sender<bool>,
    shutdown_lock: Mutex<()>,
    reset_data_workspaces: StdMutex<HashSet<WorkspaceName>>,
    states: Arc<Mutex<HashMap<WorkspaceName, WorkspaceRuntimeState>>>,
    store: WorkspaceStore,
    logs: Arc<StdMutex<WorkspaceVmLogs>>,
    request_senders: WorkspaceRequestSenders,
    pending_network_requests: StdMutex<Option<WorkspaceNetworkRequests>>,
    pending_environment_requests: StdMutex<Option<WorkspaceEnvironmentRequests>>,
    usb_devices: UsbDeviceRegistry,
}

/// Host service queues supplied to each workspace runtime.
#[derive(Clone)]
struct WorkspaceRequestSenders {
    network: WorkspaceNetworkRequestSender,
    environment: WorkspaceEnvironmentRequestSender,
}

#[derive(Default)]
struct WorkspaceVmLogs {
    instances: HashMap<GuestInstanceId, super::log::WorkspaceVmLog>,
    order: VecDeque<GuestInstanceId>,
}

impl WorkspaceVmLogs {
    /// Retains one instance log while bounding completed VM history in memory.
    fn insert(
        &mut self,
        id: GuestInstanceId,
        log: super::log::WorkspaceVmLog,
        protected: &HashSet<GuestInstanceId>,
    ) {
        if self.instances.insert(id.clone(), log).is_some() {
            return;
        }
        self.order.push_back(id);
        while self
            .order
            .iter()
            .filter(|id| !protected.contains(*id))
            .count()
            > VM_LOG_COMPLETED_INSTANCE_HISTORY_LIMIT
        {
            let index = self
                .order
                .iter()
                .position(|id| !protected.contains(id))
                .expect("an over-limit completed VM log history has an oldest instance");
            let expired = self
                .order
                .remove(index)
                .expect("the completed VM log instance index is valid");
            self.instances.remove(&expired);
        }
    }

    /// Returns one retained VM instance log.
    fn get(&self, id: &GuestInstanceId) -> Option<&super::log::WorkspaceVmLog> {
        self.instances.get(id)
    }
}

impl std::fmt::Debug for WorkspaceService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceService")
            .field("runtime_dir", &self.inner.config.runtime_dir)
            .finish_non_exhaustive()
    }
}

impl WorkspaceService {
    /// Creates an idle service. No VM is started until [`Self::connect`].
    ///
    /// # Errors
    ///
    /// Returns an error when `config` contains an invalid limit or path.
    pub fn new(config: WorkspaceServiceConfig) -> Result<Self> {
        config.validate()?;
        let watcher = match &config.mode {
            WorkspaceMode::Managed(managed) => Some(
                WatchEvents::open(
                    &managed.workspaces_dir,
                    WORKSPACE_WATCH_CHANNEL_CAPACITY.get(),
                )
                .map_err(|error| anyhow::anyhow!(error.to_string()))?,
            ),
            WorkspaceMode::External(_) => None,
        };
        let names = match &config.mode {
            WorkspaceMode::External(external) => vec![external.workspace.clone()],
            WorkspaceMode::Managed(managed) => configured_workspace_names(&managed.workspaces_dir)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?,
        };
        let workspaces = names
            .into_iter()
            .map(|name| api::Workspace {
                name: api_name(&name),
                state: api::WorkspaceState::Stopped,
            })
            .collect();
        let store = Store::new(
            api::WorkspaceList { workspaces },
            reduce_workspace_list,
            WORKSPACE_STORE_HISTORY_LIMIT,
        );
        let (shutdown, _) = watch::channel(false);
        let reset_data_workspaces = match &config.mode {
            WorkspaceMode::Managed(managed) => managed.reset_data_workspaces.clone(),
            WorkspaceMode::External(_) => HashSet::new(),
        };
        let (network_requests, pending_network_requests) =
            mpsc::channel(config.network_request_queue_capacity);
        let (environment_requests, pending_environment_requests) =
            mpsc::channel(config.network_request_queue_capacity);
        let usb_supported =
            cfg!(target_os = "linux") && matches!(&config.mode, WorkspaceMode::Managed(_));
        let usb_devices = UsbDeviceRegistry::new(usb_supported);
        let service = Self {
            inner: Arc::new(WorkspaceServiceInner {
                config,
                workers: Mutex::new(HashMap::new()),
                stopping_workspaces: StdMutex::new(HashSet::new()),
                tasks: StdMutex::new(Vec::new()),
                shutdown,
                shutdown_lock: Mutex::new(()),
                reset_data_workspaces: StdMutex::new(reset_data_workspaces),
                states: Arc::new(Mutex::new(HashMap::new())),
                store,
                logs: Arc::new(StdMutex::new(WorkspaceVmLogs::default())),
                request_senders: WorkspaceRequestSenders {
                    network: network_requests,
                    environment: environment_requests,
                },
                pending_network_requests: StdMutex::new(Some(pending_network_requests)),
                pending_environment_requests: StdMutex::new(Some(pending_environment_requests)),
                usb_devices: usb_devices.clone(),
            }),
        };
        if let Some(watcher) = watcher {
            tokio::runtime::Handle::try_current()
                .context("workspace service requires a Tokio runtime for filesystem observation")?;
            tokio::spawn(run_workspace_watcher(
                Arc::downgrade(&service.inner),
                watcher,
                service.inner.shutdown.subscribe(),
            ));
        }
        if usb_supported {
            let task = tokio::spawn(run_usb_inventory(
                usb_devices,
                service.inner.shutdown.subscribe(),
            ));
            service
                .inner
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(task);
        }
        Ok(service)
    }

    /// Takes the private network request stream for operation by hostd's
    /// network service.
    ///
    /// # Errors
    ///
    /// Returns an error if the single-consumer stream was already taken.
    pub(crate) fn take_network_requests(
        &self,
    ) -> Result<WorkspaceNetworkRequests, Report<WorkspaceServiceError>> {
        self.inner
            .pending_network_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| internal("workspace network request stream was already taken"))
    }

    /// Takes the private environment request stream for operation by hostd's
    /// secrets service.
    pub(crate) fn take_environment_requests(
        &self,
    ) -> Result<WorkspaceEnvironmentRequests, Report<WorkspaceServiceError>> {
        self.inner
            .pending_environment_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| internal("workspace environment request stream was already taken"))
    }

    /// Creates a workspace with the default host development configuration.
    pub async fn create(
        &self,
        input: api::CreateWorkspaceAction,
    ) -> Result<api::CreateWorkspaceOutput, Report<WorkspaceServiceError>> {
        let name = runtime_name(&input.name)?;
        let WorkspaceMode::Managed(managed) = &self.inner.config.mode else {
            return Err(invalid_request(
                "workspaces cannot be created while hostd uses an external guest",
            ));
        };
        let root = managed.workspaces_dir.clone();
        let created_name = name.clone();
        tokio::task::spawn_blocking(move || create_default_workspace(&root, &created_name))
            .await
            .map_err(|error| internal(format!("workspace creation task failed: {error}")))?
            .map_err(|error| internal(error.to_string()))?;
        publish_workspace_state(
            &self.inner.states,
            &self.inner.store,
            &name,
            WorkspaceRuntimeState::Stopped,
        )
        .await;
        Ok(api::CreateWorkspaceOutput {})
    }

    /// Starts one stopped workspace VM.
    pub async fn start(
        &self,
        input: api::StartWorkspaceAction,
    ) -> Result<api::StartWorkspaceOutput, Report<WorkspaceServiceError>> {
        let name = runtime_name(&input.workspace)?;
        drop(self.connect(name).await.map_err(service_remote_error)?);
        Ok(api::StartWorkspaceOutput {})
    }

    /// Stops one workspace VM while retaining its state partition.
    pub async fn stop(
        &self,
        input: api::StopWorkspaceAction,
    ) -> Result<api::StopWorkspaceOutput, Report<WorkspaceServiceError>> {
        let workspace = runtime_name(&input.workspace)?;
        self.validate_managed_workspace(&workspace)
            .map_err(service_remote_error)?;
        if let Some(guest_instance_id) = self.current_guest_instance(&workspace).await {
            publish_workspace_state(
                &self.inner.states,
                &self.inner.store,
                &workspace,
                WorkspaceRuntimeState::Stopping(guest_instance_id),
            )
            .await;
        }
        let sender = self
            .begin_workspace_stop(&workspace)
            .await
            .map_err(service_remote_error)?;
        let result = stop_workspace_sender(sender).await;
        self.end_workspace_stop(&workspace).await;
        result.map_err(service_remote_error)?;
        publish_workspace_state(
            &self.inner.states,
            &self.inner.store,
            &workspace,
            WorkspaceRuntimeState::Stopped,
        )
        .await;
        Ok(api::StopWorkspaceOutput {})
    }

    /// Destroys one workspace configuration and state partition.
    pub async fn destroy(
        &self,
        input: api::DestroyWorkspaceAction,
    ) -> Result<api::DestroyWorkspaceOutput, Report<WorkspaceServiceError>> {
        let workspace = runtime_name(&input.workspace)?;
        self.validate_managed_workspace(&workspace)
            .map_err(service_remote_error)?;
        let sender = self
            .begin_workspace_stop(&workspace)
            .await
            .map_err(service_remote_error)?;
        let result = stop_workspace_sender(sender).await;
        self.end_workspace_stop(&workspace).await;
        result.map_err(service_remote_error)?;
        publish_workspace_state(
            &self.inner.states,
            &self.inner.store,
            &workspace,
            WorkspaceRuntimeState::Destroying,
        )
        .await;
        self.delete_managed_workspace(&workspace)
            .await
            .map_err(service_remote_error)?;
        Ok(api::DestroyWorkspaceOutput {})
    }

    /// Opens a resumable subscription to the host-wide workspace list.
    pub fn subscribe(
        &self,
        input: api::WorkspaceListChangedSubscription,
    ) -> Result<WorkspaceListSubscription, Report<WorkspaceServiceError>> {
        let cursor = input.cursor.map(runtime_stamp).transpose()?;
        Ok(self.inner.store.subscribe(cursor))
    }

    /// Opens a retained and live log subscription for one VM instance.
    pub fn subscribe_vm_log(
        &self,
        input: api::WorkspaceVmLogSubscription,
    ) -> Result<super::log::WorkspaceVmLogSubscription, Report<WorkspaceServiceError>> {
        let logs = self
            .inner
            .logs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let log = logs.get(&input.guest_instance_id).ok_or_else(|| {
            invalid_request(format!(
                "workspace VM instance {} is unknown",
                input.guest_instance_id.0
            ))
        })?;
        Ok(log.subscribe(input.last_line))
    }

    /// Advertised upper bound for a cold workspace start.
    #[must_use]
    pub fn startup_timeout(&self) -> Duration {
        self.inner.config.startup_timeout
    }

    /// Returns a ready guest mux, coalescing concurrent cold-start requests for
    /// the same workspace while allowing different workspaces to boot in
    /// parallel.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the workspace is missing, the configured
    /// worker limit is reached, startup fails, or shutdown has begun.
    pub async fn connect(&self, workspace: WorkspaceName) -> Result<MuxHandle, RemoteError> {
        if *self.inner.shutdown.borrow() {
            return Err(RemoteError::new(
                ErrorCode::Busy,
                "host daemon is shutting down",
            ));
        }
        self.validate_workspace_source(&workspace)?;

        for _ in 0..2 {
            let sender = self.worker_sender(&workspace).await?;
            let (response, result) = oneshot::channel();
            if sender
                .send(WorkspaceCommand::Connect { response })
                .await
                .is_err()
            {
                self.remove_closed_worker(&workspace, &sender).await;
                continue;
            }
            return result.await.unwrap_or_else(|_| {
                Err(RemoteError::new(
                    ErrorCode::ExecutionFailed,
                    "workspace worker stopped before completing startup",
                ))
            });
        }
        Err(RemoteError::new(
            ErrorCode::ExecutionFailed,
            "workspace worker could not be restarted",
        ))
    }

    /// Validates that a workspace is configured without starting its VM.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the workspace is not assigned to this host
    /// service or lacks its required managed image directory.
    pub(crate) fn validate_workspace(&self, workspace: &WorkspaceName) -> Result<(), RemoteError> {
        self.validate_workspace_source(workspace)
    }

    /// Returns the reusable full-duplex control-plane peer for one workspace.
    ///
    /// Concurrent cold-start requests are coalesced by the workspace worker in
    /// the same way as [`Self::connect`].
    ///
    /// # Errors
    ///
    /// Returns a typed error when the workspace is missing, startup fails, the
    /// control-plane connection stops, or shutdown has begun.
    pub(crate) async fn control_plane(
        &self,
        workspace: WorkspaceName,
        host_control: HostControlService,
    ) -> Result<ControlPlanePeer, RemoteError> {
        if *self.inner.shutdown.borrow() {
            return Err(RemoteError::new(
                ErrorCode::Busy,
                "host daemon is shutting down",
            ));
        }
        self.validate_workspace_source(&workspace)?;

        for _ in 0..2 {
            let sender = self.worker_sender(&workspace).await?;
            let (response, result) = oneshot::channel();
            if sender
                .send(WorkspaceCommand::ControlPlane {
                    host_control: host_control.clone(),
                    response,
                })
                .await
                .is_err()
            {
                self.remove_closed_worker(&workspace, &sender).await;
                continue;
            }
            return result.await.unwrap_or_else(|_| {
                Err(RemoteError::new(
                    ErrorCode::ExecutionFailed,
                    "workspace worker stopped before connecting its control plane",
                ))
            });
        }
        Err(RemoteError::new(
            ErrorCode::ExecutionFailed,
            "workspace worker could not be restarted",
        ))
    }

    async fn current_guest_instance(&self, workspace: &WorkspaceName) -> Option<GuestInstanceId> {
        match self.inner.states.lock().await.get(workspace) {
            Some(
                WorkspaceRuntimeState::Starting(id)
                | WorkspaceRuntimeState::Running(id)
                | WorkspaceRuntimeState::Stopping(id),
            ) => Some(id.clone()),
            Some(WorkspaceRuntimeState::Failed {
                guest_instance_id: Some(id),
                ..
            }) => Some(id.clone()),
            _ => None,
        }
    }

    /// Attaches one connected host USB device to a running workspace VM.
    ///
    /// # Errors
    ///
    /// Returns a typed error when forwarding is unavailable, disabled, or
    /// rejected by the VM.
    pub async fn attach_usb_device(
        &self,
        input: api::AttachUsbDeviceAction,
    ) -> Result<api::AttachUsbDeviceOutput, Report<WorkspaceServiceError>> {
        let workspace = runtime_name(&input.workspace)?;
        let sender = self
            .running_usb_worker(&workspace)
            .await
            .map_err(service_remote_error)?;
        let (response, result) = oneshot::channel();
        sender
            .send(WorkspaceCommand::AttachUsb {
                device_id: input.device_id.as_str().to_owned(),
                response,
            })
            .await
            .map_err(|_| {
                WorkspaceServiceError::Unavailable(
                    "Workspace stopped before the USB device could be attached".to_owned(),
                )
                .report()
            })?;
        result
            .await
            .map_err(|_| {
                WorkspaceServiceError::Unavailable(
                    "Workspace stopped before the USB device could be attached".to_owned(),
                )
                .report()
            })?
            .map_err(service_remote_error)?;
        Ok(api::AttachUsbDeviceOutput {})
    }

    /// Detaches one host USB device from a running workspace VM.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the workspace is not running, does not own
    /// the device, or the VM rejects the operation.
    pub async fn detach_usb_device(
        &self,
        input: api::DetachUsbDeviceAction,
    ) -> Result<api::DetachUsbDeviceOutput, Report<WorkspaceServiceError>> {
        let workspace = runtime_name(&input.workspace)?;
        let sender = self
            .running_usb_worker(&workspace)
            .await
            .map_err(service_remote_error)?;
        let (response, result) = oneshot::channel();
        sender
            .send(WorkspaceCommand::DetachUsb {
                device_id: input.device_id.as_str().to_owned(),
                response,
            })
            .await
            .map_err(|_| {
                WorkspaceServiceError::Unavailable(
                    "Workspace stopped before the USB device could be detached".to_owned(),
                )
                .report()
            })?;
        result
            .await
            .map_err(|_| {
                WorkspaceServiceError::Unavailable(
                    "Workspace stopped before the USB device could be detached".to_owned(),
                )
                .report()
            })?
            .map_err(service_remote_error)?;
        Ok(api::DetachUsbDeviceOutput {})
    }

    /// Opens a full-snapshot stream of connected host USB devices.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the workspace is invalid or unavailable.
    pub fn subscribe_usb_devices(
        &self,
        input: &api::UsbDevicesChangedSubscription,
    ) -> Result<super::usb::UsbDeviceSubscription, Report<WorkspaceServiceError>> {
        let workspace = runtime_name(&input.workspace)?;
        self.validate_workspace_source(&workspace)
            .map_err(service_remote_error)?;
        Ok(self.inner.usb_devices.subscribe())
    }

    /// Stops accepting workspace starts and awaits every running VM worker.
    pub async fn shutdown(&self) {
        // Concurrent shutdown callers must all observe completed VM teardown,
        // rather than allowing one caller to take the task handles while a
        // second returns immediately.
        let _shutdown = self.inner.shutdown_lock.lock().await;
        // `send` does not update a watch channel when there are no receivers.
        // That is exactly the idle-service case, and would otherwise allow
        // a later `connect` call to start a VM after shutdown returned.
        self.inner.shutdown.send_replace(true);
        self.inner.workers.lock().await.clear();
        let tasks = {
            let mut tasks = self
                .inner
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *tasks)
        };
        for task in tasks {
            if let Err(error) = task.await {
                warn!(%error, "workspace worker task failed during shutdown");
            }
        }
        let names = self
            .inner
            .states
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for workspace in names {
            publish_workspace_state(
                &self.inner.states,
                &self.inner.store,
                &workspace,
                WorkspaceRuntimeState::Stopped,
            )
            .await;
        }
    }

    fn validate_workspace_source(&self, workspace: &WorkspaceName) -> Result<(), RemoteError> {
        match &self.inner.config.mode {
            WorkspaceMode::External(external) if &external.workspace != workspace => {
                Err(RemoteError::new(
                    ErrorCode::NotFound,
                    format!("workspace {workspace} is not assigned to the external guest"),
                ))
            }
            WorkspaceMode::Managed(managed) => {
                let image = managed
                    .workspaces_dir
                    .join(workspace.as_str())
                    .join("image");
                if !image.is_dir() {
                    return Err(RemoteError::new(
                        ErrorCode::NotFound,
                        format!(
                            "workspace {workspace} has no image directory at {}",
                            image.display()
                        ),
                    ));
                }
                Ok(())
            }
            WorkspaceMode::External(_) => Ok(()),
        }
    }

    fn validate_managed_workspace(&self, workspace: &WorkspaceName) -> Result<(), RemoteError> {
        if !matches!(self.inner.config.mode, WorkspaceMode::Managed(_)) {
            return Err(RemoteError::new(
                ErrorCode::Unsupported,
                "workspace stop and delete require a hostd-managed VM",
            ));
        }
        self.validate_workspace_source(workspace)
    }

    async fn begin_workspace_stop(
        &self,
        workspace: &WorkspaceName,
    ) -> Result<Option<mpsc::Sender<WorkspaceCommand>>, RemoteError> {
        let mut workers = self.inner.workers.lock().await;
        let mut stopping = self
            .inner
            .stopping_workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !stopping.insert(workspace.clone()) {
            return Err(RemoteError::new(
                ErrorCode::Busy,
                format!("workspace {workspace} is already stopping"),
            ));
        }
        Ok(workers.remove(workspace))
    }

    async fn end_workspace_stop(&self, workspace: &WorkspaceName) {
        // Serialize removal with `worker_sender`, which checks the same set
        // while holding the workers lock.
        let _workers = self.inner.workers.lock().await;
        self.inner
            .stopping_workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(workspace);
    }

    async fn delete_managed_workspace(&self, workspace: &WorkspaceName) -> Result<(), RemoteError> {
        let WorkspaceMode::Managed(managed) = &self.inner.config.mode else {
            unreachable!("managed workspace was validated before deletion");
        };
        let state = managed
            .state_dir
            .join("workspaces")
            .join(workspace.as_str());
        let config = managed.workspaces_dir.join(workspace.as_str());
        let preflight_state = state.clone();
        let preflight_config = config.clone();
        let preflight_workspace = workspace.clone();
        tokio::task::spawn_blocking(move || {
            inspect_owned_workspace_tree(&preflight_state, false)?;
            inspect_owned_workspace_tree(&preflight_config, true)?;
            Ok::<(), io::Error>(())
        })
        .await
        .map_err(|error| {
            RemoteError::new(
                ErrorCode::ExecutionFailed,
                format!("workspace {preflight_workspace} deletion preflight failed: {error}"),
            )
        })?
        .map_err(|error| {
            RemoteError::new(
                ErrorCode::ExecutionFailed,
                format!("cannot safely delete workspace {preflight_workspace}: {error}"),
            )
        })?;

        HostRepositoryManager::remove_workspace_cache(
            managed.state_dir.join("repos"),
            workspace.as_str().to_owned(),
        )
        .await
        .map_err(|error| repository_error(&error))?;

        let workspace = workspace.clone();
        tokio::task::spawn_blocking(move || {
            remove_owned_workspace_tree(&state, false)?;
            remove_owned_workspace_tree(&config, true)
        })
        .await
        .map_err(|error| {
            RemoteError::new(
                ErrorCode::ExecutionFailed,
                format!("workspace {workspace} deletion task failed: {error}"),
            )
        })?
        .map_err(|error| {
            RemoteError::new(
                ErrorCode::ExecutionFailed,
                format!("cannot delete workspace {workspace}: {error}"),
            )
        })?;
        self.inner.states.lock().await.remove(&workspace);
        self.inner
            .store
            .apply(api::WorkspaceListMutation::Remove(api_name(&workspace)));
        Ok(())
    }

    async fn worker_sender(
        &self,
        workspace: &WorkspaceName,
    ) -> Result<mpsc::Sender<WorkspaceCommand>, RemoteError> {
        let mut workers = self.inner.workers.lock().await;
        // Serialize this check with worker insertion. `shutdown` sets the
        // watch value before acquiring the same map lock, so it cannot miss a
        // task spawned concurrently with its task-handle snapshot.
        if *self.inner.shutdown.borrow() {
            return Err(RemoteError::new(
                ErrorCode::Busy,
                "host daemon is shutting down",
            ));
        }
        if self
            .inner
            .stopping_workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(workspace)
        {
            return Err(RemoteError::new(
                ErrorCode::Busy,
                format!("workspace {workspace} is stopping"),
            ));
        }
        workers.retain(|_, sender| !sender.is_closed());
        if let Some(sender) = workers.get(workspace) {
            return Ok(sender.clone());
        }
        if workers.len() >= self.inner.config.max_workspaces {
            return Err(RemoteError::new(
                ErrorCode::ResourceExhausted,
                format!(
                    "host daemon already manages its maximum of {} workspaces",
                    self.inner.config.max_workspaces
                ),
            ));
        }

        let (sender, receiver) = mpsc::channel(WORKSPACE_QUEUE_CAPACITY);
        workers.insert(workspace.clone(), sender.clone());
        let config = self.inner.config.clone();
        let shutdown = self.inner.shutdown.subscribe();
        let reset_data = self
            .inner
            .reset_data_workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(workspace);
        let name = workspace.clone();
        let states = Arc::clone(&self.inner.states);
        let store = self.inner.store.clone();
        let logs = Arc::clone(&self.inner.logs);
        let request_senders = self.inner.request_senders.clone();
        let usb_devices = self.inner.usb_devices.clone();
        let task = tokio::spawn(async move {
            workspace_worker(
                name,
                config,
                receiver,
                shutdown,
                reset_data,
                states,
                store,
                logs,
                request_senders,
                usb_devices,
            )
            .await;
        });
        self.inner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
        Ok(sender)
    }

    async fn running_usb_worker(
        &self,
        workspace: &WorkspaceName,
    ) -> Result<mpsc::Sender<WorkspaceCommand>, RemoteError> {
        if !cfg!(target_os = "linux") {
            return Err(RemoteError::new(
                ErrorCode::Unsupported,
                "USB forwarding is currently supported only on Linux hosts",
            ));
        }
        self.validate_managed_workspace(workspace)?;
        if !matches!(
            self.inner.states.lock().await.get(workspace),
            Some(WorkspaceRuntimeState::Running(_))
        ) {
            return Err(RemoteError::new(
                ErrorCode::Busy,
                format!("Workspace {workspace} must be running to forward USB devices"),
            ));
        }
        self.inner
            .workers
            .lock()
            .await
            .get(workspace)
            .cloned()
            .ok_or_else(|| {
                RemoteError::new(
                    ErrorCode::Busy,
                    format!("Workspace {workspace} is no longer running"),
                )
            })
    }

    async fn remove_closed_worker(
        &self,
        workspace: &WorkspaceName,
        failed: &mpsc::Sender<WorkspaceCommand>,
    ) {
        let mut workers = self.inner.workers.lock().await;
        if workers
            .get(workspace)
            .is_some_and(|current| current.same_channel(failed))
        {
            workers.remove(workspace);
        }
    }
}

/// Coalesces native root-directory notifications and reconciles the published
/// workspace inventory with the host configuration tree.
async fn run_workspace_watcher(
    inner: Weak<WorkspaceServiceInner>,
    mut events: WatchEvents,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let first = tokio::select! {
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    return;
                }
                continue;
            }
            message = events.recv() => message,
        };
        let Some(first) = first else {
            warn!("workspace inventory watcher stopped");
            return;
        };
        observe_workspace_watch_message(first);
        let mut overflowed = events.take_overflow();
        let mut deadline = tokio::time::Instant::now() + WORKSPACE_WATCH_DEBOUNCE;

        loop {
            tokio::select! {
                result = shutdown.changed() => {
                    if result.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                () = sleep_until(deadline) => break,
                message = events.recv() => {
                    let Some(message) = message else {
                        warn!("workspace inventory watcher stopped");
                        return;
                    };
                    observe_workspace_watch_message(message);
                    overflowed |= events.take_overflow();
                    deadline = tokio::time::Instant::now() + WORKSPACE_WATCH_DEBOUNCE;
                }
            }
        }

        let Some(inner) = inner.upgrade() else {
            return;
        };
        if overflowed {
            warn!("workspace inventory watcher overflowed; reconciling complete inventory");
        }
        if let Err(error) = reconcile_workspace_inventory(&inner).await {
            warn!(%error, "failed to reconcile workspace inventory");
        }
    }
}

fn observe_workspace_watch_message(message: WatchMessage) {
    match message {
        WatchMessage::Event(event) => drop(event),
        WatchMessage::Error(error) => {
            warn!(%error, "workspace inventory watcher reported an error");
        }
    }
}

async fn reconcile_workspace_inventory(inner: &WorkspaceServiceInner) -> Result<()> {
    let WorkspaceMode::Managed(managed) = &inner.config.mode else {
        return Ok(());
    };
    let root = managed.workspaces_dir.clone();
    let names = tokio::task::spawn_blocking(move || configured_workspace_names(&root))
        .await
        .context("workspace inventory reconciliation task failed")?
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let configured = names.iter().cloned().collect::<HashSet<_>>();
    let snapshot = inner.store.snapshot();
    let removed = snapshot
        .value
        .workspaces
        .iter()
        .filter_map(|workspace| WorkspaceName::new(workspace.name.as_str()).ok())
        .filter(|name| !configured.contains(name))
        .collect::<Vec<_>>();
    let removed_workers = {
        let mut workers = inner.workers.lock().await;
        removed
            .iter()
            .filter_map(|name| workers.remove(name).map(|sender| (name.clone(), sender)))
            .collect::<Vec<_>>()
    };
    for (name, sender) in removed_workers {
        if let Err(error) = stop_workspace_sender(Some(sender)).await {
            warn!(workspace = %name, %error, "failed to stop workspace removed from inventory");
        }
    }

    let mut states = inner.states.lock().await;

    for name in &names {
        let state = states
            .get(name)
            .cloned()
            .unwrap_or(WorkspaceRuntimeState::Stopped);
        let workspace = api::Workspace {
            name: api_name(name),
            state: api_state(&state),
        };
        if !snapshot
            .value
            .workspaces
            .iter()
            .any(|current| current == &workspace)
        {
            inner
                .store
                .apply(api::WorkspaceListMutation::Upsert(workspace));
        }
    }
    for workspace in &snapshot.value.workspaces {
        let Ok(name) = WorkspaceName::new(workspace.name.as_str()) else {
            inner
                .store
                .apply(api::WorkspaceListMutation::Remove(workspace.name.clone()));
            continue;
        };
        if !configured.contains(&name) {
            inner
                .store
                .apply(api::WorkspaceListMutation::Remove(workspace.name.clone()));
        }
    }
    states.retain(|name, _| configured.contains(name));
    Ok(())
}

/// Applies one mutation to the ordered workspace inventory.
fn reduce_workspace_list(list: &mut api::WorkspaceList, mutation: &api::WorkspaceListMutation) {
    match mutation {
        api::WorkspaceListMutation::Upsert(workspace) => {
            if let Some(index) = list
                .workspaces
                .iter()
                .position(|existing| existing.name == workspace.name)
            {
                list.workspaces[index] = workspace.clone();
            } else {
                list.workspaces.push(workspace.clone());
                list.workspaces
                    .sort_by(|left, right| left.name.cmp(&right.name));
            }
        }
        api::WorkspaceListMutation::Remove(name) => {
            if let Some(index) = list
                .workspaces
                .iter()
                .position(|workspace| workspace.name == *name)
            {
                list.workspaces.remove(index);
            }
        }
    }
}

fn create_default_workspace(root: &Path, name: &WorkspaceName) -> Result<()> {
    let target = root.join(name.as_str());
    std::fs::create_dir(&target)
        .with_context(|| format!("create workspace {}", target.display()))?;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
    let mut cleanup = WorkspaceCreationCleanup(Some(target.clone()));
    write_new_workspace_file(
        &target.join("config.toml"),
        DEFAULT_WORKSPACE_CONFIG.as_bytes(),
    )?;
    let image = target.join("image");
    std::fs::create_dir(&image)?;
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o755))?;
    write_new_workspace_file(
        &image.join("Dockerfile"),
        DEFAULT_WORKSPACE_DOCKERFILE.as_bytes(),
    )?;
    let agents = target.join("agents");
    let skills = agents.join("skills");
    std::fs::create_dir(&agents)?;
    std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o755))?;
    write_new_workspace_file(&agents.join("AGENTS.md"), b"")?;
    std::fs::create_dir(&skills)?;
    std::fs::set_permissions(&skills, std::fs::Permissions::from_mode(0o755))?;
    sync_directory(&skills)?;
    sync_directory(&agents)?;
    sync_directory(&image)?;
    sync_directory(&target)?;
    sync_directory(root)?;
    cleanup.0 = None;
    Ok(())
}

fn write_new_workspace_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o644)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    std::io::Write::write_all(&mut file, bytes)
        .with_context(|| format!("write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", path.display()))
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .with_context(|| format!("sync directory {}", path.display()))
}

struct WorkspaceCreationCleanup(Option<PathBuf>);

impl Drop for WorkspaceCreationCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take()
            && let Err(error) = std::fs::remove_dir_all(&path)
        {
            warn!(path = %path.display(), %error, "failed to clean incomplete workspace creation");
        }
    }
}

/// Publishes one runtime transition to internal and API state atomically by
/// ordering the store mutation after the internal map update.
async fn publish_workspace_state(
    states: &Mutex<HashMap<WorkspaceName, WorkspaceRuntimeState>>,
    store: &WorkspaceStore,
    workspace: &WorkspaceName,
    state: WorkspaceRuntimeState,
) {
    states.lock().await.insert(workspace.clone(), state.clone());
    store.apply(api::WorkspaceListMutation::Upsert(api::Workspace {
        name: api_name(workspace),
        state: api_state(&state),
    }));
}

fn api_state(state: &WorkspaceRuntimeState) -> api::WorkspaceState {
    match state {
        WorkspaceRuntimeState::Stopped => api::WorkspaceState::Stopped,
        WorkspaceRuntimeState::Starting(id) => {
            api::WorkspaceState::Starting(api::WorkspaceVmInstance {
                guest_instance_id: id.clone(),
            })
        }
        WorkspaceRuntimeState::Running(id) => {
            api::WorkspaceState::Running(api::WorkspaceVmInstance {
                guest_instance_id: id.clone(),
            })
        }
        WorkspaceRuntimeState::Stopping(id) => {
            api::WorkspaceState::Stopping(api::WorkspaceVmInstance {
                guest_instance_id: id.clone(),
            })
        }
        WorkspaceRuntimeState::Destroying => api::WorkspaceState::Destroying,
        WorkspaceRuntimeState::Failed {
            guest_instance_id,
            message,
            failed_at,
        } => api::WorkspaceState::Failed(api::WorkspaceFailure {
            guest_instance_id: guest_instance_id.clone(),
            message: message.clone().into(),
            failed_at: *failed_at,
        }),
    }
}

fn api_name(workspace: &WorkspaceName) -> api::WorkspaceName {
    api::WorkspaceName::new(workspace.as_str())
}

fn runtime_name(name: &api::WorkspaceName) -> Result<WorkspaceName, Report<WorkspaceServiceError>> {
    if name.as_str().contains('.') {
        return Err(invalid_request("workspace names must not contain '.'"));
    }
    WorkspaceName::new(name.as_str()).map_err(|error| invalid_request(error.to_string()))
}

fn runtime_stamp(
    stamp: store_api::Stamp,
) -> Result<tascarrel_store::Stamp, Report<WorkspaceServiceError>> {
    let generation = stamp.generation.parse::<uuid::Uuid>().map_err(|error| {
        invalid_request("workspace-list cursor generation is invalid").message(error.to_string())
    })?;
    Ok(tascarrel_store::Stamp {
        generation,
        version: stamp.version,
    })
}

fn service_remote_error(error: RemoteError) -> Report<WorkspaceServiceError> {
    match error.code {
        ErrorCode::InvalidRequest
        | ErrorCode::NotFound
        | ErrorCode::AlreadyExists
        | ErrorCode::PermissionDenied
        | ErrorCode::Unsupported => invalid_request(error.message),
        ErrorCode::Busy | ErrorCode::ResourceExhausted | ErrorCode::ExecutionFailed => {
            WorkspaceServiceError::Unavailable(error.message).report()
        }
        ErrorCode::Internal => internal(error.message),
    }
}

fn usb_forward_error(error: &Report<UsbForwardError>) -> RemoteError {
    RemoteError::new(
        error.error().remote_code(),
        bounded_detail(&error.to_string()),
    )
}

fn invalid_request(message: impl Into<String>) -> Report<WorkspaceServiceError> {
    WorkspaceServiceError::InvalidRequest(message.into()).report()
}

fn internal(message: impl Into<String>) -> Report<WorkspaceServiceError> {
    WorkspaceServiceError::Internal(message.into()).report()
}

fn repository_error(error: &impl std::fmt::Display) -> RemoteError {
    RemoteError::new(
        ErrorCode::ExecutionFailed,
        bounded_detail(&error.to_string()),
    )
}

fn configured_workspace_names(root: &Path) -> Result<Vec<WorkspaceName>, RemoteError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(RemoteError::new(
                ErrorCode::ExecutionFailed,
                format!(
                    "cannot read workspace directory {}: {error}",
                    root.display()
                ),
            ));
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            RemoteError::new(
                ErrorCode::ExecutionFailed,
                format!("cannot read a workspace directory entry: {error}"),
            )
        })?;
        if !entry
            .file_type()
            .map_err(|error| {
                RemoteError::new(
                    ErrorCode::ExecutionFailed,
                    format!("cannot inspect workspace entry: {error}"),
                )
            })?
            .is_dir()
        {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(name) = WorkspaceName::new(name) else {
            continue;
        };
        if entry.path().join("image").is_dir() {
            names.push(name);
        }
    }
    Ok(names)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the worker owns one workspace's complete retry and shutdown state machine"
)]
async fn workspace_worker(
    workspace: WorkspaceName,
    config: WorkspaceServiceConfig,
    mut commands: mpsc::Receiver<WorkspaceCommand>,
    mut shutdown: watch::Receiver<bool>,
    mut reset_data: bool,
    states: Arc<Mutex<HashMap<WorkspaceName, WorkspaceRuntimeState>>>,
    store: WorkspaceStore,
    logs: Arc<StdMutex<WorkspaceVmLogs>>,
    request_senders: WorkspaceRequestSenders,
    usb_devices: UsbDeviceRegistry,
) {
    loop {
        let first = tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => return,
            command = commands.recv() => match command {
                Some(command) => command,
                None => return,
            },
        };

        let first = match first {
            WorkspaceCommand::Stop { response } => {
                publish_workspace_state(
                    &states,
                    &store,
                    &workspace,
                    WorkspaceRuntimeState::Stopped,
                )
                .await;
                let _ = response.send(Ok(()));
                return;
            }
            command => command,
        };

        let guest_instance_id = GuestInstanceId::generate();
        let (vm_log, vm_log_writer) = super::log::WorkspaceVmLog::new(
            VM_LOG_LINE_HISTORY_LIMIT,
            VM_LOG_BATCH_LIMIT,
            VM_LOG_LINE_BYTE_LIMIT,
        );
        let mut protected_logs = states
            .lock()
            .await
            .values()
            .filter_map(|state| match state {
                WorkspaceRuntimeState::Starting(id)
                | WorkspaceRuntimeState::Running(id)
                | WorkspaceRuntimeState::Stopping(id) => Some(id),
                WorkspaceRuntimeState::Failed {
                    guest_instance_id: Some(id),
                    ..
                } => Some(id),
                WorkspaceRuntimeState::Stopped
                | WorkspaceRuntimeState::Destroying
                | WorkspaceRuntimeState::Failed {
                    guest_instance_id: None,
                    ..
                } => None,
            })
            .cloned()
            .collect::<HashSet<_>>();
        protected_logs.insert(guest_instance_id.clone());
        logs.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(guest_instance_id.clone(), vm_log, &protected_logs);
        publish_workspace_state(
            &states,
            &store,
            &workspace,
            WorkspaceRuntimeState::Starting(guest_instance_id.clone()),
        )
        .await;

        let started = start_workspace(
            &workspace,
            &guest_instance_id,
            vm_log_writer,
            &config,
            &mut shutdown,
            reset_data,
            &request_senders,
            &usb_devices,
        )
        .await;
        reset_data = false;

        let mut active = match started {
            Ok(active) => active,
            Err(StartupError::Cancelled) => {
                reject_command(
                    first,
                    RemoteError::new(ErrorCode::Busy, "host daemon is shutting down"),
                );
                return;
            }
            Err(error) => {
                let remote = error.remote();
                publish_workspace_state(
                    &states,
                    &store,
                    &workspace,
                    WorkspaceRuntimeState::Failed {
                        guest_instance_id: Some(guest_instance_id),
                        message: remote.message.clone(),
                        failed_at: Timestamp::now(),
                    },
                )
                .await;
                reject_command(first, remote.clone());
                let mut stop = None;
                while let Ok(command) = commands.try_recv() {
                    match command {
                        WorkspaceCommand::Stop { response } => stop = Some(response),
                        command => reject_command(command, remote.clone()),
                    }
                }
                if let Some(response) = stop {
                    publish_workspace_state(
                        &states,
                        &store,
                        &workspace,
                        WorkspaceRuntimeState::Stopped,
                    )
                    .await;
                    let _ = response.send(Ok(()));
                    return;
                }
                continue;
            }
        };

        // Readiness and shutdown can become observable in the same scheduler
        // turn. Never hand a new client a mux after shutdown has begun.
        if *shutdown.borrow() {
            let error = RemoteError::new(ErrorCode::Busy, "host daemon is shutting down");
            reject_command(first, error.clone());
            while let Ok(command) = commands.try_recv() {
                reject_command(command, error.clone());
            }
            active.shutdown().await;
            return;
        }

        info!(workspace = %workspace, "workspace VM ready");
        publish_workspace_state(
            &states,
            &store,
            &workspace,
            WorkspaceRuntimeState::Running(guest_instance_id.clone()),
        )
        .await;
        active.answer(first).await;
        let stop = active.run(&mut commands, &mut shutdown).await;
        if active.vm.is_some()
            && matches!(
                &stop,
                ActiveStop::Mux(_) | ActiveStop::ControlPlane(_) | ActiveStop::WorkspaceServices(_)
            )
        {
            wait_for_guest_failure_diagnostics(&workspace, "runtime", &mut shutdown).await;
        }
        active.shutdown().await;
        match stop {
            ActiveStop::Requested(response) => {
                publish_workspace_state(
                    &states,
                    &store,
                    &workspace,
                    WorkspaceRuntimeState::Stopped,
                )
                .await;
                let _ = response.send(Ok(()));
                return;
            }
            ActiveStop::Shutdown | ActiveStop::CommandsClosed => {
                publish_workspace_state(
                    &states,
                    &store,
                    &workspace,
                    WorkspaceRuntimeState::Stopped,
                )
                .await;
                return;
            }
            ActiveStop::Mux(error) => {
                warn!(workspace = %workspace, %error, "workspace mux stopped; next client will restart the VM");
                publish_workspace_state(
                    &states,
                    &store,
                    &workspace,
                    WorkspaceRuntimeState::Failed {
                        guest_instance_id: Some(guest_instance_id.clone()),
                        message: bounded_detail(&format!(
                            "workspace control channel stopped: {error}"
                        )),
                        failed_at: Timestamp::now(),
                    },
                )
                .await;
            }
            ActiveStop::ControlPlane(error) => {
                warn!(workspace = %workspace, %error, "workspace control plane stopped; next client will restart the VM");
                publish_workspace_state(
                    &states,
                    &store,
                    &workspace,
                    WorkspaceRuntimeState::Failed {
                        guest_instance_id: Some(guest_instance_id.clone()),
                        message: bounded_detail(&format!(
                            "workspace control plane stopped: {error}"
                        )),
                        failed_at: Timestamp::now(),
                    },
                )
                .await;
            }
            ActiveStop::WorkspaceServices(error) => {
                warn!(workspace = %workspace, %error, "workspace services stopped; next client will restart the VM");
                publish_workspace_state(
                    &states,
                    &store,
                    &workspace,
                    WorkspaceRuntimeState::Failed {
                        guest_instance_id: Some(guest_instance_id.clone()),
                        message: bounded_detail(&format!(
                            "workspace service dispatcher stopped: {error}"
                        )),
                        failed_at: Timestamp::now(),
                    },
                )
                .await;
            }
        }
    }
}

fn reject_command(command: WorkspaceCommand, error: RemoteError) {
    match command {
        WorkspaceCommand::Connect { response } => {
            let _ = response.send(Err(error));
        }
        WorkspaceCommand::ControlPlane { response, .. } => {
            let _ = response.send(Err(error));
        }
        WorkspaceCommand::AttachUsb { response, .. }
        | WorkspaceCommand::DetachUsb { response, .. }
        | WorkspaceCommand::Stop { response } => {
            let _ = response.send(Err(error));
        }
    }
}

async fn stop_workspace_sender(
    sender: Option<mpsc::Sender<WorkspaceCommand>>,
) -> Result<(), RemoteError> {
    let Some(sender) = sender else {
        return Ok(());
    };
    let (response, result) = oneshot::channel();
    if sender
        .send(WorkspaceCommand::Stop { response })
        .await
        .is_err()
    {
        // A closed worker no longer owns a running VM, which already satisfies
        // the requested postcondition.
        return Ok(());
    }
    drop(sender);
    result.await.unwrap_or(Ok(()))
}

enum StartupError {
    Cancelled,
    Missing(String),
    Busy(String),
    Failed(String),
}

impl StartupError {
    fn remote(&self) -> RemoteError {
        match self {
            Self::Cancelled => RemoteError::new(ErrorCode::Busy, "host daemon is shutting down"),
            Self::Missing(message) => RemoteError::new(ErrorCode::NotFound, message.clone()),
            Self::Busy(message) => RemoteError::new(ErrorCode::Busy, message.clone()),
            Self::Failed(message) => RemoteError::new(ErrorCode::ExecutionFailed, message.clone()),
        }
    }
}

struct ActiveWorkspace {
    workspace: WorkspaceName,
    mux: MuxHandle,
    pending_control_channel: Option<tascarrel_mux::Channel>,
    control_plane: Option<ControlPlanePeer>,
    control_plane_connection: Option<ControlPlaneConnection>,
    mux_driver:
        Pin<Box<dyn std::future::Future<Output = tascarrel_mux::Result<()>> + Send + 'static>>,
    workspace_services: Pin<
        Box<
            dyn std::future::Future<Output = Result<(), Report<WorkspaceMuxHostError>>>
                + Send
                + 'static,
        >,
    >,
    vm: Option<Vm>,
    vm_log_task: Option<JoinHandle<()>>,
    usb: Option<WorkspaceUsbForwards>,
    usb_config_path: Option<PathBuf>,
    _locks: Vec<File>,
}

enum ActiveStop {
    Requested(oneshot::Sender<Result<(), RemoteError>>),
    Shutdown,
    CommandsClosed,
    Mux(String),
    ControlPlane(String),
    WorkspaceServices(Report<WorkspaceMuxHostError>),
}

impl ActiveWorkspace {
    async fn answer(&mut self, command: WorkspaceCommand) {
        match command {
            WorkspaceCommand::Connect { response } => {
                let _ = response.send(Ok(self.mux.clone()));
            }
            WorkspaceCommand::ControlPlane {
                host_control,
                response,
            } => {
                if let Some(control_plane) = &self.control_plane {
                    let _ = response.send(Ok(control_plane.clone()));
                    return;
                }
                let Some(channel) = self.pending_control_channel.take() else {
                    let _ = response.send(Err(RemoteError::new(
                        ErrorCode::Internal,
                        "workspace control-plane channel is unavailable",
                    )));
                    return;
                };
                match host_control.connect_guest(StreamTransport::new(channel), &self.workspace) {
                    Ok((control_plane, connection)) => {
                        self.control_plane = Some(control_plane.clone());
                        self.control_plane_connection = Some(connection);
                        let _ = response.send(Ok(control_plane));
                    }
                    Err(error) => {
                        let _ = response.send(Err(RemoteError::new(
                            ErrorCode::ExecutionFailed,
                            format!("failed to configure guest control plane: {error}"),
                        )));
                    }
                }
            }
            WorkspaceCommand::AttachUsb {
                device_id,
                response,
            } => {
                let result = match (&mut self.vm, &mut self.usb) {
                    (Some(vm), Some(usb)) => match self.usb_config_path.as_deref() {
                        Some(path) => match workspace_usb_enabled(path) {
                            Ok(true) => usb
                                .attach(vm, &device_id)
                                .await
                                .map_err(|error| usb_forward_error(&error)),
                            Ok(false) => Err(RemoteError::new(
                                ErrorCode::PermissionDenied,
                                "USB forwarding is not enabled in this workspace",
                            )),
                            Err(error) => Err(RemoteError::new(
                                ErrorCode::InvalidRequest,
                                bounded_detail(&format!(
                                    "Workspace configuration is invalid: {error}"
                                )),
                            )),
                        },
                        None => Err(RemoteError::new(
                            ErrorCode::Unsupported,
                            "USB forwarding requires a managed workspace VM",
                        )),
                    },
                    _ => Err(RemoteError::new(
                        ErrorCode::Unsupported,
                        "USB forwarding requires a managed workspace VM",
                    )),
                };
                let _ = response.send(result);
            }
            WorkspaceCommand::DetachUsb {
                device_id,
                response,
            } => {
                let result = match (&mut self.vm, &mut self.usb) {
                    (Some(vm), Some(usb)) => usb
                        .detach(vm, &device_id)
                        .await
                        .map_err(|error| usb_forward_error(&error)),
                    _ => Err(RemoteError::new(
                        ErrorCode::Unsupported,
                        "USB forwarding requires a managed workspace VM",
                    )),
                };
                let _ = response.send(result);
            }
            WorkspaceCommand::Stop { response } => {
                let _ = response.send(Err(RemoteError::new(
                    ErrorCode::Internal,
                    "workspace stop reached the active command responder",
                )));
            }
        }
    }

    async fn run(
        &mut self,
        commands: &mut mpsc::Receiver<WorkspaceCommand>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> ActiveStop {
        let mut usb_poll = tokio::time::interval(DEFAULT_USB_RECONCILE_INTERVAL);
        usb_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = wait_for_shutdown(shutdown) => return ActiveStop::Shutdown,
                command = commands.recv() => match command {
                    Some(WorkspaceCommand::Stop { response }) => {
                        return ActiveStop::Requested(response);
                    }
                    Some(command) => self.answer(command).await,
                    None => return ActiveStop::CommandsClosed,
                },
                _ = usb_poll.tick(), if self.usb.is_some() => {
                    if let (Some(vm), Some(usb), Some(path)) =
                        (&mut self.vm, &mut self.usb, &self.usb_config_path)
                    {
                        let enabled = workspace_usb_enabled(path).unwrap_or_else(|error| {
                            warn!(workspace = %self.workspace, %error, "could not load USB feature configuration");
                            false
                        });
                        usb.reconcile(vm, enabled).await;
                    }
                }
                result = self.mux_driver.as_mut() => {
                    return ActiveStop::Mux(match result {
                        Ok(()) => "guest mux driver stopped unexpectedly".to_owned(),
                        Err(error) => error.to_string(),
                    });
                }
                result = async {
                    self.control_plane_connection
                        .as_mut()
                        .expect("control-plane connection exists while its select branch is enabled")
                        .await
                }, if self.control_plane_connection.is_some() => {
                    return ActiveStop::ControlPlane(match result {
                        Ok(()) => "guest control-plane connection stopped unexpectedly".to_owned(),
                        Err(error) => error.to_string(),
                    });
                }
                result = self.workspace_services.as_mut() => {
                    return ActiveStop::WorkspaceServices(match result {
                        Ok(()) => WorkspaceMuxHostError::IncomingClosed.report(),
                        Err(error) => error,
                    });
                }
            }
        }
    }

    async fn shutdown(mut self) {
        drop(self.mux);
        drop(self.mux_driver);
        drop(self.pending_control_channel);
        drop(self.control_plane);
        drop(self.control_plane_connection);
        drop(self.workspace_services);
        if let Some(vm) = self.vm.take() {
            shutdown_vm(vm).await;
        }
        if let Some(usb) = &mut self.usb {
            usb.release_all();
        }
        if let Some(task) = self.vm_log_task.take() {
            finish_vm_log_writer(task).await;
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "workspace startup receives its lifecycle state and runtime integrations explicitly"
)]
async fn start_workspace(
    workspace: &WorkspaceName,
    guest_instance_id: &GuestInstanceId,
    vm_log: super::log::WorkspaceVmLogWriter,
    config: &WorkspaceServiceConfig,
    shutdown: &mut watch::Receiver<bool>,
    reset_data: bool,
    request_senders: &WorkspaceRequestSenders,
    usb_devices: &UsbDeviceRegistry,
) -> Result<ActiveWorkspace, StartupError> {
    let deadline = Instant::now()
        .checked_add(config.startup_timeout)
        .ok_or_else(|| StartupError::Failed("workspace startup deadline overflowed".to_owned()))?;
    match &config.mode {
        WorkspaceMode::Managed(managed) => {
            start_managed_workspace(
                workspace,
                guest_instance_id,
                vm_log,
                config,
                managed,
                shutdown,
                deadline,
                reset_data,
                request_senders,
                usb_devices,
            )
            .await
        }
        WorkspaceMode::External(external) => {
            vm_log.close();
            start_external_workspace(
                workspace,
                config,
                external,
                shutdown,
                deadline,
                request_senders,
                usb_devices,
            )
            .await
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "startup keeps ordered VM resource acquisition and rollback together"
)]
async fn start_managed_workspace(
    workspace: &WorkspaceName,
    guest_instance_id: &GuestInstanceId,
    vm_log: super::log::WorkspaceVmLogWriter,
    service: &WorkspaceServiceConfig,
    managed: &ManagedWorkspaceConfig,
    shutdown: &mut watch::Receiver<bool>,
    deadline: Instant,
    reset_data: bool,
    request_senders: &WorkspaceRequestSenders,
    usb_devices: &UsbDeviceRegistry,
) -> Result<ActiveWorkspace, StartupError> {
    let runtime_dir = service
        .runtime_dir
        .join("workspaces")
        .join(workspace.as_str());
    create_private_directory(&runtime_dir).map_err(|error| {
        StartupError::Failed(format!(
            "failed to prepare workspace runtime directory {}: {error}",
            runtime_dir.display()
        ))
    })?;
    let state_dir = managed
        .state_dir
        .join("workspaces")
        .join(workspace.as_str());
    create_private_directory(&state_dir).map_err(|error| {
        StartupError::Failed(format!(
            "failed to prepare workspace state directory {}: {error}",
            state_dir.display()
        ))
    })?;
    let state_lock = lock_file(&state_dir.join("vm.lock"), "workspace VM state")
        .map_err(|error| StartupError::Busy(bounded_detail(&error.to_string())))?;
    let image_context = managed
        .workspaces_dir
        .join(workspace.as_str())
        .join("image");
    if !image_context.is_dir() {
        return Err(StartupError::Missing(format!(
            "workspace image context is not a directory: {}",
            image_context.display()
        )));
    }
    let default_resources = VmResources {
        memory_mib: managed.memory_mib,
        vcpu_count: managed.vcpu_count,
        state_disk_size: managed.data_disk_size,
    };
    let (resources, diagnostics) = load_workspace_vm_resources_with_fallback(
        &managed
            .workspaces_dir
            .join(workspace.as_str())
            .join("config.toml"),
        default_resources,
    );
    let data_disk = state_dir.join("state.raw");
    let remaining = remaining(deadline)?;
    let startup = spawn_vm(
        managed,
        VmStartSpec {
            data_disk,
            state_directory: state_dir,
            runtime_directory: runtime_dir,
            guest_instance_id: guest_instance_id.clone(),
            vm_log,
            startup_timeout: remaining,
            resources,
            reset_data,
        },
    );
    tokio::pin!(startup);
    let vm = tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => return Err(StartupError::Cancelled),
        result = &mut startup => result
            .map_err(|error| StartupError::Failed(bounded_detail(&error.to_string())))?,
    };
    finish_workspace_start(
        workspace,
        service,
        UnixStreamSource::Vm(Box::new(vm)),
        shutdown,
        deadline,
        vec![state_lock],
        diagnostics,
        request_senders,
        usb_devices,
    )
    .await
}

async fn start_external_workspace(
    workspace: &WorkspaceName,
    service: &WorkspaceServiceConfig,
    external: &ExternalWorkspaceConfig,
    shutdown: &mut watch::Receiver<bool>,
    deadline: Instant,
    request_senders: &WorkspaceRequestSenders,
    usb_devices: &UsbDeviceRegistry,
) -> Result<ActiveWorkspace, StartupError> {
    if workspace != &external.workspace {
        return Err(StartupError::Missing(format!(
            "workspace {workspace} is not assigned to the external guest"
        )));
    }
    create_private_directory(&external.workspace_state).map_err(|error| {
        StartupError::Failed(format!(
            "failed to prepare external workspace state directory {}: {error}",
            external.workspace_state.display()
        ))
    })?;
    let stream = tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => return Err(StartupError::Cancelled),
        result = connect_guest(&external.guest_socket, deadline) => result
            .map_err(|error| StartupError::Failed(bounded_detail(&error.to_string())))?,
    };
    finish_workspace_start(
        workspace,
        service,
        UnixStreamSource::External(stream),
        shutdown,
        deadline,
        Vec::new(),
        Vec::new(),
        request_senders,
        usb_devices,
    )
    .await
}

enum UnixStreamSource {
    Vm(Box<SpawnedVm>),
    External(UnixStream),
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "startup keeps transport, services, readiness, and cancellation rollback together"
)]
async fn finish_workspace_start(
    workspace: &WorkspaceName,
    service: &WorkspaceServiceConfig,
    source: UnixStreamSource,
    shutdown: &mut watch::Receiver<bool>,
    deadline: Instant,
    locks: Vec<File>,
    mut diagnostics: Vec<String>,
    request_senders: &WorkspaceRequestSenders,
    usb_devices: &UsbDeviceRegistry,
) -> Result<ActiveWorkspace, StartupError> {
    let (stream, mut vm, mut vm_log_task) = match source {
        UnixStreamSource::Vm(spawned) => {
            let SpawnedVm { mut vm, log_task } = *spawned;
            let stream = match vm.take_control_stream() {
                Ok(stream) => stream,
                Err(error) => {
                    let error = StartupError::Failed(format!(
                        "failed to take QEMU control channel: {error}"
                    ));
                    return Err(finish_failed_workspace_start(
                        workspace,
                        error,
                        Some(vm),
                        Some(log_task),
                        shutdown,
                    )
                    .await);
                }
            };
            (stream, Some(vm), Some(log_task))
        }
        UnixStreamSource::External(stream) => (stream, None, None),
    };
    let usb = vm
        .is_some()
        .then(|| WorkspaceUsbForwards::new(workspace.clone(), usb_devices.clone()));
    let usb_config_path = vm
        .is_some()
        .then(|| workspace_root(workspace, service).join("config.toml"));
    let mux_config = MuxConfig {
        initial_byte_window: service.mux_initial_byte_window,
        max_channels: service.mux_max_channels,
        ..MuxConfig::default()
    };
    let (driver, mux, incoming) = match connect_mux(stream, MuxRole::Client, mux_config) {
        Ok(connection) => connection,
        Err(error) => {
            let error = StartupError::Failed(format!("failed to configure guest mux: {error}"));
            return Err(finish_failed_workspace_start(
                workspace,
                error,
                vm.take(),
                vm_log_task.take(),
                shutdown,
            )
            .await);
        }
    };
    let runtime_span = tracing::info_span!("workspace_runtime", workspace = %workspace);
    let mut mux_driver = Box::pin(driver.run().instrument(runtime_span.clone()));
    let (policy, authority) = match network_policy(workspace, service) {
        Ok(policy) => policy,
        Err(error) => {
            diagnostics.push(bounded_detail(&format!(
                "workspace network configuration is invalid: {error:#}"
            )));
            (NetworkPolicy::deny_all(), None)
        }
    };
    let repositories = match repository_manager(workspace, service) {
        Ok(repositories) => repositories,
        Err(error) => {
            diagnostics.push(bounded_detail(&format!(
                "workspace repository configuration is invalid: {error:#}"
            )));
            None
        }
    };
    let workspace_services = WorkspaceMuxHost::new(
        incoming,
        WorkspaceMuxHostConfig {
            workspace: tascarrel_api::types::workspaces::WorkspaceName::new(workspace.as_str()),
            network_requests: request_senders.network.clone(),
            environment_requests: request_senders.environment.clone(),
            repositories,
            policy,
            authority,
            workspace_root: workspace_root(workspace, service),
            workspace_snapshot_dir: workspace_state(workspace, service),
            handshake_timeout: service.mux_service_handshake_timeout,
            max_concurrent_services: service.max_concurrent_mux_services,
        },
    );
    let mut workspace_services =
        Box::pin(workspace_services.run().instrument(runtime_span.clone()));
    let open_timeout = match remaining(deadline) {
        Ok(remaining) => remaining,
        Err(error) => {
            return Err(finish_failed_workspace_start(
                workspace,
                error,
                vm.take(),
                vm_log_task.take(),
                shutdown,
            )
            .await);
        }
    };
    let control_mux = mux.clone();
    let opening = tokio::time::timeout(open_timeout, control_mux.open(MUX_CONTROL_PLANE_ENDPOINT));
    tokio::pin!(opening);
    let channel = tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => {
            return Err(
                finish_failed_workspace_start(
                    workspace,
                    StartupError::Cancelled,
                    vm.take(),
                    vm_log_task.take(),
                    shutdown,
                )
                .await,
            );
        }
        result = &mut opening => match result {
            Ok(Ok(channel)) => channel,
            Ok(Err(error)) => {
                let error = StartupError::Failed(format!(
                    "failed to open guest control-plane channel: {error}"
                ));
                return Err(
                    finish_failed_workspace_start(
                        workspace,
                        error,
                        vm.take(),
                        vm_log_task.take(),
                        shutdown,
                    )
                    .await,
                );
            }
            Err(_) => {
                let error = StartupError::Failed(
                    "timed out opening guest control-plane channel".to_owned(),
                );
                return Err(
                    finish_failed_workspace_start(
                        workspace,
                        error,
                        vm.take(),
                        vm_log_task.take(),
                        shutdown,
                    )
                    .await,
                );
            }
        },
        result = mux_driver.as_mut() => {
            let error = StartupError::Failed(format!(
                "guest mux stopped while opening the control plane: {result:?}"
            ));
            return Err(
                finish_failed_workspace_start(
                    workspace,
                    error,
                    vm.take(),
                    vm_log_task.take(),
                    shutdown,
                )
                .await,
            );
        }
        result = workspace_services.as_mut() => {
            let error = StartupError::Failed(format!(
                "guest workspace services stopped while opening the control plane: {result:?}"
            ));
            return Err(
                finish_failed_workspace_start(
                    workspace,
                    error,
                    vm.take(),
                    vm_log_task.take(),
                    shutdown,
                )
                .await,
            );
        }
    };
    let mut framed = Framed::new(channel);
    let write_timeout = match remaining(deadline) {
        Ok(remaining) => remaining,
        Err(error) => {
            return Err(finish_failed_workspace_start(
                workspace,
                error,
                vm.take(),
                vm_log_task.take(),
                shutdown,
            )
            .await);
        }
    };
    let identity = GuestControlIdentity {
        workspace: api::WorkspaceName::new(workspace.as_str()),
    };
    let write_result = {
        let writing = tokio::time::timeout(write_timeout, framed.write(&identity));
        tokio::pin!(writing);
        tokio::select! {
            biased;
            () = wait_for_shutdown(shutdown) => Err(StartupError::Cancelled),
            result = &mut writing => match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(StartupError::Failed(format!(
                    "failed to send guest control-plane identity: {error}"
                ))),
                Err(_) => Err(StartupError::Failed(
                    "timed out sending guest control-plane identity".to_owned(),
                )),
            },
            result = mux_driver.as_mut() => Err(StartupError::Failed(format!(
                "guest mux stopped while sending the control-plane identity: {result:?}"
            ))),
            result = workspace_services.as_mut() => Err(StartupError::Failed(format!(
                "guest workspace services stopped while sending the control-plane identity: {result:?}"
            ))),
        }
    };
    if let Err(error) = write_result {
        return Err(finish_failed_workspace_start(
            workspace,
            error,
            vm.take(),
            vm_log_task.take(),
            shutdown,
        )
        .await);
    }
    let channel = framed.into_inner();
    for diagnostic in diagnostics {
        warn!(workspace = %workspace, %diagnostic, "workspace startup diagnostic");
    }
    Ok(ActiveWorkspace {
        workspace: workspace.clone(),
        mux,
        pending_control_channel: Some(channel),
        control_plane: None,
        control_plane_connection: None,
        mux_driver,
        workspace_services,
        vm,
        vm_log_task,
        usb,
        usb_config_path,
        _locks: locks,
    })
}

fn workspace_root(workspace: &WorkspaceName, service: &WorkspaceServiceConfig) -> PathBuf {
    match &service.mode {
        WorkspaceMode::Managed(managed) => managed.workspaces_dir.join(workspace.as_str()),
        WorkspaceMode::External(external) => external.workspace_root.clone(),
    }
}

fn workspace_state(workspace: &WorkspaceName, service: &WorkspaceServiceConfig) -> PathBuf {
    match &service.mode {
        WorkspaceMode::Managed(managed) => managed
            .state_dir
            .join("workspaces")
            .join(workspace.as_str()),
        WorkspaceMode::External(external) => external.workspace_state.clone(),
    }
}

fn workspace_usb_enabled(path: &Path) -> Result<bool> {
    let config = load_config_file(path, DEFAULT_MAX_CONFIG_BYTES)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(config
        .features
        .and_then(|features| features.usb)
        .unwrap_or(false))
}

fn network_policy(
    workspace: &WorkspaceName,
    service: &WorkspaceServiceConfig,
) -> Result<(NetworkPolicy, Option<Arc<WorkspaceAuthority>>)> {
    let policy = NetworkPolicy::load(&workspace_root(workspace, service).join("config.toml"))
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let authority = policy
        .needs_authority()
        .then(|| {
            WorkspaceAuthority::load_or_create(
                &workspace_state(workspace, service).join("https-ca"),
                workspace.as_str(),
            )
        })
        .transpose()?;
    Ok((policy, authority))
}

fn repository_manager(
    workspace: &WorkspaceName,
    service: &WorkspaceServiceConfig,
) -> crate::services::repositories::HostRepositoryResult<Option<Arc<HostRepositoryManager>>> {
    let WorkspaceMode::Managed(managed) = &service.mode else {
        return Ok(None);
    };
    let config = managed
        .workspaces_dir
        .join(workspace.as_str())
        .join("config.toml");
    let root = managed
        .state_dir
        .join("repos")
        .join("workspaces")
        .join(workspace.as_str());
    HostRepositoryManager::load(service.git.clone(), root, &config).map(Some)
}

/// Workspace-specific inputs for one managed VM launch.
struct VmStartSpec {
    data_disk: PathBuf,
    state_directory: PathBuf,
    runtime_directory: PathBuf,
    guest_instance_id: GuestInstanceId,
    vm_log: super::log::WorkspaceVmLogWriter,
    startup_timeout: Duration,
    resources: VmResources,
    reset_data: bool,
}

struct SpawnedVm {
    vm: Vm,
    log_task: JoinHandle<()>,
}

/// Builds the VM configuration and transfers lifecycle ownership to
/// `tascarrel-vm`.
#[tracing::instrument(
    name = "tascarrel_host.vm.spawn",
    level = "info",
    skip_all,
    fields(
        system_image = %config.image.display(),
        data_disk = %spec.data_disk.display(),
        data_disk_minimum_size = spec.resources.state_disk_size,
        runtime_directory = %spec.runtime_directory.display(),
        memory_mib = spec.resources.memory_mib,
        vcpu_count = spec.resources.vcpu_count,
        local_binaries = config.local_binaries.is_some(),
        reset_data = spec.reset_data,
    ),
    err
)]
async fn spawn_vm(config: &ManagedWorkspaceConfig, spec: VmStartSpec) -> Result<SpawnedVm> {
    let config = config.clone();
    let state_directory = spec.state_directory.clone();
    let guest_instance_id = spec.guest_instance_id.clone();
    let vm_log = spec.vm_log.clone();
    let vm_config = tokio::task::spawn_blocking(move || {
        if spec.reset_data {
            remove_regular_state_file(&spec.data_disk, "VM state disk")?;
        }
        let mut kernel_append = config.kernel_append.clone();
        if config.local_binaries.is_some() {
            kernel_append.push(' ');
            kernel_append.push_str(LOCAL_BINARIES_KERNEL_PARAMETER);
        }
        kernel_append.push(' ');
        kernel_append.push_str(GUEST_INSTANCE_KERNEL_PARAMETER);
        kernel_append.push('=');
        kernel_append.push_str(spec.guest_instance_id.0.as_ref());
        let mut builder = VmConfig::builder()
            .system_disk_image(&config.image)
            .data_disk(&spec.data_disk, spec.resources.state_disk_size)
            .runtime_directory(&spec.runtime_directory)
            .qmp_enabled(true)
            .memory_mib(spec.resources.memory_mib)
            .vcpu_count(spec.resources.vcpu_count)
            .acceleration(config.acceleration)
            .startup_timeout(spec.startup_timeout)
            .shutdown_timeout(config.shutdown_timeout)
            .direct_kernel_boot(&config.kernel, &config.initrd, kernel_append);
        if let Some(local_binaries) = config.local_binaries {
            builder = builder.shared_directory(SharedDirectory::read_only(
                local_binaries,
                LOCAL_BINARIES_MOUNT_TAG,
            ));
        }
        if let Some(architecture) = config.architecture {
            builder = builder.architecture(architecture);
        }
        if let Some(qemu) = config.qemu {
            builder = builder.qemu_binary(qemu);
        }
        builder
            .build()
            .map_err(|error| anyhow::Error::msg(error.to_string()))
            .context("invalid VM configuration")
    })
    .await
    .context("VM preparation task failed")??;
    let mut spawn = Vm::spawn(vm_config);
    let serial_output = spawn
        .take_serial_output()
        .expect("a new VM spawn owns its serial output");
    let log_task =
        start_vm_log_writer(&state_directory, &guest_instance_id, serial_output, vm_log)?;
    match spawn.await {
        Ok(vm) => Ok(SpawnedVm { vm, log_task }),
        Err(error) => {
            finish_vm_log_writer(log_task).await;
            Err(anyhow::Error::msg(error.to_string())).context("failed to start QEMU")
        }
    }
}

/// Persists and parses one instance's retained and live VM serial stream.
fn start_vm_log_writer(
    state_directory: &Path,
    guest_instance_id: &GuestInstanceId,
    mut serial: tascarrel_vm::VmSerialOutput,
    vm_log: super::log::WorkspaceVmLogWriter,
) -> Result<JoinHandle<()>> {
    let directory = state_directory.join("logs");
    create_private_directory(&directory).with_context(|| {
        format!(
            "failed to prepare workspace VM log directory {}",
            directory.display()
        )
    })?;
    let path = directory.join(format!("{}.log", guest_instance_id.0));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("failed to create workspace VM log {}", path.display()))?;
    Ok(tokio::spawn(async move {
        let mut file = Some(tokio::fs::File::from_std(file));
        let mut bytes = [0_u8; 8192];
        loop {
            let length = match serial.read(&mut bytes).await {
                Ok(0) => break,
                Ok(length) => length,
                Err(error) => {
                    warn!(%error, "failed to read workspace VM serial output");
                    break;
                }
            };
            vm_log.write(&bytes[..length]);
            if let Some(output) = file.as_mut()
                && let Err(error) = output.write_all(&bytes[..length]).await
            {
                warn!(path = %path.display(), %error, "failed to write workspace VM log");
                file = None;
            }
        }
        if let Some(output) = file.as_mut()
            && let Err(error) = output.sync_all().await
        {
            warn!(path = %path.display(), %error, "failed to sync workspace VM log");
        }
        vm_log.close();
    }))
}

/// Waits for one per-instance log writer and reports unexpected task failure.
async fn finish_vm_log_writer(task: JoinHandle<()>) {
    if let Err(error) = task.await {
        warn!(%error, "workspace VM log writer task failed");
    }
}

async fn finish_failed_workspace_start(
    workspace: &WorkspaceName,
    error: StartupError,
    vm: Option<Vm>,
    vm_log_task: Option<JoinHandle<()>>,
    shutdown: &mut watch::Receiver<bool>,
) -> StartupError {
    if vm.is_some() && matches!(&error, StartupError::Failed(_)) {
        wait_for_guest_failure_diagnostics(workspace, "startup", shutdown).await;
    }
    if let Some(vm) = vm {
        shutdown_vm(vm).await;
    }
    if let Some(task) = vm_log_task {
        finish_vm_log_writer(task).await;
    }
    error
}

async fn wait_for_guest_failure_diagnostics(
    workspace: &WorkspaceName,
    phase: &'static str,
    shutdown: &mut watch::Receiver<bool>,
) {
    info!(
        workspace = %workspace,
        phase,
        grace_seconds = VM_FAILURE_LOG_GRACE_PERIOD.as_secs(),
        "waiting for guest failure diagnostics before stopping QEMU"
    );
    tokio::select! {
        () = tokio::time::sleep(VM_FAILURE_LOG_GRACE_PERIOD) => {}
        () = wait_for_shutdown(shutdown) => {
            info!(
                workspace = %workspace,
                phase,
                "guest failure diagnostic grace period interrupted by host shutdown"
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VmResources {
    memory_mib: u32,
    vcpu_count: u16,
    state_disk_size: u64,
}

fn load_workspace_vm_resources(path: &Path, defaults: VmResources) -> Result<VmResources> {
    let parsed = load_config_file(path, DEFAULT_MAX_CONFIG_BYTES)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let vm = parsed
        .vm
        .unwrap_or(tascarrel_api::types::config::WorkspaceVmConfig {
            cores: None,
            memory: None,
            disk: None,
        });
    let resources = VmResources {
        memory_mib: vm
            .memory
            .as_deref()
            .map(parse_memory_mib)
            .transpose()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
            .unwrap_or(defaults.memory_mib),
        vcpu_count: vm.cores.unwrap_or(defaults.vcpu_count),
        state_disk_size: vm
            .disk
            .as_deref()
            .map(parse_size_bytes)
            .transpose()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
            .unwrap_or(defaults.state_disk_size),
    };
    if resources.memory_mib == 0 || resources.vcpu_count == 0 {
        bail!("VM cores and memory must be greater than zero");
    }
    if resources.state_disk_size < MINIMUM_DATA_DISK_BYTES {
        bail!("VM disk must be at least 256 MiB");
    }
    Ok(resources)
}

fn load_workspace_vm_resources_with_fallback(
    path: &Path,
    defaults: VmResources,
) -> (VmResources, Vec<String>) {
    match load_workspace_vm_resources(path, defaults) {
        Ok(resources) => (resources, Vec::new()),
        Err(error) => (
            defaults,
            vec![bounded_detail(&format!(
                "workspace VM configuration is invalid: {error:#}"
            ))],
        ),
    }
}

fn remaining(deadline: Instant) -> Result<Duration, StartupError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(StartupError::Failed(
            "workspace startup timed out".to_owned(),
        ))
    } else {
        Ok(remaining)
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() || shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn connect_guest(path: &Path, deadline: Instant) -> Result<UnixStream> {
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::Interrupted
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(error).with_context(|| {
                        format!("timed out connecting to guest socket {}", path.display())
                    });
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to connect to guest socket {}", path.display())
                });
            }
        }
    }
}

async fn shutdown_vm(mut vm: Vm) {
    match vm.shutdown().await {
        Ok(outcome) if outcome.was_forced() => warn!("QEMU required a forced shutdown"),
        Ok(_) => {}
        Err(error) => warn!(%error, "failed to stop QEMU cleanly"),
    }
}

#[doc(hidden)]
pub fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("private path is not a real directory: {}", path.display()),
        ));
    }
    let expected_uid = nix::unistd::Uid::effective().as_raw();
    if metadata.uid() != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "private directory {} is owned by uid {}, expected {expected_uid}",
                path.display(),
                metadata.uid()
            ),
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn remove_owned_workspace_tree(path: &Path, required: bool) -> io::Result<()> {
    if !inspect_owned_workspace_tree(path, required)? {
        return Ok(());
    }
    std::fs::remove_dir_all(path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn inspect_owned_workspace_tree(path: &Path, required: bool) -> io::Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("workspace path is not a real directory: {}", path.display()),
        ));
    }
    let expected_uid = nix::unistd::Uid::effective().as_raw();
    if metadata.uid() != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "workspace path {} is owned by uid {}, expected {expected_uid}",
                path.display(),
                metadata.uid()
            ),
        ));
    }
    Ok(true)
}

#[doc(hidden)]
pub fn lock_file(path: &Path, purpose: &str) -> Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to open {purpose} lock {}", path.display()))?;
    fs2::FileExt::try_lock_exclusive(&file)
        .with_context(|| format!("another {purpose} is using {}", path.display()))?;
    Ok(file)
}

fn remove_regular_state_file(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            std::fs::remove_file(path)
                .with_context(|| format!("failed to reset {label} {}", path.display()))
        }
        Ok(_) => bail!("refusing to reset unsafe {label} path {}", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect {label} {}", path.display()))
        }
    }
}

fn bounded_detail(detail: &str) -> String {
    if detail.len() <= ERROR_DETAIL_LIMIT {
        return detail.to_owned();
    }
    let mut end = ERROR_DETAIL_LIMIT;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &detail[..end])
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    fn external_config(directory: &Path, workspace: WorkspaceName) -> WorkspaceServiceConfig {
        WorkspaceServiceConfig {
            runtime_dir: directory.to_owned(),
            git: PathBuf::from("/usr/bin/git"),
            startup_timeout: Duration::from_secs(1),
            max_workspaces: 1,
            network_request_queue_capacity:
                WorkspaceServiceConfig::DEFAULT_NETWORK_REQUEST_QUEUE_CAPACITY,
            mux_initial_byte_window: WorkspaceServiceConfig::DEFAULT_MUX_INITIAL_BYTE_WINDOW,
            mux_max_channels: WorkspaceServiceConfig::DEFAULT_MUX_MAX_CHANNELS,
            mux_service_handshake_timeout:
                WorkspaceServiceConfig::DEFAULT_MUX_SERVICE_HANDSHAKE_TIMEOUT,
            max_concurrent_mux_services:
                WorkspaceServiceConfig::DEFAULT_MAX_CONCURRENT_MUX_SERVICES,
            mode: WorkspaceMode::External(ExternalWorkspaceConfig {
                workspace,
                guest_socket: directory.join("guest.sock"),
                workspace_root: directory.join("workspace"),
                workspace_state: directory.join("workspace-state"),
            }),
        }
    }

    fn managed_config(directory: &Path) -> WorkspaceServiceConfig {
        WorkspaceServiceConfig {
            runtime_dir: directory.join("run"),
            git: PathBuf::from("/git"),
            startup_timeout: Duration::from_secs(1),
            max_workspaces: 2,
            network_request_queue_capacity:
                WorkspaceServiceConfig::DEFAULT_NETWORK_REQUEST_QUEUE_CAPACITY,
            mux_initial_byte_window: WorkspaceServiceConfig::DEFAULT_MUX_INITIAL_BYTE_WINDOW,
            mux_max_channels: WorkspaceServiceConfig::DEFAULT_MUX_MAX_CHANNELS,
            mux_service_handshake_timeout:
                WorkspaceServiceConfig::DEFAULT_MUX_SERVICE_HANDSHAKE_TIMEOUT,
            max_concurrent_mux_services:
                WorkspaceServiceConfig::DEFAULT_MAX_CONCURRENT_MUX_SERVICES,
            mode: WorkspaceMode::Managed(ManagedWorkspaceConfig {
                image: directory.join("system.erofs"),
                kernel: directory.join("kernel"),
                initrd: directory.join("initrd"),
                kernel_append: "init=/nix/store/example/init".to_owned(),
                workspaces_dir: directory.join("workspaces"),
                state_dir: directory.join("state"),
                architecture: None,
                qemu: None,
                acceleration: Acceleration::Auto,
                memory_mib: 1024,
                vcpu_count: 1,
                shutdown_timeout: Duration::from_secs(1),
                data_disk_size: MINIMUM_DATA_DISK_BYTES,
                reset_data_workspaces: HashSet::new(),
                local_binaries: None,
            }),
        }
    }

    #[tokio::test]
    async fn down_preserves_and_delete_removes_an_idle_workspace() {
        let directory = tempdir().unwrap();
        let workspace = WorkspaceName::new("demo").unwrap();
        let config = managed_config(directory.path());
        let WorkspaceMode::Managed(managed) = &config.mode else {
            unreachable!();
        };
        let workspace_root = managed.workspaces_dir.join(workspace.as_str());
        let workspace_state = managed
            .state_dir
            .join("workspaces")
            .join(workspace.as_str());
        std::fs::create_dir_all(workspace_root.join("image")).unwrap();
        std::fs::write(workspace_root.join("config.toml"), "").unwrap();
        std::fs::create_dir_all(&workspace_state).unwrap();
        std::fs::write(workspace_state.join("state.raw"), "state").unwrap();

        let service = WorkspaceService::new(config).unwrap();
        service
            .stop(api::StopWorkspaceAction {
                workspace: api_name(&workspace),
            })
            .await
            .unwrap();
        assert!(workspace_root.exists());
        assert!(workspace_state.exists());
        assert_eq!(
            service.inner.store.snapshot().value.workspaces[0].state,
            api::WorkspaceState::Stopped
        );
        service
            .destroy(api::DestroyWorkspaceAction {
                workspace: api_name(&workspace),
            })
            .await
            .unwrap();
        assert!(!workspace_root.exists());
        assert!(!workspace_state.exists());
        assert!(service.inner.store.snapshot().value.workspaces.is_empty());
    }

    #[test]
    fn reset_removes_only_regular_state_files() {
        let directory = tempdir().unwrap();
        let disk = directory.path().join("state.raw");
        std::fs::write(&disk, b"state").unwrap();
        remove_regular_state_file(&disk, "test state disk").unwrap();
        assert!(!disk.exists());

        let target = directory.path().join("target");
        std::fs::write(&target, b"important").unwrap();
        symlink(&target, &disk).unwrap();
        assert!(remove_regular_state_file(&disk, "test state disk").is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"important");
    }

    #[test]
    fn workspace_vm_resources_override_host_defaults_independently() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        std::fs::write(
            &config,
            "[vm]\ncores = 6\nmemory = \"12G\"\ndisk = \"2T\"\n\n[env]\nEDITOR = \"vim\"\n",
        )
        .unwrap();
        let defaults = VmResources {
            memory_mib: 4096,
            vcpu_count: 2,
            state_disk_size: 1024_u64.pow(4),
        };

        assert_eq!(
            load_workspace_vm_resources(&config, defaults).unwrap(),
            VmResources {
                memory_mib: 12288,
                vcpu_count: 6,
                state_disk_size: 2 * 1024_u64.pow(4),
            }
        );

        std::fs::write(&config, "[vm]\ncores = 3\n").unwrap();
        assert_eq!(
            load_workspace_vm_resources(&config, defaults).unwrap(),
            VmResources {
                memory_mib: 4096,
                vcpu_count: 3,
                state_disk_size: 1024_u64.pow(4),
            }
        );
    }

    #[test]
    fn workspace_vm_resources_reject_zero_unknown_and_symlinked_config() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let defaults = VmResources {
            memory_mib: 4096,
            vcpu_count: 2,
            state_disk_size: 1024_u64.pow(4),
        };

        for contents in [
            "[vm]\ncores = 0\n",
            "[vm]\nmemory = \"0G\"\n",
            "[vm]\nmemory = \"1024\"\n",
            "[vm]\ndisk = \"128M\"\n",
            "[vm]\ndisk = \"1024\"\n",
            "[vm]\nthreads = 4\n",
        ] {
            std::fs::write(&config, contents).unwrap();
            assert!(load_workspace_vm_resources(&config, defaults).is_err());
        }

        let target = directory.path().join("target.toml");
        std::fs::write(&target, "[vm]\ncores = 4\n").unwrap();
        std::fs::remove_file(&config).unwrap();
        symlink(target, &config).unwrap();
        assert!(load_workspace_vm_resources(&config, defaults).is_err());
    }

    #[test]
    fn invalid_workspace_vm_resources_fall_back_with_a_diagnostic() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        std::fs::write(&config, "[vm]\ncores = 0\n").unwrap();
        let defaults = VmResources {
            memory_mib: 4096,
            vcpu_count: 2,
            state_disk_size: 1024_u64.pow(4),
        };

        let (resources, diagnostics) = load_workspace_vm_resources_with_fallback(&config, defaults);
        assert_eq!(resources, defaults);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("workspace VM configuration is invalid"));

        std::fs::write(&config, "[vm]\ncores = 4\n").unwrap();
        let (resources, diagnostics) = load_workspace_vm_resources_with_fallback(&config, defaults);
        assert_eq!(resources.vcpu_count, 4);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn workspace_vm_memory_accepts_binary_size_suffixes() {
        assert_eq!(parse_memory_mib("16G").unwrap(), 16 * 1024);
        assert_eq!(parse_memory_mib("1536MiB").unwrap(), 1536);
        assert_eq!(parse_memory_mib("2tib").unwrap(), 2 * 1024 * 1024);
        for invalid in ["", "16", "1.5G", "-1G", "0M", "16GB"] {
            assert!(parse_memory_mib(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn service_rejects_unbounded_configuration() {
        let directory = tempdir().unwrap();
        let workspace = WorkspaceName::new("external").unwrap();
        let mut config = external_config(directory.path(), workspace);
        config.max_workspaces = 0;
        assert!(config.validate().is_err());
        config.max_workspaces = 1;
        config.startup_timeout = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    #[test]
    fn service_rejects_invalid_managed_vm_resources() {
        let directory = tempdir().unwrap();
        let managed = ManagedWorkspaceConfig {
            image: directory.path().join("system.erofs"),
            kernel: directory.path().join("kernel"),
            initrd: directory.path().join("initrd"),
            kernel_append: "init=/nix/store/example/init".to_owned(),
            workspaces_dir: directory.path().join("workspaces"),
            state_dir: directory.path().join("state"),
            architecture: None,
            qemu: None,
            acceleration: Acceleration::Auto,
            memory_mib: 0,
            vcpu_count: 1,
            shutdown_timeout: Duration::from_secs(1),
            data_disk_size: MINIMUM_DATA_DISK_BYTES,
            reset_data_workspaces: HashSet::new(),
            local_binaries: None,
        };
        let mut config = WorkspaceServiceConfig {
            runtime_dir: directory.path().to_owned(),
            git: PathBuf::from("/usr/bin/git"),
            startup_timeout: Duration::from_secs(1),
            max_workspaces: 1,
            network_request_queue_capacity:
                WorkspaceServiceConfig::DEFAULT_NETWORK_REQUEST_QUEUE_CAPACITY,
            mux_initial_byte_window: WorkspaceServiceConfig::DEFAULT_MUX_INITIAL_BYTE_WINDOW,
            mux_max_channels: WorkspaceServiceConfig::DEFAULT_MUX_MAX_CHANNELS,
            mux_service_handshake_timeout:
                WorkspaceServiceConfig::DEFAULT_MUX_SERVICE_HANDSHAKE_TIMEOUT,
            max_concurrent_mux_services:
                WorkspaceServiceConfig::DEFAULT_MAX_CONCURRENT_MUX_SERVICES,
            mode: WorkspaceMode::Managed(managed),
        };

        assert!(config.validate().is_err());
        if let WorkspaceMode::Managed(managed) = &mut config.mode {
            managed.memory_mib = 1;
            managed.vcpu_count = 0;
        }
        assert!(config.validate().is_err());
        if let WorkspaceMode::Managed(managed) = &mut config.mode {
            managed.vcpu_count = 1;
            managed.data_disk_size = MINIMUM_DATA_DISK_BYTES - 1;
        }
        assert!(config.validate().is_err());
    }

    #[test]
    fn service_rejects_a_preconfigured_guest_instance_id() {
        let directory = tempdir().unwrap();
        let mut config = managed_config(directory.path());
        let WorkspaceMode::Managed(managed) = &mut config.mode else {
            unreachable!();
        };
        managed
            .kernel_append
            .push_str(" tascarrel.guest-instance-id=guest_instance_1111111111111111111111");

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("host-owned tascarrel.guest-instance-id"));
    }

    #[tokio::test]
    async fn concurrent_connections_share_one_workspace_worker() {
        let directory = tempdir().unwrap();
        let workspace = WorkspaceName::new("external").unwrap();
        let service =
            WorkspaceService::new(external_config(directory.path(), workspace.clone())).unwrap();
        let first = service.worker_sender(&workspace).await.unwrap();
        let second = service.worker_sender(&workspace).await.unwrap();

        assert!(first.same_channel(&second));
        service.shutdown().await;
    }

    /// Stops an active worker before removing an externally deleted workspace.
    #[tokio::test]
    async fn inventory_reconciliation_stops_removed_workspace_worker() {
        let directory = tempdir().unwrap();
        let workspace = WorkspaceName::new("demo").unwrap();
        let config = managed_config(directory.path());
        let WorkspaceMode::Managed(managed) = &config.mode else {
            unreachable!();
        };
        let workspace_root = managed.workspaces_dir.join(workspace.as_str());
        std::fs::create_dir_all(workspace_root.join("image")).unwrap();

        let service = WorkspaceService::new(config).unwrap();
        let (sender, mut receiver) = mpsc::channel(WORKSPACE_QUEUE_CAPACITY);
        service
            .inner
            .workers
            .lock()
            .await
            .insert(workspace.clone(), sender);
        let stopped = tokio::spawn(async move {
            let Some(WorkspaceCommand::Stop { response }) = receiver.recv().await else {
                return false;
            };
            response.send(Ok(())).is_ok()
        });

        std::fs::remove_dir_all(&workspace_root).unwrap();
        reconcile_workspace_inventory(&service.inner).await.unwrap();

        assert!(stopped.await.unwrap());
        assert!(!service.inner.workers.lock().await.contains_key(&workspace));
        assert!(service.inner.store.snapshot().value.workspaces.is_empty());
        service.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_before_first_connection_is_terminal() {
        let directory = tempdir().unwrap();
        let workspace = WorkspaceName::new("external").unwrap();
        let service =
            WorkspaceService::new(external_config(directory.path(), workspace.clone())).unwrap();

        let concurrent = service.clone();
        tokio::join!(service.shutdown(), concurrent.shutdown());
        let error = service.connect(workspace).await.unwrap_err();

        assert_eq!(error.code, ErrorCode::Busy);
        assert!(error.message.contains("shutting down"));
        assert!(service.inner.tasks.lock().unwrap().is_empty());
    }
}
