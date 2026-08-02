//! Tasci native-harness adaptor.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use futures_util::future::BoxFuture;
use jiff::Timestamp;
use tascarrel_agent::AgentEvent;
use tascarrel_agent::CompactionReason;
use tascarrel_agent::HttpAuthorization;
use tascarrel_agent::ModelUsage;
use tascarrel_agent::TasciHarnessCommand;
use tascarrel_agent::TasciHarnessConfiguration;
use tascarrel_agent::TasciHarnessEvent;
use tascarrel_agent::TasciHarnessSession;
use tascarrel_agent::ToolArtifact;
use tascarrel_api::ArcVec;
use tascarrel_api::ids::ChatItemId;
use tascarrel_api::ids::ChatTurnId;
use tascarrel_api::types::chats::ChatContent;
use tascarrel_api::types::chats::ChatContextUsageAccuracy;
use tascarrel_api::types::chats::ChatFailure;
use tascarrel_api::types::chats::ChatItemContentAppended;
use tascarrel_api::types::chats::ChatItemKind;
use tascarrel_api::types::chats::ChatItemState;
use tascarrel_api::types::chats::ChatModel;
use tascarrel_api::types::chats::ChatModelSelection;
use tascarrel_api::types::chats::ChatModelUsage;
use tascarrel_api::types::chats::ChatPromptAttachment;
use tascarrel_api::types::chats::ChatTokenUsage;
use tascarrel_api::types::chats::ChatTurnState;
use tascarrel_api::types::chats::ChatUsageCoverage;
use tascarrel_api::types::chats::ChatUsageSnapshot;
use tascarrel_api::types::chats::ChatUsageState;
use tascarrel_api::types::chats::StructuredContent;
use tascarrel_api::types::chats::TextContent;
use tascarrel_api::types::config as config_api;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::services::chats::harness::Harness;
use crate::services::chats::harness::HarnessControl;
use crate::services::chats::harness::HarnessEventStream;
use crate::services::chats::harness::HarnessSession;
use crate::services::chats::harness::protocol::HarnessCommand;
use crate::services::chats::harness::protocol::HarnessCommandResult;
use crate::services::chats::harness::protocol::HarnessContextUsage;
use crate::services::chats::harness::protocol::HarnessError;
use crate::services::chats::harness::protocol::HarnessErrorKind;
use crate::services::chats::harness::protocol::HarnessEvent;
use crate::services::chats::harness::protocol::HarnessEventPayload;
use crate::services::chats::harness::protocol::HarnessPrompt;
use crate::services::chats::harness::protocol::HarnessPromptAttachment;
use crate::services::chats::harness::protocol::ProviderEventReferences;
use crate::services::chats::harness::protocol::ProviderSessionId;
use crate::services::chats::harness::protocol::ResumeCursor;
use crate::services::chats::harness::protocol::SessionState;
use crate::services::chats::harness::protocol::StartSessionRequest;
use crate::services::chats::process::HarnessProcessControl;
use crate::services::chats::process::HarnessProcessLauncher;
use crate::services::chats::process::HarnessProcessSpec;

const TASCI_RESUME_CURSOR_VERSION: u32 = 1;

/// Adapter for the line-delimited protocol implemented by `tasci-exec`.
pub struct TasciAdaptor {
    executable: PathBuf,
    launcher: Arc<dyn HarnessProcessLauncher>,
    configuration: TasciRuntimeConfiguration,
    configurations: TasciConfigurationStore,
    mcp_servers: ArcVec<config_api::McpServerConfiguration>,
}

impl TasciAdaptor {
    /// Creates an adaptor for one resolved model session.
    #[must_use]
    pub fn new(
        executable: PathBuf,
        launcher: Arc<dyn HarnessProcessLauncher>,
        configuration: TasciRuntimeConfiguration,
        configurations: TasciConfigurationStore,
    ) -> Self {
        Self {
            executable,
            launcher,
            configuration,
            configurations,
            mcp_servers: ArcVec::new(),
        }
    }

    /// Applies the host-resolved MCP catalog to sessions started by this
    /// adaptor.
    #[must_use]
    pub(crate) fn with_mcp_servers(
        mut self,
        mcp_servers: ArcVec<config_api::McpServerConfiguration>,
    ) -> Self {
        self.mcp_servers = mcp_servers;
        self
    }
}

impl Harness for TasciAdaptor {
    fn models(&self) -> BoxFuture<'_, Result<ArcVec<ChatModel>, HarnessError>> {
        Box::pin(std::future::ready(Ok(self.configuration.models.clone())))
    }

    fn start_session(
        &self,
        request: StartSessionRequest,
    ) -> BoxFuture<'_, Result<HarnessSession, HarnessError>> {
        Box::pin(async move {
            let requested_session = resolve_native_session(request.resume_cursor.as_ref())?;
            if let Some(selection) = request.model
                && selection != self.configuration.selection
            {
                return Err(harness_error(
                    HarnessErrorKind::InvalidConfiguration,
                    "the resolved Tasci model differs from the requested selection",
                ));
            }
            let process = self
                .launcher
                .launch(HarnessProcessSpec {
                    title: "Tasci chat harness".to_owned(),
                    executable: self.executable.clone(),
                    arguments: vec!["--harness".to_owned()],
                    environment: HashMap::new(),
                    working_directory: PathBuf::from("/workspace"),
                })
                .await?;
            write_command(
                &process.control,
                TasciHarnessCommand::Start {
                    configuration: harness_configuration(
                        self.configuration.harness.clone(),
                        &self.mcp_servers,
                    ),
                    session: Some(requested_session.protocol),
                },
            )
            .await?;
            let mut lines = BufReader::new(process.stdout).lines();
            let started = read_event(&mut lines).await?;
            match started {
                TasciHarnessEvent::Started => {}
                TasciHarnessEvent::Failed { message } => {
                    return Err(harness_error(
                        if requested_session.resuming {
                            HarnessErrorKind::InvalidResumeCursor
                        } else {
                            HarnessErrorKind::ProcessStart
                        },
                        message,
                    ));
                }
                _ => {
                    return Err(harness_error(
                        HarnessErrorKind::Protocol,
                        "Tasci did not acknowledge harness initialization",
                    ));
                }
            }

            let state = Arc::new(Mutex::new(TasciSessionState {
                active_turn: None,
                reasoning_item: None,
                assistant_item: None,
                tool_items: HashMap::new(),
                compaction_item: None,
                turn_usage: ModelUsage::default(),
                current_model: self.configuration.selection.clone(),
                stopped: false,
            }));
            let (events, receiver) = mpsc::unbounded_channel();
            let control_events = events.clone();
            emit_session_started(
                &events,
                &requested_session.provider_session_id,
                &self.configuration.selection,
            )?;
            let reader_state = Arc::clone(&state);
            let reader_control = Arc::clone(&process.control);
            tokio::spawn(async move {
                read_harness_events(lines, reader_state, events).await;
                if let Err(error) = reader_control.stop().await {
                    tracing::debug!(message = %error.message, "failed to reap Tasci harness process");
                }
            });
            Ok(HarnessSession {
                control: Arc::new(TasciControl {
                    process: process.control,
                    state,
                    configurations: self.configurations.clone(),
                    mcp_servers: self.mcp_servers.clone(),
                    events: control_events,
                }),
                events: Box::new(TasciEvents { receiver }),
            })
        })
    }
}

