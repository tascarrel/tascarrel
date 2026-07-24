//! Shared operation serving and routing for control-plane links.
//!
//! [`Server`] consumes admitted messages from the link driver, dispatches
//! locally owned operations through a [`Service`], and forwards other
//! operations through reusable [`Peer`] handles supplied by a [`Router`]. A
//! connection can serve peer-opened operations while concurrently carrying
//! locally opened operations. The server owns operation capacity,
//! cancellation, event credit, terminal-message routing, identifier
//! translation, graceful shutdown, and task supervision.

use std::collections::HashMap;
use std::future::Future;
use std::future::pending;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use futures_util::FutureExt as _;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::protocol as wire;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::debug;
use tracing::warn;

use super::Error;
use super::Handle;
use super::Incoming;
use super::Policy;
use super::Transport;
use super::render_operation_error;

/// Runs local and forwarded operations received over control-plane links.
#[derive(Clone)]
pub struct Server {
    service: Arc<dyn Service>,
    router: Arc<dyn Router>,
    config: Config,
}

impl Server {
    /// Creates a server with default operation and shutdown settings.
    #[must_use]
    pub fn new(service: impl Service, router: impl Router) -> Self {
        Self::with_config(service, router, Config::default())
    }

    /// Creates a server with explicit operation and shutdown settings.
    #[must_use]
    pub fn with_config(service: impl Service, router: impl Router, config: Config) -> Self {
        Self {
            service: Arc::new(service),
            router: Arc::new(router),
            config,
        }
    }

    /// Serves one transport until either the link driver or dispatcher exits.
    ///
    /// The policy authenticates operation contexts before routing. Operation
    /// targets and inputs remain the responsibility of the router and local
    /// service.
    ///
    /// # Errors
    ///
    /// Returns a control-plane configuration, transport, or protocol failure.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn serve<T, P>(
        &self,
        transport: T,
        policy: P,
        link_config: super::Config,
    ) -> super::Result<()>
    where
        T: Transport + 'static,
        P: Policy,
    {
        self.serve_until_shutdown(transport, policy, link_config, pending())
            .await
    }

    /// Connects one full-duplex peer and returns its reusable operation handle.
    ///
    /// The returned [`Connection`] must be driven while [`Peer`] operations
    /// are in use. The same connection also serves operations opened by the
    /// remote peer.
    ///
    /// # Errors
    ///
    /// Returns a control-plane configuration failure.
    pub fn connect<T, P>(
        &self,
        transport: T,
        policy: P,
        link_config: super::Config,
    ) -> super::Result<(Peer, Connection)>
    where
        T: Transport + 'static,
        P: Policy,
    {
        self.connect_until_shutdown(transport, policy, link_config, pending())
    }

    /// Serves one transport until the link exits or `shutdown` resolves.
    ///
    /// Once shutdown begins, the server rejects new RPCs and subscriptions but
    /// continues driving active operations. Operations that remain after
    /// [`Config::shutdown_grace_period`] are forcefully aborted together with
    /// the control-plane link.
    ///
    /// # Errors
    ///
    /// Returns a control-plane configuration, transport, or protocol failure.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn serve_until_shutdown<T, P, S>(
        &self,
        transport: T,
        policy: P,
        link_config: super::Config,
        shutdown: S,
    ) -> super::Result<()>
    where
        T: Transport + 'static,
        P: Policy,
        S: Future<Output = ()> + Send + 'static,
    {
        let (_peer, connection) =
            self.connect_until_shutdown(transport, policy, link_config, shutdown)?;
        connection.await
    }

    /// Connects one full-duplex peer and begins graceful shutdown when
    /// `shutdown` resolves.
    ///
    /// The returned [`Connection`] must be driven while [`Peer`] operations
    /// are in use.
    ///
    /// # Errors
    ///
    /// Returns a control-plane configuration failure.
    pub fn connect_until_shutdown<T, P, S>(
        &self,
        transport: T,
        policy: P,
        link_config: super::Config,
        shutdown: S,
    ) -> super::Result<(Peer, Connection)>
    where
        T: Transport + 'static,
        P: Policy,
        S: Future<Output = ()> + Send + 'static,
    {
        self.config.validate()?;
        let (driver, handle, incoming) = super::connect(transport, policy, link_config)?;
        let outbound = Arc::new(OutboundRoutes::new(
            self.config.operation_control_queue_capacity,
        ));
        let peer = Peer {
            handle: handle.clone(),
            outbound: Arc::clone(&outbound),
        };
        let shutdown_grace_period = self.config.shutdown_grace_period;
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let drive = driver.run();
        let dispatch =
            Dispatcher::new(self.clone(), handle, outbound).run(incoming, shutdown_receiver);
        let shutdown = async move {
            shutdown.await;
            shutdown_sender.send_replace(true);
            tokio::time::sleep(shutdown_grace_period).await;
            warn!(
                ?shutdown_grace_period,
                "control-plane server shutdown grace period expired; aborting active operations"
            );
        };
        let connection = Box::pin(async move {
            tokio::pin!(drive);
            tokio::pin!(dispatch);
            tokio::pin!(shutdown);

            tokio::select! {
                biased;
                () = &mut shutdown => Ok(()),
                result = &mut drive => result,
                result = &mut dispatch => result,
            }
        });
        Ok((peer, connection))
    }
}

/// Future that drives one full-duplex control-plane server connection.
pub type Connection = Pin<Box<dyn Future<Output = super::Result<()>> + Send + 'static>>;

/// Executes operations owned by the local daemon.
pub trait Service: Send + Sync + 'static {
    /// Executes one locally addressed RPC.
    fn invoke(
        &self,
        invocation: wire::RpcInvocation,
    ) -> OperationFuture<'static, serde_json::Value>;

    /// Opens one locally addressed subscription.
    fn subscribe(
        &self,
        subscription: wire::SubscriptionStart,
    ) -> OperationFuture<'static, Box<dyn EventSource>>;
}

/// Resolves the owner of an operation target.
pub trait Router: Send + Sync + 'static {
    /// Resolves a target to the local service or an established downstream
    /// control-plane link.
    fn resolve(&self, target: wire::Address) -> OperationFuture<'static, Route>;
}

/// Receives encoded events from one locally owned subscription.
pub trait EventSource: Send + 'static {
    /// Receives the next event or reports normal source completion.
    fn recv(&mut self) -> OperationFuture<'_, Option<serde_json::Value>>;
}

/// A resolved operation destination.
pub enum Route {
    /// Dispatches the operation through the local [`Service`].
    Local,
    /// Relays the operation over an established full-duplex peer connection.
    Forward(Peer),
}

/// Reusable operation handle for one full-duplex control-plane connection.
#[derive(Clone)]
pub struct Peer {
    handle: Handle,
    outbound: Arc<OutboundRoutes>,
}

impl Peer {
    /// Opens an RPC using a fresh identifier scoped to this connection.
    ///
    /// # Errors
    ///
    /// Returns a connection or protocol failure when the opening cannot be
    /// sent.
    pub async fn invoke(&self, mut invocation: wire::RpcInvocation) -> super::Result<Rpc> {
        let (id, receiver) = self.outbound.register_rpc();
        invocation.id = id.clone();
        if let Err(error) = self
            .handle
            .send(wire::Message::Rpc(wire::RpcMessage::Invoke(invocation)))
            .await
        {
            self.outbound.remove_rpc(&id);
            return Err(error);
        }
        Ok(Rpc {
            peer: self.clone(),
            id,
            receiver,
            finished: false,
            cancel_requested: false,
        })
    }

    /// Opens a subscription using a fresh identifier scoped to this
    /// connection.
    ///
    /// # Errors
    ///
    /// Returns a connection or protocol failure when the opening cannot be
    /// sent.
    pub async fn subscribe(
        &self,
        mut start: wire::SubscriptionStart,
    ) -> super::Result<Subscription> {
        let (id, receiver) = self.outbound.register_subscription();
        start.id = id.clone();
        if let Err(error) = self
            .handle
            .send(wire::Message::Subscription(
                wire::SubscriptionMessage::Subscribe(start),
            ))
            .await
        {
            self.outbound.remove_subscription(&id);
            return Err(error);
        }
        Ok(Subscription {
            peer: self.clone(),
            id,
            receiver,
            finished: false,
            stop_requested: false,
        })
    }
}

impl std::fmt::Debug for Peer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Peer").finish_non_exhaustive()
    }
}

/// One RPC opened through a reusable [`Peer`].
pub struct Rpc {
    peer: Peer,
    id: wire::InvocationId,
    receiver: mpsc::Receiver<wire::RpcMessage>,
    finished: bool,
    cancel_requested: bool,
}

impl Rpc {
    /// Returns the connection-scoped invocation identifier.
    #[must_use]
    pub fn id(&self) -> &wire::InvocationId {
        &self.id
    }

    /// Receives the terminal response, or `None` if the connection closes.
    pub async fn recv(&mut self) -> Option<wire::RpcMessage> {
        let response = self.receiver.recv().await;
        self.finished = response.is_some();
        response
    }

