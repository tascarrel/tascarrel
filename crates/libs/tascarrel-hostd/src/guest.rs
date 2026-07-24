//! Typed host-side client interface to workspace guest daemons.
//!
//! [`GuestClient`] uses the workspace's persistent control-plane connection
//! through [`WorkspaceService`]. Its generic execution and subscription
//! methods accept only operation types registered as part of the guestd API.

use std::marker::PhantomData;

use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tascarrel_api::GuestAction;
use tascarrel_api::GuestSubscription;
use tascarrel_api::types::processes as api;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::workspaces::WorkspaceName as ApiWorkspaceName;
use tascarrel_protocol::RemoteError;
use tascarrel_protocol::WorkspaceName;
use tascarrel_protocol::control_plane::server;
use thiserror::Error;

use crate::HostControlService;
use crate::WorkspaceService;

/// Typed client for operations implemented by workspace guest daemons.
#[derive(Clone, Debug)]
pub struct GuestClient {
    workspace_service: WorkspaceService,
    host_control: HostControlService,
}

impl GuestClient {
    /// Creates a guestd client backed by `workspace_service`.
    #[must_use]
    pub fn new(workspace_service: WorkspaceService, host_control: HostControlService) -> Self {
        Self {
            workspace_service,
            host_control,
        }
    }

    /// Launches a supervised process in one workspace pod.
    ///
    /// # Errors
    ///
    /// Returns a typed workspace, transport, protocol, remote operation, or
    /// output error.
    pub async fn spawn(
        &self,
        workspace: WorkspaceName,
        context: wire::RequestContext,
        input: api::SpawnProcessAction,
    ) -> GuestResult<api::SpawnProcessOutput> {
        self.execute(workspace, context, input).await
    }

    /// Requests termination of one supervised process.
    ///
    /// # Errors
    ///
    /// Returns a typed workspace, transport, protocol, remote operation, or
    /// output error.
    pub async fn kill(
        &self,
        workspace: WorkspaceName,
        context: wire::RequestContext,
        input: api::KillProcessAction,
    ) -> GuestResult<api::KillProcessOutput> {
        self.execute(workspace, context, input).await
    }

    /// Removes one terminated process from guestd's retained process list.
    ///
    /// # Errors
    ///
    /// Returns a typed workspace, transport, protocol, remote operation, or
    /// output error.
    pub async fn remove(
        &self,
        workspace: WorkspaceName,
        context: wire::RequestContext,
        input: api::RemoveProcessAction,
    ) -> GuestResult<api::RemoveProcessOutput> {
        self.execute(workspace, context, input).await
    }

    /// Captures the current emulated screen of one terminal process.
    ///
    /// # Errors
    ///
    /// Returns a typed workspace, transport, protocol, remote operation, or
    /// output error.
    pub async fn snapshot_terminal(
        &self,
        workspace: WorkspaceName,
        context: wire::RequestContext,
        input: api::SnapshotProcessTerminalAction,
    ) -> GuestResult<api::SnapshotProcessTerminalOutput> {
        self.execute(workspace, context, input).await
    }

    /// Subscribes to the resumable workspace-wide process list.
    ///
    /// # Errors
    ///
    /// Returns a typed workspace, transport, protocol, or serialization error.
    pub async fn subscribe_process_list(
        &self,
        workspace: WorkspaceName,
        context: wire::RequestContext,
        input: api::ProcessListChangedSubscription,
    ) -> GuestResult<GuestEventStream<api::ProcessListChangedEvent>> {
        self.subscribe(workspace, context, input).await
    }

    /// Subscribes to retained and live sanitized lines for one process.
    ///
    /// # Errors
    ///
    /// Returns a typed workspace, transport, protocol, or serialization error.
    pub async fn subscribe_log(
        &self,
        workspace: WorkspaceName,
        context: wire::RequestContext,
        input: api::ProcessLogSubscription,
    ) -> GuestResult<GuestEventStream<api::ProcessLogEvent>> {
        self.subscribe(workspace, context, input).await
    }