/// Runtime endpoint selected and resolved by hostd.
#[derive(Clone)]
pub struct TasciRuntimeConfiguration {
    /// Workspace-local model selection visible to the chat engine.
    pub selection: ChatModelSelection,
    /// Endpoint configuration sent to the pod harness.
    pub harness: TasciHarnessConfiguration,
    /// Secret-free current catalog.
    pub models: ArcVec<ChatModel>,
}

/// Shared cache of host-resolved Tasci endpoint configurations.
#[derive(Clone, Default)]
pub struct TasciConfigurationStore {
    inner: Arc<Mutex<TasciConfigurationCatalog>>,
}

impl TasciConfigurationStore {
    /// Inserts a host-resolved model and returns its runtime configuration.
    pub fn configure(
        &self,
        output: tascarrel_api::types::config::ResolveTasciModelOutput,
    ) -> TasciRuntimeConfiguration {
        let default_model = output.default_model.to_string();
        let configuration = TasciRuntimeConfiguration::from(output);
        let mut catalog = self
            .inner
            .lock()
            .expect("Tasci configuration updates do not invoke user code");
        catalog.default_model = Some(default_model);
        catalog.models.insert(
            configuration.selection.model.to_string(),
            configuration.clone(),
        );
        configuration
    }

    /// Returns the configured default model, falling back to any resolved
    /// model.
    #[must_use]
    pub fn default_configuration(&self) -> Option<TasciRuntimeConfiguration> {
        let catalog = self
            .inner
            .lock()
            .expect("Tasci configuration reads do not invoke user code");
        catalog
            .default_model
            .as_ref()
            .and_then(|model| catalog.models.get(model))
            .cloned()
            .or_else(|| catalog.models.values().next().cloned())
    }

    /// Returns one previously resolved model configuration.
    #[must_use]
    pub fn configuration(&self, model: &str) -> Option<TasciRuntimeConfiguration> {
        self.inner
            .lock()
            .expect("Tasci configuration reads do not invoke user code")
            .models
            .get(model)
            .cloned()
    }
}

#[derive(Default)]
struct TasciConfigurationCatalog {
    default_model: Option<String>,
    models: HashMap<String, TasciRuntimeConfiguration>,
}

struct TasciControl {
    process: Arc<dyn HarnessProcessControl>,
    state: Arc<Mutex<TasciSessionState>>,
    configurations: TasciConfigurationStore,
    mcp_servers: ArcVec<config_api::McpServerConfiguration>,
    events: mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
}

impl HarnessControl for TasciControl {
    fn apply(
        &self,
        command: HarnessCommand,
    ) -> BoxFuture<'_, Result<HarnessCommandResult, HarnessError>> {
        Box::pin(async move {
            match command {
                HarnessCommand::SendPrompt(prompt) => self.send_prompt(prompt).await,
                HarnessCommand::InterruptAndSend(_) => Err(harness_error(
                    HarnessErrorKind::UnsupportedOperation,
                    "Tasci cannot atomically interrupt and replace a prompt",
                )),
                HarnessCommand::Interrupt => {
                    write_command(&self.process, TasciHarnessCommand::Interrupt).await?;
                    Ok(HarnessCommandResult::Accepted)
                }
                HarnessCommand::CompactContext => self.compact_context().await,
                HarnessCommand::Stop => {
                    {
                        let mut state = lock(&self.state);
                        if state.stopped {
                            return Ok(HarnessCommandResult::Stopped);
                        }
                        state.stopped = true;
                    }
                    write_command(&self.process, TasciHarnessCommand::Stop).await?;
                    Ok(HarnessCommandResult::Stopped)
                }
                HarnessCommand::ResolveUserInput { .. } => Err(harness_error(
                    HarnessErrorKind::UnsupportedOperation,
                    "Tasci does not support this harness command",
                )),
            }
        })
    }
}

impl TasciControl {
    async fn compact_context(&self) -> Result<HarnessCommandResult, HarnessError> {
        let turn_id = {
            let mut state = lock(&self.state);
            if state.stopped {
                return Err(harness_error(
                    HarnessErrorKind::SessionNotFound,
                    "the Tasci session has stopped",
                ));
            }
            if state.active_turn.is_some() {
                return Err(harness_error(
                    HarnessErrorKind::UnsupportedOperation,
                    "Tasci cannot compact context during an active turn",
                ));
            }
            let turn_id = ChatTurnId::generate();
            state.active_turn = Some(ActiveTasciTurn {
                id: turn_id.clone(),
                user_item_id: ChatItemId::generate(),
                user_content: None,
                changed_model: None,
                presentation_started: false,
            });
            state.turn_usage = ModelUsage::default();
            turn_id
        };
        if let Err(error) = write_command(&self.process, TasciHarnessCommand::Compact).await {
            let mut state = lock(&self.state);
            if state
                .active_turn
                .as_ref()
                .is_some_and(|turn| turn.id == turn_id)
            {
                state.active_turn = None;
            }
            return Err(error);
        }
        start_turn_presentation(&self.state, &self.events);
        Ok(HarnessCommandResult::Accepted)
    }

