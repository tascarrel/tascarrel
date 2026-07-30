//! Protocol exposed by the durable state layer.

use tascarrel_api::ids::ChatBindingId;
use tascarrel_api::ids::ChatId;
use tascarrel_api::types::chats::ChatCostCenterId;
use tascarrel_api::types::chats::ChatHarnessKind;
use tascarrel_api::types::chats::ChatModelSelection;
use tascarrel_api::types::pods::PodId;

use crate::services::chats::harness::protocol::HarnessEvent;
use crate::services::chats::harness::protocol::ResumeCursor;

/// Request for creating a durable chat.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CreateChatRequest {
    /// Initial user-facing title.
    pub title: String,
    /// Pod in which the chat's harness runs.
    pub pod_id: PodId,
    /// Workspace-local cost center receiving this chat's usage.
    pub cost_center_id: Option<ChatCostCenterId>,
    /// Harness implementation durably associated with the chat.
    pub harness: ChatHarnessKind,
    /// Initial model selection, or the harness default.
    pub model: Option<ChatModelSelection>,
}

/// Result of creating a durable chat.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CreateChatResult {
    /// Identifier assigned to the chat.
    pub chat_id: ChatId,
}

/// Request to durably ingest one normalized harness event.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct IngestHarnessEventRequest {
    /// Chat to which the event belongs.
    pub chat_id: ChatId,
    /// Runtime binding that emitted the event.
    ///
    /// Turns and structured user-input requests retain this identifier. Clients
    /// use it to relate historical turns to their sessions and determine
    /// whether a request's callback is still current. Request-resolution
    /// events must match the binding that created the request.
    pub binding_id: ChatBindingId,
    /// Normalized event produced by the chat's associated harness.
    pub event: HarnessEvent,
}

/// Result of durably ingesting a harness event.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct IngestHarnessEventResult {}

/// Harness-owned information needed to open a chat's next native session.
///
/// This deliberately excludes outstanding structured user-input requests:
/// native request callbacks belong to the previous running session and cannot
/// be resumed.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HarnessResumption {
    /// Harness implementation durably associated with the chat.
    pub harness: ChatHarnessKind,
    /// Latest effective model selection known to the state layer.
    pub model: Option<ChatModelSelection>,
    /// Latest complete provider-owned resume cursor durably observed by the
    /// state layer.
    pub resume_cursor: Option<ResumeCursor>,
}

/// Error returned by the durable chat state layer.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ChatStateError {
    /// Stable state error category.
    pub kind: ChatStateErrorKind,
    /// Human-readable explanation.
    pub message: String,
}

/// Stable category of a durable chat state failure.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum ChatStateErrorKind {
    /// Input violates a durable state invariant.
    InvalidInput,
    /// The supplied harness event is invalid for the chat.
    InvalidHarnessEvent,
    /// Durable storage failed.
    Storage,
    /// An unexpected state failure occurred.
    Internal,
}
