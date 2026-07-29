//! Typed pod-local control-plane client over the shared guestd multiplexer.
//!
//! [`PodClient`] binds every request to the identity guestd assigns to the
//! pod-private socket and exposes typed action and initial-snapshot helpers.

use std::path::Path;

use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use tascarrel_api::Action;
use tascarrel_api::Subscription;
use tascarrel_api::types::protocol as wire;
use tascarrel_mux::MuxHandle;
use tascarrel_protocol::Framed;
use tascarrel_protocol::MUX_CONTROL_PLANE_ENDPOINT;
use tascarrel_protocol::PodControlIdentity;
use tascarrel_protocol::control_plane;
use tascarrel_protocol::control_plane::StreamTransport;
use tascarrel_protocol::control_plane::policy::DenyAll;
use tascarrel_protocol::control_plane::server;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::error::PodctlError;
use crate::error::PodctlResult;

/// Typed client bound to the identity assigned by one pod-private listener.
pub(crate) struct PodClient {
    identity: PodControlIdentity,
    peer: server::Peer,
    mux: MuxHandle,
    _incoming: tascarrel_mux::Incoming,
    mux_task: JoinHandle<tascarrel_mux::Result<()>>,
    control_task: JoinHandle<control_plane::Result<()>>,
}

impl PodClient {
    /// Connects the pod socket and receives its trusted identity handshake.
    #[tracing::instrument(level = "debug", skip_all, err)]
    pub(crate) async fn connect(path: &Path) -> PodctlResult<Self> {
        let stream = UnixStream::connect(path)
            .await
            .escalate(PodctlError::ConnectControlSocket)?;
        let (driver, mux, incoming) = tascarrel_mux::connect(
            stream,
            tascarrel_mux::Role::Client,
            tascarrel_mux::Config::default(),
        )
        .map_err(|error| error.escalate(PodctlError::Multiplexer))?;
        let mux_task = tokio::spawn(driver.run());
        let channel = mux
            .open(MUX_CONTROL_PLANE_ENDPOINT)
            .await
            .map_err(|error| error.escalate(PodctlError::Multiplexer))?;
        let mut framed = Framed::new(channel);
        let identity = framed
            .read::<PodControlIdentity>()
            .await
            .map_err(|error| error.escalate(PodctlError::ControlPlane))?
            .ok_or_else(|| PodctlError::IdentityUnavailable.report())?;
        let server = server::Server::new(RejectService, RejectRouter);
        let (peer, connection) = server
            .connect(
                StreamTransport::new(framed.into_inner()),
                DenyAll,
                control_plane::Config::default(),
            )
            .map_err(|error| error.escalate(PodctlError::ControlPlane))?;
        let control_task = tokio::spawn(connection);
        Ok(Self {
            identity,
            peer,
            mux,
            _incoming: incoming,
            mux_task,
            control_task,
        })
    }

    /// Returns the identity assigned by guestd for this socket.
    pub(crate) const fn identity(&self) -> &PodControlIdentity {
        &self.identity
    }

    /// Invokes one action addressed to this authenticated pod.
    pub(crate) async fn invoke_pod<A>(&self, input: A) -> PodctlResult<A::Output>
    where
        A: Action,
    {
        self.invoke(self.pod_target(), input).await
    }

    /// Invokes one host-owned action while preserving the authenticated pod
    /// actor.
    pub(crate) async fn invoke_host<A>(&self, input: A) -> PodctlResult<A::Output>
    where
        A: Action,
    {
        self.invoke(wire::Address::Host, input).await
    }

    /// Reads pod events until `receive` produces the requested result.
    pub(crate) async fn pod_events_until<S, T>(
        &self,
        input: S,
        mut receive: impl FnMut(S::Event) -> PodctlResult<Option<T>>,
    ) -> PodctlResult<T>
    where
        S: Subscription,
    {
        self.events_until(self.pod_target(), input, &mut receive)
            .await
    }

    /// Reads the first event of a host-owned pod-scoped subscription.
    pub(crate) async fn first_host_event<S>(&self, input: S) -> PodctlResult<S::Event>
    where
        S: Subscription,
    {
        self.first_event(wire::Address::Host, input).await
    }

    /// Opens one pod-private streaming data-plane channel.
    pub(crate) async fn open_channel(
        &self,
        endpoint: &str,
    ) -> PodctlResult<tascarrel_mux::Channel> {
        self.mux
            .open(endpoint)
            .await
            .map_err(|error| error.escalate(PodctlError::Multiplexer))
    }

