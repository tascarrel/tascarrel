//! Model-facing inspection and control for supervised background commands.

use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;

use crate::ProcessRuntime;
use crate::Tool;
use crate::ToolContext;
use crate::ToolDefinition;
use crate::ToolError;
use crate::ToolOutput;
use crate::ToolPrompt;
use crate::ToolResult;

/// Inspects and controls background commands started by [`super::BashTool`].
#[derive(Clone)]
pub struct ProcessTool {
    processes: Arc<ProcessRuntime>,
}

impl ProcessTool {
    /// Creates a process tool sharing the supplied runtime with a shell tool.
    #[must_use]
    pub fn new(processes: Arc<ProcessRuntime>) -> Self {
        Self { processes }
    }
}

impl Tool for ProcessTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "process".to_owned(),
            description: "List, poll, wait for, terminate, kill, or remove background processes started by bash. Output is cursor-based so long-running commands can be observed incrementally.".to_owned(),
            input_schema: r#"{"type":"object","properties":{"action":{"type":"string","enum":["list","poll","wait","terminate","kill","remove"]},"processId":{"type":"string","description":"Required except for list"},"cursor":{"type":"integer","minimum":0,"description":"Output cursor for poll or wait; defaults to 0"},"timeoutMs":{"type":"integer","minimum":1,"description":"Maximum wait duration in milliseconds"}},"required":["action"],"additionalProperties":false}"#.to_owned(),
            prompt: ToolPrompt {
                summary: "Inspect and control supervised background commands".to_owned(),
                guidelines: vec![
                    "Use process to monitor every background command until it exits or is deliberately terminated."
                        .to_owned(),
                    "Pass the returned nextCursor to later poll or wait calls to avoid repeating output."
                        .to_owned(),
                    "Remove completed processes after their final output is no longer needed.".to_owned(),
                ],
            },
        }
    }

    fn execute(
        &self,
        _context: ToolContext,
        arguments: String,
    ) -> BoxFuture<'static, ToolResult<ToolOutput>> {
        let processes = Arc::clone(&self.processes);
        async move {
            let input: ProcessInput = super::parse_arguments("process", &arguments)?;
            let content = match input.action {
                ProcessAction::List => serialize(&processes.list().await)?,
                ProcessAction::Poll => {
                    let id = input.require_process_id()?;
                    serialize(&processes.poll(id, input.cursor.unwrap_or(0)).await?)?
                }
                ProcessAction::Wait => {
                    let id = input.require_process_id()?;
                    let timeout = input
                        .timeout_ms
                        .map_or(DEFAULT_PROCESS_WAIT, Duration::from_millis);
                    serialize(
                        &processes
                            .wait(id, input.cursor.unwrap_or(0), timeout)
                            .await?,
                    )?
                }
                ProcessAction::Terminate => {
                    let id = input.require_process_id()?;
                    processes.terminate(id).await?;
                    format!("Sent terminate to {id}.")
                }
                ProcessAction::Kill => {
                    let id = input.require_process_id()?;
                    processes.kill(id).await?;
                    format!("Sent kill to {id}.")
                }
                ProcessAction::Remove => {
                    let id = input.require_process_id()?;
                    processes.remove(id).await?;
                    format!("Removed {id}.")
                }
            };
            Ok(ToolOutput::text(content))
        }
        .boxed()
    }
}

/// Default long-poll duration for process output.
pub const DEFAULT_PROCESS_WAIT: Duration = Duration::from_secs(1);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProcessInput {
    action: ProcessAction,
    process_id: Option<String>,
    cursor: Option<u64>,
    timeout_ms: Option<u64>,
}

impl ProcessInput {
    fn require_process_id(&self) -> ToolResult<&str> {
        self.process_id
            .as_deref()
            .ok_or_else(|| invalid_arguments("processId is required for this action"))
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProcessAction {
    List,
    Poll,
    Wait,
    Terminate,
    Kill,
    Remove,
}

fn serialize<T>(value: &T) -> ToolResult<String>
where
    T: Serialize,
{
    serde_json::to_string_pretty(value)
        .map_err(|source| process_error(format!("failed to serialize process state: {source}")))
}

fn invalid_arguments(message: impl Into<String>) -> Report<ToolError> {
    Report::new(ToolError::InvalidArguments {
        tool: "process".to_owned(),
        message: message.into(),
    })
}

fn process_error(message: impl Into<String>) -> Report<ToolError> {
    Report::new(ToolError::Process {
        message: message.into(),
    })
}
