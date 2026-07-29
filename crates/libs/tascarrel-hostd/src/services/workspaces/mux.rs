//! Workspace-scoped host services carried by the private guest mux.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::workspaces::WorkspaceName;
use tascarrel_mux::Channel;
use tascarrel_mux::Incoming;
use tascarrel_mux::IncomingRequest;
use tascarrel_protocol::Framed;
use tascarrel_protocol::MAX_WORKSPACE_SHARES_FRAME_LEN;
use tascarrel_protocol::MUX_CA_HOST_ENDPOINT;
use tascarrel_protocol::MUX_GIT_HOST_ENDPOINT;
use tascarrel_protocol::MUX_HOST_OPERATION_INPUT_ENDPOINT;
use tascarrel_protocol::MUX_WORKSPACE_ENVIRONMENT_HOST_ENDPOINT;
use tascarrel_protocol::MUX_WORKSPACE_HOST_ENDPOINT;
use tascarrel_protocol::MUX_WORKSPACE_SHARES_HOST_ENDPOINT;
use tascarrel_protocol::WorkspaceHostSharesResponse;
use tascarrel_protocol::network::MUX_NETWORK_DNS_ENDPOINT;
use tascarrel_protocol::network::MUX_NETWORK_TCP_ENDPOINT;
use tascarrel_protocol::workspace_snapshot;
use thiserror::Error;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::debug;
use tracing::warn;

use crate::HostRepositoryManager;
use crate::WorkspaceAuthority;
use crate::services::host_operations::WorkspaceHostOperationInputRequest;
use crate::services::network::NetworkPolicy;
use crate::services::network::NetworkPolicySource;

/// Sends accepted network channels to the host network service.
pub(crate) type WorkspaceNetworkRequestSender = mpsc::Sender<WorkspaceNetworkRequest>;
/// Receives accepted network channels for the host network service.
pub(crate) type WorkspaceNetworkRequests = mpsc::Receiver<WorkspaceNetworkRequest>;
/// Sends accepted environment channels to the host secrets service.
pub(crate) type WorkspaceEnvironmentRequestSender = mpsc::Sender<WorkspaceEnvironmentRequest>;
/// Receives accepted environment channels for the host secrets service.
pub(crate) type WorkspaceEnvironmentRequests = mpsc::Receiver<WorkspaceEnvironmentRequest>;
/// Sends authenticated host-operation input channels to their durable service.
pub(crate) type WorkspaceHostOperationInputRequestSender =
    mpsc::Sender<WorkspaceHostOperationInputRequest>;
/// Receives authenticated host-operation input channels.
pub(crate) type WorkspaceHostOperationInputRequests =
    mpsc::Receiver<WorkspaceHostOperationInputRequest>;

/// One authenticated guest request for its host-resolved startup environment.
#[derive(Debug)]
pub(crate) struct WorkspaceEnvironmentRequest {
    /// Workspace identity established by the mux host.
    pub workspace: WorkspaceName,
    /// Accepted response channel.
    pub channel: Channel,
    /// Maximum time allowed for the peer-confirmed channel close.
    pub close_timeout: Duration,
}

#[derive(Debug)]
pub(crate) enum WorkspaceNetworkRequest {
    Dns {
        workspace: WorkspaceName,
        channel: Channel,
    },
    Tcp(Box<WorkspaceTcpNetworkRequest>),
}

#[derive(Debug)]
pub(crate) struct WorkspaceTcpNetworkRequest {
    pub workspace: WorkspaceName,
    pub policy: Arc<NetworkPolicy>,
    pub authority: Option<Arc<WorkspaceAuthority>>,
    pub channel: Channel,
}

#[derive(Debug)]
pub(crate) struct WorkspaceMuxHostConfig {
    pub workspace: WorkspaceName,
    pub network_requests: WorkspaceNetworkRequestSender,
    pub environment_requests: WorkspaceEnvironmentRequestSender,
    pub host_operation_input_requests: WorkspaceHostOperationInputRequestSender,
    pub policy: NetworkPolicySource,
    pub authority: Option<Arc<WorkspaceAuthority>>,
    pub repositories: Option<Arc<HostRepositoryManager>>,
    pub workspace_root: PathBuf,
    pub workspace_snapshot_dir: PathBuf,
    pub host_shares: WorkspaceHostSharesResponse,
    pub handshake_timeout: Duration,
    pub max_concurrent_services: usize,
}

