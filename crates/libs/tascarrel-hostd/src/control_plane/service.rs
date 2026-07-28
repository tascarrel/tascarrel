//! Host control-plane connection lifecycle, typed dispatch, and guest routing.

use std::future::Future;
use std::future::pending;
use std::sync::Arc;

use reportify::ErrorExt as _;
use reportify::Report;
use serde::de::DeserializeOwned;
use tascarrel_api::ArcStr;
use tascarrel_api::types::auth;
use tascarrel_api::types::config;
use tascarrel_api::types::network;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::repositories;
use tascarrel_api::types::secrets;
use tascarrel_api::types::workspaces;
use tascarrel_protocol::WorkspaceName;
use tascarrel_protocol::control_plane;
use tascarrel_protocol::control_plane::Transport;
use tascarrel_protocol::control_plane::policy::topology;
use tascarrel_protocol::control_plane::server;

use super::ConnectionPrincipal;
use super::HostState;
use super::InvocationCtx;
use super::SubscriptionCtx;
use super::invalid_request;
use super::operation_error_details;
use super::operations::EventSource;
use super::operations::ExecuteAction;
use super::operations::OpenSubscription;

/// Terminates authenticated client links and workspace peer connections.
#[derive(Clone)]
pub struct HostControlService {
    inner: Arc<HostControlServiceInner>,
}

impl std::fmt::Debug for HostControlService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostControlService")
            .finish_non_exhaustive()
    }
}

struct HostControlServiceInner {
    state: HostState,
}

impl HostControlService {
    /// Creates a control service for one host state.
    #[must_use]
    pub fn new(state: HostState) -> Self {
        Self {
            inner: Arc::new(HostControlServiceInner { state }),
        }
    }

    /// Serves one authenticated client transport until either side closes it.
    pub(crate) async fn serve<T>(
        &self,
        transport: T,
        client_id: wire::ClientId,
    ) -> control_plane::Result<()>
    where
        T: Transport + 'static,
    {
        self.serve_until_shutdown(
            transport,
            client_id,
            ConnectionPrincipal::LocalAdmin,
            pending(),
        )
        .await
    }

    /// Serves one browser transport authenticated by `session_id`.
    pub(crate) async fn serve_browser_until_shutdown<T, S>(
        &self,
        transport: T,
        client_id: wire::ClientId,
        session: crate::services::auth::AuthenticatedSession,
        shutdown: S,
    ) -> control_plane::Result<()>
    where
        T: Transport + 'static,
        S: Future<Output = ()> + Send + 'static,
    {
        self.serve_until_shutdown(
            transport,
            client_id,
            ConnectionPrincipal::Browser {
                session_id: session.id,
                origin: session.origin,
            },
            shutdown,
        )
        .await
    }

    /// Serves one authenticated client until the link exits or `shutdown`
    /// resolves.
    #[tracing::instrument(level = "debug", skip_all, fields(?client_id))]
    pub(crate) async fn serve_until_shutdown<T, S>(
        &self,
        transport: T,
        client_id: wire::ClientId,
        principal: ConnectionPrincipal,
        shutdown: S,
    ) -> control_plane::Result<()>
    where
        T: Transport + 'static,
        S: Future<Output = ()> + Send + 'static,
    {
        self.server(principal)
            .serve_until_shutdown(
                transport,
                topology::client_to_hostd(&client_id),
                control_plane::Config::default(),
                shutdown,
            )
            .await
    }

    /// Connects one authenticated, reusable guestd peer for `workspace`.
    ///
    /// # Errors
    ///
    /// Returns a control-plane configuration failure.
    pub(crate) fn connect_guest<T>(
        &self,
        transport: T,
        workspace: &WorkspaceName,
    ) -> control_plane::Result<(server::Peer, server::Connection)>
    where
        T: Transport + 'static,
    {
        let workspace = tascarrel_api::types::workspaces::WorkspaceName::new(workspace.as_str());
        self.server(ConnectionPrincipal::Internal).connect(
            transport,
            topology::guestd_to_hostd(&workspace),
            control_plane::Config::default(),
        )
    }

    /// Creates an operation server for one adjacent control-plane link.
    fn server(&self, principal: ConnectionPrincipal) -> server::Server {
        server::Server::new(
            HostOperations {
                state: self.inner.state.clone(),
                principal,
            },
            HostRouter {
                state: self.inner.state.clone(),
                control: self.clone(),
            },
        )
    }
}

/// Dispatches locally owned operations into the composed host state.
#[derive(Clone)]
struct HostOperations {
    state: HostState,
    principal: ConnectionPrincipal,
}

