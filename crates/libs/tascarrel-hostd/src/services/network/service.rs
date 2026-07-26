//! Network service state, API operations, and forwarding transports.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::Request;
use axum::http::Response;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::http::header;
use axum::http::uri::Authority;
use hickory_resolver::TokioResolver;
use hickory_resolver::config::ConnectionConfig;
use hickory_resolver::config::NameServerConfig;
use hickory_resolver::config::ResolverConfig;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hyper::client::conn::http1 as client_http1;
use hyper_util::rt::TokioIo;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::host::HostInstanceId;
use tascarrel_api::types::network as api;
use tascarrel_api::types::store as store_api;
use tascarrel_api::types::workspaces::WorkspaceName as ApiWorkspaceName;
use tascarrel_mux::Channel;
use tascarrel_protocol::Framed;
use tascarrel_protocol::MUX_PUBLISH_GUEST_ENDPOINT;
use tascarrel_protocol::PodId;
use tascarrel_protocol::PublishedPortConnect;
use tascarrel_protocol::PublishedPortConnectResponse;
use tascarrel_protocol::WorkspaceName;
use tascarrel_store::Store;
use thiserror::Error;
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::debug;
use tracing::warn;
use uuid::Uuid;

use super::activity::ActivityStream;
use super::activity::ActivitySubscription;
use super::proxy::requests_upgrade;
use super::proxy::strip_hop_by_hop;
use crate::WorkspaceService;
use crate::services::secrets::SecretsService;
use crate::services::workspaces::WorkspaceNetworkRequest;
use crate::services::workspaces::WorkspaceNetworkRequests;

const DEFAULT_HOSTNAME_SUFFIX: &str = "tascarrel.localhost";
const DEFAULT_MAX_HTTP_ROUTES: usize = 4096;
const DEFAULT_MAX_PORT_FORWARDS: usize = 256;
const DEFAULT_MAX_POD_HOST_FORWARDS: usize = 256;
const DEFAULT_MAX_CONNECTIONS: usize = 512;
const DEFAULT_MAX_GUEST_TRANSPORTS: usize = 512;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_DNS_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_DNS_ADDRESS_SUMMARY_LIMIT: NonZeroUsize =
    NonZeroUsize::new(16).expect("the DNS address summary limit is non-zero");
const DEFAULT_DNS_HOSTNAME_MAPPING_LIMIT: NonZeroUsize =
    NonZeroUsize::new(8192).expect("the DNS hostname mapping limit is non-zero");
const DEFAULT_STORE_HISTORY_LIMIT: NonZeroUsize =
    NonZeroUsize::new(256).expect("the network store history limit is non-zero");
const DEFAULT_DNS_REQUEST_HISTORY_LIMIT: NonZeroUsize =
    NonZeroUsize::new(4096).expect("the DNS request history limit is non-zero");
const DEFAULT_TCP_FLOW_HISTORY_LIMIT: NonZeroUsize =
    NonZeroUsize::new(8192).expect("the TCP flow history limit is non-zero");
const DEFAULT_ACTIVITY_BATCH_LIMIT: NonZeroUsize =
    NonZeroUsize::new(128).expect("the network activity batch limit is non-zero");
const NETWORK_TITLE_MAX_BYTES: usize = 256;

type HttpRouteStore = Store<api::HttpRouteList, api::HttpRouteListMutation>;
type PortForwardStore = Store<api::PortForwardList, api::PortForwardListMutation>;
type PodHostForwardStore = Store<api::PodHostForwardList, api::PodHostForwardListMutation>;

/// Resumable stream of HTTP route-list changes for one workspace.
pub type HttpRouteListSubscription =
    tascarrel_store::Subscription<api::HttpRouteList, api::HttpRouteListMutation>;

/// Resumable stream of dynamic port-forward changes for one workspace.
pub type PortForwardListSubscription =
    tascarrel_store::Subscription<api::PortForwardList, api::PortForwardListMutation>;

/// Resumable stream of pod-to-host forward changes for one workspace.
pub type PodHostForwardListSubscription =
    tascarrel_store::Subscription<api::PodHostForwardList, api::PodHostForwardListMutation>;

/// Resumable stream of DNS request batches for one workspace.
pub type DnsRequestsSubscription = ActivitySubscription<api::DnsRequest>;

/// Resumable stream of TCP flow lifecycle batches for one workspace.
pub type TcpFlowsSubscription = ActivitySubscription<api::TcpFlowEvent>;

/// Limits and authority policy for the host network service.
#[derive(Clone, Debug)]
pub struct NetworkServiceConfig {
    /// DNS suffix below which host-issued route labels are recognized.
    pub hostname_suffix: String,
    /// Host reached by static `network.host_ports` mappings.
    ///
    /// Dynamic pod-scoped host forwards remain bound to this daemon's
    /// loopback interface.
    pub host_port_host: String,
    /// Maximum number of host-issued HTTP routes.
    pub max_http_routes: usize,
    /// Maximum number of active host-loopback TCP listeners.
    pub max_port_forwards: usize,
    /// Maximum number of pod-scoped forwards to host-loopback TCP ports.
    pub max_pod_host_forwards: usize,
    /// Maximum number of concurrent pod connections across both transports.
    pub max_connections: usize,
    /// Maximum number of semantic guest DNS and TCP channels served at once.
    pub max_guest_transports: usize,
    /// Maximum time allowed to start a workspace and open one pod connection.
    pub connect_timeout: Duration,
    /// Upstream resolver override, or the host system resolver when absent.
    pub dns_resolver: Option<std::net::SocketAddr>,
    /// Maximum time allowed for one semantic DNS resolution.
    pub dns_timeout: Duration,
    /// Maximum number of resolved addresses retained in one DNS activity item.
    pub dns_address_summary_limit: NonZeroUsize,
    /// Maximum number of DNS-derived address-to-hostname mappings retained per
    /// workspace.
    pub dns_hostname_mapping_limit: NonZeroUsize,
    /// Number of mutations retained for each workspace-scoped route list.
    pub store_history_limit: NonZeroUsize,
    /// Number of DNS request entries retained for each workspace.
    pub dns_request_history_limit: NonZeroUsize,
    /// Number of TCP lifecycle entries retained for each workspace.
    pub tcp_flow_history_limit: NonZeroUsize,
    /// Maximum number of activity entries emitted in one subscription event.
    pub activity_batch_limit: NonZeroUsize,
}

impl Default for NetworkServiceConfig {
    fn default() -> Self {
        Self {
            hostname_suffix: DEFAULT_HOSTNAME_SUFFIX.to_owned(),
            host_port_host: Ipv4Addr::LOCALHOST.to_string(),
            max_http_routes: DEFAULT_MAX_HTTP_ROUTES,
            max_port_forwards: DEFAULT_MAX_PORT_FORWARDS,
            max_pod_host_forwards: DEFAULT_MAX_POD_HOST_FORWARDS,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_guest_transports: DEFAULT_MAX_GUEST_TRANSPORTS,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            dns_resolver: None,
            dns_timeout: DEFAULT_DNS_TIMEOUT,
            dns_address_summary_limit: DEFAULT_DNS_ADDRESS_SUMMARY_LIMIT,
            dns_hostname_mapping_limit: DEFAULT_DNS_HOSTNAME_MAPPING_LIMIT,
            store_history_limit: DEFAULT_STORE_HISTORY_LIMIT,
            dns_request_history_limit: DEFAULT_DNS_REQUEST_HISTORY_LIMIT,
            tcp_flow_history_limit: DEFAULT_TCP_FLOW_HISTORY_LIMIT,
            activity_batch_limit: DEFAULT_ACTIVITY_BATCH_LIMIT,
        }
    }
}

impl NetworkServiceConfig {
    fn validate(&self) -> Result<(), Report<NetworkServiceError>> {
        if self.max_http_routes == 0
            || self.max_port_forwards == 0
            || self.max_pod_host_forwards == 0
            || self.max_connections == 0
            || self.max_guest_transports == 0
            || self.connect_timeout.is_zero()
            || self.dns_timeout.is_zero()
            || self.activity_batch_limit > self.dns_request_history_limit
            || self.activity_batch_limit > self.tcp_flow_history_limit
        {
            return Err(NetworkServiceError::InvalidConfiguration.report());
        }
        validate_hostname_suffix(&self.hostname_suffix)?;
        validate_host_port_host(&self.host_port_host)
    }
}

/// Host-owned network routes, listeners, and resumable state.
#[derive(Clone)]
pub struct NetworkService {
    pub(crate) inner: Arc<NetworkServiceInner>,
}

impl std::fmt::Debug for NetworkService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NetworkService")
            .field("hostname_suffix", &self.inner.config.hostname_suffix)
            .finish_non_exhaustive()
    }
}

pub(crate) struct NetworkServiceInner {
    host_instance_id: HostInstanceId,
    pub(crate) resolver: TokioResolver,
    pub(crate) config: NetworkServiceConfig,
    state: Mutex<NetworkState>,
    pub(crate) connections: Arc<Semaphore>,
    shutting_down: AtomicBool,
}

#[derive(Default)]
struct NetworkState {
    http_routes: HashMap<api::HttpRouteId, api::HttpRoute>,
    http_route_targets: HashMap<PodPortTarget, api::HttpRouteId>,
    hostname_prefixes: HashMap<String, api::HttpRouteId>,
    trusted_tascarrel_frontend_route: Option<api::HttpRouteId>,
    port_forwards: HashMap<api::PortForwardId, PortForwardEntry>,
    pod_host_forwards: HashMap<api::PodHostForwardId, api::PodHostForward>,
    pod_host_forward_targets: HashMap<PodPortTarget, api::PodHostForwardId>,
    workspaces: HashMap<ApiWorkspaceName, WorkspaceNetworkStores>,
}

struct WorkspaceNetworkStores {
    http_routes: HttpRouteStore,
    port_forwards: PortForwardStore,
    pod_host_forwards: PodHostForwardStore,
    dns_hostnames: DnsHostnameMappings,
    dns_requests: ActivityStream<api::DnsRequest>,
    tcp_flows: ActivityStream<api::TcpFlowEvent>,
}

struct DnsHostnameMappings {
    entries: HashMap<IpAddr, Arc<str>>,
    insertion_order: VecDeque<(IpAddr, Arc<str>)>,
    capacity: NonZeroUsize,
}

struct PortForwardEntry {
    route: api::PortForward,
    task: JoinHandle<()>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PodPortTarget {
    workspace: ApiWorkspaceName,
    pod_id: tascarrel_api::types::pods::PodId,
    pod_port: u16,
}

/// One resolved routing layer consumed by this host daemon.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedHttpRoute {
    target: PodPortTarget,
    original_authority: Authority,
    forwarded_authority: Authority,
}

/// Caller-relevant network service failure categories.
#[derive(Debug, Error)]
pub enum NetworkServiceError {
    /// The service was constructed with invalid limits or hostname policy.
    #[error("network service configuration is invalid")]
    InvalidConfiguration,
    /// An action input, cursor, route identifier, or target is invalid.
    #[error("invalid network request: {0}")]
    InvalidRequest(String),
    /// A workspace or required forwarding resource is unavailable.
    #[error("network target is unavailable: {0}")]
    Unavailable(String),
    /// A configured network resource limit has been reached.
    #[error("network service is overloaded: {0}")]
    Overloaded(String),
    /// Host network state or listener management failed unexpectedly.
    #[error("network service failed: {0}")]
    Internal(String),
}

