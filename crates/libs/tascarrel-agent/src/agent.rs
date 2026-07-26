//! Provider-neutral agent loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::AgentError;
use crate::AgentResult;
use crate::AssistantMessage;
use crate::FileWorkspace;
use crate::FinishReason;
use crate::ModelBackend;
use crate::ModelMessage;
use crate::ModelRequest;
use crate::ModelStreamEvent;
use crate::ToolArtifact;
use crate::ToolCall;
use crate::ToolContext;
use crate::ToolDefinition;
use crate::ToolError;
use crate::ToolRegistry;

/// Provider-neutral coding agent.
pub struct Agent {
    model: Arc<dyn ModelBackend>,
    tools: ToolRegistry,
    files: Arc<FileWorkspace>,
    config: AgentConfig,
}

impl Agent {
    /// Creates an agent from independently composable runtime dependencies.
    #[must_use]
    pub fn new(
        model: Arc<dyn ModelBackend>,
        tools: ToolRegistry,
        files: Arc<FileWorkspace>,
        config: AgentConfig,
    ) -> Self {
        Self {
            model,
            tools,
            files,
            config,
        }
    }

    /// Runs one user prompt through model and tool steps until completion.
    ///
    /// # Errors
    ///
    /// Returns an error when the run is cancelled, the provider fails or
    /// violates the stream contract, or the configured step limit is reached.
    #[must_use]
    pub fn run(
        &self,
        prompt: String,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, AgentResult<AgentRun>> {
        self.run_inner(Vec::new(), prompt, cancellation, None)
            .boxed()
    }

    /// Runs one user prompt while reporting observable events as they occur.
    ///
    /// The handler runs synchronously on the agent task and should return
    /// promptly. The same events remain available in the completed
    /// [`AgentRun`].
    ///
    /// # Errors
    ///
    /// Returns an error when the run is cancelled, the provider fails or
    /// violates the stream contract, or the configured step limit is reached.
    #[must_use]
    pub fn run_with_event_handler(
        &self,
        prompt: String,
        cancellation: CancellationToken,
        event_handler: AgentEventHandler,
    ) -> BoxFuture<'_, AgentResult<AgentRun>> {
        self.run_inner(Vec::new(), prompt, cancellation, Some(event_handler))
            .boxed()
    }