    async fn send_prompt(
        &self,
        prompt: HarnessPrompt,
    ) -> Result<HarnessCommandResult, HarnessError> {
        let user_content = canonical_user_content(prompt.text.clone(), prompt.attachments.clone());
        let mut text = prompt
            .text
            .map_or_else(String::new, |text| text.to_string());
        for attachment in &prompt.attachments {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("Attached file: ");
            text.push_str(attachment.path.as_ref());
        }
        if text.is_empty() {
            return Err(harness_error(
                HarnessErrorKind::InvalidConfiguration,
                "Tasci prompts must contain text or an attachment",
            ));
        }
        let current_model = lock(&self.state).current_model.clone();
        let requested_model = prompt.model.unwrap_or_else(|| current_model.clone());
        let changed_configuration = if requested_model == current_model {
            None
        } else {
            Some(harness_configuration(
                self.configurations
                    .configuration(requested_model.model.as_ref())
                    .ok_or_else(|| {
                        harness_error(
                            HarnessErrorKind::InvalidConfiguration,
                            format!(
                                "the selected Tasci model {} has not been resolved",
                                requested_model.model
                            ),
                        )
                    })?
                    .harness,
                &self.mcp_servers,
            ))
        };
        let turn_id = {
            let mut state = lock(&self.state);
            if state.stopped {
                return Err(harness_error(
                    HarnessErrorKind::SessionNotFound,
                    "the Tasci session has stopped",
                ));
            }
            if state.active_turn.is_some() {
                return Err(harness_error(
                    HarnessErrorKind::UnsupportedOperation,
                    "Tasci does not support steering an active turn",
                ));
            }
            let turn_id = ChatTurnId::generate();
            state.active_turn = Some(ActiveTasciTurn {
                id: turn_id.clone(),
                user_item_id: ChatItemId::generate(),
                user_content: Some(user_content),
                changed_model: (requested_model != current_model).then(|| requested_model.clone()),
                presentation_started: false,
            });
            state.current_model = requested_model;
            state.turn_usage = ModelUsage::default();
            turn_id
        };
        if let Err(error) = write_command(
            &self.process,
            TasciHarnessCommand::Prompt {
                prompt: text,
                configuration: changed_configuration,
            },
        )
        .await
        {
            let mut state = lock(&self.state);
            if state
                .active_turn
                .as_ref()
                .is_some_and(|turn| turn.id == turn_id)
            {
                state.active_turn = None;
                state.current_model = current_model;
            }
            return Err(error);
        }
        start_turn_presentation(&self.state, &self.events);
        Ok(HarnessCommandResult::PromptAccepted {
            turn_id,
            provider_turn_id: None,
        })
    }
}

struct TasciEvents {
    receiver: mpsc::UnboundedReceiver<Result<HarnessEvent, HarnessError>>,
}

impl HarnessEventStream for TasciEvents {
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<HarnessEvent>, HarnessError>> {
        Box::pin(async move { self.receiver.recv().await.transpose() })
    }
}

struct TasciSessionState {
    active_turn: Option<ActiveTasciTurn>,
    reasoning_item: Option<StreamingTextItem>,
    assistant_item: Option<StreamingTextItem>,
    tool_items: HashMap<String, ToolItem>,
    compaction_item: Option<ChatItemId>,
    turn_usage: ModelUsage,
    current_model: ChatModelSelection,
    stopped: bool,
}

struct ActiveTasciTurn {
    id: ChatTurnId,
    user_item_id: ChatItemId,
    user_content: Option<ArcVec<ChatContent>>,
    changed_model: Option<ChatModelSelection>,
    presentation_started: bool,
}

struct StreamingTextItem {
    id: ChatItemId,
    content: String,
}

#[derive(Clone, Copy)]
enum StreamingTextKind {
    Reasoning,
    Assistant,
}

impl StreamingTextKind {
    const fn chat_item_kind(self) -> ChatItemKind {
        match self {
            Self::Reasoning => ChatItemKind::Reasoning,
            Self::Assistant => ChatItemKind::AssistantMessage,
        }
    }
}

struct ToolItem {
    id: ChatItemId,
    name: String,
    arguments: String,
}

async fn read_harness_events(
    mut lines: tokio::io::Lines<BufReader<std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>>>,
    state: Arc<Mutex<TasciSessionState>>,
    output: mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
) {
    loop {
        let event = match read_event(&mut lines).await {
            Ok(event) => event,
            Err(error) => {
                emit(&output, Err(error));
                return;
            }
        };
        if project_event(event, &state, &output) {
            return;
        }
    }
}

fn project_event(
    event: TasciHarnessEvent,
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
) -> bool {
    match event {
        TasciHarnessEvent::Started => emit(
            output,
            Err(harness_error(
                HarnessErrorKind::Protocol,
                "Tasci emitted a duplicate started event",
            )),
        ),
        TasciHarnessEvent::Agent { value } => project_agent_event(value, state, output),
        TasciHarnessEvent::Warning { code, message } => emit_event(
            output,
            base_event(None, None, HarnessEventPayload::Warning { code, message }),
        ),
        TasciHarnessEvent::TurnFinished { error, cancelled } => {
            finish_turn(state, output, error, cancelled);
        }
        TasciHarnessEvent::Stopped => {
            let turn = active_turn_id(state);
            emit_event(
                output,
                base_event(
                    turn,
                    None,
                    HarnessEventPayload::SessionStateChanged {
                        state: SessionState::Stopped,
                        reason: None,
                    },
                ),
            );
            emit_event(
                output,
                base_event(
                    None,
                    None,
                    HarnessEventPayload::SessionExited { error: None },
                ),
            );
            return true;
        }
        TasciHarnessEvent::Failed { message } => emit(
            output,
            Err(harness_error(HarnessErrorKind::ProcessStart, message)),
        ),
    }
    false
}

