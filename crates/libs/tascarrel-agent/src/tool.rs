//! Composable asynchronous tool interfaces and registry.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::FileWorkspace;
use crate::ToolError;
use crate::ToolResult;

/// Asynchronous operation callable by a model.
pub trait Tool: Send + Sync {
    /// Returns the model-visible tool contract.
    fn definition(&self) -> ToolDefinition;

    /// Executes one complete JSON argument document.
    ///
    /// # Errors
    ///
    /// Returns an error when the arguments are invalid, execution fails, or
    /// the call is cancelled.
    #[must_use]
    fn execute(
        &self,
        context: ToolContext,
        arguments: String,
    ) -> BoxFuture<'static, ToolResult<ToolOutput>>;
}

/// Model-visible description of one tool.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolDefinition {
    /// Stable model-safe name.
    pub name: String,
    /// Concise behavioral contract.
    pub description: String,
    /// JSON Schema document for the arguments.
    pub input_schema: String,
    /// Guidance used to compose the agent's system prompt.
    pub prompt: ToolPrompt,
}

/// System-prompt contribution associated with an enabled tool.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolPrompt {
    /// Short capability summary shown in the available-tools list.
    pub summary: String,
    /// Behavioral rules that help the model choose and use the tool.
    pub guidelines: Vec<String>,
}

/// Owned execution context supplied to a tool.
#[derive(Clone)]
pub struct ToolContext {
    /// Revision-aware filesystem access with workspace-relative path
    /// resolution.
    pub files: Arc<FileWorkspace>,
    /// Cooperative cancellation for this call.
    pub cancellation: CancellationToken,
}

/// Model-visible result of a successful tool call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolOutput {
    /// Text returned to the model.
    pub content: String,
    /// Structured results retained for harness and user-interface projection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ToolArtifact>,
}

impl ToolOutput {
    /// Creates a text-only successful result.
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            artifacts: Vec::new(),
        }
    }
}

/// Structured result produced by a tool without adding it to model context.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolArtifact {
    /// One completed batch of file changes.
    FileChanges {
        /// Files changed by the completed mutation.
        changes: Vec<FileChange>,
    },
}

/// Actual before-and-after change made to one file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileChange {
    /// Workspace-relative path for workspace files, or an absolute path.
    pub path: PathBuf,
    /// Filesystem operation represented by the change.
    pub operation: FileChangeOperation,
    /// Standard unified diff generated from committed contents.
    pub unified_diff: String,
    /// Number of added lines in the diff.
    pub additions: usize,
    /// Number of deleted lines in the diff.
    pub deletions: usize,
    /// One-based first changed line in the resulting file.
    pub first_changed_line: Option<usize>,
}

/// Operation represented by a file-change artifact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeOperation {
    /// A previously absent file was created.
    Created,
    /// An existing file was modified.
    Modified,
}

/// Deterministic collection of tools keyed by their model-visible names.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool, rejecting duplicate names.
    ///
    /// # Errors
    ///
    /// Returns an error when another tool already uses the same name.
    pub fn register<T>(&mut self, tool: T) -> ToolResult<()>
    where
        T: Tool + 'static,
    {
        let definition = tool.definition();
        if self.tools.contains_key(&definition.name) {
            return Err(Report::new(ToolError::InvalidArguments {
                tool: definition.name,
                message: "tool name is already registered".to_owned(),
            }));
        }
        self.tools.insert(definition.name, Arc::new(tool));
        Ok(())
    }

    /// Returns a stable snapshot of all model-visible definitions.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    /// Executes a registered tool.
    ///
    /// # Errors
    ///
    /// Returns an error when the tool is unavailable or its execution fails.
    #[must_use]
    pub fn execute(
        &self,
        name: String,
        context: ToolContext,
        arguments: String,
    ) -> BoxFuture<'static, ToolResult<ToolOutput>> {
        let tool = self.tools.get(&name).cloned();
        execute_tool(tool, name, context, arguments).boxed()
    }
}

/// Executes a resolved tool without recording its potentially sensitive
/// arguments.
#[tracing::instrument(level = "debug", skip_all, fields(tool = %name))]
async fn execute_tool(
    tool: Option<Arc<dyn Tool>>,
    name: String,
    context: ToolContext,
    arguments: String,
) -> ToolResult<ToolOutput> {
    let tool = tool.ok_or_else(|| Report::new(ToolError::UnknownTool { name }))?;
    tool.execute(context, arguments).await
}