    /// Continues a preceding run while reporting observable events.
    ///
    /// The supplied history should be the `messages` value returned by the
    /// previous [`AgentRun`]. Tasci rebuilds the system prompt only for an
    /// empty history.
    ///
    /// # Errors
    ///
    /// Returns an error when history is malformed, the run is cancelled, the
    /// provider fails, or a configured limit is reached.
    #[must_use]
    pub fn continue_with_event_handler(
        &self,
        history: Vec<ModelMessage>,
        prompt: String,
        cancellation: CancellationToken,
        event_handler: AgentEventHandler,
    ) -> BoxFuture<'_, AgentResult<AgentRun>> {
        self.run_inner(history, prompt, cancellation, Some(event_handler))
            .boxed()
    }

    #[tracing::instrument(level = "info", skip_all)]
    async fn run_inner(
        &self,
        mut messages: Vec<ModelMessage>,
        prompt: String,
        cancellation: CancellationToken,
        event_handler: Option<AgentEventHandler>,
    ) -> AgentResult<AgentRun> {
        let tools = self.tools.definitions();
        if messages.is_empty() {
            let system_prompt =
                crate::prompt::build_system_prompt(&tools, self.files.root(), &self.config).await?;
            messages.push(ModelMessage::System {
                content: system_prompt,
            });
        } else if !matches!(messages.first(), Some(ModelMessage::System { .. })) {
            return invalid_stream("conversation history must start with a system message");
        }
        messages.push(ModelMessage::User { content: prompt });
        let mut events = Vec::new();

        for step in 0..self.config.max_steps {
            ensure_running(&cancellation)?;
            let response = self
                .request_model(
                    step,
                    &messages,
                    &tools,
                    &cancellation,
                    &mut events,
                    event_handler.as_ref(),
                )
                .await?;
            if response.finish_reason == FinishReason::ToolCalls
                && response.message.tool_calls.is_empty()
            {
                return invalid_stream("model reported a tool-call finish without any tool calls");
            }
            messages.push(ModelMessage::Assistant(response.message.clone()));

            if response.message.tool_calls.is_empty() {
                record_event(
                    &mut events,
                    event_handler.as_ref(),
                    AgentEvent::Completed {
                        content: response.message.content.clone(),
                    },
                );
                return Ok(AgentRun { messages, events });
            }

            if response.finish_reason == FinishReason::Length {
                return Err(Report::new(AgentError::InvalidModelStream {
                    reason: "refusing to execute tool calls from a length-limited response"
                        .to_owned(),
                }));
            }

            self.execute_tool_calls(
                response.message.tool_calls,
                &cancellation,
                &mut messages,
                &mut events,
                event_handler.as_ref(),
            )
            .await?;
        }

        Err(Report::new(AgentError::StepLimit {
            limit: self.config.max_steps,
        }))
    }

    async fn request_model(
        &self,
        step: usize,
        messages: &[ModelMessage],
        tools: &[ToolDefinition],
        cancellation: &CancellationToken,
        events: &mut Vec<AgentEvent>,
        event_handler: Option<&AgentEventHandler>,
    ) -> AgentResult<CollectedResponse> {
        let mut attempt = 1;
        loop {
            ensure_running(cancellation)?;
            record_event(
                events,
                event_handler,
                AgentEvent::ModelRequestStarted { step },
            );
            let request = ModelRequest {
                messages: messages.to_vec(),
                tools: tools.to_vec(),
            };
            let model_request = self.model.stream(request, cancellation.child_token());
            let stream = match cancellation.run_until_cancelled(model_request).await {
                Some(Ok(stream)) => stream,
                Some(Err(error))
                    if is_retryable_model_error(&error)
                        && attempt <= self.config.max_model_retries =>
                {
                    attempt += 1;
                    self.wait_for_model_retry(step, attempt, cancellation, events, event_handler)
                        .await?;
                    continue;
                }
                Some(Err(error)) => return Err(model_error(error)),
                None => return Err(Report::new(AgentError::Cancelled)),
            };
            match collect_response(stream, cancellation, events, event_handler).await {
                Ok(response) => return Ok(response),
                Err(ResponseCollectionError::Model(error))
                    if is_retryable_model_error(&error)
                        && attempt <= self.config.max_model_retries =>
                {
                    attempt += 1;
                    self.wait_for_model_retry(step, attempt, cancellation, events, event_handler)
                        .await?;
                }
                Err(ResponseCollectionError::Model(error)) => return Err(model_error(error)),
                Err(ResponseCollectionError::Agent(error)) => return Err(error),
            }
        }
    }

    async fn wait_for_model_retry(
        &self,
        step: usize,
        attempt: usize,
        cancellation: &CancellationToken,
        events: &mut Vec<AgentEvent>,
        event_handler: Option<&AgentEventHandler>,
    ) -> AgentResult<()> {
        record_event(
            events,
            event_handler,
            AgentEvent::ModelRequestRetrying {
                step,
                attempt,
                delay_ms: self.config.model_retry_delay.as_millis(),
            },
        );
        cancellation
            .run_until_cancelled(tokio::time::sleep(self.config.model_retry_delay))
            .await
            .ok_or_else(|| Report::new(AgentError::Cancelled))
    }

    async fn execute_tool_calls(
        &self,
        calls: Vec<ToolCall>,
        cancellation: &CancellationToken,
        messages: &mut Vec<ModelMessage>,
        events: &mut Vec<AgentEvent>,
        event_handler: Option<&AgentEventHandler>,
    ) -> AgentResult<()> {
        for call in calls {
            ensure_running(cancellation)?;
            record_event(
                events,
                event_handler,
                AgentEvent::ToolExecutionStarted {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            );
            let result = self
                .tools
                .execute(
                    call.name.clone(),
                    ToolContext {
                        files: Arc::clone(&self.files),
                        cancellation: cancellation.child_token(),
                    },
                    call.arguments,
                )
                .await;
            match result {
                Ok(output) => {
                    record_event(
                        events,
                        event_handler,
                        AgentEvent::ToolExecutionCompleted {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            content: output.content.clone(),
                            artifacts: output.artifacts.clone(),
                            is_error: false,
                        },
                    );
                    messages.push(ModelMessage::Tool {
                        tool_call_id: call.id,
                        tool_name: call.name,
                        content: output.content,
                        is_error: false,
                    });
                }
                Err(error) => {
                    if matches!(error.error(), ToolError::Cancelled) {
                        return Err(Report::new(AgentError::Cancelled));
                    }
                    let content = error.error().to_string();
                    record_event(
                        events,
                        event_handler,
                        AgentEvent::ToolExecutionCompleted {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            content: content.clone(),
                            artifacts: Vec::new(),
                            is_error: true,
                        },
                    );
                    messages.push(ModelMessage::Tool {
                        tool_call_id: call.id,
                        tool_name: call.name,
                        content,
                        is_error: true,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Agent-loop limits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentConfig {
    /// Maximum model requests in one run.
    pub max_steps: usize,
    /// Number of retries for an interrupted model transport.
    pub max_model_retries: usize,
    /// Delay before retrying an interrupted model transport.
    pub model_retry_delay: Duration,
    /// Workspace-relative project instruction files loaded into the system
    /// prompt.
    pub project_instruction_files: Vec<PathBuf>,
    /// Additional host-controlled system guidance.
    pub additional_instructions: Vec<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 32,
            max_model_retries: DEFAULT_MODEL_RETRIES,
            model_retry_delay: DEFAULT_MODEL_RETRY_DELAY,
            project_instruction_files: vec![PathBuf::from("AGENTS.md")],
            additional_instructions: Vec::new(),
        }
    }
}

/// Default number of retries for an interrupted model transport.
pub const DEFAULT_MODEL_RETRIES: usize = 2;

/// Default delay before retrying an interrupted model transport.
pub const DEFAULT_MODEL_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Completed run retained for persistence and harness projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRun {
    /// Final provider-neutral conversation context.
    pub messages: Vec<ModelMessage>,
    /// Ordered observable run events.
    pub events: Vec<AgentEvent>,
}

/// Synchronous observer invoked for each event emitted by an agent run.
pub type AgentEventHandler = Arc<dyn Fn(&AgentEvent) + Send + Sync + 'static>;

/// Observable event emitted by the agent loop.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A model step started.
    ModelRequestStarted {
        /// Zero-based step index.
        step: usize,
    },
    /// A model step will be retried after an interrupted transport.
    ModelRequestRetrying {
        /// Zero-based step index.
        step: usize,
        /// One-based request attempt about to start.
        attempt: usize,
        /// Configured delay before the next attempt.
        delay_ms: u128,
    },
    /// Visible assistant text arrived.
    TextDelta {
        /// Fragment received from the provider.
        delta: String,
    },
    /// A structured tool call started streaming.
    ToolCallStarted {
        /// Provider-assigned call identifier.
        id: String,
        /// Registered tool name.
        name: String,
    },
    /// A structured tool call finished streaming.
    ToolCallCompleted {
        /// Provider-assigned call identifier.
        id: String,
    },
    /// Tool execution started.
    ToolExecutionStarted {
        /// Provider-assigned call identifier.
        id: String,
        /// Registered tool name.
        name: String,
        /// Complete JSON arguments supplied to the tool.
        arguments: String,
    },
    /// Tool execution finished.
    ToolExecutionCompleted {
        /// Provider-assigned call identifier.
        id: String,
        /// Registered tool name.
        name: String,
        /// Model-visible output or failure.
        content: String,
        /// Structured successful tool results for harness projection.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        artifacts: Vec<ToolArtifact>,
        /// Whether execution failed.
        is_error: bool,
    },
    /// The run completed without further tool calls.
    Completed {
        /// Final visible assistant text.
        content: String,
    },
}

struct CollectedResponse {
    message: AssistantMessage,
    finish_reason: FinishReason,
}

struct PendingToolCall {
    name: String,
    arguments: String,
    completed: bool,
}

enum ResponseCollectionError {
    Agent(Report<AgentError>),
    Model(Report<crate::ModelError>),
}

/// Collects one provider stream while enforcing tool-call lifecycle ordering.
async fn collect_response(
    mut stream: crate::ModelEventStream,
    cancellation: &CancellationToken,
    events: &mut Vec<AgentEvent>,
    event_handler: Option<&AgentEventHandler>,
) -> Result<CollectedResponse, ResponseCollectionError> {
    ensure_running(cancellation).map_err(ResponseCollectionError::Agent)?;
    let mut content = String::new();
    let mut calls = Vec::new();
    let mut call_indexes = HashMap::new();
    let mut finish_reason = None;

    loop {
        let event = cancellation
            .run_until_cancelled(stream.next())
            .await
            .ok_or_else(|| ResponseCollectionError::Agent(Report::new(AgentError::Cancelled)))?;
        let Some(event) = event else {
            break;
        };
        ensure_running(cancellation).map_err(ResponseCollectionError::Agent)?;
        if finish_reason.is_some() {
            return collection_invalid_stream("received an event after the terminal event");
        }
        match event.map_err(ResponseCollectionError::Model)? {
            ModelStreamEvent::TextDelta { delta } => {
                content.push_str(&delta);
                record_event(events, event_handler, AgentEvent::TextDelta { delta });
            }
            ModelStreamEvent::ToolCallStarted { id, name } => {
                if call_indexes.contains_key(&id) {
                    return collection_invalid_stream(
                        "a tool call identifier was started more than once",
                    );
                }
                call_indexes.insert(id.clone(), calls.len());
                calls.push((
                    id.clone(),
                    PendingToolCall {
                        name: name.clone(),
                        arguments: String::new(),
                        completed: false,
                    },
                ));
                record_event(
                    events,
                    event_handler,
                    AgentEvent::ToolCallStarted { id, name },
                );
            }
            ModelStreamEvent::ToolCallArgumentsDelta { id, delta } => {
                let index = call_indexes.get(&id).copied().ok_or_else(|| {
                    ResponseCollectionError::Agent(invalid_stream_report(
                        "arguments arrived before tool-call start",
                    ))
                })?;
                let call = &mut calls[index].1;
                if call.completed {
                    return collection_invalid_stream(
                        "arguments arrived after tool-call completion",
                    );
                }
                call.arguments.push_str(&delta);
            }
            ModelStreamEvent::ToolCallCompleted { id } => {
                let index = call_indexes.get(&id).copied().ok_or_else(|| {
                    ResponseCollectionError::Agent(invalid_stream_report(
                        "an unknown tool call completed",
                    ))
                })?;
                let call = &mut calls[index].1;
                if call.completed {
                    return collection_invalid_stream("a tool call completed more than once");
                }
                call.completed = true;
                record_event(events, event_handler, AgentEvent::ToolCallCompleted { id });
            }
            ModelStreamEvent::Completed {
                finish_reason: reason,
            } => finish_reason = Some(reason),
        }
    }

    ensure_running(cancellation).map_err(ResponseCollectionError::Agent)?;
    let finish_reason = finish_reason.ok_or_else(|| {
        ResponseCollectionError::Agent(invalid_stream_report(
            "stream ended without a terminal event",
        ))
    })?;
    if calls.iter().any(|(_, call)| !call.completed) {
        return collection_invalid_stream("stream ended with an incomplete tool call");
    }

    Ok(CollectedResponse {
        message: AssistantMessage {
            content,
            tool_calls: calls
                .into_iter()
                .map(|(id, call)| ToolCall {
                    id,
                    name: call.name,
                    arguments: call.arguments,
                })
                .collect(),
        },
        finish_reason,
    })
}

fn collection_invalid_stream<T>(reason: impl Into<String>) -> Result<T, ResponseCollectionError> {
    Err(ResponseCollectionError::Agent(invalid_stream_report(
        reason,
    )))
}

/// Retains one event after synchronously notifying the optional observer.
fn record_event(
    events: &mut Vec<AgentEvent>,
    event_handler: Option<&AgentEventHandler>,
    event: AgentEvent,
) {
    if let Some(event_handler) = event_handler {
        event_handler(&event);
    }
    events.push(event);
}

fn is_retryable_model_error(error: &Report<crate::ModelError>) -> bool {
    matches!(error.error(), crate::ModelError::Transport { .. })
}

/// Preserves cancellation as an agent-level cancellation instead of a provider
/// failure.
fn model_error(error: Report<crate::ModelError>) -> Report<AgentError> {
    if matches!(error.error(), crate::ModelError::Cancelled) {
        return error.escalate(AgentError::Cancelled);
    }
    error.escalate(AgentError::Model)
}

fn ensure_running(cancellation: &CancellationToken) -> AgentResult<()> {
    if cancellation.is_cancelled() {
        return Err(Report::new(AgentError::Cancelled));
    }
    Ok(())
}

fn invalid_stream<T>(reason: impl Into<String>) -> AgentResult<T> {
    Err(invalid_stream_report(reason))
}

fn invalid_stream_report(reason: impl Into<String>) -> Report<AgentError> {
    Report::new(AgentError::InvalidModelStream {
        reason: reason.into(),
    })
}