    /// Requests cancellation of this RPC.
    ///
    /// # Errors
    ///
    /// Returns a connection or protocol failure when cancellation cannot be
    /// sent.
    pub async fn cancel(&mut self) -> super::Result<()> {
        self.peer
            .handle
            .send(wire::Message::Rpc(wire::RpcMessage::Cancel(
                wire::RpcCancellation {
                    id: self.id.clone(),
                },
            )))
            .await?;
        self.cancel_requested = true;
        Ok(())
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        self.peer.outbound.remove_rpc(&self.id);
        if !self.finished && !self.cancel_requested {
            send_release(
                self.peer.handle.clone(),
                wire::Message::Rpc(wire::RpcMessage::Cancel(wire::RpcCancellation {
                    id: self.id.clone(),
                })),
                "RPC cancellation",
            );
        }
    }
}

/// One subscription opened through a reusable [`Peer`].
pub struct Subscription {
    peer: Peer,
    id: wire::SubscriptionId,
    receiver: mpsc::Receiver<wire::SubscriptionMessage>,
    finished: bool,
    stop_requested: bool,
}

impl Subscription {
    /// Returns the connection-scoped subscription identifier.
    #[must_use]
    pub fn id(&self) -> &wire::SubscriptionId {
        &self.id
    }

    /// Receives the next event or terminal message, or `None` if the
    /// connection closes.
    pub async fn recv(&mut self) -> Option<wire::SubscriptionMessage> {
        let response = self.receiver.recv().await;
        if matches!(
            response.as_ref(),
            Some(wire::SubscriptionMessage::Completed(_) | wire::SubscriptionMessage::Failed(_))
        ) {
            self.finished = true;
        }
        response
    }

    /// Grants event credit to the remote producer.
    ///
    /// # Errors
    ///
    /// Returns a connection or protocol failure when credit cannot be sent.
    pub async fn grant_credit(&self, events: u32) -> super::Result<()> {
        self.peer
            .handle
            .send(wire::Message::Subscription(
                wire::SubscriptionMessage::GrantCredit(wire::SubscriptionCredit {
                    id: self.id.clone(),
                    events,
                }),
            ))
            .await
    }

    /// Requests subscription shutdown.
    ///
    /// # Errors
    ///
    /// Returns a connection or protocol failure when the request cannot be
    /// sent.
    pub async fn stop(&mut self) -> super::Result<()> {
        self.peer
            .handle
            .send(wire::Message::Subscription(
                wire::SubscriptionMessage::Unsubscribe(wire::SubscriptionStop {
                    id: self.id.clone(),
                }),
            ))
            .await?;
        self.stop_requested = true;
        Ok(())
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Subscription")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.peer.outbound.remove_subscription(&self.id);
        if !self.finished && !self.stop_requested {
            send_release(
                self.peer.handle.clone(),
                wire::Message::Subscription(wire::SubscriptionMessage::Unsubscribe(
                    wire::SubscriptionStop {
                        id: self.id.clone(),
                    },
                )),
                "subscription stop",
            );
        }
    }
}

/// Sends best-effort cleanup for an operation handle dropped before its
/// terminal response.
fn send_release(handle: Handle, message: wire::Message, operation: &'static str) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        debug!(
            operation,
            "could not release a dropped control-plane operation outside a Tokio runtime"
        );
        return;
    };
    runtime.spawn(async move {
        if let Err(error) = handle.send(message).await {
            debug!(%error, operation, "could not release a dropped control-plane operation");
        }
    });
}

struct OutboundRoutes {
    queue_capacity: usize,
    rpcs: StdMutex<HashMap<wire::InvocationId, mpsc::Sender<wire::RpcMessage>>>,
    subscriptions: StdMutex<HashMap<wire::SubscriptionId, mpsc::Sender<wire::SubscriptionMessage>>>,
}

impl OutboundRoutes {
    fn new(queue_capacity: usize) -> Self {
        Self {
            queue_capacity,
            rpcs: StdMutex::new(HashMap::new()),
            subscriptions: StdMutex::new(HashMap::new()),
        }
    }

    fn register_rpc(&self) -> (wire::InvocationId, mpsc::Receiver<wire::RpcMessage>) {
        let mut routes = self
            .rpcs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            let id = wire::InvocationId::generate();
            if routes.contains_key(&id) {
                continue;
            }
            let (sender, receiver) = mpsc::channel(self.queue_capacity);
            routes.insert(id.clone(), sender);
            return (id, receiver);
        }
    }

    fn register_subscription(
        &self,
    ) -> (
        wire::SubscriptionId,
        mpsc::Receiver<wire::SubscriptionMessage>,
    ) {
        let mut routes = self
            .subscriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            let id = wire::SubscriptionId::generate();
            if routes.contains_key(&id) {
                continue;
            }
            let (sender, receiver) = mpsc::channel(self.queue_capacity);
            routes.insert(id.clone(), sender);
            return (id, receiver);
        }
    }

    fn route_rpc(&self, message: wire::RpcMessage) {
        let id = match &message {
            wire::RpcMessage::Completed(response) => response.id.clone(),
            wire::RpcMessage::Failed(response) => response.id.clone(),
            wire::RpcMessage::Canceled(response) => response.id.clone(),
            wire::RpcMessage::Invoke(_) | wire::RpcMessage::Cancel(_) => return,
        };
        let sender = self
            .rpcs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        let delivered = match sender {
            Some(sender) => sender.try_send(message).is_ok(),
            None => false,
        };
        if !delivered {
            debug!(invocation_id = ?id, "discarding response for a released outbound RPC");
        }
    }

    fn route_subscription(&self, message: wire::SubscriptionMessage) {
        let (id, terminal) = match &message {
            wire::SubscriptionMessage::Event(event) => (event.id.clone(), false),
            wire::SubscriptionMessage::Completed(response) => (response.id.clone(), true),
            wire::SubscriptionMessage::Failed(response) => (response.id.clone(), true),
            wire::SubscriptionMessage::Subscribe(_)
            | wire::SubscriptionMessage::GrantCredit(_)
            | wire::SubscriptionMessage::Unsubscribe(_) => return,
        };
        let sender = {
            let mut routes = self
                .subscriptions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if terminal {
                routes.remove(&id)
            } else {
                routes.get(&id).cloned()
            }
        };
        let delivered = match sender {
            Some(sender) => sender.try_send(message).is_ok(),
            None => false,
        };
        if !delivered {
            self.remove_subscription(&id);
            debug!(subscription_id = ?id, "discarding response for a released outbound subscription");
        }
    }

    fn remove_rpc(&self, id: &wire::InvocationId) {
        self.rpcs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    fn remove_subscription(&self, id: &wire::SubscriptionId) {
        self.subscriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    fn close(&self) {
        self.rpcs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.subscriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// Operation limits and shutdown settings for one served control-plane link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Maximum RPCs concurrently served for one peer.
    pub max_active_rpcs: usize,
    /// Maximum subscriptions concurrently served for one peer.
    pub max_active_subscriptions: usize,
    /// Pending control or response messages retained for each operation.
    pub operation_control_queue_capacity: usize,
    /// Maximum event credit forwarded to one downstream subscription.
    pub forwarded_event_window: u32,
    /// Time allowed for active operations to finish after shutdown begins.
    pub shutdown_grace_period: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_active_rpcs: 64,
            max_active_subscriptions: 64,
            operation_control_queue_capacity: 64,
            forwarded_event_window: 32,
            shutdown_grace_period: Duration::from_secs(30),
        }
    }
}

impl Config {
    /// Validates queue and forwarding settings.
    fn validate(self) -> super::Result<()> {
        if self.operation_control_queue_capacity == 0 || self.forwarded_event_window == 0 {
            Err(Error::InvalidConfig.report())
        } else {
            Ok(())
        }
    }
}

/// Future returned by local services and route resolvers.
pub type OperationFuture<'a, T> = Pin<Box<dyn Future<Output = OperationResult<T>> + Send + 'a>>;

/// Result returned by local services and route resolvers.
pub type OperationResult<T> = std::result::Result<T, Report<wire::OperationError>>;

/// Tracks operation tasks for one served connection.
struct Dispatcher {
    server: Server,
    handle: Handle,
    outbound: Arc<OutboundRoutes>,
    tasks: JoinSet<BackgroundResult>,
    task_operations: HashMap<tokio::task::Id, BackgroundOperation>,
    rpcs: HashMap<wire::InvocationId, mpsc::Sender<RpcControl>>,
    subscriptions: HashMap<wire::SubscriptionId, mpsc::Sender<SubscriptionControl>>,
}

impl Dispatcher {
    /// Creates an empty dispatcher for one connected peer.
    fn new(server: Server, handle: Handle, outbound: Arc<OutboundRoutes>) -> Self {
        Self {
            server,
            handle,
            outbound,
            tasks: JoinSet::new(),
            task_operations: HashMap::new(),
            rpcs: HashMap::new(),
            subscriptions: HashMap::new(),
        }
    }