    /// Executes one action registered as part of the guestd API.
    ///
    /// The action type determines both its procedure name and output type.
    ///
    /// # Errors
    ///
    /// Returns a typed workspace, transport, protocol, remote operation, or
    /// output error.
    #[tracing::instrument(level = "debug", skip_all, fields(workspace = %workspace, procedure = A::NAME))]
    pub async fn execute<A>(
        &self,
        workspace: WorkspaceName,
        context: wire::RequestContext,
        input: A,
    ) -> GuestResult<A::Output>
    where
        A: GuestAction,
    {
        let target = workspace_target(&workspace);
        let input = encode(input)?;
        let peer = self.connect(&workspace).await?;
        let mut rpc = peer
            .invoke(wire::RpcInvocation {
                id: wire::InvocationId::generate(),
                target,
                context: Some(context),
                procedure: A::NAME.into(),
                input,
                timeout_ms: None,
            })
            .await
            .escalate(GuestClientError::ControlPlane)?;
        let Some(message) = rpc.recv().await else {
            return Err(GuestClientError::ConnectionClosed.report());
        };
        match message {
            wire::RpcMessage::Completed(completed) => decode(completed.output),
            wire::RpcMessage::Failed(failed) => {
                Err(GuestClientError::Remote(failed.error).report())
            }
            wire::RpcMessage::Canceled(_) => Err(GuestClientError::Canceled.report()),
            wire::RpcMessage::Invoke(_) | wire::RpcMessage::Cancel(_) => {
                Err(GuestClientError::InvalidResponse.report())
            }
        }
    }

    /// Opens one subscription registered as part of the guestd API.
    ///
    /// The subscription type determines both its protocol name and event
    /// type.
    ///
    /// # Errors
    ///
    /// Returns a typed workspace, transport, protocol, remote operation, or
    /// event decoding error.
    #[tracing::instrument(level = "debug", skip_all, fields(workspace = %workspace, subscription = S::NAME))]
    pub async fn subscribe<S>(
        &self,
        workspace: WorkspaceName,
        context: wire::RequestContext,
        input: S,
    ) -> GuestResult<GuestEventStream<S::Event>>
    where
        S: GuestSubscription,
    {
        let target = workspace_target(&workspace);
        let input = encode(input)?;
        let peer = self.connect(&workspace).await?;
        let subscription = peer
            .subscribe(wire::SubscriptionStart {
                id: wire::SubscriptionId::generate(),
                target,
                context: Some(context),
                subscription: S::NAME.into(),
                input,
            })
            .await
            .escalate(GuestClientError::ControlPlane)?;
        Ok(GuestEventStream {
            subscription,
            completed: false,
            marker: PhantomData,
        })
    }

    async fn connect(&self, workspace: &WorkspaceName) -> GuestResult<server::Peer> {
        self.workspace_service
            .control_plane(workspace.clone(), self.host_control.clone())
            .await
            .map_err(|error| GuestClientError::Workspace(error).report())
    }
}

impl WorkspaceService {
    /// Returns a typed client for operations implemented by guestd.
    #[must_use]
    pub fn guestd(&self, host_control: HostControlService) -> GuestClient {
        GuestClient::new(self.clone(), host_control)
    }
}

/// Credit-driven stream of typed guestd subscription events.
pub struct GuestEventStream<E> {
    subscription: server::Subscription,
    completed: bool,
    marker: PhantomData<fn() -> E>,
}

