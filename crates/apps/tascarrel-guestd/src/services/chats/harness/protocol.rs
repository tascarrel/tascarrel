//! Shared protocol between the chat engine and concrete coding harness
//! adapters.
//!
//! This protocol is inspired by the harness abstraction in
//! [T3 Code](https://github.com/pingdotgg/t3code).

use jiff::Timestamp;
use tascarrel_api::ArcStr;
use tascarrel_api::ArcVec;
use tascarrel_api::ids::ChatAttachmentId;
use tascarrel_api::ids::ChatItemId;
use tascarrel_api::ids::ChatRequestId;
use tascarrel_api::ids::ChatTurnId;
use tascarrel_api::types::chats::ChatContent;
use tascarrel_api::types::chats::ChatContextUsageAccuracy;
use tascarrel_api::types::chats::ChatFailure;
use tascarrel_api::types::chats::ChatItemContentAppended;
use tascarrel_api::types::chats::ChatItemKind;
use tascarrel_api::types::chats::ChatItemState;
use tascarrel_api::types::chats::ChatModelSelection;
use tascarrel_api::types::chats::ChatQuestion;
use tascarrel_api::types::chats::ChatQuestionAnswer;
use tascarrel_api::types::chats::ChatTurnState;
use tascarrel_api::types::chats::ChatUsageSnapshot;
use tascarrel_api::types::chats::ChatUsageState;

/// Everything an adapter needs to create or resume a session.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StartSessionRequest {
    /// Initial model and model-specific options, or the harness default.
    pub model: Option<ChatModelSelection>,
    /// Provider-owned state used to resume a previous native session.
    ///
    /// Resumption restores conversation context, but does not restore
    /// structured user-input callbacks owned by the previous running
    /// session.
    pub resume_cursor: Option<ResumeCursor>,
}

/// Opaque provider state required to resume a native conversation.
///
/// The engine and durability layer must round-trip this value without
/// inspecting its JSON shape. Only the adaptor that produced a cursor may
/// read the fields it needs to launch its harness again.
///
/// A cursor does not preserve outstanding structured user-input callbacks.
/// Those callbacks belong to the running session that emitted them and become
/// unavailable when that session ends.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ResumeCursor(pub serde_json::Value);

/// Prompt prepared by the engine for delivery to a harness binding.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HarnessPrompt {
    /// User-authored text, or none for an attachment-only prompt.
    pub text: Option<ArcStr>,
    /// Attachments resolved to paths readable by the binding provider.
    pub attachments: ArcVec<HarnessPromptAttachment>,
    /// Model selection for this prompt, or the session's current model when
    /// omitted.
    pub model: Option<ChatModelSelection>,
}

/// One engine-managed attachment made available to a harness binding.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HarnessPromptAttachment {
    /// Stable attachment identifier from the public chat API.
    pub attachment_id: ChatAttachmentId,
    /// User-facing filename.
    pub name: ArcStr,
    /// Canonical MIME media type.
    pub media_type: ArcStr,
    /// Content size in bytes.
    pub size: u64,
    /// SHA-256 digest encoded as lowercase hexadecimal.
    pub digest: ArcStr,
    /// Guestd-visible engine-store path containing the attachment bytes.
    ///
    /// This exists only for adapters that must read and transform content
    /// before sending it to their harness process.
    pub source_path: ArcStr,
    /// Pod-visible path containing the attachment bytes.
    ///
    /// Adapters pass this path to harnesses rather than exposing guestd's
    /// private state path.
    pub path: ArcStr,
}

/// Commands that may be applied to a running harness session.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum HarnessCommand {
    /// Sends immediately, starting a turn when idle or steering the active
    /// turn.
    ///
    /// At least one of `text` or `attachments` must contain prompt content.
    SendPrompt(HarnessPrompt),
    /// Interrupts the active turn, if any, and then immediately sends the
    /// prompt.
    InterruptAndSend(HarnessPrompt),
    /// Interrupts the active turn.
    Interrupt,
    /// Requests compaction of the model context used for subsequent turns.
    CompactContext,
    /// Answers a pending structured user-input request.
    ResolveUserInput {
        /// Request being resolved in this running session.
        ///
        /// Request identifiers from an ended or resumed session are not valid
        /// here.
        request_id: ChatRequestId,
        /// Answers keyed by question identifier.
        answers: ArcVec<ChatQuestionAnswer>,
    },
    /// Stops the native session and its harness process.
    Stop,
}