impl server::Service for HostOperations {
    fn invoke(
        &self,
        invocation: wire::RpcInvocation,
    ) -> server::OperationFuture<'static, serde_json::Value> {
        let state = self.state.clone();
        let principal = self.principal.clone();
        Box::pin(async move { execute_rpc(&state, &principal, invocation).await })
    }

    fn subscribe(
        &self,
        start: wire::SubscriptionStart,
    ) -> server::OperationFuture<'static, Box<dyn server::EventSource>> {
        let state = self.state.clone();
        let principal = self.principal.clone();
        Box::pin(async move { open_subscription(&state, &principal, &start).await })
    }
}

macro_rules! define_action_dispatch {
    ($(($name:literal, $action:path, $output:path),)*) => {
        /// Selects and executes one registered hostd action.
        async fn execute_rpc(
            state: &HostState,
            principal: &ConnectionPrincipal,
            invocation: wire::RpcInvocation,
        ) -> server::OperationResult<serde_json::Value> {
            match invocation.procedure.as_ref() {
                $(
                    $name => execute_action::<$action>(state, principal, &invocation).await,
                )*
                _ => Err(invalid_request("unknown host procedure")),
            }
        }
    };
}

tascarrel_api::with_hostd_operations!(actions => define_action_dispatch);

macro_rules! define_subscription_dispatch {
    ($(($name:literal, $subscription:path, $event:path),)*) => {
        /// Selects and opens one registered hostd subscription.
        async fn open_subscription(
            state: &HostState,
            principal: &ConnectionPrincipal,
            start: &wire::SubscriptionStart,
        ) -> server::OperationResult<Box<dyn server::EventSource>> {
            match start.subscription.as_ref() {
                $(
                    $name => open_typed_subscription::<$subscription>(state, principal, start).await,
                )*
                _ => Err(invalid_request("unknown host subscription")),
            }
        }
    };
}

tascarrel_api::with_hostd_operations!(subscriptions => define_subscription_dispatch);

/// Authorizes, decodes, and executes one selected action type.
async fn execute_action<A>(
    state: &HostState,
    principal: &ConnectionPrincipal,
    invocation: &wire::RpcInvocation,
) -> server::OperationResult<serde_json::Value>
where
    A: ExecuteAction,
{
    let action = decode_typed::<A>(invocation.input.clone())?;
    let context = InvocationCtx::new(state, invocation, principal);
    action.check_permissions(&context).await?;
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
    state: &HostState,
    principal: &ConnectionPrincipal,
    start: &wire::SubscriptionStart,
) -> server::OperationResult<Box<dyn server::EventSource>>
where
    S: OpenSubscription,
{
    let subscription = decode_typed::<S>(start.input.clone())?;
    let context = SubscriptionCtx::new(state, start, principal);
    subscription.check_permissions(&context).await?;
    let source = subscription.open(context).await?;
    Ok(Box::new(TypedEventSource(source)))
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

/// Resolves host targets locally and workspace targets through guestd.
struct HostRouter {
    state: HostState,
    control: HostControlService,
}

impl server::Router for HostRouter {
    fn resolve(&self, target: wire::Address) -> server::OperationFuture<'static, server::Route> {
        if target == wire::Address::Host {
            return Box::pin(async { Ok(server::Route::Local) });
        }
        let state = self.state.clone();
        let control = self.control.clone();
        Box::pin(async move {
            let workspace = target_workspace(&target)?;
            let peer = state
                .workspaces()
                .control_plane(workspace, control)
                .await
                .map_err(|error| unavailable_error(error.to_string()))?;
            Ok(server::Route::Forward(peer))
        })
    }
}

/// Decodes one schema-typed operation input.
///
/// # Errors
///
/// Returns an invalid-request operation error when the wire value does not
/// satisfy the selected Sidex type.
pub(crate) fn decode_typed<T: DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, Report<wire::OperationError>> {
    serde_json::from_value(value)
        .map_err(|error| invalid_request(format!("invalid typed operation input: {error}")))
}

fn target_workspace(target: &wire::Address) -> Result<WorkspaceName, Report<wire::OperationError>> {
    let name = match target {
        wire::Address::Host => {
            return Err(invalid_request(
                "host operations cannot be forwarded to guestd",
            ));
        }
        wire::Address::Workspace(address) => address.workspace.clone(),
        wire::Address::Pod(address) => address.workspace.clone(),
    };
    let name: ArcStr = name.into();
    WorkspaceName::new(name.as_ref())
        .map_err(|_| invalid_request("target contains an invalid workspace name"))
}

fn unavailable_error(message: impl Into<String>) -> Report<wire::OperationError> {
    wire::OperationError::Unavailable(operation_error_details(message)).report()
}
