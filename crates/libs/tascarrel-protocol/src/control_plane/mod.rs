//! Shared implementation of the Tascarrel control plane protocol.
//!
//! [`connect`] creates a [`Driver`] for one adjacent link together with its
//! [`Handle`] and [`Incoming`] application interfaces. The driver validates
//! operation lifecycles, applies a [`policy::Policy`] to incoming RPC and
//! subscription openings, and enforces subscription event credit.
//! [`policy::topology`] provides policies for authenticated topology links.
//! [`server::Server`] adds operation dispatch, panic containment, graceful
//! shutdown, capacity, and routing through daemon-supplied local services and
//! reusable full-duplex peer connections.
//!
//! [`Transport`] is the boundary for transports that already carry complete
//! protocol messages. [`StreamTransport`] implements that boundary for an
//! asynchronous byte stream such as a Tascarrel mux channel.

use reportify::ErrorExt as _;
use reportify::Report;
use reportify::render::ColorMode;
use reportify::render::RenderOptions;
use tascarrel_api::types::protocol as wire;
use thiserror::Error as ThisError;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::debug;

/// Multiplex endpoint carrying the full-duplex Sidex control plane.
pub const MUX_CONTROL_PLANE_ENDPOINT: &str = "tascarrel-control-plane";

pub mod policy;
pub mod server;
mod state;
mod transport;

use policy::Policy;
use state::LinkState;
use state::Received;
pub use transport::StreamTransport;
pub use transport::Transport;

/// Creates a control plane link over `transport`.
///
/// The returned driver must run while the handle or incoming message stream is
/// in use. The policy is applied to opening messages received from the peer.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] when either queue capacity is zero.
pub fn connect<T, P>(
    transport: T,
    policy: P,
    config: Config,
) -> Result<(Driver<T, P>, Handle, Incoming)>
where
    T: Transport,
    P: Policy,
{
    config.validate()?;
    let (outgoing_sender, outgoing) = mpsc::channel(config.outgoing_queue_capacity);
    let (incoming, incoming_receiver) = mpsc::channel(config.incoming_queue_capacity);
    Ok((
        Driver {
            transport,
            policy,
            outgoing,
            incoming,
            state: LinkState::default(),
        },
        Handle { outgoing_sender },
        Incoming {
            receiver: incoming_receiver,
        },
    ))
}

/// Drives one control plane link.
pub struct Driver<T, P> {
    transport: T,
    policy: P,
    outgoing: mpsc::Receiver<SendCommand>,
    incoming: mpsc::Sender<wire::Message>,
    state: LinkState,
}

impl<T, P> Driver<T, P>
where
    T: Transport,
    P: Policy,
{
    /// Runs the link until the transport reaches EOF or the incoming receiver
    /// is dropped.
    ///
    /// # Errors
    ///
    /// Returns a transport, framing, serialization, or peer protocol error.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn run(mut self) -> Result<()> {
        let mut outgoing_open = true;
        loop {
            let event = tokio::select! {
                incoming = self.transport.receive() => DriverEvent::Incoming(incoming),
                outgoing = self.outgoing.recv(), if outgoing_open => {
                    DriverEvent::Outgoing(outgoing)
                }
                () = self.incoming.closed() => DriverEvent::ConsumerClosed,
            };

            match event {
                DriverEvent::Incoming(incoming) => {
                    let Some(message) = incoming? else {
                        return Ok(());
                    };
                    match self.state.receive(message, &self.policy)? {
                        Received::Deliver(message) => {
                            if self.incoming.send(message).await.is_err() {
                                debug!(
                                    "control plane incoming receiver dropped before receiving a message"
                                );
                                return Ok(());
                            }
                        }
                        Received::Reject(message) => self.transport.send(message).await?,
                        Received::Ignore => {}
                    }
                }
                DriverEvent::Outgoing(Some(command)) => {
                    self.send_command(command).await?;
                }
                DriverEvent::Outgoing(None) => outgoing_open = false,
                DriverEvent::ConsumerClosed => return Ok(()),
            }
        }
    }

    /// Validates and writes one queued local message.
    async fn send_command(&mut self, command: SendCommand) -> Result<()> {
        let update = match self.state.prepare_send(&command.message) {
            Ok(update) => update,
            Err(error) => {
                respond(command.response, Err(error));
                return Ok(());
            }
        };

        if let Err(report) = self.transport.send(command.message).await {
            respond(command.response, Err(Error::ConnectionClosed.report()));
            return Err(report);
        }
        self.state.commit(update);
        respond(command.response, Ok(()));
        Ok(())
    }
}