    /// Drives peer messages and switches to draining when shutdown begins.
    async fn run(
        mut self,
        mut incoming: Incoming,
        mut shutdown: watch::Receiver<bool>,
    ) -> super::Result<()> {
        let mut accepting = true;
        loop {
            if !accepting && self.tasks.is_empty() {
                return Ok(());
            }
            tokio::select! {
                biased;
                result = shutdown.changed(), if accepting => {
                    if result.is_err() {
                        debug!("control-plane server shutdown signal closed");
                    }
                    accepting = false;
                    debug!(
                        active_rpcs = self.rpcs.len(),
                        active_subscriptions = self.subscriptions.len(),
                        "control-plane server shutdown started"
                    );
                }
                result = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    if let Some(result) = result {
                        self.handle_task_result(result).await?;
                    }
                }
                message = incoming.recv() => {
                    let Some(message) = message else {
                        return Ok(());
                    };
                    self.handle_message(message, accepting).await?;
                }
            }
        }
    }

    /// Dispatches one admitted peer message by protocol family.
    async fn handle_message(
        &mut self,
        message: wire::Message,
        accepting: bool,
    ) -> super::Result<()> {
        match message {
            wire::Message::Control(wire::ControlMessage::Ping(ping)) => {
                self.handle
                    .send(wire::Message::Control(wire::ControlMessage::Pong(
                        wire::PongMessage { data: ping.data },
                    )))
                    .await
            }
            wire::Message::Control(wire::ControlMessage::Pong(_)) => {
                debug!("received an unsolicited control-plane pong");
                Ok(())
            }
            wire::Message::Rpc(rpc) => self.handle_rpc(rpc, accepting).await,
            wire::Message::Subscription(subscription) => {
                self.handle_subscription(subscription, accepting).await
            }
        }
    }

    /// Starts or controls one peer-owned RPC.
    async fn handle_rpc(&mut self, rpc: wire::RpcMessage, accepting: bool) -> super::Result<()> {
        match rpc {
            wire::RpcMessage::Invoke(invocation) => {
                let id = invocation.id.clone();
                if !accepting {
                    return self
                        .handle
                        .send(rpc_message(
                            id,
                            RpcTerminal::Failed(render_operation_error(unavailable_error(
                                "control-plane server is shutting down",
                            ))),
                        ))
                        .await;
                }
                if self.rpcs.len() >= self.server.config.max_active_rpcs {
                    return self
                        .handle
                        .send(rpc_message(
                            id,
                            RpcTerminal::Failed(render_operation_error(overloaded_error(
                                "peer RPC capacity is exhausted",
                            ))),
                        ))
                        .await;
                }
                let operation_id = id.clone();
                let (controls, control_receiver) =
                    mpsc::channel(self.server.config.operation_control_queue_capacity);
                let service = Arc::clone(&self.server.service);
                let router = Arc::clone(&self.server.router);
                let task = self.tasks.spawn(async move {
                    let terminal = run_rpc(service, router, invocation, control_receiver).await;
                    BackgroundResult::Rpc { id, terminal }
                });
                self.task_operations
                    .insert(task.id(), BackgroundOperation::Rpc(operation_id.clone()));
                self.rpcs.insert(operation_id, controls);
                Ok(())
            }
            wire::RpcMessage::Cancel(cancellation) => {
                if let Some(controls) = self.rpcs.get(&cancellation.id)
                    && controls.send(RpcControl::Cancel).await.is_err()
                {
                    debug!(
                        invocation_id = ?cancellation.id,
                        "RPC completed before its cancellation was delivered"
                    );
                }
                Ok(())
            }
            response @ (wire::RpcMessage::Completed(_)
            | wire::RpcMessage::Failed(_)
            | wire::RpcMessage::Canceled(_)) => {
                self.outbound.route_rpc(response);
                Ok(())
            }
        }
    }

    /// Starts or controls one peer-owned subscription.
    async fn handle_subscription(
        &mut self,
        subscription: wire::SubscriptionMessage,
        accepting: bool,
    ) -> super::Result<()> {
        match subscription {
            wire::SubscriptionMessage::Subscribe(start) => {
                let id = start.id.clone();
                if !accepting {
                    return self
                        .handle
                        .send(subscription_message(
                            id,
                            SubscriptionTerminal::Failed(render_operation_error(
                                unavailable_error("control-plane server is shutting down"),
                            )),
                        ))
                        .await;
                }
                if self.subscriptions.len() >= self.server.config.max_active_subscriptions {
                    return self
                        .handle
                        .send(subscription_message(
                            id,
                            SubscriptionTerminal::Failed(render_operation_error(overloaded_error(
                                "peer subscription capacity is exhausted",
                            ))),
                        ))
                        .await;
                }
                let operation_id = id.clone();
                let (controls, control_receiver) =
                    mpsc::channel(self.server.config.operation_control_queue_capacity);
                let service = Arc::clone(&self.server.service);
                let router = Arc::clone(&self.server.router);
                let outgoing = self.handle.clone();
                let forwarded_event_window = self.server.config.forwarded_event_window;
                let task = self.tasks.spawn(async move {
                    let terminal = run_subscription(
                        service,
                        router,
                        outgoing,
                        start,
                        control_receiver,
                        forwarded_event_window,
                    )
                    .await;
                    BackgroundResult::Subscription { id, terminal }
                });
                self.task_operations.insert(
                    task.id(),
                    BackgroundOperation::Subscription(operation_id.clone()),
                );
                self.subscriptions.insert(operation_id, controls);
                Ok(())
            }
            wire::SubscriptionMessage::GrantCredit(credit) => {
                if let Some(controls) = self.subscriptions.get(&credit.id)
                    && controls
                        .send(SubscriptionControl::Credit(credit.events))
                        .await
                        .is_err()
                {
                    debug!(
                        subscription_id = ?credit.id,
                        "subscription completed before its credit was delivered"
                    );
                }
                Ok(())
            }
            wire::SubscriptionMessage::Unsubscribe(stop) => {
                if let Some(controls) = self.subscriptions.get(&stop.id)
                    && controls.send(SubscriptionControl::Stop).await.is_err()
                {
                    debug!(
                        subscription_id = ?stop.id,
                        "subscription completed before its stop was delivered"
                    );
                }
                Ok(())
            }
            response @ (wire::SubscriptionMessage::Event(_)
            | wire::SubscriptionMessage::Completed(_)
            | wire::SubscriptionMessage::Failed(_)) => {
                self.outbound.route_subscription(response);
                Ok(())
            }
        }
    }

    /// Removes one finished task and emits its terminal message.
    async fn handle_task_result(
        &mut self,
        result: std::result::Result<(tokio::task::Id, BackgroundResult), tokio::task::JoinError>,
    ) -> super::Result<()> {
        match result {
            Ok((task_id, BackgroundResult::Rpc { id, terminal })) => {
                self.task_operations.remove(&task_id);
                self.rpcs.remove(&id);
                self.handle.send(rpc_message(id, terminal)).await
            }
            Ok((task_id, BackgroundResult::Subscription { id, terminal })) => {
                self.task_operations.remove(&task_id);
                self.subscriptions.remove(&id);
                self.handle.send(subscription_message(id, terminal)).await
            }
            Err(error) => {
                let Some(operation) = self.task_operations.remove(&error.id()) else {
                    debug!(%error, "cancelled control-plane operation task stopped");
                    return Ok(());
                };
                warn!(%error, "control-plane operation task failed");
                match operation {
                    BackgroundOperation::Rpc(id) => {
                        self.rpcs.remove(&id);
                        self.handle
                            .send(rpc_message(
                                id,
                                RpcTerminal::Failed(render_operation_error(internal_error(
                                    "RPC task failed",
                                ))),
                            ))
                            .await
                    }
                    BackgroundOperation::Subscription(id) => {
                        self.subscriptions.remove(&id);
                        self.handle
                            .send(subscription_message(
                                id,
                                SubscriptionTerminal::Failed(render_operation_error(
                                    internal_error("subscription task failed"),
                                )),
                            ))
                            .await
                    }
                }
            }
        }
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        self.outbound.close();
    }
}

/// Resolves and drives one RPC until a terminal outcome is available.
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(
        invocation_id = ?invocation.id,
        target = ?invocation.target,
        procedure = %invocation.procedure,
    )
)]
async fn run_rpc(
    service: Arc<dyn Service>,
    router: Arc<dyn Router>,
    invocation: wire::RpcInvocation,
    mut controls: mpsc::Receiver<RpcControl>,
) -> RpcTerminal {
    let deadline = match rpc_deadline(invocation.timeout_ms) {
        Ok(deadline) => deadline,
        Err(error) => return RpcTerminal::Failed(render_operation_error(error)),
    };
    let route = resolve_route(router, invocation.target.clone());
    tokio::pin!(route);
    let route = tokio::select! {
        biased;
        control = controls.recv() => return match control {
            Some(RpcControl::Cancel) | None => RpcTerminal::Canceled,
        },
        () = wait_for_deadline(deadline) => {
            return RpcTerminal::Failed(render_operation_error(timed_out_error(
                "RPC exceeded its requested lifetime",
            )));
        }
        result = &mut route => match result {
            Ok(route) => route,
            Err(error) => return RpcTerminal::Failed(render_operation_error(error)),
        },
    };

    match route {
        Route::Local => run_local_rpc(service, invocation, &mut controls, deadline).await,
        Route::Forward(downstream) => {
            run_forwarded_rpc(downstream, invocation, &mut controls, deadline).await
        }
    }
}