/// Failure while resolving or forwarding one browser HTTP request.
#[derive(Debug, Error)]
pub enum NetworkProxyError {
    /// The HTTP authority or upgrade request is malformed.
    #[error("invalid routed HTTP request: {0}")]
    InvalidRequest(String),
    /// The rightmost hostname label is not an active host-issued prefix.
    #[error("unknown Tascarrel hostname prefix {0:?}")]
    UnknownPrefix(String),
    /// The forwarding concurrency limit has been reached.
    #[error("Tascarrel network forwarding capacity is exhausted")]
    Overloaded,
    /// Opening the workspace pod connection timed out.
    #[error("timed out opening the routed pod connection")]
    TimedOut,
    /// The workspace, pod, or target port could not be reached.
    #[error("could not reach routed pod target: {0}")]
    Unavailable(String),
    /// HTTP forwarding over the established pod connection failed.
    #[error("routed HTTP connection failed: {0}")]
    Forward(String),
}

impl NetworkProxyError {
    /// Returns the HTTP status representing this forwarding failure.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match self {
            Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Self::UnknownPrefix(_) => StatusCode::NOT_FOUND,
            Self::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
            Self::TimedOut => StatusCode::GATEWAY_TIMEOUT,
            Self::Unavailable(_) | Self::Forward(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

impl NetworkService {
    /// Creates an empty host network service.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured limit, timeout, or hostname suffix
    /// is invalid.
    pub fn new(config: NetworkServiceConfig) -> Result<Self, Report<NetworkServiceError>> {
        config.validate()?;
        let resolver = build_dns_resolver(&config)?;
        let connections = Arc::new(Semaphore::new(config.max_connections));
        Ok(Self {
            inner: Arc::new(NetworkServiceInner {
                host_instance_id: HostInstanceId::generate(),
                resolver,
                config,
                state: Mutex::new(NetworkState::default()),
                connections,
                shutting_down: AtomicBool::new(false),
            }),
        })
    }

    /// Returns the current HTTP routes belonging to one pod.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or unavailable workspace, or while the
    /// service is shutting down.
    #[tracing::instrument(level = "debug", skip(self, input, workspaces), fields(workspace = %input.workspace.as_str(), pod_id = %input.pod_id.0))]
    pub fn get_pod_http_routes(
        &self,
        input: &api::GetPodHttpRoutesAction,
        workspaces: &WorkspaceService,
    ) -> Result<api::GetPodHttpRoutesOutput, Report<NetworkServiceError>> {
        self.require_running()?;
        let workspace = runtime_workspace(&input.workspace)?;
        workspaces
            .validate_workspace(&workspace)
            .map_err(|error| unavailable(error.to_string()))?;
        let state = lock(&self.inner.state);
        let mut http_routes = state
            .http_routes
            .values()
            .filter(|route| route.workspace == input.workspace && route.pod_id == input.pod_id)
            .cloned()
            .collect::<Vec<_>>();
        http_routes.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(api::GetPodHttpRoutesOutput {
            http_routes: http_routes.into(),
        })
    }

    /// Returns the current dynamic port forwards belonging to one pod.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or unavailable workspace, or while the
    /// service is shutting down.
    #[tracing::instrument(level = "debug", skip(self, input, workspaces), fields(workspace = %input.workspace.as_str(), pod_id = %input.pod_id.0))]
    pub fn get_pod_port_forwards(
        &self,
        input: &api::GetPodPortForwardsAction,
        workspaces: &WorkspaceService,
    ) -> Result<api::GetPodPortForwardsOutput, Report<NetworkServiceError>> {
        self.require_running()?;
        let workspace = runtime_workspace(&input.workspace)?;
        workspaces
            .validate_workspace(&workspace)
            .map_err(|error| unavailable(error.to_string()))?;
        let state = lock(&self.inner.state);
        let mut port_forwards = state
            .port_forwards
            .values()
            .map(|entry| &entry.route)
            .filter(|route| route.workspace == input.workspace && route.pod_id == input.pod_id)
            .cloned()
            .collect::<Vec<_>>();
        port_forwards.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(api::GetPodPortForwardsOutput {
            port_forwards: port_forwards.into(),
        })
    }

    /// Serves the private workspace transport stream supplied by the
    /// workspace runtime.
    #[tracing::instrument(
        name = "tascarrel_host.network.serve_workspace_requests",
        level = "debug",
        skip_all
    )]
    pub(crate) async fn serve_workspace_requests(
        &self,
        mut requests: WorkspaceNetworkRequests,
        secrets: SecretsService,
    ) {
        let mut transports = JoinSet::new();
        loop {
            tokio::select! {
                request = requests.recv(), if transports.len() < self.inner.config.max_guest_transports => {
                    let Some(request) = request else {
                        return;
                    };
                    let service = self.clone();
                    let secrets = secrets.clone();
                    transports.spawn(async move {
                        match request {
                            WorkspaceNetworkRequest::Dns { workspace, channel } => {
                                service.serve_dns_channel(&workspace, channel).await
                            }
                            WorkspaceNetworkRequest::Tcp(request) => {
                                service
                                    .serve_tcp_channel(
                                        &request.workspace,
                                        &request.policy,
                                        request.authority,
                                        &secrets,
                                        request.channel,
                                    )
                                    .await
                            }
                        }
                    });
                }
                Some(result) = transports.join_next(), if !transports.is_empty() => {
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => debug!(%error, "guest network transport closed"),
                        Err(error) => warn!(%error, "guest network transport task failed"),
                    }
                }
            }
        }
    }

    /// Creates or returns the route for one workspace pod port.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid target, an unknown workspace, service
    /// shutdown, or exhausted route capacity.
    #[tracing::instrument(
        level = "debug",
        skip(self, input, workspaces),
        fields(workspace = %input.workspace.as_str(), pod_id = %input.pod_id.0, pod_port = input.pod_port)
    )]
    pub fn create_http_route(
        &self,
        input: api::CreateHttpRouteAction,
        workspaces: &WorkspaceService,
    ) -> Result<api::CreateHttpRouteOutput, Report<NetworkServiceError>> {
        self.require_running()?;
        validate_title(Some(&input.title), "HTTP route")?;
        let target = validate_target(input.workspace, input.pod_id, input.pod_port, workspaces)?;
        let mut state = lock(&self.inner.state);
        self.require_running()?;
        if let Some(id) = state.http_route_targets.get(&target).cloned()
            && let Some(mut route) = state.http_routes.get(&id).cloned()
        {
            if route.title != input.title || route.internal != input.internal {
                route.title = input.title;
                route.internal = input.internal;
                state.http_routes.insert(id, route.clone());
                let store = state
                    .workspace_stores(
                        &route.workspace,
                        &self.inner.config,
                        &self.inner.host_instance_id,
                    )
                    .http_routes
                    .clone();
                store.apply(api::HttpRouteListMutation::Upsert(route.clone()));
            }
            return Ok(api::CreateHttpRouteOutput {
                http_route_id: route.id.clone(),
                hostname_prefix: route.hostname_prefix.clone(),
            });
        }
        if state.http_routes.len() >= self.inner.config.max_http_routes {
            return Err(overloaded("HTTP route capacity has been reached"));
        }
        let id = unique_http_route_id(&state);
        let prefix = unique_hostname_prefix(&state);
        let route = api::HttpRoute {
            id: id.clone(),
            workspace: target.workspace.clone(),
            pod_id: target.pod_id.clone(),
            pod_port: target.pod_port,
            title: input.title,
            internal: input.internal,
            trusted_tascarrel_frontend: false,
            hostname_prefix: api::HostnamePrefix::new(prefix.clone()),
        };
        state.http_route_targets.insert(target, id.clone());
        state.hostname_prefixes.insert(prefix, id.clone());
        state.http_routes.insert(id.clone(), route.clone());
        let store = state
            .workspace_stores(
                &route.workspace,
                &self.inner.config,
                &self.inner.host_instance_id,
            )
            .http_routes
            .clone();
        store.apply(api::HttpRouteListMutation::Upsert(route.clone()));
        drop(state);
        Ok(api::CreateHttpRouteOutput {
            http_route_id: id,
            hostname_prefix: route.hostname_prefix,
        })
    }

    /// Changes whether one route may host a browser frontend with API access.
    ///
    /// Trust is exclusive. Enabling one route revokes the previously trusted
    /// route and publishes both changes through their workspace stores.
    ///
    /// # Errors
    ///
    /// Returns an error when the route is unknown or the service is shutting
    /// down.
    #[tracing::instrument(level = "debug", skip(self, input), fields(http_route_id = %input.http_route_id.0, trusted = input.trusted_tascarrel_frontend))]
    pub fn set_http_route_trusted_tascarrel_frontend(
        &self,
        input: &api::SetHttpRouteTrustedTascarrelFrontendAction,
    ) -> Result<api::SetHttpRouteTrustedTascarrelFrontendOutput, Report<NetworkServiceError>> {
        self.require_running()?;
        let mut state = lock(&self.inner.state);
        self.require_running()?;
        let mut route = state
            .http_routes
            .get(&input.http_route_id)
            .cloned()
            .ok_or_else(|| {
                invalid_request(format!(
                    "cannot change Tascarrel frontend trust because HTTP route {} does not exist",
                    input.http_route_id.0
                ))
            })?;
        if route.trusted_tascarrel_frontend == input.trusted_tascarrel_frontend {
            return Ok(api::SetHttpRouteTrustedTascarrelFrontendOutput {});
        }

        if input.trusted_tascarrel_frontend {
            if let Some(previous_id) = state.trusted_tascarrel_frontend_route.take()
                && previous_id != route.id
                && let Some(mut previous) = state.http_routes.get(&previous_id).cloned()
            {
                previous.trusted_tascarrel_frontend = false;
                publish_http_route(
                    &mut state,
                    &self.inner.config,
                    &self.inner.host_instance_id,
                    previous,
                );
            }
            state.trusted_tascarrel_frontend_route = Some(route.id.clone());
        } else if state.trusted_tascarrel_frontend_route.as_ref() == Some(&route.id) {
            state.trusted_tascarrel_frontend_route = None;
        }
        route.trusted_tascarrel_frontend = input.trusted_tascarrel_frontend;
        publish_http_route(
            &mut state,
            &self.inner.config,
            &self.inner.host_instance_id,
            route,
        );
        Ok(api::SetHttpRouteTrustedTascarrelFrontendOutput {})
    }

    /// Deletes one host-issued HTTP route.
    ///
    /// # Errors
    ///
    /// Returns an error when the route is unknown or the service is shutting
    /// down.
    #[tracing::instrument(level = "debug", skip(self, input), fields(http_route_id = %input.http_route_id.0))]
    pub fn delete_http_route(
        &self,
        input: &api::DeleteHttpRouteAction,
    ) -> Result<api::DeleteHttpRouteOutput, Report<NetworkServiceError>> {
        self.require_running()?;
        let mut state = lock(&self.inner.state);
        self.require_running()?;
        let route = state
            .http_routes
            .remove(&input.http_route_id)
            .ok_or_else(|| {
                invalid_request(format!(
                    "HTTP route {} does not exist",
                    input.http_route_id.0
                ))
            })?;
        state
            .http_route_targets
            .remove(&PodPortTarget::from(&route));
        state
            .hostname_prefixes
            .remove(route.hostname_prefix.as_str());
        if state.trusted_tascarrel_frontend_route.as_ref() == Some(&route.id) {
            state.trusted_tascarrel_frontend_route = None;
        }
        let store = state
            .workspace_stores(
                &route.workspace,
                &self.inner.config,
                &self.inner.host_instance_id,
            )
            .http_routes
            .clone();
        store.apply(api::HttpRouteListMutation::Remove(route.id));
        drop(state);
        Ok(api::DeleteHttpRouteOutput {})
    }

    /// Returns one route for operation-time authorization.
    #[must_use]
    pub(crate) fn http_route(&self, id: &api::HttpRouteId) -> Option<api::HttpRoute> {
        lock(&self.inner.state).http_routes.get(id).cloned()
    }

    /// Creates one dynamic host-loopback TCP forward.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, an unknown workspace, exhausted
    /// listener capacity, service shutdown, or a host listener failure.
    #[tracing::instrument(
        level = "debug",
        skip(self, input, workspaces),
        fields(workspace = %input.workspace.as_str(), pod_id = %input.pod_id.0, pod_port = input.pod_port)
    )]
    pub async fn create_port_forward(
        &self,
        input: api::CreatePortForwardAction,
        workspaces: &WorkspaceService,
    ) -> Result<api::CreatePortForwardOutput, Report<NetworkServiceError>> {
        self.require_running()?;
        let target = validate_target(input.workspace, input.pod_id, input.pod_port, workspaces)?;
        validate_title(input.title.as_deref(), "port-forward")?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| internal(format!("bind host loopback listener: {error}")))?;
        let host_port = listener
            .local_addr()
            .map_err(|error| internal(format!("inspect host loopback listener: {error}")))?
            .port();
        let mut state = lock(&self.inner.state);
        self.require_running()?;
        if state.port_forwards.len() >= self.inner.config.max_port_forwards {
            return Err(overloaded("dynamic port-forward capacity has been reached"));
        }
        let id = unique_port_forward_id(&state);
        let route = api::PortForward {
            id: id.clone(),
            workspace: target.workspace.clone(),
            pod_id: target.pod_id.clone(),
            pod_port: target.pod_port,
            host_port,
            title: input.title,
        };
        let (start, started) = oneshot::channel();
        let weak = Arc::downgrade(&self.inner);
        let task_id = id.clone();
        let task_target = target.clone();
        let task_workspaces = workspaces.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = started.await {
                warn!(%error, port_forward_id = %task_id.0, "dynamic port-forward listener start was canceled");
                if let Some(inner) = weak.upgrade() {
                    remove_stopped_port_forward(&inner, &task_id, host_port);
                }
                return;
            }
            let result =
                run_port_forward(listener, task_target, task_workspaces, weak.clone()).await;
            if let Err(error) = result {
                warn!(%error, port_forward_id = %task_id.0, "dynamic port-forward listener stopped");
            }
            if let Some(inner) = weak.upgrade() {
                remove_stopped_port_forward(&inner, &task_id, host_port);
            }
        });
        state.port_forwards.insert(
            id.clone(),
            PortForwardEntry {
                route: route.clone(),
                task,
            },
        );
        let store = state
            .workspace_stores(
                &route.workspace,
                &self.inner.config,
                &self.inner.host_instance_id,
            )
            .port_forwards
            .clone();
        store.apply(api::PortForwardListMutation::Upsert(route));
        drop(state);
        if start.send(()).is_err() {
            remove_stopped_port_forward(&self.inner, &id, host_port);
            return Err(internal(
                "failed to start dynamic port-forward listener task",
            ));
        }
        Ok(api::CreatePortForwardOutput {
            port_forward_id: id,
            host_port,
        })
    }

    /// Returns one dynamic port forward for operation-time authorization.
    #[must_use]
    pub(crate) fn port_forward(&self, id: &api::PortForwardId) -> Option<api::PortForward> {
        lock(&self.inner.state)
            .port_forwards
            .get(id)
            .map(|entry| entry.route.clone())
    }

    /// Deletes one dynamic host-loopback TCP forward.
    ///
    /// # Errors
    ///
    /// Returns an error when the forward is unknown or the service is
    /// shutting down.
    #[tracing::instrument(level = "debug", skip(self, input), fields(port_forward_id = %input.port_forward_id.0))]
    pub async fn delete_port_forward(
        &self,
        input: &api::DeletePortForwardAction,
    ) -> Result<api::DeletePortForwardOutput, Report<NetworkServiceError>> {
        self.require_running()?;
        let entry = {
            let mut state = lock(&self.inner.state);
            self.require_running()?;
            let entry = state
                .port_forwards
                .remove(&input.port_forward_id)
                .ok_or_else(|| {
                    invalid_request(format!(
                        "port forward {} does not exist",
                        input.port_forward_id.0
                    ))
                })?;
            let store = state
                .workspace_stores(
                    &entry.route.workspace,
                    &self.inner.config,
                    &self.inner.host_instance_id,
                )
                .port_forwards
                .clone();
            store.apply(api::PortForwardListMutation::Remove(entry.route.id.clone()));
            entry
        };
        entry.task.abort();
        if let Err(error) = entry.task.await
            && !error.is_cancelled()
        {
            warn!(%error, port_forward_id = %input.port_forward_id.0, "dynamic port-forward listener task failed during deletion");
        }
        Ok(api::DeletePortForwardOutput {})
    }

    /// Creates or updates one pod-scoped forward to a host-loopback port.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid mapping, unknown workspace, exhausted
    /// capacity, or service shutdown.
    #[tracing::instrument(
        level = "debug",
        skip(self, input, workspaces),
        fields(workspace = %input.workspace.as_str(), pod_id = %input.pod_id.0, mapping = %input.mapping)
    )]
    pub fn create_pod_host_forward(
        &self,
        input: api::CreatePodHostForwardAction,
        workspaces: &WorkspaceService,
    ) -> Result<api::CreatePodHostForwardOutput, Report<NetworkServiceError>> {
        self.require_running()?;
        let (host_port, pod_port) = input
            .mapping
            .ports()
            .map_err(|error| invalid_request(error.to_string()))?;
        let target = validate_target(input.workspace, input.pod_id, pod_port, workspaces)?;
        validate_title(input.title.as_deref(), "pod-to-host forward")?;
        let mut state = lock(&self.inner.state);
        self.require_running()?;
        if let Some(id) = state.pod_host_forward_targets.get(&target).cloned() {
            let forward = state
                .pod_host_forwards
                .get_mut(&id)
                .ok_or_else(|| internal("pod-to-host target refers to a missing forward"))?;
            if forward.mapping != input.mapping || forward.title != input.title {
                forward.mapping = input.mapping;
                forward.title = input.title;
                let forward = forward.clone();
                let store = state
                    .workspace_stores(
                        &forward.workspace,
                        &self.inner.config,
                        &self.inner.host_instance_id,
                    )
                    .pod_host_forwards
                    .clone();
                store.apply(api::PodHostForwardListMutation::Upsert(forward));
            }
            return Ok(api::CreatePodHostForwardOutput {
                pod_host_forward_id: id,
            });
        }
        if state.pod_host_forwards.len() >= self.inner.config.max_pod_host_forwards {
            return Err(overloaded("pod-to-host forward capacity has been reached"));
        }
        let id = unique_pod_host_forward_id(&state);
        let forward = api::PodHostForward {
            id: id.clone(),
            workspace: target.workspace.clone(),
            pod_id: target.pod_id.clone(),
            mapping: input.mapping,
            title: input.title,
        };
        state.pod_host_forward_targets.insert(target, id.clone());
        state.pod_host_forwards.insert(id.clone(), forward.clone());
        let store = state
            .workspace_stores(
                &forward.workspace,
                &self.inner.config,
                &self.inner.host_instance_id,
            )
            .pod_host_forwards
            .clone();
        store.apply(api::PodHostForwardListMutation::Upsert(forward));
        debug!(host_port, pod_port, "created pod-to-host forward");
        Ok(api::CreatePodHostForwardOutput {
            pod_host_forward_id: id,
        })
    }

    /// Deletes one pod-scoped forward to a host-loopback port.
    ///
    /// # Errors
    ///
    /// Returns an error when the forward is unknown or the service is
    /// shutting down.
    #[tracing::instrument(level = "debug", skip(self, input), fields(pod_host_forward_id = %input.pod_host_forward_id.0))]
    pub fn delete_pod_host_forward(
        &self,
        input: &api::DeletePodHostForwardAction,
    ) -> Result<api::DeletePodHostForwardOutput, Report<NetworkServiceError>> {
        self.require_running()?;
        let mut state = lock(&self.inner.state);
        self.require_running()?;
        let forward = state
            .pod_host_forwards
            .remove(&input.pod_host_forward_id)
            .ok_or_else(|| {
                invalid_request(format!(
                    "pod-to-host forward {} does not exist",
                    input.pod_host_forward_id.0
                ))
            })?;
        let (_, pod_port) = forward
            .mapping
            .ports()
            .map_err(|error| internal(format!("stored pod-to-host mapping is invalid: {error}")))?;
        state.pod_host_forward_targets.remove(&PodPortTarget {
            workspace: forward.workspace.clone(),
            pod_id: forward.pod_id.clone(),
            pod_port,
        });
        let store = state
            .workspace_stores(
                &forward.workspace,
                &self.inner.config,
                &self.inner.host_instance_id,
            )
            .pod_host_forwards
            .clone();
        store.apply(api::PodHostForwardListMutation::Remove(forward.id));
        Ok(api::DeletePodHostForwardOutput {})
    }

    /// Opens a resumable HTTP route-list subscription for one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid cursor or unknown workspace.
    #[tracing::instrument(level = "debug", skip(self, input, workspaces), fields(workspace = %input.workspace.as_str()))]
    pub fn subscribe_http_routes(
        &self,
        input: &api::HttpRouteListChangedSubscription,
        workspaces: &WorkspaceService,
    ) -> Result<HttpRouteListSubscription, Report<NetworkServiceError>> {
        let workspace = runtime_workspace(&input.workspace)?;
        workspaces
            .validate_workspace(&workspace)
            .map_err(|error| unavailable(error.to_string()))?;
        let cursor = input.cursor.as_ref().map(runtime_stamp).transpose()?;
        let mut state = lock(&self.inner.state);
        Ok(state
            .workspace_stores(
                &input.workspace,
                &self.inner.config,
                &self.inner.host_instance_id,
            )
            .http_routes
            .subscribe(cursor))
    }

    /// Opens a resumable port-forward-list subscription for one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid cursor or unknown workspace.
    #[tracing::instrument(level = "debug", skip(self, input, workspaces), fields(workspace = %input.workspace.as_str()))]
    pub fn subscribe_port_forwards(
        &self,
        input: &api::PortForwardListChangedSubscription,
        workspaces: &WorkspaceService,
    ) -> Result<PortForwardListSubscription, Report<NetworkServiceError>> {
        let workspace = runtime_workspace(&input.workspace)?;
        workspaces
            .validate_workspace(&workspace)
            .map_err(|error| unavailable(error.to_string()))?;
        let cursor = input.cursor.as_ref().map(runtime_stamp).transpose()?;
        let mut state = lock(&self.inner.state);
        Ok(state
            .workspace_stores(
                &input.workspace,
                &self.inner.config,
                &self.inner.host_instance_id,
            )
            .port_forwards
            .subscribe(cursor))
    }

    /// Opens a resumable pod-to-host-forward subscription for one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid cursor or unknown workspace.
    #[tracing::instrument(level = "debug", skip(self, input, workspaces), fields(workspace = %input.workspace.as_str()))]
    pub fn subscribe_pod_host_forwards(
        &self,
        input: &api::PodHostForwardListChangedSubscription,
        workspaces: &WorkspaceService,
    ) -> Result<PodHostForwardListSubscription, Report<NetworkServiceError>> {
        let workspace = runtime_workspace(&input.workspace)?;
        workspaces
            .validate_workspace(&workspace)
            .map_err(|error| unavailable(error.to_string()))?;
        let cursor = input.cursor.as_ref().map(runtime_stamp).transpose()?;
        let mut state = lock(&self.inner.state);
        Ok(state
            .workspace_stores(
                &input.workspace,
                &self.inner.config,
                &self.inner.host_instance_id,
            )
            .pod_host_forwards
            .subscribe(cursor))
    }

    /// Opens a resumable DNS request subscription for one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid cursor or unknown workspace.
    #[tracing::instrument(level = "debug", skip(self, input, workspaces), fields(workspace = %input.workspace.as_str()))]
    pub fn subscribe_dns_requests(
        &self,
        input: &api::DnsRequestsSubscription,
        workspaces: &WorkspaceService,
    ) -> Result<DnsRequestsSubscription, Report<NetworkServiceError>> {
        let workspace = runtime_workspace(&input.workspace)?;
        workspaces
            .validate_workspace(&workspace)
            .map_err(|error| unavailable(error.to_string()))?;
        let mut state = lock(&self.inner.state);
        let stream = state
            .workspace_stores(
                &input.workspace,
                &self.inner.config,
                &self.inner.host_instance_id,
            )
            .dns_requests
            .clone();
        Ok(stream.subscribe(
            input
                .cursor
                .as_ref()
                .map(|cursor| (&cursor.host_instance_id, cursor.position)),
        ))
    }

    /// Opens a resumable TCP lifecycle subscription for one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid cursor or unknown workspace.
    #[tracing::instrument(level = "debug", skip(self, input, workspaces), fields(workspace = %input.workspace.as_str()))]
    pub fn subscribe_tcp_flows(
        &self,
        input: &api::TcpFlowsSubscription,
        workspaces: &WorkspaceService,
    ) -> Result<TcpFlowsSubscription, Report<NetworkServiceError>> {
        let workspace = runtime_workspace(&input.workspace)?;
        workspaces
            .validate_workspace(&workspace)
            .map_err(|error| unavailable(error.to_string()))?;
        let mut state = lock(&self.inner.state);
        let stream = state
            .workspace_stores(
                &input.workspace,
                &self.inner.config,
                &self.inner.host_instance_id,
            )
            .tcp_flows
            .clone();
        Ok(stream.subscribe(
            input
                .cursor
                .as_ref()
                .map(|cursor| (&cursor.host_instance_id, cursor.position)),
        ))
    }

    /// Records one completed DNS request in its workspace stream.
    pub(crate) fn record_dns_request(
        &self,
        workspace: &ApiWorkspaceName,
        request: api::DnsRequest,
        resolved_addresses: &[IpAddr],
    ) {
        let stream = {
            let mut state = lock(&self.inner.state);
            let stores =
                state.workspace_stores(workspace, &self.inner.config, &self.inner.host_instance_id);
            let hostname = request.name.trim_end_matches('.').to_ascii_lowercase();
            if !hostname.is_empty() {
                stores
                    .dns_hostnames
                    .record(&hostname, resolved_addresses.iter().copied());
            }
            stores.dns_requests.clone()
        };
        stream.append(request);
    }

    /// Returns the most recently resolved hostname for one workspace
    /// destination address.
    pub(crate) fn dns_hostname(
        &self,
        workspace: &ApiWorkspaceName,
        address: IpAddr,
    ) -> Option<String> {
        lock(&self.inner.state)
            .workspaces
            .get(workspace)?
            .dns_hostnames
            .get(address)
    }

    /// Records one TCP lifecycle event in its workspace stream.
    pub(crate) fn record_tcp_flow(&self, workspace: &ApiWorkspaceName, event: api::TcpFlowEvent) {
        let stream = lock(&self.inner.state)
            .workspace_stores(workspace, &self.inner.config, &self.inner.host_instance_id)
            .tcp_flows
            .clone();
        stream.append(event);
    }

    /// Resolves a pod-visible virtual port to its dynamic host-loopback target.
    pub(crate) fn pod_host_forward_destination(
        &self,
        workspace: &ApiWorkspaceName,
        source: &api::NetworkRequestSource,
        pod_port: u16,
    ) -> Option<std::net::SocketAddr> {
        let api::NetworkRequestSource::Pod(pod_id) = source else {
            return None;
        };
        let state = lock(&self.inner.state);
        let id = state.pod_host_forward_targets.get(&PodPortTarget {
            workspace: workspace.clone(),
            pod_id: pod_id.clone(),
            pod_port,
        })?;
        let forward = state
            .pod_host_forwards
            .get(id)
            .expect("every pod-to-host target refers to a forward");
        let (host_port, _) = forward
            .mapping
            .ports()
            .expect("stored pod-to-host mappings were validated before insertion");
        Some(std::net::SocketAddr::new(
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            host_port,
        ))
    }

    /// Stops listeners and removes all state owned by a destroyed workspace.
    #[tracing::instrument(level = "debug", skip(self), fields(workspace = %workspace.as_str()))]
    pub(crate) async fn remove_workspace(&self, workspace: &ApiWorkspaceName) {
        let tasks = {
            let mut state = lock(&self.inner.state);
            let route_ids = state
                .http_routes
                .values()
                .filter(|route| &route.workspace == workspace)
                .map(|route| route.id.clone())
                .collect::<Vec<_>>();
            for id in route_ids {
                let route = state
                    .http_routes
                    .remove(&id)
                    .expect("workspace route identifiers were collected from this map");
                if state.trusted_tascarrel_frontend_route.as_ref() == Some(&route.id) {
                    state.trusted_tascarrel_frontend_route = None;
                }
                state
                    .http_route_targets
                    .remove(&PodPortTarget::from(&route));
                state
                    .hostname_prefixes
                    .remove(route.hostname_prefix.as_str());
            }

            let forward_ids = state
                .port_forwards
                .values()
                .filter(|entry| &entry.route.workspace == workspace)
                .map(|entry| entry.route.id.clone())
                .collect::<Vec<_>>();
            let mut tasks = Vec::with_capacity(forward_ids.len());
            for id in forward_ids {
                let entry = state
                    .port_forwards
                    .remove(&id)
                    .expect("workspace forward identifiers were collected from this map");
                tasks.push((id, entry.task));
            }

            let pod_host_forward_ids = state
                .pod_host_forwards
                .values()
                .filter(|forward| &forward.workspace == workspace)
                .map(|forward| forward.id.clone())
                .collect::<Vec<_>>();
            for id in pod_host_forward_ids {
                let forward = state
                    .pod_host_forwards
                    .remove(&id)
                    .expect("workspace pod-to-host identifiers were collected from this map");
                let (_, pod_port) = forward
                    .mapping
                    .ports()
                    .expect("stored pod-to-host mappings were validated before insertion");
                state.pod_host_forward_targets.remove(&PodPortTarget {
                    workspace: forward.workspace,
                    pod_id: forward.pod_id,
                    pod_port,
                });
            }
            state.workspaces.remove(workspace);
            tasks
        };

        for (_, task) in &tasks {
            task.abort();
        }
        for (id, task) in tasks {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                warn!(%error, port_forward_id = %id.0, "dynamic port-forward listener task failed during workspace removal");
            }
        }
    }

    /// Resolves the rightmost host-issued prefix in one HTTP authority.
    pub(crate) fn resolve_http_route(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<ResolvedHttpRoute>, Report<NetworkProxyError>> {
        let original_authority = request_authority(headers)?;
        let Some((prefix, forwarded_authority)) =
            split_routed_authority(&original_authority, &self.inner.config.hostname_suffix)?
        else {
            return Ok(None);
        };
        let state = lock(&self.inner.state);
        let id = state
            .hostname_prefixes
            .get(&prefix)
            .ok_or_else(|| NetworkProxyError::UnknownPrefix(prefix.clone()).report())?;
        let route = state
            .http_routes
            .get(id)
            .expect("every hostname prefix refers to an HTTP route");
        Ok(Some(ResolvedHttpRoute {
            target: PodPortTarget::from(route),
            original_authority,
            forwarded_authority,
        }))
    }

    /// Forwards one routed HTTP request to its workspace pod target.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(workspace = %route.target.workspace.as_str(), pod_id = %route.target.pod_id.0, pod_port = route.target.pod_port)
    )]
    pub(crate) async fn forward_http(
        &self,
        mut request: Request<Body>,
        route: ResolvedHttpRoute,
        workspaces: &WorkspaceService,
    ) -> Result<Response<Body>, Report<NetworkProxyError>> {
        if request.method() == Method::CONNECT {
            return Err(NetworkProxyError::InvalidRequest(
                "HTTP CONNECT is not supported".to_owned(),
            )
            .report());
        }
        let wants_upgrade = requests_upgrade(request.headers())
            .map_err(|error| NetworkProxyError::InvalidRequest(error.to_string()).report())?;
        let downstream_upgrade = wants_upgrade.then(|| hyper::upgrade::on(&mut request));
        rewrite_forwarded_request(&mut request, &route, wants_upgrade)?;
        let permit = Arc::new(
            Arc::clone(&self.inner.connections)
                .try_acquire_owned()
                .map_err(|_| NetworkProxyError::Overloaded.report())?,
        );
        let channel = timeout(
            self.inner.config.connect_timeout,
            open_pod_channel(&route.target, workspaces),
        )
        .await
        .map_err(|_| NetworkProxyError::TimedOut.report())?
        .map_err(|error| NetworkProxyError::Unavailable(error.to_string()).report())?;
        let (mut sender, connection) = client_http1::handshake(TokioIo::new(channel))
            .await
            .map_err(|error| NetworkProxyError::Forward(error.to_string()).report())?;
        let connection_permit = Arc::clone(&permit);
        tokio::spawn(async move {
            let _permit = connection_permit;
            if let Err(error) = connection.with_upgrades().await {
                debug!(%error, "routed upstream HTTP connection stopped");
            }
        });
        let mut response = sender
            .send_request(request)
            .await
            .map_err(|error| NetworkProxyError::Forward(error.to_string()).report())?;
        rewrite_forwarded_response(response.headers_mut(), &route)?;
        if let Some(downstream_upgrade) = downstream_upgrade {
            if response.status() == StatusCode::SWITCHING_PROTOCOLS
                && requests_upgrade(response.headers()).map_err(|error| {
                    NetworkProxyError::Forward(format!("invalid upstream upgrade: {error}"))
                        .report()
                })?
            {
                let upstream_upgrade = hyper::upgrade::on(&mut response);
                let upgrade_permit = Arc::clone(&permit);
                tokio::spawn(async move {
                    let _permit = upgrade_permit;
                    if let Err(error) = relay_upgrade(downstream_upgrade, upstream_upgrade).await {
                        debug!(%error, "routed HTTP upgrade relay stopped");
                    }
                });
                strip_hop_by_hop(response.headers_mut(), true);
            } else {
                strip_hop_by_hop(response.headers_mut(), false);
            }
        } else {
            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                return Err(NetworkProxyError::Forward(
                    "upstream switched protocols without an upgrade request".to_owned(),
                )
                .report());
            }
            strip_hop_by_hop(response.headers_mut(), false);
        }
        Ok(response.map(Body::new))
    }

    /// Returns whether an authority is allowed to address hostd's own web UI.
    #[must_use]
    pub(crate) fn is_frontend_authority(
        &self,
        authority: &Authority,
        bound: std::net::SocketAddr,
    ) -> bool {
        authority.as_str() == bound.to_string()
            || (authority
                .host()
                .eq_ignore_ascii_case(&self.inner.config.hostname_suffix)
                && authority.port_u16() == Some(bound.port()))
    }

    /// Returns whether an authority is the exact public origin of the trusted
    /// Tascarrel frontend route.
    #[must_use]
    pub(crate) fn is_trusted_tascarrel_frontend_authority(
        &self,
        authority: &Authority,
        bound: std::net::SocketAddr,
    ) -> bool {
        let state = lock(&self.inner.state);
        let Some(id) = &state.trusted_tascarrel_frontend_route else {
            return false;
        };
        let Some(route) = state.http_routes.get(id) else {
            return false;
        };
        let trusted_hostname = format!(
            "{}.{}",
            route.hostname_prefix.as_str(),
            self.inner.config.hostname_suffix
        );
        authority.host().eq_ignore_ascii_case(&trusted_hostname)
            && authority.port_u16() == Some(bound.port())
    }

    /// Stops every dynamic listener and rejects future network mutations.
    pub async fn shutdown(&self) {
        if self.inner.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        self.inner.connections.close();
        let tasks = {
            let mut state = lock(&self.inner.state);
            state
                .port_forwards
                .drain()
                .map(|(id, entry)| (id, entry.task))
                .collect::<Vec<_>>()
        };
        for (_, task) in &tasks {
            task.abort();
        }
        for (id, task) in tasks {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                warn!(%error, port_forward_id = %id.0, "dynamic port-forward listener task failed during shutdown");
            }
        }
    }

    fn require_running(&self) -> Result<(), Report<NetworkServiceError>> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            Err(unavailable("host network service is shutting down"))
        } else {
            Ok(())
        }
    }
}