/// Cloneable sender for one running [`Driver`].
#[derive(Clone, Debug)]
pub struct Handle {
    outgoing_sender: mpsc::Sender<SendCommand>,
}

impl Handle {
    /// Sends one message after validating its local lifecycle transition.
    ///
    /// Queue capacity applies backpressure to concurrent senders.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ConnectionClosed`] after the driver exits, or
    /// [`Error::Protocol`] for a message inconsistent with local operation
    /// state.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn send(&self, message: wire::Message) -> Result<()> {
        let (response, receiver) = oneshot::channel();
        self.outgoing_sender
            .send(SendCommand { message, response })
            .await
            .map_err(|_| Error::ConnectionClosed.report())?;
        receiver
            .await
            .unwrap_or_else(|_| Err(Error::ConnectionClosed.report()))
    }
}

/// Admitted messages received from the peer.
#[derive(Debug)]
pub struct Incoming {
    receiver: mpsc::Receiver<wire::Message>,
}

impl Incoming {
    /// Receives the next admitted message.
    pub async fn recv(&mut self) -> Option<wire::Message> {
        self.receiver.recv().await
    }
}

/// Queue settings for a control plane link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Messages waiting for the driver to write them.
    pub outgoing_queue_capacity: usize,
    /// Admitted peer messages waiting for the application.
    pub incoming_queue_capacity: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            outgoing_queue_capacity: 64,
            incoming_queue_capacity: 64,
        }
    }
}

impl Config {
    /// Validates that both queues can accept messages.
    fn validate(self) -> Result<()> {
        if self.outgoing_queue_capacity == 0 || self.incoming_queue_capacity == 0 {
            Err(Error::InvalidConfig.report())
        } else {
            Ok(())
        }
    }
}

/// A control plane transport or lifecycle failure carried by [`Report`].
#[derive(Clone, Debug, Eq, PartialEq, ThisError)]
pub enum Error {
    /// The underlying framed transport failed.
    #[error("control plane transport failed")]
    Transport,
    /// A control message could not be encoded or decoded.
    #[error("invalid control plane message")]
    InvalidMessage,
    /// A JSON payload exceeds the configured framing limit.
    #[error("control plane frame is {len} bytes, exceeding the {max} byte limit")]
    FrameTooLarge {
        /// Encoded payload length.
        len: usize,
        /// Configured maximum payload length.
        max: usize,
    },
    /// The framing limit cannot be represented by the wire format.
    #[error("maximum frame length must be in 1..={}", u32::MAX)]
    InvalidMaxFrameLen,
    /// A required capacity or forwarding window is zero.
    #[error("invalid control plane configuration")]
    InvalidConfig,
    /// A message violates the operation lifecycle for this link.
    #[error("control plane protocol error: {0}")]
    Protocol(String),
    /// The driver has exited or its transport has failed.
    #[error("control plane connection is closed")]
    ConnectionClosed,
}

/// Result type returned by control plane operations.
pub type Result<T> = std::result::Result<T, Report<Error>>;

/// One source of progress for the link driver.
enum DriverEvent {
    /// A receive operation completed on the transport.
    Incoming(Result<Option<wire::Message>>),
    /// A local sender submitted a message or all senders were dropped.
    Outgoing(Option<SendCommand>),
    /// The application dropped its incoming message receiver.
    ConsumerClosed,
}

/// One locally submitted message and its completion channel.
struct SendCommand {
    message: wire::Message,
    response: oneshot::Sender<Result<()>>,
}