fn project_agent_event(
    event: AgentEvent,
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
) {
    match event {
        AgentEvent::ModelRequestStarted { step: 0 } => {
            start_turn_presentation(state, output);
        }
        AgentEvent::ModelUsage { usage } => project_usage(state, output, &usage),
        AgentEvent::ContextUsageUpdated {
            used_tokens,
            context_window,
            is_estimated,
        } => project_context_usage(state, output, used_tokens, context_window, is_estimated),
        AgentEvent::ContextCompactionStarted { reason } => {
            start_turn_presentation(state, output);
            complete_streaming_text(state, output, StreamingTextKind::Reasoning);
            complete_streaming_text(state, output, StreamingTextKind::Assistant);
            start_compaction_item(state, output, reason);
        }
        AgentEvent::ContextCompactionCompleted {
            reason,
            summary: _,
            tokens_before,
            estimated_tokens_after,
            usage,
        } => {
            if usage.total_tokens() > 0 {
                project_usage(state, output, &usage);
            }
            finish_compaction_item(
                state,
                output,
                reason,
                tokens_before,
                estimated_tokens_after,
                None,
            );
        }
        AgentEvent::ContextCompactionFailed { reason, message } => {
            finish_compaction_item(state, output, reason, 0, 0, Some(message));
        }
        AgentEvent::ReasoningDelta { delta } => {
            complete_streaming_text(state, output, StreamingTextKind::Assistant);
            append_streaming_text(state, output, delta, StreamingTextKind::Reasoning);
        }
        AgentEvent::TextDelta { delta } => {
            complete_streaming_text(state, output, StreamingTextKind::Reasoning);
            append_streaming_text(state, output, delta, StreamingTextKind::Assistant);
        }
        AgentEvent::ToolExecutionStarted {
            id,
            name,
            arguments,
        } => {
            complete_streaming_text(state, output, StreamingTextKind::Reasoning);
            complete_streaming_text(state, output, StreamingTextKind::Assistant);
            start_tool_item(state, output, id, name, arguments);
        }
        AgentEvent::ToolExecutionCompleted {
            id,
            name: _,
            content,
            artifacts,
            is_error,
        } => finish_tool_item(state, output, &id, content, artifacts, is_error),
        AgentEvent::ModelRequestStarted { .. }
        | AgentEvent::ModelRequestRetrying { .. }
        | AgentEvent::ToolCallStarted { .. }
        | AgentEvent::ToolCallCompleted { .. }
        | AgentEvent::Completed { .. } => {}
    }
}

fn start_turn_presentation(
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
) {
    let presentation = {
        let mut state = lock(state);
        let Some(turn) = state.active_turn.as_mut() else {
            return;
        };
        if turn.presentation_started {
            return;
        }
        turn.presentation_started = true;
        (
            turn.id.clone(),
            turn.user_item_id.clone(),
            turn.user_content.clone(),
            turn.changed_model.clone(),
        )
    };
    let (turn_id, user_item_id, user_content, changed_model) = presentation;
    emit_event(
        output,
        base_event(
            Some(turn_id.clone()),
            None,
            HarnessEventPayload::TurnStarted,
        ),
    );
    emit_event(
        output,
        base_event(
            Some(turn_id.clone()),
            None,
            HarnessEventPayload::SessionStateChanged {
                state: SessionState::Running,
                reason: None,
            },
        ),
    );
    if let Some(model) = changed_model {
        emit_event(
            output,
            base_event(
                Some(turn_id.clone()),
                None,
                HarnessEventPayload::ModelChanged { model },
            ),
        );
    }
    if let Some(user_content) = user_content {
        emit_event(
            output,
            base_event(
                Some(turn_id.clone()),
                Some(user_item_id.clone()),
                HarnessEventPayload::ItemStarted {
                    kind: ChatItemKind::UserMessage,
                },
            ),
        );
        emit_event(
            output,
            base_event(
                Some(turn_id),
                Some(user_item_id),
                HarnessEventPayload::ItemCompleted {
                    kind: ChatItemKind::UserMessage,
                    state: ChatItemState::Completed,
                    content: user_content,
                },
            ),
        );
    }
}

fn project_usage(
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    usage: &ModelUsage,
) {
    let (turn, model, tokens) = {
        let mut state = lock(state);
        state.turn_usage.input_tokens = state
            .turn_usage
            .input_tokens
            .saturating_add(usage.input_tokens);
        state.turn_usage.output_tokens = state
            .turn_usage
            .output_tokens
            .saturating_add(usage.output_tokens);
        state.turn_usage.cache_read_input_tokens = add_optional_usage(
            state.turn_usage.cache_read_input_tokens,
            usage.cache_read_input_tokens,
        );
        state.turn_usage.reasoning_output_tokens = add_optional_usage(
            state.turn_usage.reasoning_output_tokens,
            usage.reasoning_output_tokens,
        );
        (
            state.active_turn.as_ref().map(|turn| turn.id.clone()),
            state.current_model.clone(),
            state.turn_usage.clone(),
        )
    };
    let Some(turn) = turn else {
        return;
    };
    let tokens = ChatTokenUsage {
        input_tokens: tokens.input_tokens,
        output_tokens: tokens.output_tokens,
        cache_read_input_tokens: tokens.cache_read_input_tokens,
        cache_write_input_tokens: None,
        cache_writes_by_ttl: ArcVec::new(),
        reasoning_output_tokens: tokens.reasoning_output_tokens,
    };
    emit_event(
        output,
        base_event(
            Some(turn),
            None,
            HarnessEventPayload::TurnUsageUpdated {
                usage: ChatUsageSnapshot {
                    coverage: ChatUsageCoverage::PrimaryAgent,
                    tokens: tokens.clone(),
                    models: vec![ChatModelUsage {
                        model,
                        tokens,
                        pricing: None,
                        provider_estimated_cost: None,
                    }]
                    .into(),
                    provider_estimated_cost: None,
                },
                state: ChatUsageState::Provisional,
            },
        ),
    );
}

/// Projects Tasci's effective-context observation without changing turn usage.
fn project_context_usage(
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    used_tokens: u64,
    context_window_tokens: Option<u64>,
    is_estimated: bool,
) {
    let turn = lock(state).active_turn.as_ref().map(|turn| turn.id.clone());
    emit_event(
        output,
        base_event(
            turn,
            None,
            HarnessEventPayload::ContextUsageUpdated {
                usage: Some(HarnessContextUsage {
                    used_tokens,
                    context_window_tokens,
                    accuracy: if is_estimated {
                        ChatContextUsageAccuracy::Estimated
                    } else {
                        ChatContextUsageAccuracy::Reported
                    },
                }),
            },
        ),
    );
}

fn start_compaction_item(
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    _reason: CompactionReason,
) {
    let (turn, item) = {
        let mut state = lock(state);
        let turn = state.active_turn.as_ref().map(|turn| turn.id.clone());
        let item = state
            .compaction_item
            .get_or_insert_with(ChatItemId::generate)
            .clone();
        (turn, item)
    };
    emit_event(
        output,
        base_event(
            turn,
            Some(item),
            HarnessEventPayload::ItemStarted {
                kind: ChatItemKind::ContextCompaction,
            },
        ),
    );
}