/// Immediate result of applying a [`HarnessCommand`].
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum HarnessCommandResult {
    /// The command was accepted and has no more specific synchronous result.
    Accepted,
    /// The harness accepted a prompt for delivery to a turn.
    PromptAccepted {
        /// Tascarrel turn started or steered by the prompt.
        ///
        /// When a turn was already active, this is that active turn's
        /// identifier. Otherwise, the adapter allocates and returns a
        /// new turn identifier.
        turn_id: ChatTurnId,
        /// Native turn identifier, when the harness exposes one.
        provider_turn_id: Option<ProviderTurnId>,
    },
    /// The native session stopped successfully.
    Stopped,
}

/// A normalized event emitted by every harness adapter.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HarnessEvent {
    /// Time at which the adapter observed the native event.
    pub occurred_at: Timestamp,
    /// Related Tascarrel turn, when the event belongs to a turn.
    ///
    /// This is the sole canonical turn reference for the event. It is present
    /// on turn events and on item or request events associated with a turn.
    pub turn_id: Option<ChatTurnId>,
    /// Related Tascarrel item, when the event belongs to an item.
    ///
    /// This is the sole canonical item reference for the event. It is present
    /// on item lifecycle and streaming content-append events.
    pub item_id: Option<ChatItemId>,
    /// Related Tascarrel request, when the event belongs to an interaction.
    ///
    /// This is the sole canonical request reference for the event. It is
    /// present on user-input request and resolution events.
    pub request_id: Option<ChatRequestId>,
    /// Provider-native identifiers retained for correlation and diagnostics.
    pub provider_references: ProviderEventReferences,
    /// Normalized event payload.
    pub payload: HarnessEventPayload,
}

/// Provider-neutral current-context counters reported by a harness.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HarnessContextUsage {
    /// Tokens currently counted as occupied by the harness.
    pub used_tokens: u64,
    /// Effective model context-window capacity, when known.
    pub context_window_tokens: Option<u64>,
    /// Whether the occupied-token count was reported or estimated.
    pub accuracy: ChatContextUsageAccuracy,
}

/// Payload of a normalized event produced by a running harness session.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum HarnessEventPayload {
    /// The native session was created or resumed.
    SessionStarted,
    /// The native session changed lifecycle state.
    SessionStateChanged {
        /// New lifecycle state.
        state: SessionState,
        /// Optional provider-supplied explanation for the transition.
        reason: Option<String>,
    },
    /// The native session event stream exited.
    ///
    /// Any unresolved structured user-input requests owned by this session are
    /// no longer answerable. Their binding identifiers distinguish them
    /// from requests owned by a later session.
    SessionExited {
        /// Failure causing the exit, or `None` for a clean exit.
        error: Option<HarnessError>,
    },
    /// The provider supplied newer continuation state.
    ResumeCursorUpdated {
        /// Complete replacement cursor to persist.
        ///
        /// This ordered event is the authoritative source of cursor updates.
        /// The engine persists the latest cursor it has durably
        /// observed and supplies it to a later session start.
        resume_cursor: ResumeCursor,
    },
    /// The active model or its options changed.
    ModelChanged {
        /// New effective model selection.
        model: ChatModelSelection,
    },
    /// The harness supplied or invalidated its current-context observation.
    ContextUsageUpdated {
        /// Complete replacement observation, or none when no observation is
        /// currently available.
        usage: Option<HarnessContextUsage>,
    },
    /// The harness began processing a submitted turn.
    TurnStarted,
    /// The harness observed a newer absolute usage snapshot for the turn.
    ///
    /// Snapshots are turn-local replacements, never deltas. Adapters may emit
    /// provisional updates while a turn is active. A normal terminal turn
    /// lifecycle promotes the latest observation to settled state in the
    /// chat reducer.
    TurnUsageUpdated {
        /// Provider-neutral usage counters for the turn.
        usage: ChatUsageSnapshot,
        /// Stability of this observation.
        state: ChatUsageState,
    },
    /// The harness finished processing a turn.
    TurnCompleted {
        /// Terminal state reported for the turn.
        state: ChatTurnState,
        /// Failure details when the turn did not complete successfully.
        error: Option<ChatFailure>,
    },
    /// The harness began producing a conversation item.
    ItemStarted {
        /// Canonical category assigned to the item.
        kind: ChatItemKind,
    },
    /// The harness appended streaming content to an item.
    ChatItemContentAppended(ChatItemContentAppended),
    /// The harness finished an item and supplied its final snapshot.
    ItemCompleted {
        /// Canonical item category.
        kind: ChatItemKind,
        /// Final canonical item state.
        state: ChatItemState,
        /// Complete final item content.
        content: ArcVec<ChatContent>,
    },
    /// The harness paused to ask one or more structured questions.
    UserInputRequested {
        /// Questions to present to the user.
        questions: ArcVec<ChatQuestion>,
    },
    /// A previously open user-input request was resolved.
    RequestResolved,
    /// A non-fatal runtime condition worth surfacing or logging.
    Warning {
        /// Stable adapter-defined warning code.
        code: String,
        /// Human-readable warning message.
        message: String,
    },
    /// A runtime operation failed.
    Error(HarnessError),
    /// A native event could not be normalized by this adapter version.
    Unknown {
        /// Native event or method name.
        native_type: String,
        /// Redacted description of the native payload retained for diagnostics.
        payload: String,
    },
}

