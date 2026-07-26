//! Provider-neutral coding-agent primitives for Tascarrel.
//!
//! [`Agent`] runs a model until it produces a final response, executing
//! structured tool calls through a [`ToolRegistry`] between model steps.
//! [`ModelBackend`] and [`Tool`] are asynchronous, object-safe extension
//! points that make providers and tool sources independently composable.
//!
//! The built-in file tools share a [`FileWorkspace`]. Paged reads record both
//! file revisions and model-visible byte ranges. Subsequent edits and writes
//! reject stale files or changes based on content the model did not observe.
//! [`ProcessRuntime`] independently supervises local foreground and background
//! commands without depending on a Tascarrel daemon.

#![deny(unsafe_code)]

mod agent;
mod error;
mod file_workspace;
mod harness_protocol;
mod model;
mod process_runtime;
mod prompt;
mod provider;
mod tool;
mod tools;

pub use agent::Agent;
pub use agent::AgentConfig;
pub use agent::AgentEvent;
pub use agent::AgentEventHandler;
pub use agent::AgentRun;
pub use agent::DEFAULT_MODEL_RETRIES;
pub use agent::DEFAULT_MODEL_RETRY_DELAY;
pub use error::AgentError;
pub use error::AgentResult;
pub use error::ModelError;
pub use error::ModelResult;
pub use error::ToolError;
pub use error::ToolResult;
pub use file_workspace::FileWorkspace;
pub use harness_protocol::TasciHarnessCommand;
pub use harness_protocol::TasciHarnessConfiguration;
pub use harness_protocol::TasciHarnessEvent;
pub use model::AssistantMessage;
pub use model::FinishReason;
pub use model::ModelBackend;
pub use model::ModelEventStream;
pub use model::ModelMessage;
pub use model::ModelRequest;
pub use model::ModelStreamEvent;
pub use model::ToolCall;
pub use process_runtime::DEFAULT_PROCESS_OBSERVATION_BYTES;
pub use process_runtime::DEFAULT_PROCESS_TERMINATION_GRACE;
pub use process_runtime::DEFAULT_RETAINED_PROCESS_OUTPUT_BYTES;
pub use process_runtime::ProcessObservation;
pub use process_runtime::ProcessRuntime;
pub use process_runtime::ProcessRuntimeConfig;
pub use process_runtime::ProcessSnapshot;
pub use process_runtime::ProcessStatus;
pub use provider::HttpAuthorization;
pub use provider::OpenAiChatBackend;
pub use tool::FileChange;
pub use tool::FileChangeOperation;
pub use tool::Tool;
pub use tool::ToolArtifact;
pub use tool::ToolContext;
pub use tool::ToolDefinition;
pub use tool::ToolOutput;
pub use tool::ToolPrompt;
pub use tool::ToolRegistry;
pub use tools::BashTool;
pub use tools::DEFAULT_BASH_OUTPUT_LINE_LIMIT;
pub use tools::DEFAULT_BASH_TIMEOUT;
pub use tools::DEFAULT_PROCESS_WAIT;
pub use tools::DEFAULT_READ_BYTE_LIMIT;
pub use tools::DEFAULT_READ_LINE_LIMIT;
pub use tools::EditTool;
pub use tools::ProcessTool;
pub use tools::ReadTool;
pub use tools::WriteTool;