impl<E: DeserializeOwned> GuestEventStream<E> {
    /// Receives one event, granting exactly one unit of producer credit.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, protocol, remote operation, or event decoding
    /// error.
    pub async fn recv(&mut self) -> GuestResult<Option<E>> {
        if self.completed {
            return Ok(None);
        }
        self.subscription
            .grant_credit(1)
            .await
            .escalate(GuestClientError::ControlPlane)?;
        let Some(message) = self.subscription.recv().await else {
            return Err(GuestClientError::ConnectionClosed.report());
        };
        match message {
            wire::SubscriptionMessage::Event(event) => decode(event.event).map(Some),
            wire::SubscriptionMessage::Completed(_) => {
                self.completed = true;
                Ok(None)
            }
            wire::SubscriptionMessage::Failed(failed) => {
                self.completed = true;
                Err(GuestClientError::Remote(failed.error).report())
            }
            wire::SubscriptionMessage::Subscribe(_)
            | wire::SubscriptionMessage::GrantCredit(_)
            | wire::SubscriptionMessage::Unsubscribe(_) => {
                Err(GuestClientError::InvalidResponse.report())
            }
        }
    }

    /// Stops the subscription and waits for guestd to confirm completion.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, protocol, or remote operation error.
    pub async fn close(mut self) -> GuestResult<()> {
        if self.completed {
            return Ok(());
        }
        self.subscription
            .stop()
            .await
            .escalate(GuestClientError::ControlPlane)?;
        let Some(message) = self.subscription.recv().await else {
            return Err(GuestClientError::ConnectionClosed.report());
        };
        match message {
            wire::SubscriptionMessage::Completed(_) => Ok(()),
            wire::SubscriptionMessage::Failed(failed) => {
                Err(GuestClientError::Remote(failed.error).report())
            }
            wire::SubscriptionMessage::Event(_)
            | wire::SubscriptionMessage::Subscribe(_)
            | wire::SubscriptionMessage::GrantCredit(_)
            | wire::SubscriptionMessage::Unsubscribe(_) => {
                Err(GuestClientError::InvalidResponse.report())
            }
        }
    }
}

impl<E> std::fmt::Debug for GuestEventStream<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuestEventStream")
            .field("id", self.subscription.id())
            .finish_non_exhaustive()
    }
}

/// Host-side guestd client failure carried by [`Report`].
#[derive(Debug, Error)]
pub enum GuestClientError {
    /// The workspace could not be validated, started, or connected.
    #[error("workspace guest daemon is unavailable: {0}")]
    Workspace(RemoteError),
    /// The shared control-plane implementation failed.
    #[error("workspace guest control-plane link failed")]
    ControlPlane,
    /// The guest closed the connection before completing the operation.
    #[error("workspace guest control-plane connection closed")]
    ConnectionClosed,
    /// Guestd returned a non-domain operation failure.
    #[error("guest operation failed")]
    Remote(wire::OperationError),
    /// Guestd canceled the RPC invocation.
    #[error("guest RPC was canceled")]
    Canceled,
    /// Guestd sent a message inconsistent with the requested operation.
    #[error("guest daemon returned an unexpected response")]
    InvalidResponse,
    /// A typed operation input could not be encoded.
    #[error("could not encode typed guest operation input")]
    InvalidInput,
    /// A typed operation output or event could not be decoded.
    #[error("could not decode typed guest operation output")]
    InvalidOutput,
}

impl GuestClientError {
    /// Returns the peer-supplied operation error when this is a remote failure.
    #[must_use]
    pub fn operation_error(&self) -> Option<&wire::OperationError> {
        match self {
            Self::Remote(error) => Some(error),
            _ => None,
        }
    }
}

/// Result returned by the typed guestd client.
pub type GuestResult<T> = Result<T, Report<GuestClientError>>;

fn workspace_target(workspace: &WorkspaceName) -> wire::Address {
    wire::Address::Workspace(wire::WorkspaceAddress {
        workspace: ApiWorkspaceName::new(workspace.as_str()),
    })
}

fn encode<T: Serialize>(value: T) -> GuestResult<serde_json::Value> {
    serde_json::to_value(value).map_err(|error| {
        GuestClientError::InvalidInput
            .report()
            .message(error.to_string())
    })
}

fn decode<T: DeserializeOwned>(value: serde_json::Value) -> GuestResult<T> {
    serde_json::from_value(value).map_err(|error| {
        GuestClientError::InvalidOutput
            .report()
            .message(error.to_string())
    })
}