#[derive(Debug, Error)]
pub(crate) enum WorkspaceMuxHostError {
    #[error("guest workspace-service request stream closed")]
    IncomingClosed,
    #[error("invalid workspace-service configuration")]
    InvalidConfig,
}

#[derive(Debug, Error)]
enum WorkspaceMuxServiceError {
    #[error("workspace mux service failed: {0}")]
    Failed(String),
}

#[derive(Debug)]
pub(crate) struct WorkspaceMuxHost {
    incoming: Incoming,
    config: WorkspaceMuxHostConfig,
}

impl WorkspaceMuxHost {
    pub(crate) const fn new(incoming: Incoming, config: WorkspaceMuxHostConfig) -> Self {
        Self { incoming, config }
    }

    pub(crate) async fn run(mut self) -> Result<(), Report<WorkspaceMuxHostError>> {
        if self.config.max_concurrent_services == 0 || self.config.handshake_timeout.is_zero() {
            return Err(WorkspaceMuxHostError::InvalidConfig.report());
        }
        let mut services = JoinSet::new();
        loop {
            tokio::select! {
                request = self.incoming.recv() => {
                    let Some(request) = request else {
                        return Err(WorkspaceMuxHostError::IncomingClosed.report());
                    };
                    self.accept(request, &mut services);
                }
                Some(result) = services.join_next(), if !services.is_empty() => {
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => debug!(%error, "guest workspace service closed"),
                        Err(error) => warn!(%error, "guest workspace service task failed"),
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)] // Endpoint dispatch remains a single bounded routing table.
    fn accept(
        &self,
        request: IncomingRequest,
        services: &mut JoinSet<Result<(), Report<WorkspaceMuxServiceError>>>,
    ) {
        if services.len() >= self.config.max_concurrent_services {
            reject_request(request, b"workspace service capacity exhausted");
            return;
        }
        let endpoint = request.endpoint();
        if endpoint == MUX_NETWORK_DNS_ENDPOINT {
            let Ok(permit) = self.config.network_requests.try_reserve() else {
                reject_request(request, b"host network request capacity exhausted");
                return;
            };
            let Ok(channel) = request.accept() else {
                return;
            };
            permit.send(WorkspaceNetworkRequest::Dns {
                workspace: self.config.workspace.clone(),
                channel,
            });
        } else if endpoint == MUX_NETWORK_TCP_ENDPOINT {
            let Ok(permit) = self.config.network_requests.try_reserve() else {
                reject_request(request, b"host network request capacity exhausted");
                return;
            };
            let Ok(channel) = request.accept() else {
                return;
            };
            permit.send(WorkspaceNetworkRequest::Tcp(Box::new(
                WorkspaceTcpNetworkRequest {
                    workspace: self.config.workspace.clone(),
                    policy: self.config.policy.current(),
                    authority: self.config.authority.clone(),
                    channel,
                },
            )));
        } else if endpoint == MUX_CA_HOST_ENDPOINT {
            let Some(authority) = self.config.authority.clone() else {
                reject_request(request, b"workspace HTTPS interception is disabled");
                return;
            };
            let Ok(channel) = request.accept() else {
                return;
            };
            let drain_timeout = self.config.handshake_timeout;
            services.spawn(async move { serve_ca(channel, authority, drain_timeout).await });
        } else if endpoint == MUX_WORKSPACE_ENVIRONMENT_HOST_ENDPOINT {
            let Ok(permit) = self.config.environment_requests.try_reserve() else {
                reject_request(request, b"host environment request capacity exhausted");
                return;
            };
            let Ok(channel) = request.accept() else {
                return;
            };
            permit.send(WorkspaceEnvironmentRequest {
                workspace: self.config.workspace.clone(),
                channel,
                close_timeout: self.config.handshake_timeout,
            });
        } else if endpoint == MUX_GIT_HOST_ENDPOINT {
            let Some(repositories) = self.config.repositories.clone() else {
                reject_request(request, b"repository service unavailable");
                return;
            };
            let Ok(channel) = request.accept() else {
                return;
            };
            services.spawn(async move {
                repositories
                    .serve_upload_pack(channel)
                    .await
                    .map_err(|error| service_error(format!("repository service failed: {error:?}")))
            });
        } else if endpoint == MUX_HOST_OPERATION_INPUT_ENDPOINT {
            let Ok(permit) = self.config.host_operation_input_requests.try_reserve() else {
                reject_request(request, b"host operation input capacity exhausted");
                return;
            };
            let channel = match request.accept() {
                Ok(channel) => channel,
                Err(error) => {
                    warn!(?error, "failed to accept host operation input channel");
                    return;
                }
            };
            permit.send(WorkspaceHostOperationInputRequest {
                workspace: self.config.workspace.clone(),
                channel,
            });
        } else if endpoint == MUX_WORKSPACE_HOST_ENDPOINT {
            let Ok(channel) = request.accept() else {
                return;
            };
            let root = self.config.workspace_root.clone();
            let snapshot_dir = self.config.workspace_snapshot_dir.clone();
            let drain_timeout = self.config.handshake_timeout;
            services.spawn(async move {
                serve_workspace_snapshot(channel, root, snapshot_dir, drain_timeout).await
            });
        } else if endpoint == MUX_WORKSPACE_SHARES_HOST_ENDPOINT {
            let Ok(channel) = request.accept() else {
                return;
            };
            let host_shares = self.config.host_shares.clone();
            let drain_timeout = self.config.handshake_timeout;
            services.spawn(async move {
                serve_workspace_host_shares(channel, host_shares, drain_timeout).await
            });
        } else {
            reject_request(request, b"unknown host endpoint");
        }
    }
}

async fn serve_workspace_host_shares(
    channel: Channel,
    host_shares: WorkspaceHostSharesResponse,
    drain_timeout: Duration,
) -> Result<(), Report<WorkspaceMuxServiceError>> {
    let mut framed = Framed::with_max_frame_len(channel, MAX_WORKSPACE_SHARES_FRAME_LEN)
        .map_err(service_error)?;
    framed.write(&host_shares).await.map_err(service_error)?;
    let mut channel = framed.into_inner();
    channel.shutdown().await.map_err(service_error)?;
    drain_channel(&mut channel, drain_timeout).await;
    Ok(())
}

async fn serve_ca(
    mut channel: Channel,
    authority: Arc<WorkspaceAuthority>,
    drain_timeout: Duration,
) -> Result<(), Report<WorkspaceMuxServiceError>> {
    channel
        .write_all(&authority.certificate_pem())
        .await
        .map_err(service_error)?;
    channel.shutdown().await.map_err(service_error)?;
    drain_channel(&mut channel, drain_timeout).await;
    Ok(())
}

async fn serve_workspace_snapshot(
    mut channel: Channel,
    root: PathBuf,
    snapshot_dir: PathBuf,
    drain_timeout: Duration,
) -> Result<(), Report<WorkspaceMuxServiceError>> {
    let archive = snapshot_dir.join(format!("workspace-input-{}.tar", uuid::Uuid::new_v4()));
    let build_path = archive.clone();
    let workspace_root = root.clone();
    match tokio::task::spawn_blocking(move || {
        workspace_snapshot::create_snapshot(&root, &build_path)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!(
                root = %workspace_root.display(),
                path = %archive.display(),
                %error,
                "failed to create workspace snapshot"
            );
            return Err(service_error(format!("workspace snapshot failed: {error}")));
        }
        Err(error) => {
            warn!(
                root = %workspace_root.display(),
                path = %archive.display(),
                %error,
                "workspace snapshot task failed"
            );
            return Err(service_error(format!(
                "workspace snapshot task failed: {error}"
            )));
        }
    }
    let result = async {
        let mut file = tokio::fs::File::open(&archive).await?;
        tokio::io::copy(&mut file, &mut channel).await?;
        channel.shutdown().await?;
        drain_channel(&mut channel, drain_timeout).await;
        Ok::<(), io::Error>(())
    }
    .await;
    if let Err(error) = tokio::fs::remove_file(&archive).await {
        debug!(%error, path = %archive.display(), "failed to remove workspace snapshot archive");
    }
    result.map_err(|error| service_error(format!("workspace snapshot transport failed: {error}")))
}

async fn drain_channel(channel: &mut Channel, drain_timeout: Duration) {
    let drain = async {
        let mut buffer = [0_u8; 1024];
        loop {
            if channel.read(&mut buffer).await? == 0 {
                return Ok::<(), io::Error>(());
            }
        }
    };
    match timeout(drain_timeout, drain).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => debug!(%error, "failed to drain workspace service channel"),
        Err(_) => debug!("timed out draining workspace service channel"),
    }
}

fn reject_request(request: IncomingRequest, reason: &[u8]) {
    if let Err(error) = request.reject(reason) {
        debug!(%error, "failed to reject workspace service request");
    }
}

fn service_error(error: impl std::fmt::Display) -> Report<WorkspaceMuxServiceError> {
    WorkspaceMuxServiceError::Failed(error.to_string()).report()
}
