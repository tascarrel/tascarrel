//! Guest control-plane connection lifecycle and typed local dispatch.

use std::future::Future;
use std::future::pending;
use std::sync::Arc;
use std::sync::OnceLock;

use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use serde::de::DeserializeOwned;
use tascarrel_api::types::changes;
use tascarrel_api::types::chats;
use tascarrel_api::types::code;
use tascarrel_api::types::files;
use tascarrel_api::types::guest;
use tascarrel_api::types::images;
use tascarrel_api::types::pods;
use tascarrel_api::types::processes;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::workspaces::WorkspaceName;
use tascarrel_protocol::control_plane;
use tascarrel_protocol::control_plane::StreamTransport;
use tascarrel_protocol::control_plane::policy::topology;
use tascarrel_protocol::control_plane::server;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

use super::GuestState;
use super::HostClient;
use super::InvocationCtx;
use super::SubscriptionCtx;
use super::operations::EventSource;
use super::operations::ExecuteAction;
use super::operations::OpenSubscription;

/// Serves typed guest RPCs and subscriptions on the authenticated hostd link.
pub struct GuestControlService {
    state: GuestState,
    host: HostClient,
    workspace: Arc<OnceLock<WorkspaceName>>,
    config: GuestControlServiceConfig,
}

impl Clone for GuestControlService {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            host: self.host.clone(),
            workspace: Arc::clone(&self.workspace),
            config: self.config,
        }
    }
}

impl GuestControlService {
    /// Creates a control service for one guest state.
    #[must_use]
    pub fn new(state: GuestState) -> Self {
        Self::with_config(state, GuestControlServiceConfig::default())
    }

    /// Creates a service with explicit link and operation settings.
    #[must_use]
    pub fn with_config(state: GuestState, config: GuestControlServiceConfig) -> Self {
        Self {
            state,
            host: HostClient::pending(),
            workspace: Arc::new(OnceLock::new()),
            config,
        }
    }

    /// Serves the hostd control-plane connection carried by this guest mux.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the shared control-plane server cannot be
    /// configured or driven.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn serve_host_connection<T>(
        &self,
        stream: T,
        workspace: WorkspaceName,
    ) -> Result<(), Report<GuestControlServiceError>>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        self.serve_host_connection_until_shutdown(stream, workspace, pending())
            .await
    }

    /// Serves one hostd connection until the link exits or `shutdown`
    /// resolves.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the shared control-plane server cannot be
    /// configured or driven.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn serve_host_connection_until_shutdown<T, S>(
        &self,
        stream: T,
        workspace: WorkspaceName,
        shutdown: S,
    ) -> Result<(), Report<GuestControlServiceError>>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        S: Future<Output = ()> + Send + 'static,
    {
        if self
            .workspace
            .set(workspace.clone())
            .is_err_and(|existing| existing != workspace)
        {
            return Err(GuestControlServiceError::WorkspaceChanged.report());
        }
        let server = self.server();
        let (peer, connection) = server
            .connect_until_shutdown(
                StreamTransport::new(stream),
                topology::hostd_to_guestd(),
                self.config.control,
                shutdown,
            )
            .escalate(GuestControlServiceError::ControlPlane)?;
        self.host.attach(peer).await;
        let result = connection
            .await
            .escalate(GuestControlServiceError::ControlPlane);
        self.host.detach().await;
        result
    }

    /// Serves one control-plane connection authenticated as `pod_id`.
    ///
    /// # Errors
    ///
    /// Returns a typed error if hostd has not assigned the workspace identity
    /// or the control-plane connection fails.
    #[tracing::instrument(level = "debug", skip_all, fields(pod_id = %pod_id.0), err)]
    pub async fn serve_pod_connection<T>(
        &self,
        stream: T,
        pod_id: pods::PodId,
    ) -> Result<(), Report<GuestControlServiceError>>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let workspace = self
            .workspace
            .get()
            .cloned()
            .ok_or_else(|| GuestControlServiceError::WorkspaceUnavailable.report())?;
        let result = self
            .server()
            .serve(
                StreamTransport::new(stream),
                topology::podctl_to_guestd(&workspace, &pod_id),
                self.config.control,
            )
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(report)
                if matches!(
                    report.error(),
                    control_plane::Error::Transport | control_plane::Error::ConnectionClosed
                ) =>
            {
                Ok(())
            }
            Err(report) => Err(report.escalate(GuestControlServiceError::ControlPlane)),
        }
    }

    /// Returns the identity sent to an authenticated pod client.
    ///
    /// # Errors
    ///
    /// Returns a typed error until hostd assigns the workspace identity.
    pub fn pod_identity(
        &self,
        pod_id: pods::PodId,
    ) -> Result<tascarrel_protocol::PodControlIdentity, Report<GuestControlServiceError>> {
        let workspace = self
            .workspace
            .get()
            .cloned()
            .ok_or_else(|| GuestControlServiceError::WorkspaceUnavailable.report())?;
        Ok(tascarrel_protocol::PodControlIdentity { workspace, pod_id })
    }

    /// Builds one full-duplex server sharing the current host forwarding peer.
    fn server(&self) -> server::Server {
        server::Server::with_config(
            GuestOperations {
                state: self.state.clone(),
                host: self.host.clone(),
            },
            GuestRouter {
                host: self.host.clone(),
            },
            self.config.server,
        )
    }
}

/// Failure while serving a guest control-plane link.
#[derive(Clone, Copy, Debug, Error)]
pub enum GuestControlServiceError {
    /// The shared control-plane implementation failed.
    #[error("guest control-plane link failed")]
    ControlPlane,
    /// Hostd attempted to change the identity of a running guest.
    #[error("guest control-plane workspace identity changed")]
    WorkspaceChanged,
    /// A pod connected before hostd assigned the workspace identity.
    #[error("guest control-plane workspace identity is unavailable")]
    WorkspaceUnavailable,
}

