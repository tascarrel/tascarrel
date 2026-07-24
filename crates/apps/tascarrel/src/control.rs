//! Typed client for the host daemon's local control-plane socket.
//!
//! The CLI uses the same Sidex actions and subscriptions as the browser UI.
//! The Unix socket authenticates the local client while operation targets
//! select host-owned resources.

use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use reportify::Report;
use tascarrel_api::HostAction;
use tascarrel_api::HostSubscription;
use tascarrel_api::types::protocol as wire;
use tascarrel_protocol::control_plane;
use tascarrel_protocol::control_plane::StreamTransport;
use tascarrel_protocol::control_plane::policy::DenyAll;
use tascarrel_protocol::control_plane::server;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

/// Typed control-plane client connected to one running host daemon.
pub(crate) struct ControlClient {
    peer: server::Peer,
    connection: JoinHandle<control_plane::Result<()>>,
}

impl ControlClient {
    /// Connects to the host daemon's local control-plane socket.
    pub(crate) async fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("cannot connect to host daemon at {}", path.display()))?;
        let server = server::Server::new(RejectService, RejectRouter);
        let (peer, connection) = server
            .connect(
                StreamTransport::new(stream),
                DenyAll,
                control_plane::Config::default(),
            )
            .map_err(|error| anyhow!(error.to_string()))?;
        Ok(Self {
            peer,
            connection: tokio::spawn(connection),
        })
    }

    /// Invokes one host-owned typed action.
    pub(crate) async fn invoke<A>(&self, input: A) -> Result<A::Output>
    where
        A: HostAction,
    {
        let input = serde_json::to_value(input).context("encode control-plane action")?;
        let mut rpc = self
            .peer
            .invoke(wire::RpcInvocation {
                id: wire::InvocationId::generate(),
                target: wire::Address::Host,
                context: None,
                procedure: A::NAME.into(),
                input,
                timeout_ms: None,
            })
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        match rpc.recv().await {
            Some(wire::RpcMessage::Completed(completed)) => {
                serde_json::from_value(completed.output).context("decode control-plane output")
            }
            Some(wire::RpcMessage::Failed(failed)) => Err(anyhow!(failed.error.to_string())),
            Some(wire::RpcMessage::Canceled(_)) => {
                Err(anyhow!("control-plane action was canceled"))
            }
            Some(wire::RpcMessage::Invoke(_) | wire::RpcMessage::Cancel(_)) => {
                Err(anyhow!("host daemon returned an invalid action response"))
            }
            None => Err(anyhow!(
                "host daemon closed the control plane before replying"
            )),
        }
    }

    /// Reads the initial event of one host-owned typed subscription.
    pub(crate) async fn first_event<S>(&self, input: S) -> Result<S::Event>
    where
        S: HostSubscription,
    {
        let input = serde_json::to_value(input).context("encode control-plane subscription")?;
        let mut subscription = self
            .peer
            .subscribe(wire::SubscriptionStart {
                id: wire::SubscriptionId::generate(),
                target: wire::Address::Host,
                context: None,
                subscription: S::NAME.into(),
                input,
            })
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        subscription
            .grant_credit(1)
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        match subscription.recv().await {
            Some(wire::SubscriptionMessage::Event(event)) => {
                serde_json::from_value(event.event).context("decode control-plane event")
            }
            Some(wire::SubscriptionMessage::Failed(failed)) => {
                Err(anyhow!(failed.error.to_string()))
            }
            Some(wire::SubscriptionMessage::Completed(_)) => Err(anyhow!(
                "control-plane subscription completed before its initial event"
            )),
            Some(
                wire::SubscriptionMessage::Subscribe(_)
                | wire::SubscriptionMessage::GrantCredit(_)
                | wire::SubscriptionMessage::Unsubscribe(_),
            ) => Err(anyhow!(
                "host daemon returned an invalid subscription response"
            )),
            None => Err(anyhow!(
                "host daemon closed the control plane before sending subscription state"
            )),
        }
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        self.connection.abort();
    }
}

#[derive(Clone, Copy)]
struct RejectService;

impl server::Service for RejectService {
    fn invoke(
        &self,
        _invocation: wire::RpcInvocation,
    ) -> server::OperationFuture<'static, serde_json::Value> {
        Box::pin(async { Err(forbidden()) })
    }

    fn subscribe(
        &self,
        _subscription: wire::SubscriptionStart,
    ) -> server::OperationFuture<'static, Box<dyn server::EventSource>> {
        Box::pin(async { Err(forbidden()) })
    }
}

#[derive(Clone, Copy)]
struct RejectRouter;

impl server::Router for RejectRouter {
    fn resolve(&self, _target: wire::Address) -> server::OperationFuture<'static, server::Route> {
        Box::pin(async { Err(forbidden()) })
    }
}

fn forbidden() -> Report<wire::OperationError> {
    wire::OperationError::forbidden()
}
