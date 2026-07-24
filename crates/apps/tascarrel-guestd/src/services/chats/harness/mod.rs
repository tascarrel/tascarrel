//! Interface between the chat engine and a concrete coding harness.
//!
//! This interface is inspired by the harness abstraction in
//! [T3 Code](https://github.com/pingdotgg/t3code).

use std::sync::Arc;

use futures_util::future::BoxFuture;
use protocol::HarnessCommand;
use protocol::HarnessCommandResult;
use protocol::HarnessError;
use protocol::HarnessEvent;
use protocol::StartSessionRequest;
use tascarrel_api::ArcVec;
use tascarrel_api::types::chats::ChatModel;

pub mod protocol;

/// Factory for one kind of coding harness.
pub trait Harness: Send + Sync {
    /// Discovers the models and model-specific options currently offered by the
    /// harness.
    fn models(&self) -> BoxFuture<'_, Result<ArcVec<ChatModel>, HarnessError>>;

    /// Creates a native session or resumes one when the request contains a
    /// cursor.
    fn start_session(
        &self,
        request: StartSessionRequest,
    ) -> BoxFuture<'_, Result<HarnessSession, HarnessError>>;
}

/// Concurrent control interface for one running native harness session.
pub trait HarnessControl: Send + Sync {
    /// Applies one control command to the native session.
    ///
    /// A successful result means the command was handed to the harness, not
    /// necessarily consumed by the model. An adapter must not report a
    /// failure as retryable when the command may already have been handed
    /// to the harness.
    fn apply(
        &self,
        command: HarnessCommand,
    ) -> BoxFuture<'_, Result<HarnessCommandResult, HarnessError>>;
}

/// Single-consumer event interface for one running native harness session.
pub trait HarnessEventStream: Send {
    /// Waits for the next normalized event.
    ///
    /// [`SessionExited`](protocol::HarnessEventPayload::SessionExited) is the
    /// final event for a session shutdown. Its payload distinguishes an
    /// orderly shutdown from a failure. After it has been returned,
    /// subsequent calls return `None`.
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<HarnessEvent>, HarnessError>>;
}

/// Independently usable control and event handles for a started harness
/// session.
pub struct HarnessSession {
    /// Shareable handle used to control the running session.
    ///
    /// The caller should explicitly send [`HarnessCommand::Stop`] when it no
    /// longer needs the session. Dropping the event stream does not stop
    /// the native session.
    pub control: Arc<dyn HarnessControl>,
    /// Ordered, single-consumer stream of normalized session events.
    pub events: Box<dyn HarnessEventStream>,
}