/// Resource limits for one guest control-plane connection.
#[derive(Clone, Copy, Debug, Default)]
pub struct GuestControlServiceConfig {
    /// Shared control-plane driver queue settings.
    pub control: control_plane::Config,
    /// Shared operation server limits, forwarding, and shutdown settings.
    pub server: server::Config,
}

/// Dispatches locally owned operations into the composed guest state.
#[derive(Clone)]
struct GuestOperations {
    state: GuestState,
    host: HostClient,
}

impl server::Service for GuestOperations {
    fn invoke(
        &self,
        invocation: wire::RpcInvocation,
    ) -> server::OperationFuture<'static, serde_json::Value> {
        let state = self.state.clone();
        let host = self.host.clone();
        Box::pin(async move { execute_rpc(&state, &host, invocation).await })
    }

    fn subscribe(
        &self,
        start: wire::SubscriptionStart,
    ) -> server::OperationFuture<'static, Box<dyn server::EventSource>> {
        let state = self.state.clone();
        Box::pin(async move { open_subscription(&state, &start).await })
    }
}

/// Resolves non-host operations to this guest daemon.
struct GuestRouter {
    host: HostClient,
}

impl server::Router for GuestRouter {
    fn resolve(&self, target: wire::Address) -> server::OperationFuture<'static, server::Route> {
        let host = self.host.clone();
        Box::pin(async move {
            match target {
                wire::Address::Workspace(_) | wire::Address::Pod(_) => Ok(server::Route::Local),
                wire::Address::Host => host
                    .peer()
                    .await
                    .map(server::Route::Forward)
                    .map_err(|error| unavailable_error(error.to_string())),
            }
        })
    }
}

/// Creates a peer-visible failure for an unavailable forwarding destination.
fn unavailable_error(message: impl Into<String>) -> Report<wire::OperationError> {
    wire::OperationError::Unavailable(operation_error_details(message)).report()
}

/// Encodes events received from one typed source.
struct TypedEventSource<S>(S);

impl<S> server::EventSource for TypedEventSource<S>
where
    S: EventSource,
{
    fn recv(&mut self) -> server::OperationFuture<'_, Option<serde_json::Value>> {
        Box::pin(async move {
            match self.0.recv().await? {
                Some(event) => serde_json::to_value(event).map(Some).map_err(|error| {
                    error.report().escalate(wire::OperationError::Internal(
                        operation_error_details("could not encode subscription event"),
                    ))
                }),
                None => Ok(None),
            }
        })
    }
}

macro_rules! define_action_dispatch {
    ($(($name:literal, $action:path, $output:path),)*) => {
        /// Selects and executes one registered guestd action.
        async fn execute_rpc(
            state: &GuestState,
            host: &HostClient,
            invocation: wire::RpcInvocation,
        ) -> server::OperationResult<serde_json::Value> {
            match invocation.procedure.as_ref() {
                $(
                    $name => execute_action::<$action>(state, host, &invocation).await,
                )*
                _ => Err(invalid_request("unknown guest procedure")),
            }
        }
    };
}

tascarrel_api::with_guestd_operations!(actions => define_action_dispatch);

macro_rules! define_subscription_dispatch {
    ($(($name:literal, $subscription:path, $event:path),)*) => {
        /// Selects and opens one registered guestd subscription.
        async fn open_subscription(
            state: &GuestState,
            start: &wire::SubscriptionStart,
        ) -> server::OperationResult<Box<dyn server::EventSource>> {
            match start.subscription.as_ref() {
                $(
                    $name => open_typed_subscription::<$subscription>(state, start).await,
                )*
                _ => Err(invalid_request("unknown guest subscription")),
            }
        }
    };
}

tascarrel_api::with_guestd_operations!(subscriptions => define_subscription_dispatch);

/// Authorizes, decodes, and executes one selected action type.
async fn execute_action<A>(
    state: &GuestState,
    host: &HostClient,
    invocation: &wire::RpcInvocation,
) -> server::OperationResult<serde_json::Value>
where
    A: ExecuteAction,
{
    let action = decode::<A>(invocation.input.clone())?;
    let context = InvocationCtx::new(state, host, invocation);
    action.check_permissions(&context)?;
    let output = action.execute(context).await?;
    serde_json::to_value(output).map_err(|error| {
        error
            .report()
            .escalate(wire::OperationError::Internal(operation_error_details(
                "could not encode typed action output",
            )))
    })
}

/// Authorizes, decodes, and opens one selected subscription type.
async fn open_typed_subscription<S>(
    state: &GuestState,
    start: &wire::SubscriptionStart,
) -> server::OperationResult<Box<dyn server::EventSource>>
where
    S: OpenSubscription,
{
    let subscription = decode::<S>(start.input.clone())?;
    let context = SubscriptionCtx::new(state, start);
    subscription.check_permissions(&context)?;
    let source = subscription.open(context).await?;
    Ok(Box::new(TypedEventSource(source)))
}

/// Decodes one schema-typed operation input.
fn decode<T: DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, Report<wire::OperationError>> {
    serde_json::from_value(value)
        .map_err(|error| invalid_request(format!("invalid typed operation input: {error}")))
}

/// Creates a contract operation error.
fn invalid_request(message: impl Into<String>) -> Report<wire::OperationError> {
    wire::OperationError::InvalidRequest(operation_error_details(message)).report()
}

/// Creates operation details for a standalone diagnostic message.
fn operation_error_details(message: impl Into<String>) -> wire::OperationErrorDetails {
    wire::OperationErrorDetails {
        message: message.into().into(),
        report: None,
    }
}
