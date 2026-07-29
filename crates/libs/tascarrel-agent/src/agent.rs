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
use crate::AgentSession;
use crate::AssistantMessage;
use crate::CompactionConfig;
use crate::CompactionReason;
use crate::CompactionRecord;
use crate::FileWorkspace;
use crate::FinishReason;
use crate::ModelBackend;
use crate::ModelMessage;
use crate::ModelRequest;
use crate::ModelStreamEvent;
use crate::ModelUsage;
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

struct AgentEventSink<'a> {
    events: &'a mut Vec<AgentEvent>,
    handler: Option<&'a AgentEventHandler>,
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
        self.run_inner(AgentSession::new(), prompt, cancellation, None)
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
        self.run_inner(
            AgentSession::new(),
            prompt,
            cancellation,
            Some(event_handler),
        )
        .boxed()
    }

    /// Continues a preceding run while reporting observable events.
    ///
    /// The supplied session should be the `session` value returned by the
    /// previous [`AgentRun`]. Its append-only log retains original messages
    /// even when the effective model context has been compacted.
    ///
    /// # Errors
    ///
    /// Returns an error when history is malformed, the run is cancelled, the
    /// provider fails, or a configured limit is reached.
    #[must_use]
    pub fn continue_with_event_handler(
        &self,
        session: AgentSession,
        prompt: String,
        cancellation: CancellationToken,
        event_handler: AgentEventHandler,
    ) -> BoxFuture<'_, AgentResult<AgentRun>> {
        self.run_inner(session, prompt, cancellation, Some(event_handler))
            .boxed()
    }

    /// Compacts an idle session while reporting compaction lifecycle events.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no discardable history, cancellation is
    /// requested, or the summary model request fails.
    #[must_use]
    pub fn compact_with_event_handler(
        &self,
        session: AgentSession,
        cancellation: CancellationToken,
        event_handler: AgentEventHandler,
    ) -> BoxFuture<'_, AgentResult<AgentRun>> {
        async move {
            let mut session = session;
            let mut events = Vec::new();
            let result = self
                .compact_session(
                    &mut session,
                    CompactionReason::Manual,
                    &cancellation,
                    &mut events,
                    Some(&event_handler),
                )
                .await;
            if let Err(error) = result {
                record_event(
                    &mut events,
                    Some(&event_handler),
                    AgentEvent::ContextCompactionFailed {
                        reason: CompactionReason::Manual,
                        message: error.error().to_string(),
                    },
                );
                return Err(error);
            }
            Ok(AgentRun { session, events })
        }
        .boxed()
    }

    #[tracing::instrument(level = "info", skip_all)]
    async fn run_inner(
        &self,
        mut session: AgentSession,
        prompt: String,
        cancellation: CancellationToken,
        event_handler: Option<AgentEventHandler>,
    ) -> AgentResult<AgentRun> {
        let tools = self.tools.definitions();
        if session.entries().is_empty() {
            let system_prompt =
                crate::prompt::build_system_prompt(&tools, self.files.root(), &self.config).await?;
            append_session_message(
                &mut session,
                ModelMessage::System {
                    content: system_prompt,
                },
            )?;
        } else {
            effective_messages(&session)?;
        }
        append_session_message(&mut session, ModelMessage::User { content: prompt })?;
        let mut events = Vec::new();
        let mut overflow_recovery_attempted = false;

        for step in 0..self.config.max_steps {
            ensure_running(&cancellation)?;
            let mut event_sink = AgentEventSink {
                events: &mut events,
                handler: event_handler.as_ref(),
            };
            let response = self
                .request_with_overflow_recovery(
                    step,
                    &mut session,
                    &tools,
                    &cancellation,
                    &mut event_sink,
                    &mut overflow_recovery_attempted,
                )
                .await?;
            if response.finish_reason == FinishReason::ToolCalls
                && response.message.tool_calls.is_empty()
            {
                return invalid_stream("model reported a tool-call finish without any tool calls");
            }
            if let Some(usage) = response.message.usage.clone() {
                record_event(
                    &mut events,
                    event_handler.as_ref(),
                    AgentEvent::ModelUsage { usage },
                );
            }
            append_session_message(
                &mut session,
                ModelMessage::Assistant(response.message.clone()),
            )?;

            if response.message.tool_calls.is_empty() {
                record_event(
                    &mut events,
                    event_handler.as_ref(),
                    AgentEvent::Completed {
                        content: response.message.content.clone(),
                    },
                );
                self.compact_completed_session(
                    &mut session,
                    &tools,
                    &cancellation,
                    &mut events,
                    event_handler.as_ref(),
                )
                .await?;
                return Ok(AgentRun { session, events });
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
                &mut session,
                &mut events,
                event_handler.as_ref(),
            )
            .await?;
        }

        Err(Report::new(AgentError::StepLimit {
            limit: self.config.max_steps,
        }))
    }

    async fn request_with_overflow_recovery(
        &self,
        step: usize,
        session: &mut AgentSession,
        tools: &[ToolDefinition],
        cancellation: &CancellationToken,
        event_sink: &mut AgentEventSink<'_>,
        overflow_recovery_attempted: &mut bool,
    ) -> AgentResult<CollectedResponse> {
        loop {
            let messages = effective_messages(session)?;
            match self
                .request_model(
                    step,
                    &messages,
                    tools,
                    cancellation,
                    event_sink.events,
                    event_sink.handler,
                )
                .await
            {
                Ok(response) => return Ok(response),
                Err(error)
                    if matches!(error.error(), AgentError::ContextOverflow)
                        && self.config.compaction.enabled
                        && !*overflow_recovery_attempted =>
                {
                    *overflow_recovery_attempted = true;
                    if let Err(compaction_error) = self
                        .compact_session(
                            session,
                            CompactionReason::Overflow,
                            cancellation,
                            event_sink.events,
                            event_sink.handler,
                        )
                        .await
                    {
                        record_event(
                            event_sink.events,
                            event_sink.handler,
                            AgentEvent::ContextCompactionFailed {
                                reason: CompactionReason::Overflow,
                                message: compaction_error.error().to_string(),
                            },
                        );
                        return Err(compaction_error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn compact_completed_session(
        &self,
        session: &mut AgentSession,
        tools: &[ToolDefinition],
        cancellation: &CancellationToken,
        events: &mut Vec<AgentEvent>,
        event_handler: Option<&AgentEventHandler>,
    ) -> AgentResult<()> {
        if !self.config.compaction.enabled {
            return Ok(());
        }
        let effective = self
            .config
            .compaction
            .effective(self.config.context_window, self.config.max_output_tokens);
        if crate::compaction::should_compact(session, tools, effective)
            .map_err(session_error)?
            .is_none()
        {
            return Ok(());
        }
        if let Err(error) = self
            .compact_session(
                session,
                CompactionReason::Threshold,
                cancellation,
                events,
                event_handler,
            )
            .await
        {
            tracing::warn!(%error, "automatic Tasci context compaction failed");
            record_event(
                events,
                event_handler,
                AgentEvent::ContextCompactionFailed {
                    reason: CompactionReason::Threshold,
                    message: error.error().to_string(),
                },
            );
        }
        Ok(())
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
                max_output_tokens: self.config.max_output_tokens,
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
                delay_ms: duration_millis(self.config.model_retry_delay),
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
        session: &mut AgentSession,
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
                    append_session_message(
                        session,
                        ModelMessage::Tool {
                            tool_call_id: call.id,
                            tool_name: call.name,
                            content: output.content,
                            is_error: false,
                        },
                    )?;
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
                    append_session_message(
                        session,
                        ModelMessage::Tool {
                            tool_call_id: call.id,
                            tool_name: call.name,
                            content,
                            is_error: true,
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    #[tracing::instrument(level = "info", skip_all, fields(?reason))]
    async fn compact_session(
        &self,
        session: &mut AgentSession,
        reason: CompactionReason,
        cancellation: &CancellationToken,
        events: &mut Vec<AgentEvent>,
        event_handler: Option<&AgentEventHandler>,
    ) -> AgentResult<CompactionRecord> {
        ensure_running(cancellation)?;
        let config = self
            .config
            .compaction
            .effective(self.config.context_window, self.config.max_output_tokens);
        let preparation = crate::compaction::prepare_compaction(session, config)
            .map_err(session_error)?
            .ok_or_else(|| Report::new(AgentError::NothingToCompact))?;
        record_event(
            events,
            event_handler,
            AgentEvent::ContextCompactionStarted { reason },
        );
        let history = if let Some(prompt) =
            crate::compaction::history_summary_prompt(&preparation, config.summary_output)
        {
            Some(self.request_summary(prompt, cancellation).await?)
        } else {
            None
        };
        let turn_prefix = if let Some(prompt) =
            crate::compaction::turn_prefix_summary_prompt(&preparation, config.turn_prefix_output)
        {
            Some(self.request_summary(prompt, cancellation).await?)
        } else {
            None
        };
        let summary = crate::compaction::combine_summary(
            &preparation,
            history
                .as_ref()
                .map(|response| response.message.content.clone()),
            turn_prefix
                .as_ref()
                .map(|response| response.message.content.clone()),
        );
        if summary.trim().is_empty() {
            return invalid_stream("compaction model returned an empty summary");
        }
        let usage = crate::compaction::combine_usage(
            history.and_then(|response| response.message.usage),
            turn_prefix.and_then(|response| response.message.usage),
        );
        let mut record = CompactionRecord {
            summary,
            first_kept_entry_id: preparation.first_kept_entry_id,
            tokens_before: preparation.tokens_before,
            estimated_tokens_after: 0,
            usage,
            read_files: preparation.read_files,
            modified_files: preparation.modified_files,
        };
        let mut projected = session.clone();
        projected
            .append_compaction(record.clone())
            .map_err(session_error)?;
        record.estimated_tokens_after =
            crate::compaction::estimate_messages(&effective_messages(&projected)?);
        session
            .append_compaction(record.clone())
            .map_err(session_error)?;
        record_event(
            events,
            event_handler,
            AgentEvent::ContextCompactionCompleted {
                reason,
                summary: record.summary.clone(),
                tokens_before: record.tokens_before,
                estimated_tokens_after: record.estimated_tokens_after,
                usage: record.usage.clone(),
            },
        );
        Ok(record)
    }

    async fn request_summary(
        &self,
        prompt: crate::compaction::SummaryPrompt,
        cancellation: &CancellationToken,
    ) -> AgentResult<CollectedResponse> {
        let request = ModelRequest {
            messages: vec![
                ModelMessage::System {
                    content: prompt.system,
                },
                ModelMessage::User {
                    content: prompt.user,
                },
            ],
            tools: Vec::new(),
            max_output_tokens: Some(prompt.max_output_tokens),
        };
        let mut attempt = 1;
        loop {
            ensure_running(cancellation)?;
            let stream = self
                .model
                .stream(request.clone(), cancellation.child_token())
                .await;
            let stream = match stream {
                Ok(stream) => stream,
                Err(error)
                    if is_retryable_model_error(&error)
                        && attempt <= self.config.max_model_retries =>
                {
                    attempt += 1;
                    cancellation
                        .run_until_cancelled(tokio::time::sleep(self.config.model_retry_delay))
                        .await
                        .ok_or_else(|| Report::new(AgentError::Cancelled))?;
                    continue;
                }
                Err(error) => return Err(model_error(error)),
            };
            let mut ignored_events = Vec::new();
            match collect_response(stream, cancellation, &mut ignored_events, None).await {
                Ok(response)
                    if response.finish_reason != FinishReason::ToolCalls
                        && response.message.tool_calls.is_empty() =>
                {
                    return Ok(response);
                }
                Ok(_) => {
                    return invalid_stream(
                        "compaction model requested a tool despite receiving no tools",
                    );
                }
                Err(ResponseCollectionError::Model(error))
                    if is_retryable_model_error(&error)
                        && attempt <= self.config.max_model_retries =>
                {
                    attempt += 1;
                    cancellation
                        .run_until_cancelled(tokio::time::sleep(self.config.model_retry_delay))
                        .await
                        .ok_or_else(|| Report::new(AgentError::Cancelled))?;
                }
                Err(ResponseCollectionError::Model(error)) => return Err(model_error(error)),
                Err(ResponseCollectionError::Agent(error)) => return Err(error),
            }
        }
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
    /// Model context-window size used for automatic compaction.
    pub context_window: Option<u64>,
    /// Maximum output supported by the selected model.
    pub max_output_tokens: Option<u64>,
    /// Context compaction policy.
    pub compaction: CompactionConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 32,
            max_model_retries: DEFAULT_MODEL_RETRIES,
            model_retry_delay: DEFAULT_MODEL_RETRY_DELAY,
            project_instruction_files: vec![PathBuf::from("AGENTS.md")],
            additional_instructions: Vec::new(),
            context_window: None,
            max_output_tokens: None,
            compaction: CompactionConfig::default(),
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
    /// Complete append-only native session.
    pub session: AgentSession,
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
        /// Configured delay before the next attempt in milliseconds.
        delay_ms: u64,
    },
    /// The provider reported token usage for a primary model response.
    ModelUsage {
        /// Provider-neutral token counters.
        usage: ModelUsage,
    },
    /// Context compaction started.
    ContextCompactionStarted {
        /// Trigger for this compaction.
        reason: CompactionReason,
    },
    /// Context compaction completed and was appended to the session.
    ContextCompactionCompleted {
        /// Trigger for this compaction.
        reason: CompactionReason,
        /// Structured checkpoint used in later requests.
        summary: String,
        /// Context size before compaction.
        tokens_before: u64,
        /// Estimated effective context size after compaction.
        estimated_tokens_after: u64,
        /// Usage of model calls that generated the checkpoint.
        usage: ModelUsage,
    },
    /// Context compaction failed without changing effective context.
    ContextCompactionFailed {
        /// Trigger for this compaction.
        reason: CompactionReason,
        /// Secret-safe failure description.
        message: String,
    },
    /// Model reasoning arrived.
    ReasoningDelta {
        /// Fragment received from the provider.
        delta: String,
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

struct ResponseAccumulator {
    reasoning: String,
    content: String,
    calls: Vec<(String, PendingToolCall)>,
    call_indexes: HashMap<String, usize>,
    finish_reason: Option<FinishReason>,
    usage: Option<ModelUsage>,
}

impl ResponseAccumulator {
    fn new() -> Self {
        Self {
            reasoning: String::new(),
            content: String::new(),
            calls: Vec::new(),
            call_indexes: HashMap::new(),
            finish_reason: None,
            usage: None,
        }
    }

    fn accept(
        &mut self,
        event: ModelStreamEvent,
        events: &mut Vec<AgentEvent>,
        event_handler: Option<&AgentEventHandler>,
    ) -> Result<(), ResponseCollectionError> {
        match event {
            ModelStreamEvent::ReasoningDelta { delta } => {
                self.ensure_open("received reasoning after the terminal event")?;
                self.reasoning.push_str(&delta);
                record_event(events, event_handler, AgentEvent::ReasoningDelta { delta });
            }
            ModelStreamEvent::TextDelta { delta } => {
                self.ensure_open("received text after the terminal event")?;
                self.content.push_str(&delta);
                record_event(events, event_handler, AgentEvent::TextDelta { delta });
            }
            ModelStreamEvent::ToolCallStarted { id, name } => {
                self.start_tool_call(id.clone(), name.clone())?;
                record_event(
                    events,
                    event_handler,
                    AgentEvent::ToolCallStarted { id, name },
                );
            }
            ModelStreamEvent::ToolCallArgumentsDelta { id, delta } => {
                self.append_tool_arguments(&id, &delta)?;
            }
            ModelStreamEvent::ToolCallCompleted { id } => {
                self.complete_tool_call(&id)?;
                record_event(events, event_handler, AgentEvent::ToolCallCompleted { id });
            }
            ModelStreamEvent::Usage { usage } => {
                if self.usage.replace(usage).is_some() {
                    return collection_invalid_stream(
                        "received more than one usage report for a response",
                    );
                }
            }
            ModelStreamEvent::Completed { finish_reason } => {
                if self.finish_reason.replace(finish_reason).is_some() {
                    return collection_invalid_stream(
                        "received more than one terminal event for a response",
                    );
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<CollectedResponse, ResponseCollectionError> {
        let finish_reason = self.finish_reason.ok_or_else(|| {
            ResponseCollectionError::Agent(invalid_stream_report(
                "stream ended without a terminal event",
            ))
        })?;
        validate_collected_response(&self.content, &self.calls)?;
        Ok(build_collected_response(
            self.reasoning,
            self.content,
            self.calls,
            finish_reason,
            self.usage,
        ))
    }

    fn start_tool_call(&mut self, id: String, name: String) -> Result<(), ResponseCollectionError> {
        self.ensure_open("received a tool call after the terminal event")?;
        if self.call_indexes.contains_key(&id) {
            return collection_invalid_stream("a tool call identifier was started more than once");
        }
        self.call_indexes.insert(id.clone(), self.calls.len());
        self.calls.push((
            id,
            PendingToolCall {
                name,
                arguments: String::new(),
                completed: false,
            },
        ));
        Ok(())
    }

    fn append_tool_arguments(
        &mut self,
        id: &str,
        delta: &str,
    ) -> Result<(), ResponseCollectionError> {
        self.ensure_open("received tool arguments after the terminal event")?;
        let index = self.call_indexes.get(id).copied().ok_or_else(|| {
            ResponseCollectionError::Agent(invalid_stream_report(
                "arguments arrived before tool-call start",
            ))
        })?;
        let call = &mut self.calls[index].1;
        if call.completed {
            return collection_invalid_stream("arguments arrived after tool-call completion");
        }
        call.arguments.push_str(delta);
        Ok(())
    }

    fn complete_tool_call(&mut self, id: &str) -> Result<(), ResponseCollectionError> {
        self.ensure_open("completed a tool call after the terminal event")?;
        let index = self.call_indexes.get(id).copied().ok_or_else(|| {
            ResponseCollectionError::Agent(invalid_stream_report("an unknown tool call completed"))
        })?;
        let call = &mut self.calls[index].1;
        if call.completed {
            return collection_invalid_stream("a tool call completed more than once");
        }
        call.completed = true;
        Ok(())
    }

    fn ensure_open(&self, reason: &'static str) -> Result<(), ResponseCollectionError> {
        if self.finish_reason.is_some() {
            return collection_invalid_stream(reason);
        }
        Ok(())
    }
}

/// Collects one provider stream while enforcing tool-call lifecycle ordering.
async fn collect_response(
    mut stream: crate::ModelEventStream,
    cancellation: &CancellationToken,
    events: &mut Vec<AgentEvent>,
    event_handler: Option<&AgentEventHandler>,
) -> Result<CollectedResponse, ResponseCollectionError> {
    ensure_running(cancellation).map_err(ResponseCollectionError::Agent)?;
    let mut response = ResponseAccumulator::new();

    loop {
        let event = cancellation
            .run_until_cancelled(stream.next())
            .await
            .ok_or_else(|| ResponseCollectionError::Agent(Report::new(AgentError::Cancelled)))?;
        let Some(event) = event else {
            break;
        };
        ensure_running(cancellation).map_err(ResponseCollectionError::Agent)?;
        response.accept(
            event.map_err(ResponseCollectionError::Model)?,
            events,
            event_handler,
        )?;
    }

    ensure_running(cancellation).map_err(ResponseCollectionError::Agent)?;
    response.finish()
}

/// Assembles one validated provider response.
fn build_collected_response(
    reasoning: String,
    content: String,
    calls: Vec<(String, PendingToolCall)>,
    finish_reason: FinishReason,
    usage: Option<ModelUsage>,
) -> CollectedResponse {
    CollectedResponse {
        message: AssistantMessage {
            reasoning,
            content,
            tool_calls: calls
                .into_iter()
                .map(|(id, call)| ToolCall {
                    id,
                    name: call.name,
                    arguments: call.arguments,
                })
                .collect(),
            usage,
        },
        finish_reason,
    }
}

fn collection_invalid_stream<T>(reason: impl Into<String>) -> Result<T, ResponseCollectionError> {
    Err(ResponseCollectionError::Agent(invalid_stream_report(
        reason,
    )))
}

/// Rejects terminal streams without one complete model contribution.
fn validate_collected_response(
    content: &str,
    calls: &[(String, PendingToolCall)],
) -> Result<(), ResponseCollectionError> {
    if calls.iter().any(|(_, call)| !call.completed) {
        return collection_invalid_stream("stream ended with an incomplete tool call");
    }
    if content.trim().is_empty() && calls.is_empty() {
        return collection_invalid_stream("model returned no text or tool calls");
    }
    Ok(())
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
    if matches!(error.error(), crate::ModelError::ContextOverflow) {
        return error.escalate(AgentError::ContextOverflow);
    }
    error.escalate(AgentError::Model)
}

fn append_session_message(session: &mut AgentSession, message: ModelMessage) -> AgentResult<()> {
    session
        .append_message(message)
        .map(|_| ())
        .map_err(session_error)
}

fn effective_messages(session: &AgentSession) -> AgentResult<Vec<ModelMessage>> {
    session.effective_messages().map_err(session_error)
}

fn session_error(error: Report<crate::SessionError>) -> Report<AgentError> {
    let reason = error.error().to_string();
    error.escalate(AgentError::InvalidSession { reason })
}

fn ensure_running(cancellation: &CancellationToken) -> AgentResult<()> {
    if cancellation.is_cancelled() {
        return Err(Report::new(AgentError::Cancelled));
    }
    Ok(())
}

/// Converts a retry delay into the JSON-compatible protocol representation.
fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn invalid_stream<T>(reason: impl Into<String>) -> AgentResult<T> {
    Err(invalid_stream_report(reason))
}

fn invalid_stream_report(reason: impl Into<String>) -> Report<AgentError> {
    Report::new(AgentError::InvalidModelStream {
        reason: reason.into(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use futures_util::future::BoxFuture;
    use futures_util::stream;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::ModelError;
    use crate::ModelEventStream;
    use crate::ModelResult;

    /// Verifies one provider overflow compacts append-only session state and
    /// retries with the new checkpoint plus a safe verbatim suffix.
    #[tokio::test]
    async fn context_overflow_compacts_and_retries_once() {
        let directory = tempdir().unwrap();
        let files = Arc::new(FileWorkspace::open(directory.path()).await.unwrap());
        let backend = Arc::new(QueuedBackend {
            replies: Mutex::new(
                vec![
                    BackendReply::Overflow,
                    BackendReply::Text("## Goal\nPreserve earlier work."),
                    BackendReply::Text("## Original Request\nKeep recent work."),
                    BackendReply::Text("Recovered answer."),
                ]
                .into(),
            ),
            requests: Mutex::new(Vec::new()),
        });
        let agent = Agent::new(
            backend.clone(),
            ToolRegistry::new(),
            files,
            AgentConfig {
                max_model_retries: 0,
                context_window: Some(100),
                max_output_tokens: Some(10),
                compaction: CompactionConfig {
                    enabled: true,
                    reserve_tokens: 10,
                    keep_recent_tokens: 10,
                },
                ..AgentConfig::default()
            },
        );
        let mut session = AgentSession::new();
        append(
            &mut session,
            ModelMessage::System {
                content: "system".into(),
            },
        );
        append(
            &mut session,
            ModelMessage::User {
                content: "old request ".repeat(20),
            },
        );
        append(&mut session, assistant("old answer ".repeat(20)));
        append(
            &mut session,
            ModelMessage::User {
                content: "recent request".into(),
            },
        );
        append(&mut session, assistant("recent answer"));

        let run = agent
            .continue_with_event_handler(
                session,
                "continue".into(),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();

        assert!(run.events.iter().any(|event| matches!(
            event,
            AgentEvent::ContextCompactionCompleted {
                reason: CompactionReason::Overflow,
                ..
            }
        )));
        assert_eq!(run.session.messages().count(), 7);
        let requests = backend.requests.lock().await;
        assert_eq!(requests.len(), 4);
        assert!(matches!(
            requests[3].messages.as_slice(),
            [
                ModelMessage::System { .. },
                ModelMessage::ContextSummary { .. },
                ModelMessage::Assistant(_),
                ModelMessage::User { content },
            ] if content == "continue"
        ));
    }

    fn append(session: &mut AgentSession, message: ModelMessage) {
        session.append_message(message).unwrap();
    }

    fn assistant(content: impl Into<String>) -> ModelMessage {
        ModelMessage::Assistant(AssistantMessage {
            reasoning: String::new(),
            content: content.into(),
            tool_calls: Vec::new(),
            usage: None,
        })
    }

    struct QueuedBackend {
        replies: Mutex<VecDeque<BackendReply>>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    impl ModelBackend for QueuedBackend {
        fn stream(
            &self,
            request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> BoxFuture<'_, ModelResult<ModelEventStream>> {
            async move {
                self.requests.lock().await.push(request);
                match self.replies.lock().await.pop_front().unwrap() {
                    BackendReply::Overflow => Err(Report::new(ModelError::ContextOverflow)),
                    BackendReply::Text(content) => Ok(stream::iter([
                        Ok(ModelStreamEvent::TextDelta {
                            delta: content.to_owned(),
                        }),
                        Ok(ModelStreamEvent::Completed {
                            finish_reason: FinishReason::Stop,
                        }),
                    ])
                    .boxed()),
                }
            }
            .boxed()
        }
    }

    enum BackendReply {
        Overflow,
        Text(&'static str),
    }
}
