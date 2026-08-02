//! Private line-delimited protocol spoken by the bundled Tasci harness.

use std::collections::BTreeMap;

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
    /// Maximum model context in tokens, when configured.
    pub context_window: Option<u64>,
    /// Maximum generated output in tokens, when configured.
    pub max_output_tokens: Option<u64>,
    /// Non-secret authorization header template, when required.
    ///
    /// Host-side HTTP secret injection may replace a placeholder in the value.
    pub authorization: Option<HttpAuthorization>,
    /// Absolute working directory inside the pod.
    pub working_directory: String,
    /// Streamable HTTP MCP servers connected for this session.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfiguration>,
}

/// One Streamable HTTP MCP server connected by the Tasci harness.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpServerConfiguration {
    /// Stable settings name used to namespace discovered tools.
    pub name: String,
    /// Human-readable server name used in warnings and model disclosures.
    pub display_name: String,
    /// Absolute Streamable HTTP endpoint.
    pub endpoint: String,
    /// HTTP header templates sent with every MCP request.
    pub headers: BTreeMap<String, String>,
}

/// Persistent native session requested during harness initialization.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TasciHarnessSession {
    /// Creates an empty append-only session journal.
    Create {
        /// Harness-owned opaque session identifier.
        session_id: String,
    },
    /// Restores and continues an existing session journal.
    Resume {
        /// Harness-owned opaque session identifier.
        session_id: String,
    },
}

/// One command written to a Tasci harness process.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum TasciHarnessCommand {
    /// Initializes the process. This must be the first command.
    Start {
        /// Endpoint, model, and workspace configuration.
        configuration: TasciHarnessConfiguration,
        /// Persistent session operation, or none for an ephemeral process.
        session: Option<TasciHarnessSession>,
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
    /// Summarizes older context while retaining the complete native session
    /// log.
    Compact,
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
    /// A non-fatal harness condition worth presenting to the user.
    Warning {
        /// Stable warning code.
        code: String,
        /// Secret-safe warning description.
        message: String,
    },
    /// The active turn reached a terminal state.
    TurnFinished {
        /// Secret-safe, location-free failure report including nested causes,
        /// or none for a successful turn.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies retry events survive the JSON line protocol without unsupported
    /// 128-bit integers.
    #[test]
    fn retry_event_round_trips_through_json() {
        let event = TasciHarnessEvent::Agent {
            value: AgentEvent::ModelRequestRetrying {
                step: 0,
                attempt: 2,
                delay_ms: 250,
            },
        };

        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: TasciHarnessEvent = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, event);
    }
}