impl NetworkState {
    fn workspace_stores(
        &mut self,
        workspace: &ApiWorkspaceName,
        config: &NetworkServiceConfig,
        host_instance_id: &HostInstanceId,
    ) -> &mut WorkspaceNetworkStores {
        self.workspaces
            .entry(workspace.clone())
            .or_insert_with(|| WorkspaceNetworkStores::new(config, host_instance_id))
    }
}

impl WorkspaceNetworkStores {
    fn new(config: &NetworkServiceConfig, host_instance_id: &HostInstanceId) -> Self {
        Self {
            http_routes: Store::new(
                api::HttpRouteList {
                    http_routes: Vec::new().into(),
                },
                reduce_http_routes,
                config.store_history_limit,
            ),
            port_forwards: Store::new(
                api::PortForwardList {
                    port_forwards: Vec::new().into(),
                },
                reduce_port_forwards,
                config.store_history_limit,
            ),
            pod_host_forwards: Store::new(
                api::PodHostForwardList {
                    pod_host_forwards: Vec::new().into(),
                },
                reduce_pod_host_forwards,
                config.store_history_limit,
            ),
            dns_hostnames: DnsHostnameMappings::new(config.dns_hostname_mapping_limit),
            dns_requests: ActivityStream::new(
                host_instance_id.clone(),
                config.dns_request_history_limit,
                config.activity_batch_limit,
            ),
            tcp_flows: ActivityStream::new(
                host_instance_id.clone(),
                config.tcp_flow_history_limit,
                config.activity_batch_limit,
            ),
        }
    }
}