/// Drives one RPC owned by the local service.
async fn run_local_rpc(
    service: Arc<dyn Service>,
    invocation: wire::RpcInvocation,
    controls: &mut mpsc::Receiver<RpcControl>,
    deadline: Option<Instant>,
) -> RpcTerminal {
    let execution = invoke_local(service, invocation);
    tokio::pin!(execution);

    tokio::select! {
        biased;
        control = controls.recv() => match control {
            Some(RpcControl::Cancel) | None => RpcTerminal::Canceled,
        },
        () = wait_for_deadline(deadline) => {
            RpcTerminal::Failed(render_operation_error(timed_out_error(
                "RPC exceeded its requested lifetime",
            )))
        }
        result = &mut execution => match result {
            Ok(output) => RpcTerminal::Completed(output),
            Err(error) => RpcTerminal::Failed(render_operation_error(error)),
        },
    }
}

/// Relays one RPC over a reusable peer connection.
async fn run_forwarded_rpc(
    peer: Peer,
    invocation: wire::RpcInvocation,
    controls: &mut mpsc::Receiver<RpcControl>,
    deadline: Option<Instant>,
) -> RpcTerminal {
    let mut rpc = match peer.invoke(invocation).await {
        Ok(rpc) => rpc,
        Err(error) => {
            return RpcTerminal::Failed(render_operation_error(unavailable_error(format!(
                "failed to invoke downstream RPC: {error}"
            ))));
        }
    };

    tokio::select! {
        biased;
        message = rpc.recv() => {
            let Some(message) = message else {
                return RpcTerminal::Failed(render_operation_error(unavailable_error(
                    "downstream RPC connection closed",
                )));
            };
            downstream_rpc_terminal(message)
        }
        control = controls.recv() => match control {
            Some(RpcControl::Cancel) | None => {
                if let Err(error) = rpc.cancel().await {
                    debug!(%error, "failed to forward downstream RPC cancellation");
                    return RpcTerminal::Canceled;
                }
                wait_for_downstream_rpc_cancellation(rpc).await
            }
        },
        () = wait_for_deadline(deadline) => {
            if let Err(error) = rpc.cancel().await {
                debug!(%error, "failed to cancel a timed-out downstream RPC");
            }
            RpcTerminal::Failed(render_operation_error(timed_out_error(
                "RPC exceeded its requested lifetime",
            )))
        }
    }
}

/// Waits for a downstream terminal response after forwarding cancellation.
async fn wait_for_downstream_rpc_cancellation(mut rpc: Rpc) -> RpcTerminal {
    let Some(message) = rpc.recv().await else {
        return RpcTerminal::Canceled;
    };
    downstream_rpc_terminal(message)
}

/// Converts a downstream RPC response into a local terminal outcome.
fn downstream_rpc_terminal(message: wire::RpcMessage) -> RpcTerminal {
    match message {
        wire::RpcMessage::Completed(completed) => RpcTerminal::Completed(completed.output),
        wire::RpcMessage::Failed(failed) => RpcTerminal::Failed(failed.error),
        wire::RpcMessage::Canceled(_) => RpcTerminal::Canceled,
        wire::RpcMessage::Invoke(_) | wire::RpcMessage::Cancel(_) => {
            RpcTerminal::Failed(render_operation_error(internal_error(
                "downstream returned an unexpected RPC message",
            )))
        }
    }
}

/// Resolves and drives one subscription until a terminal outcome is available.
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(
        subscription_id = ?start.id,
        target = ?start.target,
        subscription = %start.subscription,
    )
)]
async fn run_subscription(
    service: Arc<dyn Service>,
    router: Arc<dyn Router>,
    outgoing: Handle,
    start: wire::SubscriptionStart,
    mut controls: mpsc::Receiver<SubscriptionControl>,
    forwarded_event_window: u32,
) -> SubscriptionTerminal {
    let route = resolve_route(router, start.target.clone());
    tokio::pin!(route);
    let mut pending_credit = 0_u64;
    let route = loop {
        tokio::select! {
            biased;
            control = controls.recv() => match control {
                Some(SubscriptionControl::Credit(events)) => {
                    let Some(updated) = pending_credit.checked_add(u64::from(events)) else {
                        return SubscriptionTerminal::Failed(render_operation_error(
                            internal_error("subscription credit overflowed"),
                        ));
                    };
                    pending_credit = updated;
                }
                Some(SubscriptionControl::Stop) | None => {
                    return SubscriptionTerminal::Completed;
                }
            },
            result = &mut route => break match result {
                Ok(route) => route,
                Err(error) => {
                    return SubscriptionTerminal::Failed(render_operation_error(error));
                }
            },
        }
    };

    match route {
        Route::Local => {
            run_local_subscription(service, outgoing, start, &mut controls, pending_credit).await
        }
        Route::Forward(downstream) => {
            run_forwarded_subscription(
                downstream,
                outgoing,
                start,
                &mut controls,
                pending_credit,
                forwarded_event_window,
            )
            .await
        }
    }
}

/// Drives one subscription owned by the local service.
async fn run_local_subscription(
    service: Arc<dyn Service>,
    outgoing: Handle,
    start: wire::SubscriptionStart,
    controls: &mut mpsc::Receiver<SubscriptionControl>,
    mut credit: u64,
) -> SubscriptionTerminal {
    let id = start.id.clone();
    let source = subscribe_local(service, start);
    tokio::pin!(source);
    let mut source = loop {
        tokio::select! {
            biased;
            control = controls.recv() => match control {
                Some(SubscriptionControl::Credit(events)) => {
                    let Some(updated) = credit.checked_add(u64::from(events)) else {
                        return SubscriptionTerminal::Failed(render_operation_error(
                            internal_error("subscription credit overflowed"),
                        ));
                    };
                    credit = updated;
                }
                Some(SubscriptionControl::Stop) | None => {
                    return SubscriptionTerminal::Completed;
                }
            },
            result = &mut source => break match result {
                Ok(source) => source,
                Err(error) => {
                    return SubscriptionTerminal::Failed(render_operation_error(error));
                }
            },
        }
    };

    let mut buffered_event = None;
    loop {
        if credit > 0
            && let Some(event) = buffered_event.take()
        {
            if let Err(error) = outgoing.send(subscription_event(id.clone(), event)).await {
                return SubscriptionTerminal::Failed(render_operation_error(unavailable_error(
                    format!("failed to send local subscription event: {error}"),
                )));
            }
            credit -= 1;
            continue;
        }
        tokio::select! {
            biased;
            control = controls.recv() => match control {
                Some(SubscriptionControl::Credit(events)) => {
                    let Some(updated) = credit.checked_add(u64::from(events)) else {
                        return SubscriptionTerminal::Failed(render_operation_error(
                            internal_error("subscription credit overflowed"),
                        ));
                    };
                    credit = updated;
                }
                Some(SubscriptionControl::Stop) | None => {
                    return SubscriptionTerminal::Completed;
                }
            },
            event = receive_event(source.as_mut()), if buffered_event.is_none() => match event {
                Ok(Some(event)) => buffered_event = Some(event),
                Ok(None) => return SubscriptionTerminal::Completed,
                Err(error) => {
                    return SubscriptionTerminal::Failed(render_operation_error(error));
                }
            },
        }
    }
}