fn finish_compaction_item(
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    reason: CompactionReason,
    tokens_before: u64,
    estimated_tokens_after: u64,
    error: Option<String>,
) {
    let (turn, item) = {
        let mut state = lock(state);
        (
            state.active_turn.as_ref().map(|turn| turn.id.clone()),
            state
                .compaction_item
                .take()
                .unwrap_or_else(ChatItemId::generate),
        )
    };
    let state = if error.is_some() {
        ChatItemState::Failed
    } else {
        ChatItemState::Completed
    };
    let content = vec![ChatContent::Structured(StructuredContent {
        value: match serde_json::to_value(CompactionPresentation {
            reason,
            tokens_before,
            estimated_tokens_after,
            error,
        }) {
            Ok(value) => value,
            Err(error) => {
                emit(
                    output,
                    Err(harness_error(
                        HarnessErrorKind::Internal,
                        format!("failed to project Tasci compaction: {error}"),
                    )),
                );
                return;
            }
        },
    })]
    .into();
    emit_event(
        output,
        base_event(
            turn,
            Some(item),
            HarnessEventPayload::ItemCompleted {
                kind: ChatItemKind::ContextCompaction,
                state,
                content,
            },
        ),
    );
}

/// Appends one model text stream to its current timeline item.
fn append_streaming_text(
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    delta: String,
    kind: StreamingTextKind,
) {
    let (turn, item, started) = {
        let mut state = lock(state);
        let turn = state.active_turn.as_ref().map(|turn| turn.id.clone());
        let item = match kind {
            StreamingTextKind::Reasoning => &mut state.reasoning_item,
            StreamingTextKind::Assistant => &mut state.assistant_item,
        };
        let started = item.is_none();
        let item = item.get_or_insert_with(|| StreamingTextItem {
            id: ChatItemId::generate(),
            content: String::new(),
        });
        item.content.push_str(&delta);
        (turn, item.id.clone(), started)
    };
    if started {
        emit_event(
            output,
            base_event(
                turn.clone(),
                Some(item.clone()),
                HarnessEventPayload::ItemStarted {
                    kind: kind.chat_item_kind(),
                },
            ),
        );
    }
    emit_event(
        output,
        base_event(
            turn,
            Some(item.clone()),
            HarnessEventPayload::ChatItemContentAppended(ChatItemContentAppended {
                item_id: item,
                delta: delta.into(),
            }),
        ),
    );
}

/// Completes one model text item for the current response.
fn complete_streaming_text(
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    kind: StreamingTextKind,
) {
    let (turn, item) = {
        let mut state = lock(state);
        let item = match kind {
            StreamingTextKind::Reasoning => state.reasoning_item.take(),
            StreamingTextKind::Assistant => state.assistant_item.take(),
        };
        (state.active_turn.as_ref().map(|turn| turn.id.clone()), item)
    };
    let Some(item) = item else {
        return;
    };
    emit_event(
        output,
        base_event(
            turn,
            Some(item.id),
            HarnessEventPayload::ItemCompleted {
                kind: kind.chat_item_kind(),
                state: ChatItemState::Completed,
                content: vec![ChatContent::Text(TextContent {
                    value: item.content.into(),
                })]
                .into(),
            },
        ),
    );
}

fn start_tool_item(
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    id: String,
    name: String,
    arguments: String,
) {
    let (turn, item, kind) = {
        let mut state = lock(state);
        let turn = state.active_turn.as_ref().map(|turn| turn.id.clone());
        let item = ChatItemId::generate();
        let kind = tool_kind(&name);
        state.tool_items.insert(
            id,
            ToolItem {
                id: item.clone(),
                name,
                arguments,
            },
        );
        (turn, item, kind)
    };
    emit_event(
        output,
        base_event(turn, Some(item), HarnessEventPayload::ItemStarted { kind }),
    );
}

fn finish_tool_item(
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    id: &str,
    content: String,
    artifacts: Vec<ToolArtifact>,
    is_error: bool,
) {
    let (turn, tool) = {
        let mut state = lock(state);
        (
            state.active_turn.as_ref().map(|turn| turn.id.clone()),
            state.tool_items.remove(id),
        )
    };
    let Some(tool) = tool else {
        emit(
            output,
            Err(harness_error(
                HarnessErrorKind::Protocol,
                "Tasci completed an unknown tool item",
            )),
        );
        return;
    };
    let kind = tool_kind(&tool.name);
    let value = serde_json::to_value(ToolPresentation {
        tool: tool.name,
        arguments: tool.arguments,
        result: content,
    });
    match value {
        Ok(value) => emit_event(
            output,
            base_event(
                turn.clone(),
                Some(tool.id),
                HarnessEventPayload::ItemCompleted {
                    kind,
                    state: if is_error {
                        ChatItemState::Failed
                    } else {
                        ChatItemState::Completed
                    },
                    content: vec![ChatContent::Structured(StructuredContent { value })].into(),
                },
            ),
        ),
        Err(error) => emit(
            output,
            Err(harness_error(
                HarnessErrorKind::Internal,
                format!("failed to project Tasci tool output: {error}"),
            )),
        ),
    }
    for artifact in artifacts {
        project_artifact(turn.clone(), artifact, output);
    }
}

fn project_artifact(
    turn: Option<ChatTurnId>,
    artifact: ToolArtifact,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
) {
    let item = ChatItemId::generate();
    emit_event(
        output,
        base_event(
            turn.clone(),
            Some(item.clone()),
            HarnessEventPayload::ItemStarted {
                kind: ChatItemKind::FileChange,
            },
        ),
    );
    match serde_json::to_value(artifact) {
        Ok(value) => emit_event(
            output,
            base_event(
                turn,
                Some(item),
                HarnessEventPayload::ItemCompleted {
                    kind: ChatItemKind::FileChange,
                    state: ChatItemState::Completed,
                    content: vec![ChatContent::Structured(StructuredContent { value })].into(),
                },
            ),
        ),
        Err(error) => emit(
            output,
            Err(harness_error(
                HarnessErrorKind::Internal,
                format!("failed to project Tasci file changes: {error}"),
            )),
        ),
    }
}