impl From<&api::HttpRoute> for PodPortTarget {
    fn from(route: &api::HttpRoute) -> Self {
        Self {
            workspace: route.workspace.clone(),
            pod_id: route.pod_id.clone(),
            pod_port: route.pod_port,
        }
    }
}

async fn run_port_forward(
    listener: TcpListener,
    target: PodPortTarget,
    workspaces: WorkspaceService,
    inner: Weak<NetworkServiceInner>,
) -> Result<(), Report<NetworkServiceError>> {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = accepted
                    .map_err(|error| internal(format!("failed to accept host loopback connection: {error}")))?;
                let Some(inner) = inner.upgrade() else {
                    return Ok(());
                };
                let Ok(permit) = Arc::clone(&inner.connections).try_acquire_owned() else {
                    debug!("dropping port-forward connection because network capacity is exhausted");
                    continue;
                };
                let target = target.clone();
                let workspaces = workspaces.clone();
                let connect_timeout = inner.config.connect_timeout;
                connections.spawn(async move {
                    let _permit = permit;
                    relay_port_forward(socket, &target, &workspaces, connect_timeout).await
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => debug!(%error, "dynamic port-forward connection stopped"),
                    Err(error) => debug!(%error, "dynamic port-forward task failed"),
                }
            }
        }
    }
}