    /// Invokes one typed action at an explicitly selected daemon target.
    #[tracing::instrument(level = "debug", skip_all, err)]
    async fn invoke<A>(&self, target: wire::Address, input: A) -> PodctlResult<A::Output>
    where
        A: Action,
    {
        let input = serde_json::to_value(input).escalate(PodctlError::InvalidControlInput)?;
        let mut rpc = self
            .peer
            .invoke(wire::RpcInvocation {
                id: wire::InvocationId::generate(),
                target,
                context: None,
                procedure: A::NAME.into(),
                input,
                timeout_ms: None,
            })
            .await
            .map_err(|error| error.escalate(PodctlError::ControlPlane))?;
        match rpc.recv().await {
            Some(wire::RpcMessage::Completed(completed)) => {
                serde_json::from_value(completed.output).escalate(PodctlError::InvalidControlOutput)
            }
            Some(wire::RpcMessage::Failed(failed)) => {
                Err(PodctlError::RemoteOperation(failed.error).report())
            }
            Some(wire::RpcMessage::Canceled(_)) => Err(PodctlError::ActionCanceled.report()),
            Some(wire::RpcMessage::Invoke(_) | wire::RpcMessage::Cancel(_)) => {
                Err(PodctlError::InvalidControlResponse.report())
            }
            None => Err(PodctlError::ControlConnectionClosed.report()),
        }
    }

    /// Opens one typed subscription and consumes its initial event.
    #[tracing::instrument(level = "debug", skip_all, err)]
    async fn first_event<S>(&self, target: wire::Address, input: S) -> PodctlResult<S::Event>
    where
        S: Subscription,
    {
        self.events_until(target, input, &mut |event| Ok(Some(event)))
            .await
    }

    /// Opens one typed subscription and consumes events until `receive`
    /// produces a result.
    #[tracing::instrument(level = "debug", skip_all, err)]
    async fn events_until<S, T>(
        &self,
        target: wire::Address,
        input: S,
        receive: &mut impl FnMut(S::Event) -> PodctlResult<Option<T>>,
    ) -> PodctlResult<T>
    where
        S: Subscription,
    {
        let input = serde_json::to_value(input).escalate(PodctlError::InvalidControlInput)?;
        let mut subscription = self
            .peer
            .subscribe(wire::SubscriptionStart {
                id: wire::SubscriptionId::generate(),
                target,
                context: None,
                subscription: S::NAME.into(),
                input,
            })
            .await
            .map_err(|error| error.escalate(PodctlError::ControlPlane))?;
        loop {
            subscription
                .grant_credit(1)
                .await
                .map_err(|error| error.escalate(PodctlError::ControlPlane))?;
            match subscription.recv().await {
                Some(wire::SubscriptionMessage::Event(event)) => {
                    let event = serde_json::from_value(event.event)
                        .escalate(PodctlError::InvalidControlOutput)?;
                    if let Some(output) = receive(event)? {
                        return Ok(output);
                    }
                }
                Some(wire::SubscriptionMessage::Failed(failed)) => {
                    return Err(PodctlError::RemoteOperation(failed.error).report());
                }
                Some(wire::SubscriptionMessage::Completed(_)) => {
                    return Err(PodctlError::SubscriptionCompleted.report());
                }
                Some(
                    wire::SubscriptionMessage::Subscribe(_)
                    | wire::SubscriptionMessage::GrantCredit(_)
                    | wire::SubscriptionMessage::Unsubscribe(_),
                ) => return Err(PodctlError::InvalidControlResponse.report()),
                None => return Err(PodctlError::ControlConnectionClosed.report()),
            }
        }
    }

    /// Builds the only pod target this client is authorized to address.
    fn pod_target(&self) -> wire::Address {
        wire::Address::Pod(wire::PodAddress {
            workspace: self.identity.workspace.clone(),
            pod_id: self.identity.pod_id.clone(),
        })
    }
}

impl Drop for PodClient {
    fn drop(&mut self) {
        self.control_task.abort();
        self.mux_task.abort();
    }
}

/// Rejects daemon-initiated operations on the command's client connection.
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

/// Rejects forwarding requests received by the command's client connection.
#[derive(Clone, Copy)]
struct RejectRouter;

impl server::Router for RejectRouter {
    fn resolve(&self, _target: wire::Address) -> server::OperationFuture<'static, server::Route> {
        Box::pin(async { Err(forbidden()) })
    }
}

/// Creates the response for a forbidden daemon-initiated operation.
fn forbidden() -> Report<wire::OperationError> {
    wire::OperationError::forbidden()
}