fn finish_turn(
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    error: Option<String>,
    cancelled: bool,
) {
    start_turn_presentation(state, output);
    complete_streaming_text(state, output, StreamingTextKind::Reasoning);
    complete_streaming_text(state, output, StreamingTextKind::Assistant);
    let turn = {
        let mut state = lock(state);
        let turn = state.active_turn.take().map(|turn| turn.id);
        state.tool_items.clear();
        state.compaction_item = None;
        turn
    };
    let turn_state = if error.is_some() {
        ChatTurnState::Failed
    } else if cancelled {
        ChatTurnState::Interrupted
    } else {
        ChatTurnState::Completed
    };
    let failure = error.map(|message| ChatFailure {
        code: "tasci_run_failed".into(),
        message: message.into(),
    });
    emit_event(
        output,
        base_event(
            turn.clone(),
            None,
            HarnessEventPayload::TurnCompleted {
                state: turn_state,
                error: failure,
            },
        ),
    );
    emit_event(
        output,
        base_event(
            turn,
            None,
            HarnessEventPayload::SessionStateChanged {
                state: SessionState::Ready,
                reason: None,
            },
        ),
    );
}

fn tool_kind(name: &str) -> ChatItemKind {
    if name == "bash" {
        ChatItemKind::CommandExecution
    } else {
        ChatItemKind::ToolCall
    }
}

fn canonical_user_content(
    text: Option<tascarrel_api::ArcStr>,
    attachments: ArcVec<HarnessPromptAttachment>,
) -> ArcVec<ChatContent> {
    let mut content = Vec::new();
    if let Some(text) = text {
        content.push(ChatContent::Text(TextContent { value: text }));
    }
    content.extend(attachments.into_iter().map(|attachment| {
        ChatContent::Attachment(ChatPromptAttachment {
            attachment_id: attachment.attachment_id,
            name: attachment.name,
            media_type: attachment.media_type,
            size: attachment.size,
            digest: attachment.digest,
        })
    }));
    content.into()
}

async fn write_command(
    control: &Arc<dyn HarnessProcessControl>,
    command: TasciHarnessCommand,
) -> Result<(), HarnessError> {
    let mut bytes = serde_json::to_vec(&command).map_err(|error| {
        harness_error(
            HarnessErrorKind::Internal,
            format!("failed to encode Tasci command: {error}"),
        )
    })?;
    bytes.push(b'\n');
    control.write(bytes).await
}

async fn read_event<R>(
    lines: &mut tokio::io::Lines<BufReader<R>>,
) -> Result<TasciHarnessEvent, HarnessError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let line = lines
        .next_line()
        .await
        .map_err(|error| {
            harness_error(
                HarnessErrorKind::Protocol,
                format!("failed to read Tasci event: {error}"),
            )
        })?
        .ok_or_else(|| {
            harness_error(
                HarnessErrorKind::ProcessExited,
                "Tasci harness output ended unexpectedly",
            )
        })?;
    serde_json::from_str(&line).map_err(|error| {
        harness_error(
            HarnessErrorKind::Protocol,
            format!("failed to decode Tasci event: {error}"),
        )
    })
}

fn base_event(
    turn_id: Option<ChatTurnId>,
    item_id: Option<ChatItemId>,
    payload: HarnessEventPayload,
) -> HarnessEvent {
    HarnessEvent {
        occurred_at: Timestamp::now(),
        turn_id,
        item_id,
        request_id: None,
        provider_references: ProviderEventReferences::default(),
        payload,
    }
}

fn provider_event(
    provider_session_id: &ProviderSessionId,
    turn_id: Option<ChatTurnId>,
    item_id: Option<ChatItemId>,
    payload: HarnessEventPayload,
) -> HarnessEvent {
    let mut event = base_event(turn_id, item_id, payload);
    event.provider_references.provider_session_id = Some(provider_session_id.clone());
    event
}

fn emit_event(
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    event: HarnessEvent,
) {
    emit(output, Ok(event));
}

fn emit(
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    event: Result<HarnessEvent, HarnessError>,
) {
    if output.send(event).is_err() {
        tracing::debug!("Tasci event receiver closed before event delivery");
    }
}

fn harness_error(kind: HarnessErrorKind, message: impl Into<String>) -> HarnessError {
    HarnessError {
        kind,
        message: message.into(),
        retryable: false,
    }
}

fn lock(state: &Arc<Mutex<TasciSessionState>>) -> MutexGuard<'_, TasciSessionState> {
    state.lock().expect(
        "the Tasci session mutex remains unpoisoned because no state operation invokes user code",
    )
}

fn active_turn_id(state: &Arc<Mutex<TasciSessionState>>) -> Option<ChatTurnId> {
    lock(state).active_turn.as_ref().map(|turn| turn.id.clone())
}

#[derive(serde::Serialize)]
struct ToolPresentation {
    tool: String,
    arguments: String,
    result: String,
}