impl DnsHostnameMappings {
    fn new(capacity: NonZeroUsize) -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
            capacity,
        }
    }

    fn record(&mut self, hostname: &str, addresses: impl IntoIterator<Item = IpAddr>) {
        for address in addresses {
            let recorded_hostname: Arc<str> = Arc::from(hostname);
            self.entries.insert(address, Arc::clone(&recorded_hostname));
            self.insertion_order.push_back((address, recorded_hostname));
            self.evict_oldest();
        }
        if self.insertion_order.len() > self.capacity.get().saturating_mul(2) {
            self.compact_insertion_order();
        }
    }

    fn get(&self, address: IpAddr) -> Option<String> {
        self.entries.get(&address).map(ToString::to_string)
    }

    fn evict_oldest(&mut self) {
        while self.entries.len() > self.capacity.get() {
            let Some((address, recorded_hostname)) = self.insertion_order.pop_front() else {
                break;
            };
            if self
                .entries
                .get(&address)
                .is_some_and(|current| Arc::ptr_eq(current, &recorded_hostname))
            {
                self.entries.remove(&address);
            }
        }
    }

    fn compact_insertion_order(&mut self) {
        self.insertion_order.retain(|(address, recorded_hostname)| {
            self.entries
                .get(address)
                .is_some_and(|current| Arc::ptr_eq(current, recorded_hostname))
        });
    }
}

async fn relay_port_forward(
    mut socket: TcpStream,
    target: &PodPortTarget,
    workspaces: &WorkspaceService,
    connect_timeout: Duration,
) -> Result<(), Report<NetworkServiceError>> {
    let mut channel = timeout(connect_timeout, open_pod_channel(target, workspaces))
        .await
        .map_err(|_| unavailable("timed out opening pod connection"))??;
    copy_bidirectional(&mut socket, &mut channel)
        .await
        .map_err(|error| {
            unavailable(format!(
                "failed to relay dynamic port-forward connection: {error}"
            ))
        })?;
    Ok(())
}

async fn open_pod_channel(
    target: &PodPortTarget,
    workspaces: &WorkspaceService,
) -> Result<Channel, Report<NetworkServiceError>> {
    let workspace = runtime_workspace(&target.workspace)?;
    let mux = workspaces
        .connect(workspace)
        .await
        .map_err(|error| unavailable(format!("failed to connect to workspace guest: {error}")))?;
    let channel = mux
        .open(MUX_PUBLISH_GUEST_ENDPOINT)
        .await
        .map_err(|error| unavailable(format!("failed to open pod connection channel: {error}")))?;
    let mut framed = Framed::new(channel);
    framed
        .write(&PublishedPortConnect {
            pod_id: PodId(target.pod_id.0.to_string()),
            pod_port: target.pod_port,
        })
        .await
        .map_err(|error| unavailable(format!("failed to send pod connection target: {error}")))?;
    let response = framed
        .read::<PublishedPortConnectResponse>()
        .await
        .map_err(|error| unavailable(format!("failed to read pod connection result: {error}")))?
        .ok_or_else(|| unavailable("workspace guest closed the pod connection handshake"))?;
    response
        .result
        .map_err(|error| unavailable(error.to_string()))?;
    Ok(framed.into_inner())
}

fn remove_stopped_port_forward(
    inner: &NetworkServiceInner,
    id: &api::PortForwardId,
    host_port: u16,
) {
    let mut state = lock(&inner.state);
    if state
        .port_forwards
        .get(id)
        .is_none_or(|entry| entry.route.host_port != host_port)
    {
        return;
    }
    let entry = state
        .port_forwards
        .remove(id)
        .expect("the stopped port forward was checked while holding the state lock");
    let store = state
        .workspace_stores(
            &entry.route.workspace,
            &inner.config,
            &inner.host_instance_id,
        )
        .port_forwards
        .clone();
    store.apply(api::PortForwardListMutation::Remove(entry.route.id));
    drop(state);
}

fn validate_target(
    workspace: ApiWorkspaceName,
    pod_id: tascarrel_api::types::pods::PodId,
    pod_port: u16,
    workspaces: &WorkspaceService,
) -> Result<PodPortTarget, Report<NetworkServiceError>> {
    if pod_port == 0 {
        return Err(invalid_request("pod port must not be zero"));
    }
    let runtime = runtime_workspace(&workspace)?;
    workspaces
        .validate_workspace(&runtime)
        .map_err(|error| unavailable(error.to_string()))?;
    Ok(PodPortTarget {
        workspace,
        pod_id,
        pod_port,
    })
}

fn build_dns_resolver(
    config: &NetworkServiceConfig,
) -> Result<TokioResolver, Report<NetworkServiceError>> {
    let mut builder = if let Some(address) = config.dns_resolver {
        let mut udp = ConnectionConfig::udp();
        udp.port = address.port();
        let mut tcp = ConnectionConfig::tcp();
        tcp.port = address.port();
        let mut resolver = ResolverConfig::default();
        resolver
            .name_servers
            .push(NameServerConfig::new(address.ip(), true, vec![udp, tcp]));
        TokioResolver::builder_with_config(resolver, TokioRuntimeProvider::default())
    } else {
        TokioResolver::builder_tokio().map_err(|error| {
            invalid_configuration("host DNS resolver configuration is unavailable")
                .message(error.to_string())
        })?
    };
    builder.options_mut().timeout = config.dns_timeout;
    builder.options_mut().attempts = 2;
    builder.build().map_err(|error| {
        invalid_configuration("host DNS resolver could not be initialized")
            .message(error.to_string())
    })
}

/// Replaces one HTTP route and publishes the corresponding store mutation.
fn publish_http_route(
    state: &mut NetworkState,
    config: &NetworkServiceConfig,
    host_instance_id: &HostInstanceId,
    route: api::HttpRoute,
) {
    state.http_routes.insert(route.id.clone(), route.clone());
    let store = state
        .workspace_stores(&route.workspace, config, host_instance_id)
        .http_routes
        .clone();
    store.apply(api::HttpRouteListMutation::Upsert(route));
}

fn validate_title(
    title: Option<&str>,
    entry_kind: &str,
) -> Result<(), Report<NetworkServiceError>> {
    if title.is_some_and(|title| {
        title.trim().is_empty()
            || title.len() > NETWORK_TITLE_MAX_BYTES
            || title.chars().any(char::is_control)
    }) {
        return Err(invalid_request(format!(
            "{entry_kind} title must contain 1-256 bytes without control characters"
        )));
    }
    Ok(())
}

fn runtime_workspace(
    workspace: &ApiWorkspaceName,
) -> Result<WorkspaceName, Report<NetworkServiceError>> {
    if workspace.as_str().contains('.') {
        return Err(invalid_request("workspace names must not contain '.'"));
    }
    WorkspaceName::new(workspace.as_str()).map_err(|error| invalid_request(error.to_string()))
}

fn runtime_stamp(
    stamp: &store_api::Stamp,
) -> Result<tascarrel_store::Stamp, Report<NetworkServiceError>> {
    let generation = stamp.generation.parse::<Uuid>().map_err(|error| {
        invalid_request("network-list cursor generation is invalid").message(error.to_string())
    })?;
    Ok(tascarrel_store::Stamp {
        generation,
        version: stamp.version,
    })
}

fn unique_http_route_id(state: &NetworkState) -> api::HttpRouteId {
    loop {
        let id = api::HttpRouteId::generate();
        if !state.http_routes.contains_key(&id) {
            return id;
        }
    }
}

fn unique_port_forward_id(state: &NetworkState) -> api::PortForwardId {
    loop {
        let id = api::PortForwardId::generate();
        if !state.port_forwards.contains_key(&id) {
            return id;
        }
    }
}

fn unique_pod_host_forward_id(state: &NetworkState) -> api::PodHostForwardId {
    loop {
        let id = api::PodHostForwardId::generate();
        if !state.pod_host_forwards.contains_key(&id) {
            return id;
        }
    }
}

fn unique_hostname_prefix(state: &NetworkState) -> String {
    loop {
        let prefix = format!("r-{}", Uuid::new_v4().simple());
        if !state.hostname_prefixes.contains_key(&prefix) {
            return prefix;
        }
    }
}

fn request_authority(headers: &HeaderMap) -> Result<Authority, Report<NetworkProxyError>> {
    let mut values = headers.get_all(header::HOST).iter();
    let value = values
        .next()
        .ok_or_else(|| invalid_proxy_request("HTTP Host header is required"))?;
    if values.next().is_some() {
        return Err(invalid_proxy_request(
            "multiple HTTP Host headers are not allowed",
        ));
    }
    value
        .to_str()
        .map_err(|_| invalid_proxy_request("HTTP Host is not text"))?
        .parse()
        .map_err(|error| invalid_proxy_request(format!("HTTP Host is invalid: {error}")))
}

