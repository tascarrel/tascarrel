//! Private line-delimited protocol spoken by the bundled Tasci harness.

use serde::Deserialize;
use serde::Serialize;

use crate::AgentEvent;
use crate::HttpAuthorization;

/// Complete configuration sent before any harness command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TasciHarnessConfiguration {
    /// `OpenAI` Chat Completions API base URL.
    pub base_url: String,
    /// Provider-native model identifier.
    pub model: String,
    /// Complete authorization header, when required.
    pub authorization: Option<HttpAuthorization>,
    /// Absolute working directory inside the pod.
    pub working_directory: String,
}

/// One command written to a Tasci harness process.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum TasciHarnessCommand {
    /// Initializes the process. This must be the first command.
    Start {
        /// Endpoint, model, and workspace configuration.
        configuration: TasciHarnessConfiguration,
    },
    /// Starts one agent turn while retaining prior model context.
    Prompt {
        /// User-authored prompt text.
        prompt: String,
        /// Replacement endpoint and model configuration for this and later
        /// turns.
        configuration: Option<TasciHarnessConfiguration>,
    },
    /// Cooperatively cancels the active turn.
    Interrupt,
    /// Cancels active work and exits the process.
    Stop,
}

/// One event written by a Tasci harness process.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TasciHarnessEvent {
    /// The process initialized and is ready for prompts.
    Started,
    /// An observable coding-agent event.
    Agent {
        /// Provider-neutral agent event.
        value: AgentEvent,
    },
    /// The active turn reached a terminal state.
    TurnFinished {
        /// Secret-safe failure message, or none for a successful turn.
        error: Option<String>,
        /// Whether cooperative cancellation ended the turn.
        cancelled: bool,
    },
    /// The process stopped cleanly.
    Stopped,
    /// The process rejected protocol input or could not initialize.
    Failed {
        /// Secret-safe failure description.
        message: String,
    },
}