#[derive(serde::Serialize)]
struct CompactionPresentation {
    reason: CompactionReason,
    tokens_before: u64,
    estimated_tokens_after: u64,
    error: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct TasciResumeCursor {
    version: u32,
    session_id: String,
}

struct TasciNativeSession {
    provider_session_id: ProviderSessionId,
    protocol: TasciHarnessSession,
    resuming: bool,
}

/// Resolves a durable native-session operation from the engine cursor.
fn resolve_native_session(
    cursor: Option<&ResumeCursor>,
) -> Result<TasciNativeSession, HarnessError> {
    let resumed_session_id = cursor.map(parse_resume_cursor).transpose()?;
    let resuming = resumed_session_id.is_some();
    let provider_session_id =
        ProviderSessionId(resumed_session_id.unwrap_or_else(|| Uuid::new_v4().to_string()));
    let protocol = if resuming {
        TasciHarnessSession::Resume {
            session_id: provider_session_id.0.clone(),
        }
    } else {
        TasciHarnessSession::Create {
            session_id: provider_session_id.0.clone(),
        }
    };
    Ok(TasciNativeSession {
        provider_session_id,
        protocol,
        resuming,
    })
}

/// Validates and decodes the adaptor-owned cursor representation.
fn parse_resume_cursor(cursor: &ResumeCursor) -> Result<String, HarnessError> {
    let cursor = serde_json::from_value::<TasciResumeCursor>(cursor.0.clone()).map_err(|_| {
        harness_error(
            HarnessErrorKind::InvalidResumeCursor,
            "the Tasci resume cursor has an invalid shape",
        )
    })?;
    if cursor.version != TASCI_RESUME_CURSOR_VERSION {
        return Err(harness_error(
            HarnessErrorKind::InvalidResumeCursor,
            format!(
                "Tasci resume cursor version {} is unsupported",
                cursor.version
            ),
        ));
    }
    let session_id = Uuid::parse_str(&cursor.session_id).map_err(|_| {
        harness_error(
            HarnessErrorKind::InvalidResumeCursor,
            "the Tasci resume cursor session id is not a UUID",
        )
    })?;
    Ok(session_id.to_string())
}

/// Encodes a native session identifier as an opaque engine cursor.
fn resume_cursor(provider_session_id: &ProviderSessionId) -> Result<ResumeCursor, HarnessError> {
    serde_json::to_value(TasciResumeCursor {
        version: TASCI_RESUME_CURSOR_VERSION,
        session_id: provider_session_id.0.clone(),
    })
    .map(ResumeCursor)
    .map_err(|error| {
        harness_error(
            HarnessErrorKind::Internal,
            format!("failed to encode the Tasci resume cursor: {error}"),
        )
    })
}

/// Publishes the normalized initial state for a durable Tasci session.
fn emit_session_started(
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    provider_session_id: &ProviderSessionId,
    model: &ChatModelSelection,
) -> Result<(), HarnessError> {
    emit_event(
        output,
        provider_event(
            provider_session_id,
            None,
            None,
            HarnessEventPayload::SessionStarted,
        ),
    );
    emit_event(
        output,
        provider_event(
            provider_session_id,
            None,
            None,
            HarnessEventPayload::ResumeCursorUpdated {
                resume_cursor: resume_cursor(provider_session_id)?,
            },
        ),
    );
    emit_event(
        output,
        provider_event(
            provider_session_id,
            None,
            None,
            HarnessEventPayload::ModelChanged {
                model: model.clone(),
            },
        ),
    );
    emit_event(
        output,
        provider_event(
            provider_session_id,
            None,
            None,
            HarnessEventPayload::SessionStateChanged {
                state: SessionState::Ready,
                reason: None,
            },
        ),
    );
    Ok(())
}

fn add_optional_usage(first: Option<u64>, second: Option<u64>) -> Option<u64> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.saturating_add(second)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

impl From<tascarrel_api::types::config::ResolveTasciModelOutput> for TasciRuntimeConfiguration {
    fn from(output: tascarrel_api::types::config::ResolveTasciModelOutput) -> Self {
        let authorization = output
            .authorization_header
            .zip(output.authorization_value)
            .map(|(header, value)| HttpAuthorization::new(header, value));
        Self {
            selection: ChatModelSelection {
                model: output.selected_model,
                options: ArcVec::new(),
            },
            harness: TasciHarnessConfiguration {
                base_url: output.base_url.to_string(),
                model: output.provider_model.to_string(),
                context_window: output.context_window,
                max_output_tokens: output.max_output_tokens,
                authorization,
                working_directory: "/workspace".to_owned(),
                mcp_servers: Vec::new(),
            },
            models: output.models,
        }
    }
}

/// Applies workspace MCP servers to one resolved Tasci model configuration.
fn harness_configuration(
    mut configuration: TasciHarnessConfiguration,
    servers: &[config_api::McpServerConfiguration],
) -> TasciHarnessConfiguration {
    configuration.mcp_servers = servers
        .iter()
        .map(|server| tascarrel_agent::McpServerConfiguration {
            name: server.name.to_string(),
            display_name: server.display_name.to_string(),
            endpoint: server.endpoint.to_string(),
            headers: server
                .headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        })
        .collect();
    configuration
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the durable engine cursor round-trips only the versioned native
    /// session identifier.
    #[test]
    fn resume_cursor_round_trips_the_native_session_identifier() {
        let provider_session_id =
            ProviderSessionId("018f2a26-4c89-7f70-a65f-3f956a7d88a1".to_owned());

        let cursor = resume_cursor(&provider_session_id).unwrap();

        assert_eq!(parse_resume_cursor(&cursor).unwrap(), provider_session_id.0);
    }

    /// Verifies Tasci preserves estimated context usage independently of
    /// cumulative turn usage.
    #[test]
    fn projects_estimated_context_usage() {
        let turn_id = ChatTurnId::generate();
        let state = Arc::new(Mutex::new(TasciSessionState {
            active_turn: Some(ActiveTasciTurn {
                id: turn_id.clone(),
                user_item_id: ChatItemId::generate(),
                user_content: None,
                changed_model: None,
                presentation_started: true,
            }),
            reasoning_item: None,
            assistant_item: None,
            tool_items: HashMap::new(),
            compaction_item: None,
            turn_usage: ModelUsage::default(),
            current_model: selection("local-model"),
            stopped: false,
        }));
        let (events, mut receiver) = mpsc::unbounded_channel();

        project_agent_event(
            AgentEvent::ContextUsageUpdated {
                used_tokens: 12_345,
                context_window: Some(128_000),
                is_estimated: true,
            },
            &state,
            &events,
        );

        let event = receiver.try_recv().unwrap().unwrap();
        assert_eq!(event.turn_id, Some(turn_id));
        assert!(matches!(
            event.payload,
            HarnessEventPayload::ContextUsageUpdated {
                usage: Some(HarnessContextUsage {
                    used_tokens: 12_345,
                    context_window_tokens: Some(128_000),
                    accuracy: ChatContextUsageAccuracy::Estimated,
                })
            }
        ));
    }

    /// Verifies one turn presents its user message and model change exactly
    /// once.
    #[test]
    fn turn_presentation_includes_the_user_message_and_model_change() {
        let previous_model = selection("previous-model");
        let selected_model = selection("selected-model");
        let turn_id = ChatTurnId::generate();
        let user_item_id = ChatItemId::generate();
        let state = Arc::new(Mutex::new(TasciSessionState {
            active_turn: Some(ActiveTasciTurn {
                id: turn_id.clone(),
                user_item_id: user_item_id.clone(),
                user_content: Some(
                    vec![ChatContent::Text(TextContent {
                        value: "Change models and keep this visible.".into(),
                    })]
                    .into(),
                ),
                changed_model: Some(selected_model.clone()),
                presentation_started: false,
            }),
            reasoning_item: None,
            assistant_item: None,
            tool_items: HashMap::new(),
            compaction_item: None,
            turn_usage: ModelUsage::default(),
            current_model: previous_model,
            stopped: false,
        }));
        let (events, mut receiver) = mpsc::unbounded_channel();

        start_turn_presentation(&state, &events);
        start_turn_presentation(&state, &events);

        let events = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 5);
        assert!(matches!(
            events[0].payload,
            HarnessEventPayload::TurnStarted
        ));
        assert!(matches!(
            events[1].payload,
            HarnessEventPayload::SessionStateChanged {
                state: SessionState::Running,
                ..
            }
        ));
        assert!(matches!(
            &events[2].payload,
            HarnessEventPayload::ModelChanged { model } if model == &selected_model
        ));
        assert!(matches!(
            events[3].payload,
            HarnessEventPayload::ItemStarted {
                kind: ChatItemKind::UserMessage
            }
        ));
        assert_eq!(events[3].turn_id.as_ref(), Some(&turn_id));
        assert_eq!(events[3].item_id.as_ref(), Some(&user_item_id));
        assert!(matches!(
            &events[4].payload,
            HarnessEventPayload::ItemCompleted {
                kind: ChatItemKind::UserMessage,
                state: ChatItemState::Completed,
                content,
            } if matches!(
                content.as_ref(),
                [ChatContent::Text(TextContent { value })]
                    if value.as_ref() == "Change models and keep this visible."
            )
        ));
    }