/// Reports a local send result and logs when its receiver has been dropped.
fn respond(response: oneshot::Sender<Result<()>>, result: Result<()>) {
    if response.send(result).is_err() {
        debug!("control plane message sender dropped before receiving its result");
    }
}

/// Renders a reported contract or internal failure into its wire form.
pub(crate) fn render_operation_error(report: Report<wire::OperationError>) -> wire::OperationError {
    let rendered = report.render(RenderOptions::new().color(ColorMode::Never));
    let mut error = report.into_error();
    let details = match &mut error {
        wire::OperationError::InvalidRequest(details)
        | wire::OperationError::Forbidden(details)
        | wire::OperationError::Unavailable(details)
        | wire::OperationError::Overloaded(details)
        | wire::OperationError::TimedOut(details)
        | wire::OperationError::Internal(details) => details,
    };
    if details.report.is_none() {
        details.report = Some(rendered.into());
    }
    error
}

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::*;
    use crate::control_plane::policy::DenyAll;
    use crate::control_plane::policy::PolicyResult;

    /// Verifies policy context assignment and a complete RPC lifecycle.
    #[tokio::test]
    async fn driver_applies_policy_to_rpc_invocations() {
        let (client_io, server_io) = duplex(64 * 1024);
        let expected_context = host_context();
        let (client_driver, client, mut client_incoming) =
            connect(StreamTransport::new(client_io), DenyAll, Config::default())
                .expect("client control configuration");
        let (server_driver, server, mut server_incoming) = connect(
            StreamTransport::new(server_io),
            Admit {
                context: expected_context.clone(),
            },
            Config::default(),
        )
        .expect("server control configuration");
        let client_driver = tokio::spawn(client_driver.run());
        let server_driver = tokio::spawn(server_driver.run());
        let id = wire::InvocationId::generate();

        client
            .send(invocation(id.clone()))
            .await
            .expect("send invocation");
        let wire::Message::Rpc(wire::RpcMessage::Invoke(received)) = server_incoming
            .recv()
            .await
            .expect("server receives invocation")
        else {
            panic!("server received an unexpected message");
        };
        assert_eq!(received.context, Some(expected_context));

        server
            .send(wire::Message::Rpc(wire::RpcMessage::Completed(
                wire::RpcCompleted {
                    id: id.clone(),
                    output: serde_json::Value::Null,
                },
            )))
            .await
            .expect("complete invocation");
        let wire::Message::Rpc(wire::RpcMessage::Completed(completed)) = client_incoming
            .recv()
            .await
            .expect("client receives completion")
        else {
            panic!("client received an unexpected message");
        };
        assert_eq!(completed.id, id);

        drop(client_incoming);
        drop(server_incoming);
        client_driver
            .await
            .expect("client driver task")
            .expect("client driver shutdown");
        server_driver
            .await
            .expect("server driver task")
            .expect("server driver shutdown");
    }

    /// Verifies that a policy rejection becomes a peer-visible operation
    /// failure.
    #[tokio::test]
    async fn driver_returns_policy_rejections_to_the_opener() {
        let (client_io, server_io) = duplex(64 * 1024);
        let (client_driver, client, mut client_incoming) =
            connect(StreamTransport::new(client_io), DenyAll, Config::default())
                .expect("client control configuration");
        let (server_driver, _server, server_incoming) =
            connect(StreamTransport::new(server_io), DenyAll, Config::default())
                .expect("server control configuration");
        let client_driver = tokio::spawn(client_driver.run());
        let server_driver = tokio::spawn(server_driver.run());
        let id = wire::InvocationId::generate();

        client
            .send(invocation(id.clone()))
            .await
            .expect("send invocation");
        let wire::Message::Rpc(wire::RpcMessage::Failed(failed)) = client_incoming
            .recv()
            .await
            .expect("client receives rejection")
        else {
            panic!("client received an unexpected message");
        };
        assert_eq!(failed.id, id);
        let wire::OperationError::Forbidden(details) = failed.error else {
            panic!("policy rejection was not a forbidden error");
        };
        assert!(details.report.is_some());

        drop(client_incoming);
        drop(server_incoming);
        client_driver
            .await
            .expect("client driver task")
            .expect("client driver shutdown");
        server_driver
            .await
            .expect("server driver task")
            .expect("server driver shutdown");
    }

    /// Verifies that subscription events consume consumer-granted credit.
    #[tokio::test]
    async fn driver_enforces_subscription_credit() {
        let (client_io, server_io) = duplex(64 * 1024);
        let context = host_context();
        let (client_driver, client, mut client_incoming) =
            connect(StreamTransport::new(client_io), DenyAll, Config::default())
                .expect("client control configuration");
        let (server_driver, server, mut server_incoming) = connect(
            StreamTransport::new(server_io),
            Admit { context },
            Config::default(),
        )
        .expect("server control configuration");
        let client_driver = tokio::spawn(client_driver.run());
        let server_driver = tokio::spawn(server_driver.run());
        let id = wire::SubscriptionId::generate();

        client
            .send(subscription(id.clone()))
            .await
            .expect("open subscription");
        assert!(matches!(
            server_incoming.recv().await,
            Some(wire::Message::Subscription(
                wire::SubscriptionMessage::Subscribe(_)
            ))
        ));

        let error = server
            .send(subscription_event(id.clone()))
            .await
            .expect_err("event requires credit");
        assert!(matches!(error.error(), Error::Protocol(_)));

        client
            .send(wire::Message::Subscription(
                wire::SubscriptionMessage::GrantCredit(wire::SubscriptionCredit {
                    id: id.clone(),
                    events: 1,
                }),
            ))
            .await
            .expect("grant event credit");
        assert!(matches!(
            server_incoming.recv().await,
            Some(wire::Message::Subscription(
                wire::SubscriptionMessage::GrantCredit(_)
            ))
        ));

        server
            .send(subscription_event(id.clone()))
            .await
            .expect("send credited event");
        assert!(matches!(
            client_incoming.recv().await,
            Some(wire::Message::Subscription(
                wire::SubscriptionMessage::Event(_)
            ))
        ));

        server
            .send(wire::Message::Subscription(
                wire::SubscriptionMessage::Completed(wire::SubscriptionCompleted { id }),
            ))
            .await
            .expect("complete subscription");
        assert!(matches!(
            client_incoming.recv().await,
            Some(wire::Message::Subscription(
                wire::SubscriptionMessage::Completed(_)
            ))
        ));

        drop(client_incoming);
        drop(server_incoming);
        client_driver
            .await
            .expect("client driver task")
            .expect("client driver shutdown");
        server_driver
            .await
            .expect("server driver task")
            .expect("server driver shutdown");
    }

    #[derive(Clone)]
    struct Admit {
        context: wire::RequestContext,
    }

    impl Policy for Admit {
        fn validate_context(&self, _context: Option<&wire::RequestContext>) -> PolicyResult {
            Ok(self.context.clone())
        }
    }

    fn host_context() -> wire::RequestContext {
        wire::RequestContext {
            origin: wire::Actor::Host,
            caller: wire::Actor::Host,
            trace_id: wire::TraceId::generate(),
            caused_by: None,
        }
    }

    fn invocation(id: wire::InvocationId) -> wire::Message {
        wire::Message::Rpc(wire::RpcMessage::Invoke(wire::RpcInvocation {
            id,
            target: wire::Address::Host,
            context: None,
            procedure: "tests_Invoke".into(),
            input: serde_json::Value::Null,
            timeout_ms: None,
        }))
    }

    fn subscription(id: wire::SubscriptionId) -> wire::Message {
        wire::Message::Subscription(wire::SubscriptionMessage::Subscribe(
            wire::SubscriptionStart {
                id,
                target: wire::Address::Host,
                context: None,
                subscription: "tests_Changed".into(),
                input: serde_json::Value::Null,
            },
        ))
    }

    fn subscription_event(id: wire::SubscriptionId) -> wire::Message {
        wire::Message::Subscription(wire::SubscriptionMessage::Event(wire::SubscriptionEvent {
            id,
            event: serde_json::Value::Bool(true),
        }))
    }
}
