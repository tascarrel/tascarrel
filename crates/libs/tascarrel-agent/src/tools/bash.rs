//! Cancellable foreground and supervised background shell commands.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use reportify::Report;
use serde::Deserialize;
use tokio::time::Instant;

use crate::DEFAULT_PROCESS_OBSERVATION_BYTES;
use crate::ProcessObservation;
use crate::ProcessRuntime;
use crate::ProcessStatus;
use crate::Tool;
use crate::ToolContext;
use crate::ToolDefinition;
use crate::ToolError;
use crate::ToolOutput;
use crate::ToolPrompt;
use crate::ToolResult;

/// Executes commands through a shared standalone process runtime.
#[derive(Clone)]
pub struct BashTool {
    processes: Arc<ProcessRuntime>,
    default_timeout: Duration,
    output_byte_limit: usize,
    output_line_limit: usize,
}

impl BashTool {
    /// Creates a shell tool using the default foreground timeout.
    #[must_use]
    pub fn new(processes: Arc<ProcessRuntime>) -> Self {
        Self {
            processes,
            default_timeout: DEFAULT_BASH_TIMEOUT,
            output_byte_limit: DEFAULT_PROCESS_OBSERVATION_BYTES,
            output_line_limit: DEFAULT_BASH_OUTPUT_LINE_LIMIT,
        }
    }

    /// Creates a shell tool with a caller-controlled foreground timeout.
    #[must_use]
    pub fn with_timeout(processes: Arc<ProcessRuntime>, default_timeout: Duration) -> Self {
        Self {
            processes,
            default_timeout,
            output_byte_limit: DEFAULT_PROCESS_OBSERVATION_BYTES,
            output_line_limit: DEFAULT_BASH_OUTPUT_LINE_LIMIT,
        }
    }
}

impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_owned(),
            description: format!(
                "Run a command in a fresh Bash shell inside the workspace. Foreground commands default to a {} ms timeout, return at most {} lines or {} bytes, and are terminated as a process group on timeout or cancellation. Set background=true only for commands that need later supervision with the process tool.",
                self.default_timeout.as_millis(),
                self.output_line_limit,
                self.output_byte_limit
            ),
            input_schema: r#"{"type":"object","properties":{"command":{"type":"string","minLength":1,"description":"Bash command to execute"},"cwd":{"type":"string","description":"Optional workspace-relative or in-workspace absolute working directory"},"timeoutMs":{"type":"integer","minimum":1,"description":"Foreground timeout in milliseconds"},"background":{"type":"boolean","description":"Start under process supervision and return immediately"}},"required":["command"],"additionalProperties":false}"#.to_owned(),
            prompt: ToolPrompt {
                summary: "Run foreground commands or start supervised background commands"
                    .to_owned(),
                guidelines: vec![
                    "Use bash for commands and file discovery such as rg, find, and ls.".to_owned(),
                    "Use read, edit, and write rather than shell commands to inspect or change file contents."
                        .to_owned(),
                    "Use background mode only when the command must continue while other work proceeds."
                        .to_owned(),
                ],
            },
        }
    }

    fn execute(
        &self,
        context: ToolContext,
        arguments: String,
    ) -> BoxFuture<'static, ToolResult<ToolOutput>> {
        let processes = Arc::clone(&self.processes);
        let default_timeout = self.default_timeout;
        let output_byte_limit = self.output_byte_limit;
        let output_line_limit = self.output_line_limit;
        async move {
            let input: BashInput = super::parse_arguments("bash", &arguments)?;
            let snapshot = processes.start(input.command, input.cwd).await?;
            if input.background {
                return Ok(ToolOutput::text(format!(
                    "Started background process {} with pid {}.",
                    snapshot.id,
                    snapshot
                        .process_id
                        .map_or_else(|| "unknown".to_owned(), |id| id.to_string())
                )));
            }

            let timeout = input
                .timeout_ms
                .map_or(default_timeout, Duration::from_millis);
            let result = collect_foreground(
                Arc::clone(&processes),
                &snapshot.id,
                timeout,
                output_byte_limit,
                output_line_limit,
                &context.cancellation,
            )
            .await;
            if result.is_err() {
                if let Err(error) = processes.kill(&snapshot.id).await {
                    tracing::warn!(
                        process = %snapshot.id,
                        error = %error.error(),
                        "failed to stop a foreground process after tool failure"
                    );
                }
                wait_for_terminal(&processes, &snapshot.id, processes.termination_grace()).await;
            }
            if let Err(error) = processes.remove(&snapshot.id).await {
                tracing::warn!(
                    process = %snapshot.id,
                    error = %error.error(),
                    "failed to remove a completed foreground process"
                );
            }
            result
        }
        .boxed()
    }
}

/// Default foreground command timeout.
pub const DEFAULT_BASH_TIMEOUT: Duration = Duration::from_mins(2);

