//! Failures produced by the agent runtime and its extension interfaces.

use std::io;
use std::path::PathBuf;

use reportify::Report;
use thiserror::Error;

/// Failure while running an agent.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The provider rejected the effective context as too large.
    #[error("model context window was exceeded")]
    ContextOverflow,
    /// A compaction could not be prepared from the current context.
    #[error("the current session has no context that can be compacted")]
    NothingToCompact,
    /// Session data violates native log invariants.
    #[error("agent session is invalid: {reason}")]
    InvalidSession {
        /// Safe description of the violated invariant.
        reason: String,
    },
    /// The configured model produced an invalid event sequence.
    #[error("model produced an invalid event sequence: {reason}")]
    InvalidModelStream {
        /// Safe explanation of the protocol violation.
        reason: String,
    },
    /// The model backend failed.
    #[error("model request failed")]
    Model,
    /// Project instructions could not be read.
    #[error("failed to load project instructions from {path}")]
    ProjectInstructions {
        /// Instruction file that could not be loaded.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: io::Error,
    },
    /// A configured instruction file escaped the workspace.
    #[error("project instruction path {path} is outside the workspace")]
    ProjectInstructionsOutsideWorkspace {
        /// Rejected canonical path.
        path: PathBuf,
    },
    /// The configured model exceeded the agent step limit.
    #[error("agent exceeded its step limit of {limit}")]
    StepLimit {
        /// Configured maximum number of model requests.
        limit: usize,
    },
    /// The run was cancelled.
    #[error("agent run was cancelled")]
    Cancelled,
}

/// Failure while communicating with a model provider.
#[derive(Debug, Error)]
pub enum ModelError {
    /// The provider rejected a request that exceeded its context window.
    #[error("model provider rejected a request that exceeded the context window")]
    ContextOverflow,
    /// The provider transport failed before a complete response arrived.
    #[error("model provider transport failed: {message}")]
    Transport {
        /// Safe transport diagnostic.
        message: String,
    },
    /// A provider request failed.
    #[error("model provider request failed: {message}")]
    Request {
        /// Safe provider diagnostic.
        message: String,
    },
    /// The provider returned malformed protocol data.
    #[error("model provider returned invalid protocol data: {message}")]
    Protocol {
        /// Safe protocol diagnostic.
        message: String,
    },
    /// The request was cancelled.
    #[error("model provider request was cancelled")]
    Cancelled,
}

/// Failure while validating or executing a tool call.
#[derive(Debug, Error)]
pub enum ToolError {
    /// Tool arguments do not conform to the tool contract.
    #[error("invalid arguments for tool {tool}: {message}")]
    InvalidArguments {
        /// Tool receiving the arguments.
        tool: String,
        /// Safe validation diagnostic.
        message: String,
    },
    /// A requested tool is not registered.
    #[error("tool {name} is not available")]
    UnknownTool {
        /// Requested tool name.
        name: String,
    },
    /// A process operation attempted to use a path outside the workspace.
    #[error("path {path} is outside the process workspace")]
    PathOutsideWorkspace {
        /// Rejected path.
        path: PathBuf,
    },
    /// A required file does not exist.
    #[error("file {path} does not exist")]
    MissingFile {
        /// Missing path.
        path: PathBuf,
    },
    /// A file must be read before it can be changed.
    #[error("file {path} must be read before it can be changed")]
    UnreadFile {
        /// Unobserved path.
        path: PathBuf,
    },
    /// A complete rewrite requires the entire current file to be observed.
    #[error("file {path} was only partially read; read the complete file before rewriting it")]
    PartiallyReadFile {
        /// Partially observed path.
        path: PathBuf,
    },
    /// An edit tried to match text that was not shown to the model.
    #[error("edit for {path} uses text outside the ranges returned by read")]
    UnobservedEdit {
        /// Partially observed path.
        path: PathBuf,
    },
    /// A file changed after the agent read it.
    #[error("file {path} changed after it was read; read it again before changing it")]
    StaleFile {
        /// Stale path.
        path: PathBuf,
    },
    /// An exact edit is absent, ambiguous, or overlapping.
    #[error("edit for {path} is invalid: {message}")]
    InvalidEdit {
        /// File targeted by the edit.
        path: PathBuf,
        /// Safe edit diagnostic.
        message: String,
    },
    /// File content is not UTF-8 text.
    #[error("file {path} is not UTF-8 text")]
    NonUtf8File {
        /// Non-text path.
        path: PathBuf,
    },
    /// A supervised process operation could not be completed.
    #[error("process operation failed: {message}")]
    Process {
        /// Safe process diagnostic.
        message: String,
    },
    /// A filesystem operation failed.
    #[error("failed to {action}")]
    Io {
        /// Operation that failed.
        action: &'static str,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },
    /// Tool execution was cancelled.
    #[error("tool execution was cancelled")]
    Cancelled,
}

/// Result returned by agent runs.
pub type AgentResult<T> = Result<T, Report<AgentError>>;

/// Result returned by model backends.
pub type ModelResult<T> = Result<T, Report<ModelError>>;

/// Result returned by tools.
pub type ToolResult<T> = Result<T, Report<ToolError>>;