/// Relays one subscription over a reusable peer connection.
async fn run_forwarded_subscription(
    peer: Peer,
    outgoing: Handle,
    start: wire::SubscriptionStart,
    controls: &mut mpsc::Receiver<SubscriptionControl>,
    mut pending_credit: u64,
    forwarded_event_window: u32,
) -> SubscriptionTerminal {
    let upstream_id = start.id.clone();
    let mut subscription = match peer.subscribe(start).await {
        Ok(subscription) => subscription,
        Err(error) => {
            return SubscriptionTerminal::Failed(render_operation_error(unavailable_error(
                format!("failed to open downstream subscription: {error}"),
            )));
        }
    };

    let forwarded_event_window = u64::from(forwarded_event_window);
    let mut downstream_credit = 0_u64;
    loop {
        if let Err(error) = forward_available_credit(
            &subscription,
            &mut pending_credit,
            &mut downstream_credit,
            forwarded_event_window,
        )
        .await
        {
            return SubscriptionTerminal::Failed(render_operation_error(error));
        }

        tokio::select! {
            biased;
            control = controls.recv() => match control {
                Some(SubscriptionControl::Credit(events)) => {
                    let Some(updated) = pending_credit.checked_add(u64::from(events)) else {
                        return SubscriptionTerminal::Failed(render_operation_error(
                            internal_error("subscription credit overflowed"),
                        ));
                    };
                    pending_credit = updated;
                }
                Some(SubscriptionControl::Stop) | None => {
                    return stop_forwarded_subscription(subscription).await;
                }
            },
            message = subscription.recv() => {
                let Some(message) = message else {
                    return SubscriptionTerminal::Failed(render_operation_error(unavailable_error(
                        "downstream subscription connection closed",
                    )));
                };
                match message {
                    wire::SubscriptionMessage::Event(event) => {
                        let Some(remaining_credit) = downstream_credit.checked_sub(1) else {
                            return SubscriptionTerminal::Failed(render_operation_error(
                                internal_error(
                                    "downstream emitted a subscription event without forwarded credit",
                                ),
                            ));
                        };
                        downstream_credit = remaining_credit;
                        if let Err(error) = outgoing.send(subscription_event(
                            upstream_id.clone(),
                            event.event,
                        )).await {
                            return SubscriptionTerminal::Failed(render_operation_error(
                                unavailable_error(format!(
                                    "failed to forward subscription event: {error}"
                                )),
                            ));
                        }
                    }
                    wire::SubscriptionMessage::Completed(_) => {
                        return SubscriptionTerminal::Completed;
                    }
                    wire::SubscriptionMessage::Failed(failed) => {
                        return SubscriptionTerminal::Failed(failed.error);
                    }
                    _ => return SubscriptionTerminal::Failed(render_operation_error(
                        internal_error("downstream returned an unexpected subscription message"),
                    )),
                }
            }
        }
    }
}

/// Resolves a route while containing synchronous and asynchronous router
/// panics within the affected operation.
async fn resolve_route(router: Arc<dyn Router>, target: wire::Address) -> OperationResult<Route> {
    let route = catch_unwind(AssertUnwindSafe(|| router.resolve(target))).map_err(|_| {
        warn!("control-plane route resolver panicked before returning a future");
        internal_error("control-plane route resolver panicked")
    })?;
    AssertUnwindSafe(route).catch_unwind().await.map_err(|_| {
        warn!("control-plane route resolver future panicked");
        internal_error("control-plane route resolver panicked")
    })?
}

/// Executes a local RPC while containing synchronous and asynchronous service
/// panics within the invocation.
async fn invoke_local(
    service: Arc<dyn Service>,
    invocation: wire::RpcInvocation,
) -> OperationResult<serde_json::Value> {
    let invocation =
        catch_unwind(AssertUnwindSafe(|| service.invoke(invocation))).map_err(|_| {
            warn!("local RPC implementation panicked before returning a future");
            internal_error("local RPC implementation panicked")
        })?;
    AssertUnwindSafe(invocation)
        .catch_unwind()
        .await
        .map_err(|_| {
            warn!("local RPC implementation future panicked");
            internal_error("local RPC implementation panicked")
        })?
}

/// Opens a local subscription while containing synchronous and asynchronous
/// service panics within the subscription.
async fn subscribe_local(
    service: Arc<dyn Service>,
    start: wire::SubscriptionStart,
) -> OperationResult<Box<dyn EventSource>> {
    let subscription =
        catch_unwind(AssertUnwindSafe(|| service.subscribe(start))).map_err(|_| {
            warn!("local subscription implementation panicked before returning a future");
            internal_error("local subscription implementation panicked")
        })?;
    AssertUnwindSafe(subscription)
        .catch_unwind()
        .await
        .map_err(|_| {
            warn!("local subscription implementation future panicked");
            internal_error("local subscription implementation panicked")
        })?
}

/// Polls one local event source while containing synchronous and asynchronous
/// source panics within the subscription.
async fn receive_event(source: &mut dyn EventSource) -> OperationResult<Option<serde_json::Value>> {
    AssertUnwindSafe(async { source.recv().await })
        .catch_unwind()
        .await
        .map_err(|_| {
            warn!("subscription event source future panicked");
            internal_error("subscription event source panicked")
        })?
}

/// Forwards available consumer credit without exceeding the downstream event
/// window.
async fn forward_available_credit(
    subscription: &Subscription,
    pending_credit: &mut u64,
    downstream_credit: &mut u64,
    forwarded_event_window: u64,
) -> OperationResult<()> {
    if *pending_credit == 0 || *downstream_credit >= forwarded_event_window {
        return Ok(());
    }
    let granted = (*pending_credit).min(forwarded_event_window - *downstream_credit);
    let events =
        u32::try_from(granted).expect("forwarded credit is bounded by a u32 configuration value");
    subscription.grant_credit(events).await.map_err(|error| {
        unavailable_error(format!("failed to forward subscription credit: {error}"))
    })?;
    *pending_credit -= granted;
    *downstream_credit += granted;
    Ok(())
}

/// Propagates subscription shutdown and waits for its downstream terminal.
async fn stop_forwarded_subscription(mut subscription: Subscription) -> SubscriptionTerminal {
    if let Err(error) = subscription.stop().await {
        debug!(%error, "failed to forward downstream subscription stop");
        return SubscriptionTerminal::Completed;
    }
    loop {
        tokio::select! {
            biased;
            message = subscription.recv() => {
                let Some(message) = message else {
                    return SubscriptionTerminal::Completed;
                };
                match message {
                    wire::SubscriptionMessage::Completed(_) => {
                        return SubscriptionTerminal::Completed;
                    }
                    wire::SubscriptionMessage::Failed(failed) => {
                        return SubscriptionTerminal::Failed(failed.error);
                    }
                    wire::SubscriptionMessage::Event(_) => {
                            debug!("discarding downstream subscription event while stopping");
                        }
                    _ => return SubscriptionTerminal::Failed(render_operation_error(
                        internal_error(
                            "downstream returned an unexpected subscription message while stopping",
                        ),
                    )),
                }
            }
        }
    }
}

/// Terminal outcome produced by one supervised operation task.
enum BackgroundResult {
    /// Outcome of an RPC task.
    Rpc {
        /// Upstream invocation identifier.
        id: wire::InvocationId,
        /// Terminal RPC outcome.
        terminal: RpcTerminal,
    },
    /// Outcome of a subscription task.
    Subscription {
        /// Upstream subscription identifier.
        id: wire::SubscriptionId,
        /// Terminal subscription outcome.
        terminal: SubscriptionTerminal,
    },
}

/// Identifies the upstream operation owned by a supervised task.
enum BackgroundOperation {
    /// RPC invocation identifier.
    Rpc(wire::InvocationId),
    /// Subscription identifier.
    Subscription(wire::SubscriptionId),
}

/// Control messages accepted by a running RPC task.
enum RpcControl {
    /// Requests RPC cancellation.
    Cancel,
}

/// Control messages accepted by a running subscription task.
enum SubscriptionControl {
    /// Adds consumer event credit.
    Credit(u32),
    /// Requests subscription shutdown.
    Stop,
}

/// Terminal outcome of one RPC task.
enum RpcTerminal {
    /// RPC output returned successfully.
    Completed(serde_json::Value),
    /// Peer-visible RPC failure.
    Failed(wire::OperationError),
    /// RPC cancellation acknowledged.
    Canceled,
}

/// Terminal outcome of one subscription task.
enum SubscriptionTerminal {
    /// Subscription ended normally.
    Completed,
    /// Peer-visible subscription failure.
    Failed(wire::OperationError),
}

/// Encodes an RPC terminal outcome for its upstream peer.
fn rpc_message(id: wire::InvocationId, terminal: RpcTerminal) -> wire::Message {
    wire::Message::Rpc(match terminal {
        RpcTerminal::Completed(output) => {
            wire::RpcMessage::Completed(wire::RpcCompleted { id, output })
        }
        RpcTerminal::Failed(error) => wire::RpcMessage::Failed(wire::RpcFailed { id, error }),
        RpcTerminal::Canceled => wire::RpcMessage::Canceled(wire::RpcCanceled { id }),
    })
}

/// Encodes a subscription terminal outcome for its upstream peer.
fn subscription_message(id: wire::SubscriptionId, terminal: SubscriptionTerminal) -> wire::Message {
    wire::Message::Subscription(match terminal {
        SubscriptionTerminal::Completed => {
            wire::SubscriptionMessage::Completed(wire::SubscriptionCompleted { id })
        }
        SubscriptionTerminal::Failed(error) => {
            wire::SubscriptionMessage::Failed(wire::SubscriptionFailed { id, error })
        }
    })
}

/// Encodes one subscription event for its upstream peer.
fn subscription_event(id: wire::SubscriptionId, event: serde_json::Value) -> wire::Message {
    wire::Message::Subscription(wire::SubscriptionMessage::Event(wire::SubscriptionEvent {
        id,
        event,
    }))
}

/// Resolves an optional wire timeout to a monotonic deadline.
fn rpc_deadline(timeout_ms: Option<u64>) -> OperationResult<Option<Instant>> {
    timeout_ms
        .map(|milliseconds| {
            Instant::now()
                .checked_add(Duration::from_millis(milliseconds))
                .ok_or_else(|| invalid_request("requested RPC timeout is too large"))
        })
        .transpose()
}

