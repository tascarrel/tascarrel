//! Tasci native-harness adaptor.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use futures_util::future::BoxFuture;
use jiff::Timestamp;
use tascarrel_agent::AgentEvent;
use tascarrel_agent::HttpAuthorization;
use tascarrel_agent::TasciHarnessCommand;
use tascarrel_agent::TasciHarnessConfiguration;
use tascarrel_agent::TasciHarnessEvent;
use tascarrel_agent::ToolArtifact;
use tascarrel_api::ArcVec;
use tascarrel_api::ids::ChatItemId;
use tascarrel_api::ids::ChatTurnId;
use tascarrel_api::types::chats::ChatContent;
use tascarrel_api::types::chats::ChatFailure;
use tascarrel_api::types::chats::ChatItemContentAppended;
use tascarrel_api::types::chats::ChatItemKind;
use tascarrel_api::types::chats::ChatItemState;
use tascarrel_api::types::chats::ChatModel;
use tascarrel_api::types::chats::ChatModelSelection;
use tascarrel_api::types::chats::ChatPromptAttachment;
use tascarrel_api::types::chats::ChatTurnState;
use tascarrel_api::types::chats::StructuredContent;
use tascarrel_api::types::chats::TextContent;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::BufReader;
use tokio::sync::mpsc;

use crate::services::chats::harness::Harness;
use crate::services::chats::harness::HarnessControl;
use crate::services::chats::harness::HarnessEventStream;
use crate::services::chats::harness::HarnessSession;
use crate::services::chats::harness::protocol::HarnessCommand;
use crate::services::chats::harness::protocol::HarnessCommandResult;
use crate::services::chats::harness::protocol::HarnessError;
use crate::services::chats::harness::protocol::HarnessErrorKind;
use crate::services::chats::harness::protocol::HarnessEvent;
use crate::services::chats::harness::protocol::HarnessEventPayload;
use crate::services::chats::harness::protocol::HarnessPrompt;
use crate::services::chats::harness::protocol::HarnessPromptAttachment;
use crate::services::chats::harness::protocol::ProviderEventReferences;
use crate::services::chats::harness::protocol::SessionState;
use crate::services::chats::harness::protocol::StartSessionRequest;
use crate::services::chats::process::HarnessProcessControl;
use crate::services::chats::process::HarnessProcessLauncher;
use crate::services::chats::process::HarnessProcessSpec;

/// Adapter for the line-delimited protocol implemented by `tasci-exec`.
pub struct TasciAdaptor {
    executable: PathBuf,
    launcher: Arc<dyn HarnessProcessLauncher>,
    configuration: TasciRuntimeConfiguration,
    configurations: TasciConfigurationStore,
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
        }
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
            if request.resume_cursor.is_some() {
                return Err(harness_error(
                    HarnessErrorKind::InvalidResumeCursor,
                    "Tasci session resumption is not available yet",
                ));
            }
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
                    configuration: self.configuration.harness.clone(),
                },
            )
            .await?;
            let mut lines = BufReader::new(process.stdout).lines();
            let started = read_event(&mut lines).await?;
            if started != TasciHarnessEvent::Started {
                return Err(harness_error(
                    HarnessErrorKind::Protocol,
                    "Tasci did not acknowledge harness initialization",
                ));
            }

            let state = Arc::new(Mutex::new(TasciSessionState {
                active_turn: None,
                assistant_item: None,
                tool_items: HashMap::new(),
                current_model: self.configuration.selection.clone(),
                stopped: false,
            }));
            let (events, receiver) = mpsc::unbounded_channel();
            let control_events = events.clone();
            emit_event(
                &events,
                base_event(None, None, HarnessEventPayload::SessionStarted),
            );
            emit_event(
                &events,
                base_event(
                    None,
                    None,
                    HarnessEventPayload::ModelChanged {
                        model: self.configuration.selection.clone(),
                    },
                ),
            );
            emit_event(
                &events,
                base_event(
                    None,
                    None,
                    HarnessEventPayload::SessionStateChanged {
                        state: SessionState::Ready,
                        reason: None,
                    },
                ),
            );
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
    /// Endpoint configuration sent privately to the pod harness.
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
                HarnessCommand::CompactContext | HarnessCommand::ResolveUserInput { .. } => {
                    Err(harness_error(
                        HarnessErrorKind::UnsupportedOperation,
                        "Tasci does not support this harness command",
                    ))
                }
            }
        })
    }
}

impl TasciControl {
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
            Some(
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
            )
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
                user_content,
                changed_model: (requested_model != current_model).then(|| requested_model.clone()),
                presentation_started: false,
            });
            state.current_model = requested_model;
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
    assistant_item: Option<StreamingAssistantItem>,
    tool_items: HashMap<String, ToolItem>,
    current_model: ChatModelSelection,
    stopped: bool,
}

struct ActiveTasciTurn {
    id: ChatTurnId,
    user_item_id: ChatItemId,
    user_content: ArcVec<ChatContent>,
    changed_model: Option<ChatModelSelection>,
    presentation_started: bool,
}

struct StreamingAssistantItem {
    id: ChatItemId,
    content: String,
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
        AgentEvent::TextDelta { delta } => append_assistant_text(state, output, delta),
        AgentEvent::ToolExecutionStarted {
            id,
            name,
            arguments,
        } => start_tool_item(state, output, id, name, arguments),
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

fn append_assistant_text(
    state: &Arc<Mutex<TasciSessionState>>,
    output: &mpsc::UnboundedSender<Result<HarnessEvent, HarnessError>>,
    delta: String,
) {
    let (turn, item, started) = {
        let mut state = lock(state);
        let turn = state.active_turn.as_ref().map(|turn| turn.id.clone());
        let started = state.assistant_item.is_none();
        let item = state
            .assistant_item
            .get_or_insert_with(|| StreamingAssistantItem {
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
                    kind: ChatItemKind::AssistantMessage,
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
    let (turn, assistant) = {
        let mut state = lock(state);
        let turn = state.active_turn.take().map(|turn| turn.id);
        let assistant = state.assistant_item.take();
        state.tool_items.clear();
        (turn, assistant)
    };
    if let Some(assistant) = assistant {
        emit_event(
            output,
            base_event(
                turn.clone(),
                Some(assistant.id),
                HarnessEventPayload::ItemCompleted {
                    kind: ChatItemKind::AssistantMessage,
                    state: ChatItemState::Completed,
                    content: vec![ChatContent::Text(TextContent {
                        value: assistant.content.into(),
                    })]
                    .into(),
                },
            ),
        );
    }
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
                authorization,
                working_directory: "/workspace".to_owned(),
            },
            models: output.models,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                user_content: vec![ChatContent::Text(TextContent {
                    value: "Change models and keep this visible.".into(),
                })]
                .into(),
                changed_model: Some(selected_model.clone()),
                presentation_started: false,
            }),
            assistant_item: None,
            tool_items: HashMap::new(),
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

    fn selection(model: &str) -> ChatModelSelection {
        ChatModelSelection {
            model: model.into(),
            options: ArcVec::new(),
        }
    }
}
