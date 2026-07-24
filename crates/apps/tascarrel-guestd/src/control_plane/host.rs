//! Typed guest-side client for actions owned by hostd.

use std::sync::Arc;

use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tascarrel_api::HostAction;
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
}

/// Failures returned by the guest-side host action client.
#[derive(Debug, Error)]
pub(crate) enum HostClientError {
    /// No active host peer is attached.
    #[error("host action client is unavailable")]
    Unavailable,
    /// A typed action could not be encoded.
    #[error("host action input is invalid")]
    InvalidInput,
    /// A typed action result could not be decoded.
    #[error("host action output is invalid")]
    InvalidOutput,
    /// The host returned a peer-visible operation failure.
    #[error("host action failed: {0}")]
    Remote(wire::OperationError),
    /// The control-plane operation protocol returned an unexpected message.
    #[error("host action returned an invalid response")]
    InvalidResponse,
    /// The control-plane link closed before the action completed.
    #[error("host action connection closed")]
    ConnectionClosed,
    /// The host action was canceled.
    #[error("host action was canceled")]
    Canceled,
    /// The control-plane transport failed.
    #[error("host action control plane failed")]
    ControlPlane,
}