/// Provider-native conversation or session identifier.
#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
pub struct ProviderSessionId(pub String);

/// Provider-native turn identifier.
#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
pub struct ProviderTurnId(pub String);

/// Provider-native item or content-block identifier.
#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
pub struct ProviderItemId(pub String);

/// Provider-native structured user-input request identifier.
#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
pub struct ProviderRequestId(pub String);

/// Provider-native references attached to one event for correlation and
/// diagnostics.
///
/// Engine logic must use the canonical Tascarrel identifiers on
/// [`HarnessEvent`].
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
#[allow(clippy::struct_field_names)] // Prefixes distinguish provider IDs from canonical IDs.
pub struct ProviderEventReferences {
    /// Native conversation or session identifier.
    pub provider_session_id: Option<ProviderSessionId>,
    /// Native turn identifier.
    pub provider_turn_id: Option<ProviderTurnId>,
    /// Native item or content-block identifier.
    pub provider_item_id: Option<ProviderItemId>,
    /// Native user-input request identifier.
    pub provider_request_id: Option<ProviderRequestId>,
}

/// Lifecycle state of a native harness session.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum SessionState {
    /// The harness process or native session is being initialized.
    Starting,
    /// The session is ready to accept a turn.
    Ready,
    /// The session is processing a turn.
    Running,
    /// The session is waiting for a structured user-input response.
    WaitingForInput,
    /// The session ended normally or was explicitly stopped.
    Stopped,
    /// The session ended because of an error.
    Failed,
}

/// Current adapter-side state for a running harness session.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HarnessSessionInfo {
    /// Current lifecycle state.
    pub state: SessionState,
    /// Effective model and options, when known.
    pub model: Option<ChatModelSelection>,
    /// Tascarrel turn currently being processed, when the session is busy.
    pub active_turn_id: Option<ChatTurnId>,
    /// Latest provider-owned continuation state known by the adapter.
    ///
    /// This value is informational. Durable cursor changes are delivered
    /// through [`HarnessEventPayload::ResumeCursorUpdated`].
    pub resume_cursor: Option<ResumeCursor>,
}

/// Stable category used to handle a harness failure.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum HarnessErrorKind {
    /// Adapter or process configuration is invalid.
    InvalidConfiguration,
    /// The requested operation is not supported by this harness.
    UnsupportedOperation,
    /// Persisted continuation state is invalid or unsupported.
    InvalidResumeCursor,
    /// The harness process could not be launched.
    ProcessStart,
    /// The harness process exited unexpectedly.
    ProcessExited,
    /// Native protocol input or output violated the adapter's expectations.
    Protocol,
    /// A valid native request failed.
    RequestFailed,
    /// The requested native session is not active or cannot be found.
    SessionNotFound,
    /// The requested turn is not active or cannot be found.
    TurnNotFound,
    /// An unexpected adapter failure with no more specific category.
    Internal,
}

/// Error returned or emitted by a harness adapter.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HarnessError {
    /// Stable error category.
    pub kind: HarnessErrorKind,
    /// Human-readable explanation safe to surface to the engine.
    pub message: String,
    /// Whether retrying the same operation may succeed without user changes.
    ///
    /// For a command failure, this must be `false` whenever the adapter cannot
    /// prove that the command was not handed to the harness. In particular,
    /// an ambiguously accepted prompt must never be automatically retried.
    pub retryable: bool,
}
