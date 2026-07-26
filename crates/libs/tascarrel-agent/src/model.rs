//! Provider-neutral model requests and streaming responses.

use std::pin::Pin;

use futures_core::Stream;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::ModelResult;
use crate::ToolDefinition;

/// Asynchronous source of normalized model streams.
pub trait ModelBackend: Send + Sync {
    /// Starts one model request.
    ///
    /// # Errors
    ///
    /// Returns an error when the request cannot be started or its response
    /// stream cannot be established.
    #[must_use]
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, ModelResult<ModelEventStream>>;
}

/// A provider-neutral model request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRequest {
    /// Conversation context supplied to the model.
    pub messages: Vec<ModelMessage>,
    /// Tools available for this request.
    pub tools: Vec<ToolDefinition>,
}

/// One message in provider-neutral conversation context.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ModelMessage {
    /// Harness-authored agent and tool guidance.
    System {
        /// Instructions supplied to the model.
        content: String,
    },
    /// User-authored input.
    User {
        /// Text supplied by the user.
        content: String,
    },
    /// A completed assistant response.
    Assistant(AssistantMessage),
    /// Result of one assistant tool call.
    Tool {
        /// Provider-assigned tool call identifier.
        tool_call_id: String,
        /// Tool name requested by the model.
        tool_name: String,
        /// Model-visible tool output.
        content: String,
        /// Whether execution failed.
        is_error: bool,
    },
}

/// Completed assistant content retained in conversation context.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AssistantMessage {
    /// Visible assistant text.
    pub content: String,
    /// Structured tool calls in provider order.
    pub tool_calls: Vec<ToolCall>,
}

/// A complete structured tool call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCall {
    /// Provider-assigned call identifier.
    pub id: String,
    /// Registered tool name.
    pub name: String,
    /// Complete JSON arguments.
    pub arguments: String,
}

/// One normalized event from a streaming model response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelStreamEvent {
    /// Visible text fragment.
    TextDelta {
        /// Fragment to append.
        delta: String,
    },
    /// Start of a structured tool call.
    ToolCallStarted {
        /// Provider-assigned call identifier.
        id: String,
        /// Registered tool name.
        name: String,
    },
    /// JSON argument fragment for a started tool call.
    ToolCallArgumentsDelta {
        /// Provider-assigned call identifier.
        id: String,
        /// Fragment to append.
        delta: String,
    },
    /// End of a structured tool call.
    ToolCallCompleted {
        /// Provider-assigned call identifier.
        id: String,
    },
    /// Terminal response event.
    Completed {
        /// Reason generation stopped.
        finish_reason: FinishReason,
    },
}

/// Reason a model response stopped.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The model completed its response normally.
    Stop,
    /// The model requested one or more tools.
    ToolCalls,
    /// The provider stopped at an output limit.
    Length,
}

/// Owned stream of normalized model events.
pub type ModelEventStream =
    Pin<Box<dyn Stream<Item = ModelResult<ModelStreamEvent>> + Send + 'static>>;