/// Default maximum foreground lines returned to the model.
pub const DEFAULT_BASH_OUTPUT_LINE_LIMIT: usize = 2_000;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BashInput {
    command: String,
    cwd: Option<PathBuf>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    background: bool,
}

async fn collect_foreground(
    processes: Arc<ProcessRuntime>,
    id: &str,
    timeout: Duration,
    output_byte_limit: usize,
    output_line_limit: usize,
    cancellation: &tokio_util::sync::CancellationToken,
) -> ToolResult<ToolOutput> {
    let deadline = Instant::now() + timeout;
    let mut cursor = 0;
    let mut output = CapturedOutput::new(output_byte_limit, output_line_limit);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            stop_timed_out_process(&processes, id).await;
            return Err(process_error(format!(
                "command timed out after {} ms",
                timeout.as_millis()
            )));
        }
        let observation = tokio::select! {
            () = cancellation.cancelled() => {
                if let Err(error) = processes.kill(id).await {
                    tracing::warn!(process = id, error = %error.error(), "failed to kill a cancelled command");
                }
                wait_for_terminal(&processes, id, processes.termination_grace()).await;
                return Err(Report::new(ToolError::Cancelled));
            }
            result = processes.wait(id, cursor, remaining) => result?,
        };
        append_observation(&observation, &mut cursor, &mut output);
        if observation.snapshot.status.is_terminal() {
            return Ok(ToolOutput::text(format_foreground_result(
                &observation.snapshot.status,
                &output.render(),
            )));
        }
    }
}

async fn stop_timed_out_process(processes: &ProcessRuntime, id: &str) {
    if let Err(error) = processes.terminate(id).await {
        tracing::warn!(process = id, error = %error.error(), "failed to terminate a timed-out command");
    }
    if !wait_for_terminal(processes, id, processes.termination_grace()).await {
        if let Err(error) = processes.kill(id).await {
            tracing::warn!(process = id, error = %error.error(), "failed to kill a timed-out command");
        }
        wait_for_terminal(processes, id, processes.termination_grace()).await;
    }
}

async fn wait_for_terminal(processes: &ProcessRuntime, id: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match processes.wait(id, u64::MAX, remaining).await {
            Ok(observation) if observation.snapshot.status.is_terminal() => return true,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(process = id, error = %error.error(), "failed while waiting for process termination");
                return false;
            }
        }
    }
}

fn append_observation(
    observation: &ProcessObservation,
    cursor: &mut u64,
    output: &mut CapturedOutput,
) {
    if observation.output_truncated {
        output.truncated = true;
    }
    output.append(&observation.output);
    *cursor = observation.next_cursor;
}

struct CapturedOutput {
    content: String,
    byte_limit: usize,
    line_limit: usize,
    truncated: bool,
}

impl CapturedOutput {
    fn new(byte_limit: usize, line_limit: usize) -> Self {
        Self {
            content: String::new(),
            byte_limit,
            line_limit,
            truncated: false,
        }
    }

    fn append(&mut self, content: &str) {
        self.content.push_str(content);
        if self.content.len() > self.byte_limit {
            let mut remove = self.content.len() - self.byte_limit;
            while !self.content.is_char_boundary(remove) {
                remove += 1;
            }
            self.content.drain(..remove);
            self.truncated = true;
        }
        let line_count = if self.content.is_empty() {
            0
        } else {
            self.content.bytes().filter(|byte| *byte == b'\n').count()
                + usize::from(!self.content.ends_with('\n'))
        };
        let excess_lines = line_count.saturating_sub(self.line_limit);
        if excess_lines > 0 {
            let remove = self
                .content
                .match_indices('\n')
                .nth(excess_lines - 1)
                .map_or(0, |(index, _)| index + 1);
            self.content.drain(..remove);
            self.truncated = true;
        }
    }

    fn render(&self) -> String {
        if self.truncated {
            format!("[Earlier command output was truncated.]\n{}", self.content)
        } else {
            self.content.clone()
        }
    }
}

fn format_foreground_result(status: &ProcessStatus, output: &str) -> String {
    let status = match status {
        ProcessStatus::Running => "Command is still running.".to_owned(),
        ProcessStatus::Exited { exit_code } => exit_code.map_or_else(
            || "Command was terminated by a signal.".to_owned(),
            |code| format!("Command exited with code {code}."),
        ),
        ProcessStatus::Failed { message } => format!("Command supervision failed: {message}"),
    };
    format!("{status}{}", output_suffix(output))
}

fn output_suffix(output: &str) -> String {
    if output.is_empty() {
        String::new()
    } else {
        format!("\n\n{output}")
    }
}

fn process_error(message: impl Into<String>) -> Report<ToolError> {
    Report::new(ToolError::Process {
        message: message.into(),
    })
}