fn split_routed_authority(
    authority: &Authority,
    suffix: &str,
) -> Result<Option<(String, Authority)>, Report<NetworkProxyError>> {
    let host = authority.host().to_ascii_lowercase();
    if host == suffix {
        return Ok(None);
    }
    let Some(route_labels) = host
        .strip_suffix(suffix)
        .and_then(|prefix| prefix.strip_suffix('.'))
    else {
        return Ok(None);
    };
    if route_labels.is_empty() {
        return Err(invalid_proxy_request(
            "routed Tascarrel hostname has no prefix",
        ));
    }
    let (remaining, prefix) = route_labels
        .rsplit_once('.')
        .map_or(("", route_labels), |(remaining, prefix)| {
            (remaining, prefix)
        });
    let forwarded_host = if remaining.is_empty() {
        suffix.to_owned()
    } else {
        format!("{remaining}.{suffix}")
    };
    let forwarded = match authority.port_u16() {
        Some(port) => format!("{forwarded_host}:{port}"),
        None => forwarded_host,
    }
    .parse()
    .map_err(|error| {
        invalid_proxy_request(format!("forwarded HTTP authority is invalid: {error}"))
    })?;
    Ok(Some((prefix.to_owned(), forwarded)))
}

fn rewrite_forwarded_request(
    request: &mut Request<Body>,
    route: &ResolvedHttpRoute,
    preserve_upgrade: bool,
) -> Result<(), Report<NetworkProxyError>> {
    rewrite_same_origin(
        request.headers_mut(),
        &route.original_authority,
        &route.forwarded_authority,
    )?;
    rewrite_request_uri_header(
        request.headers_mut(),
        &header::REFERER,
        &route.original_authority,
        &route.forwarded_authority,
    )?;
    request.headers_mut().insert(
        header::HOST,
        HeaderValue::from_str(route.forwarded_authority.as_str()).map_err(|error| {
            invalid_proxy_request(format!("forwarded HTTP Host is invalid: {error}"))
        })?,
    );
    strip_hop_by_hop(request.headers_mut(), preserve_upgrade);
    *request.uri_mut() = request
        .uri()
        .path_and_query()
        .map_or_else(|| "/".parse(), |path| path.as_str().parse())
        .map_err(|error| invalid_proxy_request(format!("request path is invalid: {error}")))?;
    Ok(())
}

fn rewrite_same_origin(
    headers: &mut HeaderMap,
    original: &Authority,
    forwarded: &Authority,
) -> Result<(), Report<NetworkProxyError>> {
    let mut values = headers.get_all(header::ORIGIN).iter();
    let Some(value) = values.next() else {
        return Ok(());
    };
    if values.next().is_some() {
        return Err(invalid_proxy_request(
            "multiple HTTP Origin headers are not allowed",
        ));
    }
    let text = value
        .to_str()
        .map_err(|_| invalid_proxy_request("HTTP Origin is not text"))?;
    let origin: Uri = text
        .parse()
        .map_err(|error| invalid_proxy_request(format!("HTTP Origin is invalid: {error}")))?;
    let Some(authority) = origin.authority() else {
        return Ok(());
    };
    if !same_authority(authority, original) {
        return Ok(());
    }
    let scheme = origin
        .scheme_str()
        .ok_or_else(|| invalid_proxy_request("HTTP Origin has no scheme"))?;
    let rewritten = format!("{scheme}://{forwarded}");
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_str(&rewritten).map_err(|error| {
            invalid_proxy_request(format!("forwarded HTTP Origin is invalid: {error}"))
        })?,
    );
    Ok(())
}

fn rewrite_request_uri_header(
    headers: &mut HeaderMap,
    name: &header::HeaderName,
    original: &Authority,
    forwarded: &Authority,
) -> Result<(), Report<NetworkProxyError>> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(());
    };
    if values.next().is_some() {
        return Err(invalid_proxy_request(format!(
            "multiple HTTP {name} headers are not allowed"
        )));
    }
    let text = value
        .to_str()
        .map_err(|_| invalid_proxy_request(format!("HTTP {name} is not text")))?;
    let uri: Uri = text
        .parse()
        .map_err(|error| invalid_proxy_request(format!("HTTP {name} is invalid: {error}")))?;
    let Some(authority) = uri.authority() else {
        return Ok(());
    };
    if !same_authority(authority, original) {
        return Ok(());
    }
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| invalid_proxy_request(format!("HTTP {name} has no scheme")))?;
    let path = uri.path_and_query().map_or("/", |path| path.as_str());
    let rewritten = format!("{scheme}://{forwarded}{path}");
    headers.insert(
        name.clone(),
        HeaderValue::from_str(&rewritten).map_err(|error| {
            invalid_proxy_request(format!("forwarded HTTP {name} is invalid: {error}"))
        })?,
    );
    Ok(())
}

fn rewrite_forwarded_response(
    headers: &mut HeaderMap,
    route: &ResolvedHttpRoute,
) -> Result<(), Report<NetworkProxyError>> {
    rewrite_response_uri_header(headers, header::LOCATION, route)?;
    make_response_cookies_host_only(headers)?;
    Ok(())
}

fn rewrite_response_uri_header(
    headers: &mut HeaderMap,
    name: header::HeaderName,
    route: &ResolvedHttpRoute,
) -> Result<(), Report<NetworkProxyError>> {
    let mut values = headers.get_all(&name).iter();
    let Some(value) = values.next() else {
        return Ok(());
    };
    if values.next().is_some() {
        return Err(forward_proxy_error(format!(
            "upstream returned multiple {name} headers"
        )));
    }
    let text = value
        .to_str()
        .map_err(|_| forward_proxy_error(format!("upstream {name} header is not text")))?;
    let Ok(uri) = text.parse::<Uri>() else {
        return Ok(());
    };
    let Some(authority) = uri.authority() else {
        return Ok(());
    };
    if !same_authority(authority, &route.forwarded_authority) {
        return Ok(());
    }
    let mut parts = uri.into_parts();
    parts.authority = Some(route.original_authority.clone());
    let rewritten = Uri::from_parts(parts).map_err(|error| {
        forward_proxy_error(format!("could not rewrite upstream {name}: {error}"))
    })?;
    headers.insert(
        name,
        HeaderValue::from_str(&rewritten.to_string()).map_err(|error| {
            forward_proxy_error(format!("rewritten upstream URI is invalid: {error}"))
        })?,
    );
    Ok(())
}

fn make_response_cookies_host_only(
    headers: &mut HeaderMap,
) -> Result<(), Report<NetworkProxyError>> {
    let cookies = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    if cookies.is_empty() {
        return Ok(());
    }
    headers.remove(header::SET_COOKIE);
    for cookie in cookies {
        let cookie = cookie
            .to_str()
            .map_err(|_| forward_proxy_error("upstream Set-Cookie header is not text"))?;
        let sanitized = cookie
            .split(';')
            .filter(|attribute| {
                !attribute
                    .split_once('=')
                    .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("domain"))
            })
            .collect::<Vec<_>>()
            .join(";");
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_str(&sanitized).map_err(|error| {
                forward_proxy_error(format!("could not make upstream cookie host-only: {error}"))
            })?,
        );
    }
    Ok(())
}

fn same_authority(left: &Authority, right: &Authority) -> bool {
    left.host().eq_ignore_ascii_case(right.host()) && left.port_u16() == right.port_u16()
}

async fn relay_upgrade(
    downstream: hyper::upgrade::OnUpgrade,
    upstream: hyper::upgrade::OnUpgrade,
) -> Result<(), Report<NetworkProxyError>> {
    let downstream = downstream.await.map_err(|error| {
        NetworkProxyError::Forward(format!(
            "failed to upgrade browser HTTP connection: {error}"
        ))
        .report()
    })?;
    let upstream = upstream.await.map_err(|error| {
        NetworkProxyError::Forward(format!("failed to upgrade pod HTTP connection: {error}"))
            .report()
    })?;
    let mut downstream = TokioIo::new(downstream);
    let mut upstream = TokioIo::new(upstream);
    copy_bidirectional(&mut downstream, &mut upstream)
        .await
        .map_err(|error| {
            NetworkProxyError::Forward(format!(
                "failed to relay upgraded routed HTTP connection: {error}"
            ))
            .report()
        })?;
    Ok(())
}

fn validate_hostname_suffix(suffix: &str) -> Result<(), Report<NetworkServiceError>> {
    if suffix.is_empty() || suffix.len() > 253 || suffix != suffix.to_ascii_lowercase() {
        return Err(invalid_configuration(
            "network hostname suffix must be 1-253 lowercase ASCII bytes",
        ));
    }
    for label in suffix.split('.') {
        if label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || !label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
        {
            return Err(invalid_configuration(
                "network hostname suffix contains an invalid DNS label",
            ));
        }
    }
    Ok(())
}

fn validate_host_port_host(host: &str) -> Result<(), Report<NetworkServiceError>> {
    if host.parse::<IpAddr>().is_ok() || validate_hostname_suffix(host).is_ok() {
        return Ok(());
    }
    Err(invalid_configuration(
        "static host-port target must be an IP address or lowercase DNS hostname",
    ))
}

fn reduce_http_routes(list: &mut api::HttpRouteList, mutation: &api::HttpRouteListMutation) {
    match mutation {
        api::HttpRouteListMutation::Upsert(route) => {
            if let Some(index) = list
                .http_routes
                .iter()
                .position(|entry| entry.id == route.id)
            {
                list.http_routes[index] = route.clone();
            } else {
                list.http_routes.push(route.clone());
                list.http_routes
                    .sort_by(|left, right| left.id.cmp(&right.id));
            }
        }
        api::HttpRouteListMutation::Remove(id) => {
            if let Some(index) = list.http_routes.iter().position(|entry| entry.id == *id) {
                list.http_routes.remove(index);
            }
        }
    }
}

fn reduce_port_forwards(list: &mut api::PortForwardList, mutation: &api::PortForwardListMutation) {
    match mutation {
        api::PortForwardListMutation::Upsert(route) => {
            if let Some(index) = list
                .port_forwards
                .iter()
                .position(|entry| entry.id == route.id)
            {
                list.port_forwards[index] = route.clone();
            } else {
                list.port_forwards.push(route.clone());
                list.port_forwards
                    .sort_by(|left, right| left.id.cmp(&right.id));
            }
        }
        api::PortForwardListMutation::Remove(id) => {
            if let Some(index) = list.port_forwards.iter().position(|entry| entry.id == *id) {
                list.port_forwards.remove(index);
            }
        }
    }
}

fn reduce_pod_host_forwards(
    list: &mut api::PodHostForwardList,
    mutation: &api::PodHostForwardListMutation,
) {
    match mutation {
        api::PodHostForwardListMutation::Upsert(forward) => {
            if let Some(index) = list
                .pod_host_forwards
                .iter()
                .position(|entry| entry.id == forward.id)
            {
                list.pod_host_forwards[index] = forward.clone();
            } else {
                list.pod_host_forwards.push(forward.clone());
                list.pod_host_forwards
                    .sort_by(|left, right| left.id.cmp(&right.id));
            }
        }
        api::PodHostForwardListMutation::Remove(id) => {
            if let Some(index) = list
                .pod_host_forwards
                .iter()
                .position(|entry| entry.id == *id)
            {
                list.pod_host_forwards.remove(index);
            }
        }
    }
}

fn invalid_request(message: impl Into<String>) -> Report<NetworkServiceError> {
    NetworkServiceError::InvalidRequest(message.into()).report()
}

fn invalid_configuration(message: impl Into<String>) -> Report<NetworkServiceError> {
    NetworkServiceError::InvalidConfiguration
        .report()
        .message(message.into())
}

fn unavailable(message: impl Into<String>) -> Report<NetworkServiceError> {
    NetworkServiceError::Unavailable(message.into()).report()
}

