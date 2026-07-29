//! Typed guest-side client for operations owned by hostd.
//!
//! [`HostClient`] executes registered host actions and opens registered host
//! subscriptions over the full-duplex control-plane connection.

use std::marker::PhantomData;
use std::sync::Arc;

use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tascarrel_api::HostAction;
use tascarrel_api::HostSubscription;
use tascarrel_api::types::protocol as wire;
use tascarrel_protocol::control_plane::server;
use thiserror::Error;
use tokio::sync::RwLock;

/// Reusable typed client carried by one full-duplex hostd connection.
#[derive(Clone)]
pub(crate) struct HostClient {
    peer: Arc<RwLock<Option<server::Peer>>>,
}

impl HostClient {
    /// Creates a client that is attached before its connection is driven.
    #[must_use]
    pub(crate) fn pending() -> Self {
        Self {
            peer: Arc::new(RwLock::new(None)),
        }
    }

    /// Attaches the currently active host control-plane peer.
    pub(crate) async fn attach(&self, peer: server::Peer) {
        *self.peer.write().await = Some(peer);
    }

    /// Detaches the host peer when its connection terminates.
    pub(crate) async fn detach(&self) {
        *self.peer.write().await = None;
    }

    /// Returns the currently active host peer.
    pub(crate) async fn peer(&self) -> Result<server::Peer, Report<HostClientError>> {
        self.peer
            .read()
            .await
            .clone()
            .ok_or_else(|| HostClientError::Unavailable.report())
    }

    /// Executes one action implemented by hostd.
    pub(crate) async fn execute<A>(
        &self,
        context: wire::RequestContext,
        input: A,
    ) -> Result<A::Output, Report<HostClientError>>
    where
        A: HostAction,
        A: Serialize,
        A::Output: DeserializeOwned,
    {
        let input = serde_json::to_value(input)
            .map_err(|error| error.escalate(HostClientError::InvalidInput))?;
        let peer = self.peer().await?;
        let mut rpc = peer
            .invoke(wire::RpcInvocation {
                id: wire::InvocationId::generate(),
                target: wire::Address::Host,
                context: Some(context),
                procedure: A::NAME.into(),
                input,
                timeout_ms: None,
            })
            .await
            .escalate(HostClientError::ControlPlane)?;
        let Some(message) = rpc.recv().await else {
            return Err(HostClientError::ConnectionClosed.report());
        };
        match message {
            wire::RpcMessage::Completed(completed) => serde_json::from_value(completed.output)
                .map_err(|error| error.escalate(HostClientError::InvalidOutput)),
            wire::RpcMessage::Failed(failed) => Err(HostClientError::Remote(failed.error).report()),
            wire::RpcMessage::Canceled(_) => Err(HostClientError::Canceled.report()),
            wire::RpcMessage::Invoke(_) | wire::RpcMessage::Cancel(_) => {
                Err(HostClientError::InvalidResponse.report())
            }
        }
    }

    /// Opens one typed subscription implemented by hostd.
    pub(crate) async fn subscribe<S>(
        &self,
        context: wire::RequestContext,
        input: S,
    ) -> Result<HostEventStream<S::Event>, Report<HostClientError>>
    where
        S: HostSubscription,
    {
        let input = serde_json::to_value(input)
            .map_err(|error| error.escalate(HostClientError::InvalidInput))?;
        let peer = self.peer().await?;
        let subscription = peer
            .subscribe(wire::SubscriptionStart {
                id: wire::SubscriptionId::generate(),
                target: wire::Address::Host,
                context: Some(context),
                subscription: S::NAME.into(),
                input,
            })
            .await
            .escalate(HostClientError::ControlPlane)?;
        Ok(HostEventStream {
            subscription,
            completed: false,
            marker: PhantomData,
        })
    }
}

/// Credit-driven stream of typed hostd subscription events.
pub(crate) struct HostEventStream<E> {
    subscription: server::Subscription,
    completed: bool,
    marker: PhantomData<fn() -> E>,
}

impl<E: DeserializeOwned> HostEventStream<E> {
    /// Receives one event from hostd.
    pub(crate) async fn recv(&mut self) -> Result<Option<E>, Report<HostClientError>> {
        if self.completed {
            return Ok(None);
        }
        self.subscription
            .grant_credit(1)
            .await
            .escalate(HostClientError::ControlPlane)?;
        let Some(message) = self.subscription.recv().await else {
            return Err(HostClientError::ConnectionClosed.report());
        };
        match message {
            wire::SubscriptionMessage::Event(event) => serde_json::from_value(event.event)
                .map(Some)
                .map_err(|error| error.escalate(HostClientError::InvalidOutput)),
            wire::SubscriptionMessage::Completed(_) => {
                self.completed = true;
                Ok(None)
            }
            wire::SubscriptionMessage::Failed(failed) => {
                self.completed = true;
                Err(HostClientError::Remote(failed.error).report())
            }
            wire::SubscriptionMessage::Subscribe(_)
            | wire::SubscriptionMessage::GrantCredit(_)
            | wire::SubscriptionMessage::Unsubscribe(_) => {
                Err(HostClientError::InvalidResponse.report())
            }
        }
    }
}

/// Failures returned by the guest-side host operation client.
#[derive(Debug, Error)]
pub(crate) enum HostClientError {
    /// No active host peer is attached.
    #[error("host operation client is unavailable")]
    Unavailable,
    /// A typed operation could not be encoded.
    #[error("host operation input is invalid")]
    InvalidInput,
    /// A typed operation result or event could not be decoded.
    #[error("host operation output is invalid")]
    InvalidOutput,
    /// The host returned a peer-visible operation failure.
    #[error("host operation failed: {0}")]
    Remote(wire::OperationError),
    /// The control-plane operation protocol returned an unexpected message.
    #[error("host operation returned an invalid response")]
    InvalidResponse,
    /// The control-plane link closed before the operation completed.
    #[error("host operation connection closed")]
    ConnectionClosed,
    /// The host action was canceled.
    #[error("host action was canceled")]
    Canceled,
    /// The control-plane transport failed.
    #[error("host action control plane failed")]
    ControlPlane,
}