/// Waits indefinitely when an RPC has no requested deadline.
async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending::<()>().await,
    }
}

/// Creates a reported contract failure.
fn invalid_request(message: impl Into<String>) -> Report<wire::OperationError> {
    wire::OperationError::InvalidRequest(operation_error_details(message)).report()
}

/// Creates a reported dependency-availability failure.
fn unavailable_error(message: impl Into<String>) -> Report<wire::OperationError> {
    wire::OperationError::Unavailable(operation_error_details(message)).report()
}

/// Creates a reported capacity failure.
fn overloaded_error(message: impl Into<String>) -> Report<wire::OperationError> {
    wire::OperationError::Overloaded(operation_error_details(message)).report()
}

/// Creates a reported deadline failure.
fn timed_out_error(message: impl Into<String>) -> Report<wire::OperationError> {
    wire::OperationError::TimedOut(operation_error_details(message)).report()
}

/// Creates a reported implementation failure.
fn internal_error(message: impl Into<String>) -> Report<wire::OperationError> {
    wire::OperationError::Internal(operation_error_details(message)).report()
}

/// Creates operation details for a standalone diagnostic message.
fn operation_error_details(message: impl Into<String>) -> wire::OperationErrorDetails {
    wire::OperationErrorDetails {
        message: message.into().into(),
        report: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use tascarrel_api::types::workspaces::WorkspaceName;
    use tokio::io::DuplexStream;
    use tokio::io::duplex;
    use tokio::sync::Mutex;
    use tokio::sync::Notify;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::control_plane::StreamTransport;
    use crate::control_plane::policy::DenyAll;
    use crate::control_plane::policy::topology;

    /// Verifies local RPC completion and cancellation together with
    /// credit-driven subscription completion.
    #[tokio::test]
    async fn server_drives_local_operation_lifecycles() {
        let server = Server::new(FakeService, QueueRouter::default());
        let (handle, mut incoming, tasks) = connect_client(server);

        let invocation_id = wire::InvocationId::generate();
        handle
            .send(rpc(
                invocation_id.clone(),
                wire::Address::Host,
                "tests_Echo",
                serde_json::Value::Bool(true),
            ))
            .await
            .expect("invoke local RPC");
        assert!(matches!(
            incoming.recv().await,
            Some(wire::Message::Rpc(wire::RpcMessage::Completed(completed)))
                if completed.id == invocation_id
                    && completed.output == serde_json::Value::Bool(true)
        ));

        let canceled_id = wire::InvocationId::generate();
        handle
            .send(rpc(
                canceled_id.clone(),
                wire::Address::Host,
                "tests_Wait",
                serde_json::Value::Null,
            ))
            .await
            .expect("invoke cancellable local RPC");
        handle
            .send(wire::Message::Rpc(wire::RpcMessage::Cancel(
                wire::RpcCancellation {
                    id: canceled_id.clone(),
                },
            )))
            .await
            .expect("cancel local RPC");
        assert!(matches!(
            incoming.recv().await,
            Some(wire::Message::Rpc(wire::RpcMessage::Canceled(canceled)))
                if canceled.id == canceled_id
        ));

        let subscription_id = wire::SubscriptionId::generate();
        handle
            .send(subscription(subscription_id.clone(), wire::Address::Host))
            .await
            .expect("open local subscription");
        handle
            .send(credit(subscription_id.clone(), 1))
            .await
            .expect("grant local subscription credit");
        assert!(matches!(
            incoming.recv().await,
            Some(wire::Message::Subscription(wire::SubscriptionMessage::Event(event)))
                if event.id == subscription_id
                    && event.event == serde_json::Value::String("event".to_owned())
        ));
        handle
            .send(unsubscribe(subscription_id.clone()))
            .await
            .expect("stop local subscription");
        assert!(matches!(
            incoming.recv().await,
            Some(wire::Message::Subscription(wire::SubscriptionMessage::Completed(completed)))
                if completed.id == subscription_id
        ));

        stop_tasks(tasks).await;
    }

    /// Verifies each endpoint can open and serve operations concurrently over
    /// the same connection.
    #[tokio::test]
    async fn server_connection_is_full_duplex() {
        let (left_io, right_io) = duplex(1024 * 1024);
        let left_server = Server::new(FakeService, QueueRouter::default());
        let right_server = Server::new(FakeService, QueueRouter::default());
        let (left, left_connection) = left_server
            .connect(
                StreamTransport::new(left_io),
                topology::client_to_hostd(&wire::ClientId::generate()),
                super::super::Config::default(),
            )
            .expect("configure left full-duplex endpoint");
        let (right, right_connection) = right_server
            .connect(
                StreamTransport::new(right_io),
                topology::client_to_hostd(&wire::ClientId::generate()),
                super::super::Config::default(),
            )
            .expect("configure right full-duplex endpoint");
        let tasks = [
            tokio::spawn(left_connection),
            tokio::spawn(right_connection),
        ];

        let (left_rpc, right_rpc) = tokio::join!(
            left.invoke(echo_invocation(serde_json::Value::String(
                "from-left".to_owned()
            ))),
            right.invoke(echo_invocation(serde_json::Value::String(
                "from-right".to_owned()
            ))),
        );
        let mut left_rpc = left_rpc.expect("open left-to-right RPC");
        let mut right_rpc = right_rpc.expect("open right-to-left RPC");
        let (left_response, right_response) = tokio::join!(left_rpc.recv(), right_rpc.recv());

        assert!(matches!(
            left_response,
            Some(wire::RpcMessage::Completed(completed))
                if completed.output == serde_json::Value::String("from-left".to_owned())
        ));
        assert!(matches!(
            right_response,
            Some(wire::RpcMessage::Completed(completed))
                if completed.output == serde_json::Value::String("from-right".to_owned())
        ));

        stop_tasks(tasks).await;
    }

    /// Verifies panics in local operation implementations become internal
    /// failures without terminating the control-plane link.
    #[tokio::test]
    async fn server_contains_local_operation_panics() {
        let server = Server::new(FakeService, QueueRouter::default());
        let (handle, mut incoming, tasks) = connect_client(server);

        for procedure in ["tests_PanicSync", "tests_PanicAsync"] {
            let id = wire::InvocationId::generate();
            handle
                .send(rpc(
                    id.clone(),
                    wire::Address::Host,
                    procedure,
                    serde_json::Value::Null,
                ))
                .await
                .expect("invoke panicking local RPC");
            assert!(matches!(
                incoming.recv().await,
                Some(wire::Message::Rpc(wire::RpcMessage::Failed(failed)))
                    if failed.id == id
                        && matches!(failed.error, wire::OperationError::Internal(_))
            ));
        }

        for name in [
            "tests_PanicSubscribeSync",
            "tests_PanicOpen",
            "tests_PanicEvent",
        ] {
            let id = wire::SubscriptionId::generate();
            handle
                .send(named_subscription(id.clone(), wire::Address::Host, name))
                .await
                .expect("open panicking local subscription");
            assert!(matches!(
                incoming.recv().await,
                Some(wire::Message::Subscription(wire::SubscriptionMessage::Failed(failed)))
                    if failed.id == id
                        && matches!(failed.error, wire::OperationError::Internal(_))
            ));
        }

        let healthy_id = wire::InvocationId::generate();
        handle
            .send(rpc(
                healthy_id.clone(),
                wire::Address::Host,
                "tests_Echo",
                serde_json::Value::Bool(true),
            ))
            .await
            .expect("invoke local RPC after contained panics");
        assert!(matches!(
            incoming.recv().await,
            Some(wire::Message::Rpc(wire::RpcMessage::Completed(completed)))
                if completed.id == healthy_id
        ));

        stop_tasks(tasks).await;
    }

    /// Verifies shutdown rejects new openings while allowing active RPCs to
    /// finish within the grace period.
    #[tokio::test]
    async fn server_gracefully_drains_active_operations() {
        let started = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let service = ShutdownService {
            started: Arc::clone(&started),
            finish: Arc::clone(&finish),
            dropped: None,
        };
        let server = Server::new(service, QueueRouter::default());
        let (shutdown_sender, shutdown) = oneshot::channel();
        let (handle, mut incoming, [server_task, client_task]) =
            connect_client_until_shutdown(server, async move {
                shutdown.await.expect("shutdown sender remains available");
            });

        let active_id = wire::InvocationId::generate();
        handle
            .send(rpc(
                active_id.clone(),
                wire::Address::Host,
                "tests_Drain",
                serde_json::Value::Bool(true),
            ))
            .await
            .expect("invoke RPC before shutdown");
        started.notified().await;
        shutdown_sender.send(()).expect("request server shutdown");

        let rejected_id = wire::InvocationId::generate();
        handle
            .send(rpc(
                rejected_id.clone(),
                wire::Address::Host,
                "tests_Echo",
                serde_json::Value::Null,
            ))
            .await
            .expect("invoke RPC after shutdown begins");
        assert!(matches!(
            incoming.recv().await,
            Some(wire::Message::Rpc(wire::RpcMessage::Failed(failed)))
                if failed.id == rejected_id
                    && matches!(failed.error, wire::OperationError::Unavailable(_))
        ));

        finish.notify_one();
        assert!(matches!(
            incoming.recv().await,
            Some(wire::Message::Rpc(wire::RpcMessage::Completed(completed)))
                if completed.id == active_id
                    && completed.output == serde_json::Value::Bool(true)
        ));

        server_task
            .await
            .expect("server task remains healthy")
            .expect("server drains active RPCs");
        stop_task(client_task).await;
    }

    /// Verifies operations exceeding the shutdown grace period are aborted.
    #[tokio::test]
    async fn server_forcefully_aborts_operations_after_shutdown_grace_period() {
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let service = ShutdownService {
            started: Arc::clone(&started),
            finish: Arc::new(Notify::new()),
            dropped: Some(Arc::clone(&dropped)),
        };
        let config = Config {
            shutdown_grace_period: Duration::from_millis(10),
            ..Config::default()
        };
        let server = Server::with_config(service, QueueRouter::default(), config);
        let (shutdown_sender, shutdown) = oneshot::channel();
        let (handle, _incoming, [server_task, client_task]) =
            connect_client_until_shutdown(server, async move {
                shutdown.await.expect("shutdown sender remains available");
            });

        handle
            .send(rpc(
                wire::InvocationId::generate(),
                wire::Address::Host,
                "tests_Drain",
                serde_json::Value::Null,
            ))
            .await
            .expect("invoke RPC before forced shutdown");
        started.notified().await;
        shutdown_sender.send(()).expect("request server shutdown");

        server_task
            .await
            .expect("server task remains healthy")
            .expect("server shutdown succeeds");
        dropped.notified().await;
        stop_task(client_task).await;
    }

    /// Verifies RPC route resolution and downstream identifier allocation.
    #[tokio::test]
    async fn server_routes_rpcs_over_downstream_links() {
        let (rpc_downstream, mut rpc_peer, rpc_peer_tasks) = downstream();
        let router = QueueRouter::new([rpc_downstream]);
        let server = Server::new(FakeService, router);
        let (handle, mut incoming, tasks) = connect_client(server);
        let target = workspace_target();

        let upstream_rpc_id = wire::InvocationId::generate();
        handle
            .send(rpc(
                upstream_rpc_id.clone(),
                target.clone(),
                "tests_Forwarded",
                serde_json::Value::Null,
            ))
            .await
            .expect("invoke routed RPC");
        let wire::Message::Rpc(wire::RpcMessage::Invoke(invocation)) =
            rpc_peer.recv().await.expect("forwarded invocation")
        else {
            panic!("downstream did not receive an RPC invocation");
        };
        assert_ne!(invocation.id, upstream_rpc_id);
        assert!(invocation.context.is_some());
        rpc_peer
            .handle
            .send(wire::Message::Rpc(wire::RpcMessage::Completed(
                wire::RpcCompleted {
                    id: invocation.id,
                    output: serde_json::Value::String("forwarded".to_owned()),
                },
            )))
            .await
            .expect("complete downstream RPC");
        assert!(matches!(
            incoming.recv().await,
            Some(wire::Message::Rpc(wire::RpcMessage::Completed(completed)))
                if completed.id == upstream_rpc_id
                    && completed.output == serde_json::Value::String("forwarded".to_owned())
        ));

        stop_tasks(tasks).await;
        stop_tasks(rpc_peer_tasks).await;
    }

    /// Verifies bounded credit forwarding, event relay, and downstream stop
    /// propagation.
    #[tokio::test]
    async fn server_routes_subscriptions_over_downstream_links() {
        let (subscription_downstream, mut subscription_peer, subscription_peer_tasks) =
            downstream();
        let router = QueueRouter::new([subscription_downstream]);
        let server = Server::new(FakeService, router);
        let (handle, mut incoming, tasks) = connect_client(server);
        let target = workspace_target();

        let upstream_subscription_id = wire::SubscriptionId::generate();
        handle
            .send(subscription(upstream_subscription_id.clone(), target))
            .await
            .expect("open routed subscription");
        let wire::Message::Subscription(wire::SubscriptionMessage::Subscribe(start)) =
            subscription_peer
                .recv()
                .await
                .expect("forwarded subscription")
        else {
            panic!("downstream did not receive a subscription opening");
        };
        assert_ne!(start.id, upstream_subscription_id);

        handle
            .send(credit(upstream_subscription_id.clone(), 100))
            .await
            .expect("grant upstream credit");
        let wire::Message::Subscription(wire::SubscriptionMessage::GrantCredit(granted)) =
            subscription_peer
                .recv()
                .await
                .expect("forwarded subscription credit")
        else {
            panic!("downstream did not receive subscription credit");
        };
        assert_eq!(granted.id, start.id);
        assert_eq!(granted.events, Config::default().forwarded_event_window);

        subscription_peer
            .handle
            .send(wire::Message::Subscription(
                wire::SubscriptionMessage::Event(wire::SubscriptionEvent {
                    id: start.id.clone(),
                    event: serde_json::Value::String("forwarded-event".to_owned()),
                }),
            ))
            .await
            .expect("emit downstream subscription event");
        assert!(matches!(
            incoming.recv().await,
            Some(wire::Message::Subscription(wire::SubscriptionMessage::Event(event)))
                if event.id == upstream_subscription_id
                    && event.event
                        == serde_json::Value::String("forwarded-event".to_owned())
        ));

        handle
            .send(unsubscribe(upstream_subscription_id.clone()))
            .await
            .expect("stop routed subscription");
        loop {
            match subscription_peer.recv().await {
                Some(wire::Message::Subscription(wire::SubscriptionMessage::Unsubscribe(stop)))
                    if stop.id == start.id =>
                {
                    break;
                }
                Some(wire::Message::Subscription(wire::SubscriptionMessage::GrantCredit(
                    granted,
                ))) if granted.id == start.id => {}
                message => panic!("unexpected message while stopping subscription: {message:?}"),
            }
        }
        subscription_peer
            .handle
            .send(wire::Message::Subscription(
                wire::SubscriptionMessage::Completed(wire::SubscriptionCompleted { id: start.id }),
            ))
            .await
            .expect("complete downstream subscription");
        assert!(matches!(
            incoming.recv().await,
            Some(wire::Message::Subscription(wire::SubscriptionMessage::Completed(completed)))
                if completed.id == upstream_subscription_id
        ));

        stop_tasks(tasks).await;
        stop_tasks(subscription_peer_tasks).await;
    }

    /// Verifies an abruptly disconnected upstream client releases only its
    /// forwarded subscription and leaves the reusable downstream peer alive.
    #[tokio::test]
    async fn upstream_disconnect_preserves_shared_downstream_peer() {
        let (downstream, mut downstream_peer, downstream_tasks) = downstream();
        let router = QueueRouter::new([downstream.clone()]);
        let server = Server::new(FakeService, router);
        let (handle, _incoming, [server_task, client_task]) = connect_client(server);
        let upstream_id = wire::SubscriptionId::generate();
        handle
            .send(subscription(upstream_id.clone(), workspace_target()))
            .await
            .expect("open forwarded subscription");
        let wire::Message::Subscription(wire::SubscriptionMessage::Subscribe(start)) =
            tokio::time::timeout(Duration::from_secs(1), downstream_peer.recv())
                .await
                .expect("forwarded subscription timed out")
                .expect("receive forwarded subscription")
        else {
            panic!("downstream did not receive a subscription opening");
        };

        client_task.abort();
        assert!(client_task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), server_task)
            .await
            .expect("upstream server stopped after client disconnect")
            .expect("upstream server task did not panic")
            .expect("upstream server stopped cleanly");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match downstream_peer.recv().await {
                    Some(wire::Message::Subscription(wire::SubscriptionMessage::Unsubscribe(
                        stop,
                    ))) if stop.id == start.id => {
                        break;
                    }
                    message => panic!("unexpected downstream release message: {message:?}"),
                }
            }
        })
        .await
        .expect("downstream subscription release timed out");
        downstream_peer
            .handle
            .send(wire::Message::Subscription(
                wire::SubscriptionMessage::Completed(wire::SubscriptionCompleted { id: start.id }),
            ))
            .await
            .expect("complete released downstream subscription");

        let expected = serde_json::Value::String("peer-still-live".to_owned());
        let mut invocation = echo_invocation(expected.clone());
        invocation.context = Some(wire::RequestContext {
            origin: wire::Actor::Host,
            caller: wire::Actor::Host,
            trace_id: wire::TraceId::generate(),
            caused_by: None,
        });
        let mut rpc = downstream
            .invoke(invocation)
            .await
            .expect("open RPC on shared downstream peer");
        let wire::Message::Rpc(wire::RpcMessage::Invoke(invocation)) =
            tokio::time::timeout(Duration::from_secs(1), downstream_peer.recv())
                .await
                .expect("follow-up RPC timed out")
                .expect("receive RPC after upstream disconnect")
        else {
            panic!("downstream did not receive the follow-up RPC");
        };
        downstream_peer
            .handle
            .send(wire::Message::Rpc(wire::RpcMessage::Completed(
                wire::RpcCompleted {
                    id: invocation.id,
                    output: expected.clone(),
                },
            )))
            .await
            .expect("complete follow-up RPC");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), rpc.recv())
                .await
                .expect("follow-up RPC response timed out"),
            Some(wire::RpcMessage::Completed(completed)) if completed.output == expected
        ));

        stop_tasks(downstream_tasks).await;
    }

    struct FakeService;

    impl Service for FakeService {
        fn invoke(
            &self,
            invocation: wire::RpcInvocation,
        ) -> OperationFuture<'static, serde_json::Value> {
            assert_ne!(
                invocation.procedure, "tests_PanicSync",
                "synchronous RPC panic"
            );
            Box::pin(async move {
                match invocation.procedure.as_ref() {
                    "tests_Echo" => Ok(invocation.input),
                    "tests_Wait" => pending().await,
                    "tests_PanicAsync" => panic!("asynchronous RPC panic"),
                    _ => Err(invalid_request("unknown test RPC")),
                }
            })
        }

        fn subscribe(
            &self,
            start: wire::SubscriptionStart,
        ) -> OperationFuture<'static, Box<dyn EventSource>> {
            assert_ne!(
                start.subscription, "tests_PanicSubscribeSync",
                "synchronous subscription panic"
            );
            Box::pin(async move {
                match start.subscription.as_ref() {
                    "tests_Changed" => Ok(Box::new(SingleEventSource {
                        event: Some(serde_json::Value::String("event".to_owned())),
                    }) as Box<dyn EventSource>),
                    "tests_PanicOpen" => panic!("subscription opening panic"),
                    "tests_PanicEvent" => {
                        Ok(Box::new(PanickingEventSource) as Box<dyn EventSource>)
                    }
                    _ => Err(invalid_request("unknown test subscription")),
                }
            })
        }
    }

    struct SingleEventSource {
        event: Option<serde_json::Value>,
    }

    impl EventSource for SingleEventSource {
        fn recv(&mut self) -> OperationFuture<'_, Option<serde_json::Value>> {
            Box::pin(async move {
                match self.event.take() {
                    Some(event) => Ok(Some(event)),
                    None => pending().await,
                }
            })
        }
    }

    struct PanickingEventSource;

    impl EventSource for PanickingEventSource {
        fn recv(&mut self) -> OperationFuture<'_, Option<serde_json::Value>> {
            Box::pin(async { panic!("subscription event source panic") })
        }
    }

    struct ShutdownService {
        started: Arc<Notify>,
        finish: Arc<Notify>,
        dropped: Option<Arc<Notify>>,
    }

    impl Service for ShutdownService {
        fn invoke(
            &self,
            invocation: wire::RpcInvocation,
        ) -> OperationFuture<'static, serde_json::Value> {
            if invocation.procedure != "tests_Drain" {
                return Box::pin(async move { Ok(invocation.input) });
            }
            let started = Arc::clone(&self.started);
            let finish = Arc::clone(&self.finish);
            let dropped = self.dropped.clone();
            Box::pin(async move {
                let _drop_signal = dropped.map(DropSignal);
                started.notify_one();
                finish.notified().await;
                Ok(invocation.input)
            })
        }

        fn subscribe(
            &self,
            _start: wire::SubscriptionStart,
        ) -> OperationFuture<'static, Box<dyn EventSource>> {
            Box::pin(async { Err(invalid_request("test service has no subscriptions")) })
        }
    }

    struct DropSignal(Arc<Notify>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    #[derive(Default)]
    struct QueueRouter {
        downstreams: Arc<Mutex<VecDeque<Peer>>>,
    }

    impl QueueRouter {
        fn new(downstreams: impl IntoIterator<Item = Peer>) -> Self {
            Self {
                downstreams: Arc::new(Mutex::new(downstreams.into_iter().collect())),
            }
        }
    }

    impl Router for QueueRouter {
        fn resolve(&self, target: wire::Address) -> OperationFuture<'static, Route> {
            if target == wire::Address::Host {
                return Box::pin(async { Ok(Route::Local) });
            }
            let downstreams = Arc::clone(&self.downstreams);
            Box::pin(async move {
                downstreams
                    .lock()
                    .await
                    .pop_front()
                    .map(Route::Forward)
                    .ok_or_else(|| unavailable_error("no downstream test connection"))
            })
        }
    }

    struct TestPeer {
        handle: Handle,
        incoming: Incoming,
    }

    impl TestPeer {
        async fn recv(&mut self) -> Option<wire::Message> {
            self.incoming.recv().await
        }
    }

    fn connect_client(
        server: Server,
    ) -> (Handle, Incoming, [JoinHandle<super::super::Result<()>>; 2]) {
        connect_client_until_shutdown(server, pending())
    }

    fn connect_client_until_shutdown<S>(
        server: Server,
        shutdown: S,
    ) -> (Handle, Incoming, [JoinHandle<super::super::Result<()>>; 2])
    where
        S: Future<Output = ()> + Send + 'static,
    {
        let (server_io, client_io) = duplex(1024 * 1024);
        let server_task = tokio::spawn(async move {
            server
                .serve_until_shutdown(
                    StreamTransport::new(server_io),
                    topology::client_to_hostd(&wire::ClientId::generate()),
                    super::super::Config::default(),
                    shutdown,
                )
                .await
        });
        let (driver, handle, incoming) = super::super::connect(
            StreamTransport::new(client_io),
            DenyAll,
            super::super::Config::default(),
        )
        .expect("configure test client");
        let driver_task = tokio::spawn(driver.run());
        (handle, incoming, [server_task, driver_task])
    }

    fn downstream() -> (Peer, TestPeer, [JoinHandle<super::super::Result<()>>; 2]) {
        let (upstream_io, downstream_io): (DuplexStream, DuplexStream) = duplex(1024 * 1024);
        let server = Server::new(FakeService, QueueRouter::default());
        let (peer, connection) = server
            .connect(
                StreamTransport::new(upstream_io),
                DenyAll,
                super::super::Config::default(),
            )
            .expect("configure upstream side of downstream link");
        let (driver, handle, incoming) = super::super::connect(
            StreamTransport::new(downstream_io),
            topology::hostd_to_guestd(),
            super::super::Config::default(),
        )
        .expect("configure downstream test peer");
        (
            peer,
            TestPeer { handle, incoming },
            [tokio::spawn(connection), tokio::spawn(driver.run())],
        )
    }

    fn rpc(
        id: wire::InvocationId,
        target: wire::Address,
        procedure: &str,
        input: serde_json::Value,
    ) -> wire::Message {
        wire::Message::Rpc(wire::RpcMessage::Invoke(wire::RpcInvocation {
            id,
            target,
            context: None,
            procedure: procedure.into(),
            input,
            timeout_ms: None,
        }))
    }

    fn echo_invocation(input: serde_json::Value) -> wire::RpcInvocation {
        wire::RpcInvocation {
            id: wire::InvocationId::generate(),
            target: wire::Address::Host,
            context: None,
            procedure: "tests_Echo".into(),
            input,
            timeout_ms: None,
        }
    }

    fn subscription(id: wire::SubscriptionId, target: wire::Address) -> wire::Message {
        named_subscription(id, target, "tests_Changed")
    }

    fn named_subscription(
        id: wire::SubscriptionId,
        target: wire::Address,
        name: &str,
    ) -> wire::Message {
        wire::Message::Subscription(wire::SubscriptionMessage::Subscribe(
            wire::SubscriptionStart {
                id,
                target,
                context: None,
                subscription: name.into(),
                input: serde_json::Value::Null,
            },
        ))
    }

    fn credit(id: wire::SubscriptionId, events: u32) -> wire::Message {
        wire::Message::Subscription(wire::SubscriptionMessage::GrantCredit(
            wire::SubscriptionCredit { id, events },
        ))
    }

    fn unsubscribe(id: wire::SubscriptionId) -> wire::Message {
        wire::Message::Subscription(wire::SubscriptionMessage::Unsubscribe(
            wire::SubscriptionStop { id },
        ))
    }

    fn workspace_target() -> wire::Address {
        wire::Address::Workspace(wire::WorkspaceAddress {
            workspace: WorkspaceName::new("alpha"),
        })
    }

    async fn stop_tasks(tasks: [JoinHandle<super::super::Result<()>>; 2]) {
        for task in tasks {
            stop_task(task).await;
        }
    }

    async fn stop_task(task: JoinHandle<super::super::Result<()>>) {
        task.abort();
        match task.await {
            Ok(Ok(())) => {}
            Err(error) if error.is_cancelled() => {}
            Ok(Err(error)) => panic!("control-plane test task failed: {error}"),
            Err(error) => panic!("control-plane test task panicked: {error}"),
        }
    }
}