fn overloaded(message: impl Into<String>) -> Report<NetworkServiceError> {
    NetworkServiceError::Overloaded(message.into()).report()
}

fn internal(message: impl Into<String>) -> Report<NetworkServiceError> {
    NetworkServiceError::Internal(message.into()).report()
}

fn invalid_proxy_request(message: impl Into<String>) -> Report<NetworkProxyError> {
    NetworkProxyError::InvalidRequest(message.into()).report()
}

fn forward_proxy_error(message: impl Into<String>) -> Report<NetworkProxyError> {
    NetworkProxyError::Forward(message.into()).report()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(error) => {
            tracing::error!("Network service state mutex was poisoned");
            error.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use tascarrel_store::StoreEvent;
    use tempfile::tempdir;

    use super::*;

    fn workspace_service(directory: &Path) -> WorkspaceService {
        let workspace = WorkspaceName::new("demo").unwrap();
        WorkspaceService::new(crate::services::workspaces::WorkspaceServiceConfig {
            runtime_dir: directory.to_owned(),
            git: PathBuf::from("/usr/bin/git"),
            startup_timeout: Duration::from_secs(1),
            max_workspaces: 1,
            network_request_queue_capacity: crate::services::workspaces::WorkspaceServiceConfig::DEFAULT_NETWORK_REQUEST_QUEUE_CAPACITY,
            mux_initial_byte_window: crate::services::workspaces::WorkspaceServiceConfig::DEFAULT_MUX_INITIAL_BYTE_WINDOW,
            mux_max_channels: crate::services::workspaces::WorkspaceServiceConfig::DEFAULT_MUX_MAX_CHANNELS,
            mux_service_handshake_timeout: crate::services::workspaces::WorkspaceServiceConfig::DEFAULT_MUX_SERVICE_HANDSHAKE_TIMEOUT,
            max_concurrent_mux_services: crate::services::workspaces::WorkspaceServiceConfig::DEFAULT_MAX_CONCURRENT_MUX_SERVICES,
            mode: crate::services::workspaces::WorkspaceMode::External(
                crate::services::workspaces::ExternalWorkspaceConfig {
                    workspace,
                    guest_socket: directory.join("guest.sock"),
                    workspace_root: directory.join("workspace"),
                    workspace_state: directory.join("workspace-state"),
                },
            ),
        })
        .unwrap()
    }

    fn api_workspace() -> ApiWorkspaceName {
        ApiWorkspaceName::new("demo")
    }

    fn resolved_route(original: &str, forwarded: &str) -> ResolvedHttpRoute {
        ResolvedHttpRoute {
            target: PodPortTarget {
                workspace: api_workspace(),
                pod_id: tascarrel_api::types::pods::PodId::generate(),
                pod_port: 8080,
            },
            original_authority: original.parse().unwrap(),
            forwarded_authority: forwarded.parse().unwrap(),
        }
    }

    /// Verifies hostname mappings retain the latest name and evict the least
    /// recent address.
    #[test]
    fn dns_hostname_mappings_are_bounded_and_replace_names() {
        let first = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let second = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));
        let third = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 3));
        let mut mappings = DnsHostnameMappings::new(NonZeroUsize::new(2).unwrap());

        mappings.record("first.example", [first, second]);
        mappings.record("replacement.example", [first]);
        mappings.record("third.example", [third]);

        assert_eq!(mappings.get(first).as_deref(), Some("replacement.example"));
        assert_eq!(mappings.get(second), None);
        assert_eq!(mappings.get(third).as_deref(), Some("third.example"));
    }

    /// Exercises route creation, exclusive frontend trust, subscription, and
    /// deletion together.
    #[tokio::test]
    async fn http_route_api_is_idempotent_and_resumable() {
        let directory = tempdir().unwrap();
        let workspaces = workspace_service(directory.path());
        let network = NetworkService::new(NetworkServiceConfig::default()).unwrap();
        let pod_id = tascarrel_api::types::pods::PodId::generate();
        let action = api::CreateHttpRouteAction {
            workspace: api_workspace(),
            pod_id: pod_id.clone(),
            pod_port: 8080,
            title: "Development server".into(),
            internal: false,
        };

        let created = network
            .create_http_route(action.clone(), &workspaces)
            .unwrap();
        let repeated = network.create_http_route(action, &workspaces).unwrap();
        assert_eq!(created, repeated);
        assert!(created.hostname_prefix.as_str().starts_with("r-"));
        assert!(created.hostname_prefix.as_str().len() <= 63);
        assert!(
            created
                .hostname_prefix
                .as_str()
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::HOST,
            format!("inner.{}.tascarrel.localhost:8272", created.hostname_prefix)
                .parse()
                .unwrap(),
        );
        let resolved = network.resolve_http_route(&headers).unwrap().unwrap();
        assert_eq!(
            resolved.forwarded_authority.as_str(),
            "inner.tascarrel.localhost:8272"
        );

        let mut subscription = network
            .subscribe_http_routes(
                &api::HttpRouteListChangedSubscription {
                    workspace: api_workspace(),
                    cursor: None,
                },
                &workspaces,
            )
            .unwrap();
        let StoreEvent::Snapshot(snapshot) = subscription.recv().await.unwrap() else {
            panic!("initial route-list event was not a snapshot");
        };
        assert_eq!(snapshot.value.http_routes.len(), 1);
        assert_eq!(snapshot.value.http_routes[0].pod_id, pod_id);
        assert_eq!(snapshot.value.http_routes[0].title, "Development server");
        assert!(!snapshot.value.http_routes[0].trusted_tascarrel_frontend);

        let renamed = network
            .create_http_route(
                api::CreateHttpRouteAction {
                    workspace: api_workspace(),
                    pod_id: pod_id.clone(),
                    pod_port: 8080,
                    title: "Renamed server".into(),
                    internal: true,
                },
                &workspaces,
            )
            .unwrap();
        assert_eq!(created, renamed);
        let StoreEvent::Mutation(mutation) = subscription.recv().await.unwrap() else {
            panic!("route title update was not emitted as a mutation");
        };
        let api::HttpRouteListMutation::Upsert(route) = mutation.mutation.as_ref() else {
            panic!("route title update was not emitted as an upsert");
        };
        assert_eq!(route.title, "Renamed server");
        assert!(route.internal);

        network
            .set_http_route_trusted_tascarrel_frontend(
                &api::SetHttpRouteTrustedTascarrelFrontendAction {
                    http_route_id: created.http_route_id.clone(),
                    trusted_tascarrel_frontend: true,
                },
            )
            .unwrap();
        let StoreEvent::Mutation(mutation) = subscription.recv().await.unwrap() else {
            panic!("frontend trust update was not emitted as a mutation");
        };
        let api::HttpRouteListMutation::Upsert(route) = mutation.mutation.as_ref() else {
            panic!("frontend trust update was not emitted as an upsert");
        };
        assert!(route.trusted_tascarrel_frontend);

        let trusted_authority = format!(
            "{}.tascarrel.localhost:8272",
            created.hostname_prefix.as_str()
        )
        .parse()
        .unwrap();
        let nested_authority = format!(
            "nested.{}.tascarrel.localhost:8272",
            created.hostname_prefix.as_str()
        )
        .parse()
        .unwrap();
        let web_address = "127.0.0.1:8272".parse().unwrap();
        assert!(network.is_trusted_tascarrel_frontend_authority(&trusted_authority, web_address));
        assert!(!network.is_trusted_tascarrel_frontend_authority(&nested_authority, web_address));

        let replacement = network
            .create_http_route(
                api::CreateHttpRouteAction {
                    workspace: api_workspace(),
                    pod_id: pod_id.clone(),
                    pod_port: 8081,
                    title: "Replacement frontend".into(),
                    internal: false,
                },
                &workspaces,
            )
            .unwrap();
        let StoreEvent::Mutation(_) = subscription.recv().await.unwrap() else {
            panic!("replacement route creation was not emitted as a mutation");
        };
        network
            .set_http_route_trusted_tascarrel_frontend(
                &api::SetHttpRouteTrustedTascarrelFrontendAction {
                    http_route_id: replacement.http_route_id.clone(),
                    trusted_tascarrel_frontend: true,
                },
            )
            .unwrap();
        let StoreEvent::Mutation(revoked) = subscription.recv().await.unwrap() else {
            panic!("previous frontend trust revocation was not emitted as a mutation");
        };
        let api::HttpRouteListMutation::Upsert(revoked) = revoked.mutation.as_ref() else {
            panic!("previous frontend trust revocation was not emitted as an upsert");
        };
        assert_eq!(revoked.id, created.http_route_id);
        assert!(!revoked.trusted_tascarrel_frontend);
        let StoreEvent::Mutation(granted) = subscription.recv().await.unwrap() else {
            panic!("replacement frontend trust grant was not emitted as a mutation");
        };
        let api::HttpRouteListMutation::Upsert(granted) = granted.mutation.as_ref() else {
            panic!("replacement frontend trust grant was not emitted as an upsert");
        };
        assert_eq!(granted.id, replacement.http_route_id);
        assert!(granted.trusted_tascarrel_frontend);
        assert!(!network.is_trusted_tascarrel_frontend_authority(&trusted_authority, web_address));

        let replacement_authority = format!(
            "{}.tascarrel.localhost:8272",
            replacement.hostname_prefix.as_str()
        )
        .parse()
        .unwrap();
        assert!(
            network.is_trusted_tascarrel_frontend_authority(&replacement_authority, web_address)
        );
        network
            .delete_http_route(&api::DeleteHttpRouteAction {
                http_route_id: replacement.http_route_id.clone(),
            })
            .unwrap();
        let StoreEvent::Mutation(_) = subscription.recv().await.unwrap() else {
            panic!("replacement route deletion was not emitted as a mutation");
        };
        assert!(
            !network.is_trusted_tascarrel_frontend_authority(&replacement_authority, web_address)
        );

        network
            .delete_http_route(&api::DeleteHttpRouteAction {
                http_route_id: created.http_route_id.clone(),
            })
            .unwrap();
        let StoreEvent::Mutation(mutation) = subscription.recv().await.unwrap() else {
            panic!("route deletion was not emitted as a mutation");
        };
        assert_eq!(
            mutation.mutation.as_ref(),
            &api::HttpRouteListMutation::Remove(created.http_route_id)
        );
        workspaces.shutdown().await;
    }

    /// Verifies a dynamic listener is host-owned, observable with its assigned
    /// loopback port, and withdrawn through the delete API.
    #[tokio::test]
    async fn port_forward_api_owns_listener_and_observable_state() {
        let directory = tempdir().unwrap();
        let workspaces = workspace_service(directory.path());
        let network = NetworkService::new(NetworkServiceConfig::default()).unwrap();
        let pod_id = tascarrel_api::types::pods::PodId::generate();
        let created = network
            .create_port_forward(
                api::CreatePortForwardAction {
                    workspace: api_workspace(),
                    pod_id: pod_id.clone(),
                    pod_port: 3000,
                    title: Some("development server".into()),
                },
                &workspaces,
            )
            .await
            .unwrap();
        let mut subscription = network
            .subscribe_port_forwards(
                &api::PortForwardListChangedSubscription {
                    workspace: api_workspace(),
                    cursor: None,
                },
                &workspaces,
            )
            .unwrap();
        let StoreEvent::Snapshot(snapshot) = subscription.recv().await.unwrap() else {
            panic!("initial port-forward event was not a snapshot");
        };
        assert_eq!(snapshot.value.port_forwards.len(), 1);
        let forward = &snapshot.value.port_forwards[0];
        assert_eq!(forward.id, created.port_forward_id);
        assert_eq!(forward.pod_id, pod_id);
        assert_ne!(forward.host_port, 0);
        drop(
            TcpStream::connect((Ipv4Addr::LOCALHOST, forward.host_port))
                .await
                .unwrap(),
        );

        network
            .delete_port_forward(&api::DeletePortForwardAction {
                port_forward_id: created.port_forward_id.clone(),
            })
            .await
            .unwrap();
        let StoreEvent::Mutation(mutation) = subscription.recv().await.unwrap() else {
            panic!("port-forward deletion was not emitted as a mutation");
        };
        assert_eq!(
            mutation.mutation.as_ref(),
            &api::PortForwardListMutation::Remove(created.port_forward_id)
        );
        network.shutdown().await;
        workspaces.shutdown().await;
    }

    /// Verifies pod-to-host forwards are pod-scoped, resumable, idempotently
    /// updated by pod-visible port, and withdrawn through the delete API.
    #[tokio::test]
    async fn pod_host_forward_api_routes_attributed_virtual_ports() {
        let directory = tempdir().unwrap();
        let workspaces = workspace_service(directory.path());
        let network = NetworkService::new(NetworkServiceConfig::default()).unwrap();
        let workspace = api_workspace();
        let pod_id = tascarrel_api::types::pods::PodId::generate();
        let other_pod_id = tascarrel_api::types::pods::PodId::generate();
        let created = network
            .create_pod_host_forward(
                api::CreatePodHostForwardAction {
                    workspace: workspace.clone(),
                    pod_id: pod_id.clone(),
                    mapping: api::PortMapping::parse("5432:15432").unwrap(),
                    title: Some("database".into()),
                },
                &workspaces,
            )
            .unwrap();
        let mut subscription = network
            .subscribe_pod_host_forwards(
                &api::PodHostForwardListChangedSubscription {
                    workspace: workspace.clone(),
                    cursor: None,
                },
                &workspaces,
            )
            .unwrap();
        let StoreEvent::Snapshot(snapshot) = subscription.recv().await.unwrap() else {
            panic!("initial pod-to-host event was not a snapshot");
        };
        assert_eq!(snapshot.value.pod_host_forwards.len(), 1);
        assert_eq!(
            network.pod_host_forward_destination(
                &workspace,
                &api::NetworkRequestSource::Pod(pod_id.clone()),
                15432,
            ),
            Some((Ipv4Addr::LOCALHOST, 5432).into())
        );
        assert_eq!(
            network.pod_host_forward_destination(
                &workspace,
                &api::NetworkRequestSource::Pod(other_pod_id),
                15432,
            ),
            None
        );

        let updated = network
            .create_pod_host_forward(
                api::CreatePodHostForwardAction {
                    workspace: workspace.clone(),
                    pod_id: pod_id.clone(),
                    mapping: api::PortMapping::parse("6432:15432").unwrap(),
                    title: Some("updated database".into()),
                },
                &workspaces,
            )
            .unwrap();
        assert_eq!(updated.pod_host_forward_id, created.pod_host_forward_id);
        let StoreEvent::Mutation(mutation) = subscription.recv().await.unwrap() else {
            panic!("pod-to-host update was not emitted as a mutation");
        };
        assert!(matches!(
            mutation.mutation.as_ref(),
            api::PodHostForwardListMutation::Upsert(forward)
                if forward.mapping.as_str() == "6432:15432"
        ));
        assert_eq!(
            network.pod_host_forward_destination(
                &workspace,
                &api::NetworkRequestSource::Pod(pod_id),
                15432,
            ),
            Some((Ipv4Addr::LOCALHOST, 6432).into())
        );

        network
            .delete_pod_host_forward(&api::DeletePodHostForwardAction {
                pod_host_forward_id: created.pod_host_forward_id.clone(),
            })
            .unwrap();
        let StoreEvent::Mutation(mutation) = subscription.recv().await.unwrap() else {
            panic!("pod-to-host deletion was not emitted as a mutation");
        };
        assert_eq!(
            mutation.mutation.as_ref(),
            &api::PodHostForwardListMutation::Remove(created.pod_host_forward_id)
        );
        network.shutdown().await;
        workspaces.shutdown().await;
    }

    /// Verifies workspace destruction withdraws its routes, closes its
    /// subscriptions, and releases bound host listeners together.
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the test verifies every workspace-owned network resource in one lifecycle"
    )]
    async fn workspace_removal_withdraws_network_routes_and_listeners() {
        let directory = tempdir().unwrap();
        let workspaces = workspace_service(directory.path());
        let network = NetworkService::new(NetworkServiceConfig::default()).unwrap();
        let workspace = api_workspace();
        let pod_id = tascarrel_api::types::pods::PodId::generate();
        let route = network
            .create_http_route(
                api::CreateHttpRouteAction {
                    workspace: workspace.clone(),
                    pod_id: pod_id.clone(),
                    pod_port: 8080,
                    title: "web".into(),
                    internal: false,
                },
                &workspaces,
            )
            .unwrap();
        let forward = network
            .create_port_forward(
                api::CreatePortForwardAction {
                    workspace: workspace.clone(),
                    pod_id: pod_id.clone(),
                    pod_port: 3000,
                    title: Some("development server".into()),
                },
                &workspaces,
            )
            .await
            .unwrap();
        let pod_host_forward = network
            .create_pod_host_forward(
                api::CreatePodHostForwardAction {
                    workspace: workspace.clone(),
                    pod_id,
                    mapping: api::PortMapping::parse("5432:15432").unwrap(),
                    title: Some("database".into()),
                },
                &workspaces,
            )
            .unwrap();
        let mut route_events = network
            .subscribe_http_routes(
                &api::HttpRouteListChangedSubscription {
                    workspace: workspace.clone(),
                    cursor: None,
                },
                &workspaces,
            )
            .unwrap();
        let mut forward_events = network
            .subscribe_port_forwards(
                &api::PortForwardListChangedSubscription {
                    workspace: workspace.clone(),
                    cursor: None,
                },
                &workspaces,
            )
            .unwrap();
        let mut pod_host_forward_events = network
            .subscribe_pod_host_forwards(
                &api::PodHostForwardListChangedSubscription {
                    workspace: workspace.clone(),
                    cursor: None,
                },
                &workspaces,
            )
            .unwrap();
        let StoreEvent::Snapshot(route_snapshot) = route_events.recv().await.unwrap() else {
            panic!("initial route event was not a snapshot");
        };
        let StoreEvent::Snapshot(forward_snapshot) = forward_events.recv().await.unwrap() else {
            panic!("initial port-forward event was not a snapshot");
        };
        let StoreEvent::Snapshot(pod_host_forward_snapshot) =
            pod_host_forward_events.recv().await.unwrap()
        else {
            panic!("initial pod-to-host event was not a snapshot");
        };
        assert_eq!(route_snapshot.value.http_routes[0].id, route.http_route_id);
        assert_eq!(
            forward_snapshot.value.port_forwards[0].id,
            forward.port_forward_id
        );
        assert_eq!(
            pod_host_forward_snapshot.value.pod_host_forwards[0].id,
            pod_host_forward.pod_host_forward_id
        );
        let host_port = forward_snapshot.value.port_forwards[0].host_port;

        network.remove_workspace(&workspace).await;

        assert!(route_events.recv().await.is_none());
        assert!(forward_events.recv().await.is_none());
        assert!(pod_host_forward_events.recv().await.is_none());
        {
            let state = lock(&network.inner.state);
            assert!(state.http_routes.is_empty());
            assert!(state.port_forwards.is_empty());
            assert!(state.pod_host_forwards.is_empty());
            assert!(state.pod_host_forward_targets.is_empty());
            assert!(!state.workspaces.contains_key(&workspace));
        }
        assert!(
            TcpStream::connect((Ipv4Addr::LOCALHOST, host_port))
                .await
                .is_err()
        );
        network.shutdown().await;
        workspaces.shutdown().await;
    }

    /// Verifies a nested authority consumes only its rightmost route label.
    #[test]
    fn routed_authority_consumes_one_rightmost_prefix() {
        let authority = "inner.outer.tascarrel.localhost:8272".parse().unwrap();
        let (prefix, forwarded) = split_routed_authority(&authority, DEFAULT_HOSTNAME_SUFFIX)
            .unwrap()
            .unwrap();
        assert_eq!(prefix, "outer");
        assert_eq!(forwarded.as_str(), "inner.tascarrel.localhost:8272");
    }

    /// Verifies each proxy layer exposes the remaining route stack to the pod
    /// in both Host and same-origin Origin.
    #[test]
    fn request_rewrite_strips_one_route_layer() {
        let route = resolved_route(
            "inner.outer.tascarrel.localhost:8272",
            "inner.tascarrel.localhost:8272",
        );
        let mut request = Request::builder()
            .uri("/socket?mode=watch")
            .header(header::HOST, route.original_authority.as_str())
            .header(
                header::ORIGIN,
                "http://inner.outer.tascarrel.localhost:8272",
            )
            .header(
                header::REFERER,
                "http://inner.outer.tascarrel.localhost:8272/page?mode=edit",
            )
            .body(Body::empty())
            .unwrap();

        rewrite_forwarded_request(&mut request, &route, false).unwrap();

        assert_eq!(
            request.headers()[header::HOST],
            "inner.tascarrel.localhost:8272"
        );
        assert_eq!(
            request.headers()[header::ORIGIN],
            "http://inner.tascarrel.localhost:8272"
        );
        assert_eq!(
            request.headers()[header::REFERER],
            "http://inner.tascarrel.localhost:8272/page?mode=edit"
        );
        assert_eq!(request.uri().to_string(), "/socket?mode=watch");
    }

    /// Verifies absolute redirects return through every consumed layer and
    /// pod cookies cannot target hostd or sibling route origins.
    #[test]
    fn response_rewrite_restores_route_and_isolates_cookies() {
        let route = resolved_route(
            "inner.outer.tascarrel.localhost:8272",
            "inner.tascarrel.localhost:8272",
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::LOCATION,
            "http://inner.tascarrel.localhost:8272/login"
                .parse()
                .unwrap(),
        );
        headers.append(
            header::SET_COOKIE,
            "session=one; Domain=tascarrel.localhost; Path=/; HttpOnly"
                .parse()
                .unwrap(),
        );
        headers.append(
            header::SET_COOKIE,
            "preference=dark; SameSite=Lax".parse().unwrap(),
        );

        rewrite_forwarded_response(&mut headers, &route).unwrap();

        assert_eq!(
            headers[header::LOCATION],
            "http://inner.outer.tascarrel.localhost:8272/login"
        );
        let cookies = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|cookie| cookie.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0], "session=one; Path=/; HttpOnly");
        assert_eq!(cookies[1], "preference=dark; SameSite=Lax");
    }

    /// Verifies the canonical frontend authority is not treated as a route.
    #[test]
    fn base_authority_is_not_routed() {
        let authority = "tascarrel.localhost:8272".parse().unwrap();
        assert!(
            split_routed_authority(&authority, DEFAULT_HOSTNAME_SUFFIX)
                .unwrap()
                .is_none()
        );
    }

    /// Verifies configured hostname suffixes consist only of valid DNS labels.
    #[test]
    fn hostname_suffix_validation_rejects_ambiguous_values() {
        assert!(validate_hostname_suffix(DEFAULT_HOSTNAME_SUFFIX).is_ok());
        for invalid in [
            "",
            ".localhost",
            "Tascarrel.localhost",
            "tascarrel_.localhost",
        ] {
            assert!(validate_hostname_suffix(invalid).is_err());
        }
    }

    /// Verifies static host-port targets accept outer DNS names and IP
    /// addresses without accepting an embedded port or URL.
    #[test]
    fn host_port_host_validation_accepts_only_bare_hosts() {
        for valid in ["host.tascarrel.internal", "127.0.0.1", "::1"] {
            assert!(validate_host_port_host(valid).is_ok());
        }
        for invalid in ["", "http://outer", "outer:18080", "Outer.internal"] {
            assert!(validate_host_port_host(invalid).is_err());
        }
    }
}