    /// Verifies reasoning, assistant text, and tool execution retain model-step
    /// order as separate timeline items.
    #[test]
    fn tool_execution_separates_assistant_timeline_items() {
        let state = Arc::new(Mutex::new(TasciSessionState {
            active_turn: Some(ActiveTasciTurn {
                id: ChatTurnId::generate(),
                user_item_id: ChatItemId::generate(),
                user_content: Some(ArcVec::new()),
                changed_model: None,
                presentation_started: true,
            }),
            reasoning_item: None,
            assistant_item: None,
            tool_items: HashMap::new(),
            compaction_item: None,
            turn_usage: ModelUsage::default(),
            current_model: selection("local-model"),
            stopped: false,
        }));
        let (events, mut receiver) = mpsc::unbounded_channel();

        for event in [
            AgentEvent::ReasoningDelta {
                delta: "Reasoning before the tool.".to_owned(),
            },
            AgentEvent::TextDelta {
                delta: "Before the tool.".to_owned(),
            },
            AgentEvent::ToolExecutionStarted {
                id: "call-1".to_owned(),
                name: "read".to_owned(),
                arguments: r#"{"path":"src/lib.rs"}"#.to_owned(),
            },
            AgentEvent::ToolExecutionCompleted {
                id: "call-1".to_owned(),
                name: "read".to_owned(),
                content: "file contents".to_owned(),
                artifacts: Vec::new(),
                is_error: false,
            },
            AgentEvent::ReasoningDelta {
                delta: "Reasoning after the tool.".to_owned(),
            },
            AgentEvent::TextDelta {
                delta: "After the tool.".to_owned(),
            },
        ] {
            project_agent_event(event, &state, &events);
        }
        finish_turn(&state, &events, None, false);

        let events = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        let transitions = events
            .iter()
            .filter_map(|event| {
                let transition = match &event.payload {
                    HarnessEventPayload::ItemStarted { kind } => ("started", *kind),
                    HarnessEventPayload::ItemCompleted { kind, .. } => ("completed", *kind),
                    _ => return None,
                };
                Some((transition.0, transition.1, event.item_id.clone().unwrap()))
            })
            .collect::<Vec<_>>();

        assert_eq!(
            transitions
                .iter()
                .map(|(transition, kind, _)| (*transition, *kind))
                .collect::<Vec<_>>(),
            vec![
                ("started", ChatItemKind::Reasoning),
                ("completed", ChatItemKind::Reasoning),
                ("started", ChatItemKind::AssistantMessage),
                ("completed", ChatItemKind::AssistantMessage),
                ("started", ChatItemKind::ToolCall),
                ("completed", ChatItemKind::ToolCall),
                ("started", ChatItemKind::Reasoning),
                ("completed", ChatItemKind::Reasoning),
                ("started", ChatItemKind::AssistantMessage),
                ("completed", ChatItemKind::AssistantMessage),
            ]
        );
        assert_eq!(transitions[0].2, transitions[1].2);
        assert_eq!(transitions[2].2, transitions[3].2);
        assert_eq!(transitions[4].2, transitions[5].2);
        assert_eq!(transitions[6].2, transitions[7].2);
        assert_eq!(transitions[8].2, transitions[9].2);
        assert_ne!(transitions[0].2, transitions[6].2);
        assert_ne!(transitions[2].2, transitions[8].2);
    }

    /// Verifies a compaction failure remains visible when preparation failed
    /// before Tasci could emit a compaction-started event.
    #[test]
    fn compaction_failure_without_started_event_is_presented() {
        let turn_id = ChatTurnId::generate();
        let state = Arc::new(Mutex::new(TasciSessionState {
            active_turn: Some(ActiveTasciTurn {
                id: turn_id.clone(),
                user_item_id: ChatItemId::generate(),
                user_content: Some(ArcVec::new()),
                changed_model: None,
                presentation_started: true,
            }),
            reasoning_item: None,
            assistant_item: None,
            tool_items: HashMap::new(),
            compaction_item: None,
            turn_usage: ModelUsage::default(),
            current_model: selection("local-model"),
            stopped: false,
        }));
        let (events, mut receiver) = mpsc::unbounded_channel();

        project_agent_event(
            AgentEvent::ContextCompactionFailed {
                reason: CompactionReason::Threshold,
                message: "the current session has no context that can be compacted".to_owned(),
            },
            &state,
            &events,
        );

        let event = receiver.try_recv().unwrap().unwrap();
        assert_eq!(event.turn_id.as_ref(), Some(&turn_id));
        assert!(event.item_id.is_some());
        assert!(matches!(
            event.payload,
            HarnessEventPayload::ItemCompleted {
                kind: ChatItemKind::ContextCompaction,
                state: ChatItemState::Failed,
                ..
            }
        ));
        assert!(receiver.try_recv().is_err());
    }

    fn selection(model: &str) -> ChatModelSelection {
        ChatModelSelection {
            model: model.into(),
            options: ArcVec::new(),
        }
    }
}
