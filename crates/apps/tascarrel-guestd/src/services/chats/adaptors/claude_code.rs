//! Claude Code stream-JSON adaptor.
//!
//! Each session owns an independent Claude Code process and translates its
//! typed stream protocol into the common chat event model.
//!
//! Provider wire types in this module are intentionally minimal. They contain
//! only fields consumed by the adaptor, and Serde ignores every other field.
//! Do not expand them merely to mirror the complete provider protocol.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::future::BoxFuture;
use jiff::Timestamp;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::IgnoredAny;
use serde_json::Value as JsonValue;
use serde_json::value::RawValue;
use tascarrel_api::ArcStr;
use tascarrel_api::ArcVec;
use tascarrel_api::ids::ChatItemId;
use tascarrel_api::ids::ChatQuestionId;
use tascarrel_api::ids::ChatRequestId;
use tascarrel_api::ids::ChatTurnId;
use tascarrel_api::types::chats::ChatContent;
use tascarrel_api::types::chats::ChatFailure;
use tascarrel_api::types::chats::ChatItemContentAppended;
use tascarrel_api::types::chats::ChatItemKind;
use tascarrel_api::types::chats::ChatItemState;
use tascarrel_api::types::chats::ChatModel;
use tascarrel_api::types::chats::ChatModelOptionChoice;
use tascarrel_api::types::chats::ChatModelOptionDescriptor;
use tascarrel_api::types::chats::ChatModelOptionValue;
use tascarrel_api::types::chats::ChatModelSelectOptionDescriptor;
use tascarrel_api::types::chats::ChatModelSelection;
use tascarrel_api::types::chats::ChatModelUsage;
use tascarrel_api::types::chats::ChatPromptAttachment;
use tascarrel_api::types::chats::ChatQuestion;
use tascarrel_api::types::chats::ChatQuestionAnswer;
use tascarrel_api::types::chats::ChatQuestionOption;
use tascarrel_api::types::chats::ChatTokenUsage;
use tascarrel_api::types::chats::ChatTurnState;
use tascarrel_api::types::chats::ChatUsageCoverage;
use tascarrel_api::types::chats::ChatUsageSnapshot;
use tascarrel_api::types::chats::ChatUsageState;
use tascarrel_api::types::chats::StructuredContent;
use tascarrel_api::types::chats::TextContent;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::BufReader;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use uuid::Uuid;

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
use crate::services::chats::harness::protocol::HarnessSessionInfo;
use crate::services::chats::harness::protocol::ProviderEventReferences;
use crate::services::chats::harness::protocol::ProviderItemId;
use crate::services::chats::harness::protocol::ProviderRequestId;
use crate::services::chats::harness::protocol::ProviderSessionId;
use crate::services::chats::harness::protocol::ResumeCursor;
use crate::services::chats::harness::protocol::SessionState;
use crate::services::chats::harness::protocol::StartSessionRequest;
use crate::services::chats::process::HarnessProcessControl;
use crate::services::chats::process::HarnessProcessLauncher;
use crate::services::chats::process::HarnessProcessSpec;
use crate::services::chats::process::ProcessEnvironment;

const REQUEST_ID_PREFIX: &str = "tascarrel-claude-";
const SETTING_SOURCES: &str = "user,project,local";

struct CuratedClaudeModel {
    id: &'static str,
    display_name: &'static str,
    short_name: &'static str,
    minimum_version: Option<(u64, u64, u64)>,
    efforts: &'static [&'static str],
    default_effort: Option<&'static str>,
    context_window: bool,
}

const CURATED_MODELS: &[CuratedClaudeModel] = &[
    CuratedClaudeModel {
        id: "claude-opus-5",
        display_name: "Claude Opus 5",
        short_name: "Opus 5",
        minimum_version: Some((2, 1, 219)),
        efforts: &["low", "medium", "high", "xhigh", "max"],
        default_effort: Some("xhigh"),
        context_window: true,
    },
    CuratedClaudeModel {
        id: "claude-fable-5",
        display_name: "Claude Fable 5",
        short_name: "Fable 5",
        minimum_version: Some((2, 1, 169)),
        efforts: &["low", "medium", "high", "xhigh", "max"],
        default_effort: Some("high"),
        context_window: true,
    },
    CuratedClaudeModel {
        id: "claude-opus-4-8",
        display_name: "Claude Opus 4.8",
        short_name: "Opus 4.8",
        minimum_version: Some((2, 1, 154)),
        efforts: &["low", "medium", "high", "xhigh", "max"],
        default_effort: Some("high"),
        context_window: false,
    },
    CuratedClaudeModel {
        id: "claude-opus-4-7",
        display_name: "Claude Opus 4.7",
        short_name: "Opus 4.7",
        minimum_version: Some((2, 1, 111)),
        efforts: &["low", "medium", "high", "xhigh", "max"],
        default_effort: Some("xhigh"),
        context_window: false,
    },
    CuratedClaudeModel {
        id: "claude-opus-4-6",
        display_name: "Claude Opus 4.6",
        short_name: "Opus 4.6",
        minimum_version: None,
        efforts: &["low", "medium", "high", "max"],
        default_effort: Some("high"),
        context_window: true,
    },
    CuratedClaudeModel {
        id: "claude-opus-4-5",
        display_name: "Claude Opus 4.5",
        short_name: "Opus 4.5",
        minimum_version: None,
        efforts: &["low", "medium", "high", "max"],
        default_effort: Some("high"),
        context_window: false,
    },
    CuratedClaudeModel {
        id: "claude-sonnet-5",
        display_name: "Claude Sonnet 5",
        short_name: "Sonnet 5",
        minimum_version: None,
        efforts: &["low", "medium", "high", "xhigh", "max"],
        default_effort: Some("high"),
        context_window: true,
    },
    CuratedClaudeModel {
        id: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        short_name: "Sonnet 4.6",
        minimum_version: None,
        efforts: &["low", "medium", "high", "max"],
        default_effort: Some("high"),
        context_window: true,
    },
    CuratedClaudeModel {
        id: "claude-haiku-4-5",
        display_name: "Claude Haiku 4.5",
        short_name: "Haiku 4.5",
        minimum_version: None,
        efforts: &[],
        default_effort: None,
        context_window: false,
    },
];

/// Claude Code adaptor backed by one stream-JSON process per session.
#[derive(Clone)]
pub struct ClaudeCodeAdaptor {
    executable: PathBuf,
    launcher: Arc<dyn HarnessProcessLauncher>,
    process_environment: Option<Arc<dyn ProcessEnvironment>>,
    working_directory: PathBuf,
    harness_version: Option<String>,
}

impl ClaudeCodeAdaptor {
    /// Creates an adaptor for one Claude Code executable.
    #[must_use]
    pub(crate) fn new(executable: PathBuf, launcher: Arc<dyn HarnessProcessLauncher>) -> Self {
        Self {
            executable,
            launcher,
            process_environment: None,
            working_directory: PathBuf::from("/workspace"),
            harness_version: None,
        }
    }

    /// Uses the pinned harness version to select the compatible built-in
    /// model catalog.
    #[must_use]
    pub(crate) fn with_harness_version(mut self, version: impl Into<String>) -> Self {
        self.harness_version = Some(version.into());
        self
    }

    /// Applies application-owned environment and credential locations.
    #[must_use]
    pub fn with_process_environment(
        mut self,
        process_environment: Arc<dyn ProcessEnvironment>,
    ) -> Self {
        self.process_environment = Some(process_environment);
        self
    }

    /// Overrides the working directory used by Claude Code.
    #[must_use]
    pub(crate) fn with_working_directory(mut self, working_directory: PathBuf) -> Self {
        self.working_directory = working_directory;
        self
    }
}

impl Harness for ClaudeCodeAdaptor {
    fn models(&self) -> BoxFuture<'_, Result<ArcVec<ChatModel>, HarnessError>> {
        let executable = self.executable.clone();
        let launcher = Arc::clone(&self.launcher);
        let environment = self.process_environment.clone();
        let working_directory = self.working_directory.clone();
        let harness_version = self.harness_version.clone();
        Box::pin(async move {
            let server = start_server(
                executable,
                launcher,
                &LaunchOptions::default(),
                environment.as_deref(),
                working_directory,
            )
            .await?;
            let response: InitializeResponse =
                server.control.request(ControlCommand::Initialize).await?;
            let stopped = server.control.stop().await;
            stopped?;
            let native_models = response
                .models
                .into_iter()
                .filter_map(normalize_model)
                .collect::<Vec<_>>();
            let models = match harness_version.as_deref() {
                Some(version) => curate_models(version, native_models),
                None => native_models,
            };
            if models.is_empty() {
                return Err(harness_error(
                    HarnessErrorKind::Protocol,
                    "Claude Code returned an empty model catalog",
                ));
            }
            Ok(models.into())
        })
    }

    #[allow(clippy::too_many_lines)] // Session startup keeps ordered provider handshakes together.
    fn start_session(
        &self,
        request: StartSessionRequest,
    ) -> BoxFuture<'_, Result<HarnessSession, HarnessError>> {
        let executable = self.executable.clone();
        let launcher = Arc::clone(&self.launcher);
        let environment = self.process_environment.clone();
        let working_directory = self.working_directory.clone();
        Box::pin(async move {
            let resume = request
                .resume_cursor
                .as_ref()
                .map(parse_resume_cursor)
                .transpose()?;
            let provider_session_id = resume.as_ref().map_or_else(
                || ProviderSessionId(Uuid::new_v4().to_string()),
                |resume| resume.provider_session_id.clone(),
            );
            let launch = LaunchOptions {
                model: request.model.clone(),
                provider_session_id: provider_session_id.clone(),
                resume_session_at: resume.and_then(|resume| resume.resume_session_at),
                resuming: request.resume_cursor.is_some(),
            };
            let server = start_server(
                executable,
                launcher,
                &launch,
                environment.as_deref(),
                working_directory,
            )
            .await?;
            let initialize: Result<InitializeResponse, HarnessError> =
                server.control.request(ControlCommand::Initialize).await;
            if let Err(error) = initialize {
                if let Err(stop_error) = server.control.stop().await {
                    tracing::warn!(
                        message = %stop_error.message,
                        "failed to stop Claude Code after initialization failed"
                    );
                }
                return Err(error);
            }
            let cursor = resume_cursor(&provider_session_id, launch.resume_session_at.as_deref());
            let state = Arc::new(Mutex::new(ClaudeSessionState {
                info: HarnessSessionInfo {
                    state: SessionState::Ready,
                    model: request.model,
                    active_turn_id: None,
                    resume_cursor: Some(cursor.clone()),
                },
                provider_session_id: provider_session_id.clone(),
                active_turn: None,
                turn_queue: VecDeque::new(),
                completed_turns: HashSet::new(),
                current_message_id: None,
                blocks: HashMap::new(),
                tools: HashMap::new(),
                tool_indices: HashMap::new(),
                completed_provider_items: HashSet::new(),
                pending_requests: HashMap::new(),
                provider_requests: HashMap::new(),
                last_assistant_uuid: launch.resume_session_at,
                stopped: false,
            }));
            let pending = VecDeque::from([
                event(
                    session_references(&state, None, None),
                    None,
                    None,
                    None,
                    HarnessEventPayload::SessionStarted,
                ),
                event(
                    session_references(&state, None, None),
                    None,
                    None,
                    None,
                    HarnessEventPayload::ResumeCursorUpdated {
                        resume_cursor: cursor,
                    },
                ),
                event(
                    session_references(&state, None, None),
                    None,
                    None,
                    None,
                    HarnessEventPayload::SessionStateChanged {
                        state: SessionState::Ready,
                        reason: None,
                    },
                ),
            ]);
            let control = Arc::new(ClaudeControl {
                server: Arc::clone(&server.control),
                state: Arc::clone(&state),
                command_lock: AsyncMutex::new(()),
            });
            Ok(HarnessSession {
                control,
                events: Box::new(ClaudeEvents {
                    server: server.control,
                    messages: server.messages,
                    state,
                    pending,
                    ended: false,
                }),
            })
        })
    }
}

struct ClaudeControl {
    server: Arc<ClaudeProcess>,
    state: Arc<Mutex<ClaudeSessionState>>,
    command_lock: AsyncMutex<()>,
}

impl HarnessControl for ClaudeControl {
    fn apply(
        &self,
        command: HarnessCommand,
    ) -> BoxFuture<'_, Result<HarnessCommandResult, HarnessError>> {
        Box::pin(async move {
            let _guard = self.command_lock.lock().await;
            match command {
                HarnessCommand::SendPrompt(prompt) => self.send_prompt(prompt).await,
                HarnessCommand::InterruptAndSend(prompt) => {
                    if lock(&self.state).active_turn.is_some() {
                        self.interrupt().await?;
                    }
                    self.send_prompt(prompt).await
                }
                HarnessCommand::Interrupt => self.interrupt().await,
                HarnessCommand::CompactContext => Err(harness_error(
                    HarnessErrorKind::UnsupportedOperation,
                    "Claude Code exposes automatic compaction but no imperative compact operation",
                )),
                HarnessCommand::ResolveUserInput {
                    request_id,
                    answers,
                } => self.resolve_user_input(request_id, answers).await,
                HarnessCommand::Stop => self.stop().await,
            }
        })
    }
}

impl ClaudeControl {
    #[allow(clippy::too_many_lines)] // Prompt submission keeps the provider turn transition atomic.
    async fn send_prompt(
        &self,
        prompt: HarnessPrompt,
    ) -> Result<HarnessCommandResult, HarnessError> {
        let content = prompt_content(prompt.text.clone(), &prompt.attachments).await?;
        let (active_turn, current_model) = {
            let state = lock(&self.state);
            ensure_running(&state)?;
            (state.active_turn.clone(), state.info.model.clone())
        };
        let requested_model = prompt.model.or_else(|| current_model.clone());
        let model_changed = requested_model != current_model;
        if model_changed {
            let _: IgnoredAny = self
                .server
                .request(ControlCommand::SetModel {
                    model: requested_model.as_ref().map(native_model_id),
                })
                .await?;
        }
        let turn_id = active_turn.clone().unwrap_or_else(ChatTurnId::generate);
        let user_item_id = ChatItemId::generate();
        let mut immediate = Vec::new();
        if active_turn.is_none() {
            immediate.push(event(
                session_references(&self.state, None, None),
                Some(turn_id.clone()),
                None,
                None,
                HarnessEventPayload::TurnStarted,
            ));
        }
        if model_changed && let Some(model) = requested_model.clone() {
            immediate.push(event(
                session_references(&self.state, None, None),
                active_turn.clone(),
                None,
                None,
                HarnessEventPayload::ModelChanged { model },
            ));
        }
        immediate.extend([
            event(
                session_references(&self.state, None, None),
                Some(turn_id.clone()),
                Some(user_item_id.clone()),
                None,
                HarnessEventPayload::ItemStarted {
                    kind: ChatItemKind::UserMessage,
                },
            ),
            event(
                session_references(&self.state, None, None),
                Some(turn_id.clone()),
                Some(user_item_id),
                None,
                HarnessEventPayload::ItemCompleted {
                    kind: ChatItemKind::UserMessage,
                    state: ChatItemState::Completed,
                    content: canonical_user_content(prompt.text, prompt.attachments),
                },
            ),
        ]);
        {
            let mut state = lock(&self.state);
            if active_turn.is_none() {
                state.turn_queue.push_back(turn_id.clone());
                state.blocks.clear();
                state.tools.clear();
                state.tool_indices.clear();
                state.completed_provider_items.clear();
                state.current_message_id = None;
            }
            state.active_turn = Some(turn_id.clone());
            state.info.active_turn_id = Some(turn_id.clone());
            state.info.state = SessionState::Running;
            state.info.model = requested_model;
        }
        self.server.emit(immediate);
        if let Err(mut error) = self
            .server
            .write_message(&UserMessage {
                kind: "user",
                session_id: "",
                parent_tool_use_id: None,
                message: UserMessageBody {
                    role: "user",
                    content,
                },
            })
            .await
        {
            error.retryable = false;
            finish_turn(
                &self.server,
                &self.state,
                turn_id.clone(),
                ChatTurnState::Failed,
                Some(ChatFailure {
                    code: "process_exited".into(),
                    message: error.message.clone().into(),
                }),
            );
            return Err(error);
        }
        Ok(HarnessCommandResult::PromptAccepted {
            turn_id,
            provider_turn_id: None,
        })
    }

    async fn interrupt(&self) -> Result<HarnessCommandResult, HarnessError> {
        let turn_id = {
            let state = lock(&self.state);
            ensure_running(&state)?;
            state.active_turn.clone().ok_or_else(|| {
                harness_error(
                    HarnessErrorKind::TurnNotFound,
                    "the Claude Code session has no active turn",
                )
            })?
        };
        let _: IgnoredAny = self.server.request(ControlCommand::Interrupt).await?;
        let mut events = complete_open_items(&self.state, ChatItemState::Failed);
        events.push(turn_completed_event(
            &self.state,
            turn_id.clone(),
            ChatTurnState::Interrupted,
            None,
        ));
        {
            let mut state = lock(&self.state);
            if state.active_turn.as_ref() == Some(&turn_id) {
                state.completed_turns.insert(turn_id.clone());
                state.active_turn = None;
                state.info.active_turn_id = None;
                state.info.state = SessionState::Ready;
            }
        }
        self.server.emit(events);
        Ok(HarnessCommandResult::Accepted)
    }

    async fn resolve_user_input(
        &self,
        request_id: ChatRequestId,
        answers: ArcVec<ChatQuestionAnswer>,
    ) -> Result<HarnessCommandResult, HarnessError> {
        let pending = {
            let state = lock(&self.state);
            ensure_running(&state)?;
            state.pending_requests.get(&request_id).cloned()
        }
        .ok_or_else(|| {
            harness_error(
                HarnessErrorKind::RequestFailed,
                "the Claude Code user-input request is not pending",
            )
        })?;
        let mut native_answers = HashMap::new();
        for answer in answers {
            let question = pending.questions.get(&answer.question_id).ok_or_else(|| {
                harness_error(
                    HarnessErrorKind::RequestFailed,
                    "an answer refers to an unknown Claude Code question",
                )
            })?;
            let answer = if answer.answers.len() == 1 {
                NativeAnswer::One(answer.answers[0].to_string())
            } else {
                NativeAnswer::Many(answer.answers.iter().map(ToString::to_string).collect())
            };
            native_answers.insert(question.clone(), answer);
        }
        self.server
            .respond(
                &pending.control_request_id,
                ToolPermissionResponse {
                    behavior: "allow",
                    tool_use_id: pending
                        .provider_item_id
                        .as_ref()
                        .map(|item| item.0.as_str()),
                    updated_input: AskUserQuestionResponse {
                        questions: &pending.native_questions,
                        answers: native_answers,
                    },
                },
            )
            .await?;
        let turn_id = {
            let mut state = lock(&self.state);
            state.pending_requests.remove(&request_id);
            state.provider_requests.remove(&pending.control_request_id);
            state.info.state = SessionState::Running;
            state.active_turn.clone()
        };
        self.server.emit(vec![event(
            session_references(
                &self.state,
                pending.provider_item_id,
                Some(ProviderRequestId(pending.control_request_id)),
            ),
            turn_id,
            None,
            Some(request_id),
            HarnessEventPayload::RequestResolved,
        )]);
        Ok(HarnessCommandResult::Accepted)
    }

    async fn stop(&self) -> Result<HarnessCommandResult, HarnessError> {
        {
            let mut state = lock(&self.state);
            state.stopped = true;
            state.info.state = SessionState::Stopped;
            state.info.active_turn_id = None;
            state.active_turn = None;
        }
        self.server.stop().await?;
        Ok(HarnessCommandResult::Stopped)
    }
}

struct ClaudeEvents {
    server: Arc<ClaudeProcess>,
    messages: ClaudeMessages,
    state: Arc<Mutex<ClaudeSessionState>>,
    pending: VecDeque<HarnessEvent>,
    ended: bool,
}

impl HarnessEventStream for ClaudeEvents {
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<HarnessEvent>, HarnessError>> {
        Box::pin(async move {
            loop {
                if let Some(event) = self.pending.pop_front() {
                    return Ok(Some(event));
                }
                if self.ended {
                    return Ok(None);
                }
                let Some(message) = self.messages.receiver.recv().await else {
                    self.ended = true;
                    return Ok(Some(unexpected_exit(&self.state)));
                };
                match message {
                    ClaudeMessage::Native(message) => {
                        self.pending
                            .extend(normalize_native(&self.server, &self.state, &message).await?);
                    }
                    ClaudeMessage::Events(events) => self.pending.extend(events),
                    ClaudeMessage::Exited(error) => {
                        self.ended = true;
                        let stopped = lock(&self.state).stopped;
                        let failure = if stopped {
                            None
                        } else {
                            Some(error.unwrap_or_else(|| {
                                harness_error(
                                    HarnessErrorKind::ProcessExited,
                                    "the Claude Code process exited unexpectedly",
                                )
                            }))
                        };
                        mark_exited(&self.state, failure.is_some());
                        return Ok(Some(session_exited(&self.state, failure)));
                    }
                }
            }
        })
    }
}

#[derive(Clone)]
struct PendingUserInput {
    control_request_id: String,
    provider_item_id: Option<ProviderItemId>,
    questions: HashMap<ChatQuestionId, String>,
    native_questions: Vec<NativeQuestion>,
}

struct ContentBlockState {
    item_id: ChatItemId,
    provider_item_id: ProviderItemId,
    kind: ChatItemKind,
    text: String,
}

struct ToolState {
    item_id: ChatItemId,
    provider_item_id: ProviderItemId,
    kind: ChatItemKind,
    name: String,
    input: RawJson,
    partial_input: String,
}

struct ClaudeSessionState {
    info: HarnessSessionInfo,
    provider_session_id: ProviderSessionId,
    active_turn: Option<ChatTurnId>,
    turn_queue: VecDeque<ChatTurnId>,
    completed_turns: HashSet<ChatTurnId>,
    current_message_id: Option<String>,
    blocks: HashMap<u64, ContentBlockState>,
    tools: HashMap<String, ToolState>,
    tool_indices: HashMap<u64, String>,
    completed_provider_items: HashSet<String>,
    pending_requests: HashMap<ChatRequestId, PendingUserInput>,
    provider_requests: HashMap<String, ChatRequestId>,
    last_assistant_uuid: Option<String>,
    stopped: bool,
}

async fn normalize_native(
    server: &ClaudeProcess,
    state: &Arc<Mutex<ClaudeSessionState>>,
    message: &RawJson,
) -> Result<Vec<HarnessEvent>, HarnessError> {
    let header: MessageHeader = decode(message, "message header")?;
    match header.kind.as_str() {
        "control_request" => normalize_control_request(server, state, message).await,
        "control_cancel_request" => normalize_control_cancel(state, message),
        "stream_event" => normalize_stream_event(state, message),
        "assistant" => normalize_assistant(state, message),
        "user" => normalize_user_message(state, message),
        "result" => normalize_result(state, message),
        "system" => normalize_system(state, message),
        "tool_progress" | "tool_use_summary" | "rate_limit_event" | "auth_status" => Ok(Vec::new()),
        native_type => Ok(vec![unknown_event(state, native_type.to_owned(), message)]),
    }
}

async fn normalize_control_request(
    server: &ClaudeProcess,
    state: &Arc<Mutex<ClaudeSessionState>>,
    message: &RawJson,
) -> Result<Vec<HarnessEvent>, HarnessError> {
    let message: ControlRequestMessage = decode(message, "control request")?;
    if message.request.subtype != "can_use_tool" {
        server
            .respond_error(
                &message.request_id,
                format!(
                    "unsupported Claude Code control request: {}",
                    message.request.subtype
                ),
            )
            .await?;
        return Ok(Vec::new());
    }
    if message.request.tool_name.as_deref() != Some("AskUserQuestion") {
        server
            .respond(
                &message.request_id,
                ToolPermissionResponse {
                    behavior: "allow",
                    tool_use_id: message.request.tool_use_id.as_deref(),
                    updated_input: message.request.input,
                },
            )
            .await?;
        return Ok(Vec::new());
    }
    let input: AskUserQuestionInput = decode(&message.request.input, "AskUserQuestion input")?;
    if input.questions.is_empty() {
        return Err(harness_error(
            HarnessErrorKind::Protocol,
            "Claude Code AskUserQuestion contained no questions",
        ));
    }
    let mut questions = Vec::with_capacity(input.questions.len());
    let mut question_ids = HashMap::new();
    for question in &input.questions {
        let question_id = ChatQuestionId::generate();
        question_ids.insert(question_id.clone(), question.question.clone());
        questions.push(ChatQuestion {
            question_id,
            header: question.header.clone().unwrap_or_default().into(),
            prompt: question.question.clone().into(),
            options: question
                .options
                .iter()
                .map(|option| ChatQuestionOption {
                    label: option.label.clone().into(),
                    description: option.description.clone().map(Into::into),
                })
                .collect::<Vec<_>>()
                .into(),
            multiple: question.multi_select,
        });
    }
    let request_id = ChatRequestId::generate();
    let provider_item_id = message.request.tool_use_id.map(ProviderItemId);
    let turn_id = current_turn(state);
    {
        let mut session = lock(state);
        session.pending_requests.insert(
            request_id.clone(),
            PendingUserInput {
                control_request_id: message.request_id.clone(),
                provider_item_id: provider_item_id.clone(),
                questions: question_ids,
                native_questions: input.questions,
            },
        );
        session
            .provider_requests
            .insert(message.request_id.clone(), request_id.clone());
        session.info.state = SessionState::WaitingForInput;
    }
    Ok(vec![event(
        session_references(
            state,
            provider_item_id,
            Some(ProviderRequestId(message.request_id)),
        ),
        turn_id,
        None,
        Some(request_id),
        HarnessEventPayload::UserInputRequested {
            questions: questions.into(),
        },
    )])
}

fn normalize_control_cancel(
    state: &Arc<Mutex<ClaudeSessionState>>,
    message: &RawJson,
) -> Result<Vec<HarnessEvent>, HarnessError> {
    let message: ControlCancelMessage = decode(message, "control cancellation")?;
    let mut session = lock(state);
    let Some(request_id) = session.provider_requests.remove(&message.request_id) else {
        return Ok(Vec::new());
    };
    let pending = session.pending_requests.remove(&request_id);
    session.info.state = SessionState::Running;
    let turn_id = session.active_turn.clone();
    drop(session);
    Ok(vec![event(
        session_references(
            state,
            pending.and_then(|pending| pending.provider_item_id),
            Some(ProviderRequestId(message.request_id)),
        ),
        turn_id,
        None,
        Some(request_id),
        HarnessEventPayload::RequestResolved,
    )])
}

fn normalize_stream_event(
    state: &Arc<Mutex<ClaudeSessionState>>,
    message: &RawJson,
) -> Result<Vec<HarnessEvent>, HarnessError> {
    let stream_event: StreamEventMessage = decode(message, "stream event")?;
    let header: StreamEventHeader = decode(&stream_event.event, "stream event header")?;
    match header.kind.as_str() {
        "message_start" => {
            let event: MessageStartEvent = decode(&stream_event.event, "message start event")?;
            lock(state).current_message_id = Some(event.message.id);
            Ok(Vec::new())
        }
        "content_block_start" => {
            let event: ContentBlockStartEvent =
                decode(&stream_event.event, "content block start event")?;
            Ok(content_block_start(
                state,
                event.index,
                decode_claude_block(&event.content_block)?,
            ))
        }
        "content_block_delta" => {
            let event: ContentBlockDeltaEvent =
                decode(&stream_event.event, "content block delta event")?;
            Ok(content_block_delta(state, event.index, event.delta))
        }
        "content_block_stop" => {
            let event: ContentBlockStopEvent =
                decode(&stream_event.event, "content block stop event")?;
            Ok(content_block_stop(state, event.index))
        }
        "message_stop" | "message_delta" | "ping" => Ok(Vec::new()),
        _ => Ok(vec![unknown_event(
            state,
            "stream_event".to_owned(),
            message,
        )]),
    }
}

fn content_block_start(
    state: &Arc<Mutex<ClaudeSessionState>>,
    index: u64,
    block: ClaudeBlock,
) -> Vec<HarnessEvent> {
    let mut events = ensure_native_turn(state);
    let Some(turn_id) = current_turn(state) else {
        return events;
    };
    if native_turn_is_completed(state, &turn_id) {
        return events;
    }
    match block {
        ClaudeBlock::Text { text } => start_text_block(
            state,
            &mut events,
            turn_id,
            index,
            ChatItemKind::AssistantMessage,
            text,
        ),
        ClaudeBlock::Thinking { thinking } => start_text_block(
            state,
            &mut events,
            turn_id,
            index,
            ChatItemKind::Reasoning,
            thinking,
        ),
        ClaudeBlock::RedactedThinking => start_text_block(
            state,
            &mut events,
            turn_id,
            index,
            ChatItemKind::Reasoning,
            String::new(),
        ),
        ClaudeBlock::ToolUse { id, name, input }
        | ClaudeBlock::ServerToolUse { id, name, input }
        | ClaudeBlock::McpToolUse { id, name, input } => {
            if lock(state).completed_provider_items.contains(&id)
                || lock(state).tools.contains_key(&id)
            {
                return events;
            }
            let kind = tool_kind(&name);
            let item_id = ChatItemId::generate();
            let provider_item_id = ProviderItemId(id.clone());
            let mut session = lock(state);
            session.tool_indices.insert(index, id.clone());
            session.tools.insert(
                id,
                ToolState {
                    item_id: item_id.clone(),
                    provider_item_id: provider_item_id.clone(),
                    kind,
                    name,
                    input,
                    partial_input: String::new(),
                },
            );
            drop(session);
            events.push(item_started_event(
                state,
                turn_id,
                item_id,
                provider_item_id,
                kind,
            ));
            events
        }
        ClaudeBlock::ToolResult { .. } | ClaudeBlock::Other => events,
    }
}

fn start_text_block(
    state: &Arc<Mutex<ClaudeSessionState>>,
    events: &mut Vec<HarnessEvent>,
    turn_id: ChatTurnId,
    index: u64,
    kind: ChatItemKind,
    initial: String,
) -> Vec<HarnessEvent> {
    let provider_item_id = ProviderItemId(content_block_provider_id(state, index));
    if lock(state)
        .completed_provider_items
        .contains(&provider_item_id.0)
    {
        return std::mem::take(events);
    }
    let item_id = ChatItemId::generate();
    lock(state).blocks.insert(
        index,
        ContentBlockState {
            item_id: item_id.clone(),
            provider_item_id: provider_item_id.clone(),
            kind,
            text: initial.clone(),
        },
    );
    events.push(item_started_event(
        state,
        turn_id.clone(),
        item_id.clone(),
        provider_item_id.clone(),
        kind,
    ));
    if !initial.is_empty() {
        events.push(content_delta_event(
            state,
            turn_id,
            item_id,
            provider_item_id,
            initial,
        ));
    }
    std::mem::take(events)
}

fn content_block_delta(
    state: &Arc<Mutex<ClaudeSessionState>>,
    index: u64,
    delta: ClaudeDelta,
) -> Vec<HarnessEvent> {
    let Some(turn_id) = current_turn(state) else {
        return Vec::new();
    };
    if native_turn_is_completed(state, &turn_id) {
        return Vec::new();
    }
    let text = match delta {
        ClaudeDelta::TextDelta { text } => text,
        ClaudeDelta::ThinkingDelta { thinking } => thinking,
        ClaudeDelta::InputJsonDelta { partial_json } => {
            let mut session = lock(state);
            let Some(tool_id) = session.tool_indices.get(&index).cloned() else {
                return Vec::new();
            };
            let Some(tool) = session.tools.get_mut(&tool_id) else {
                return Vec::new();
            };
            tool.partial_input.push_str(&partial_json);
            if let Ok(input) = serde_json::from_str::<RawJson>(&tool.partial_input) {
                tool.input = input;
            }
            return Vec::new();
        }
        ClaudeDelta::Other => return Vec::new(),
    };
    if text.is_empty() {
        return Vec::new();
    }
    let (item_id, provider_item_id) = {
        let mut session = lock(state);
        let Some(block) = session.blocks.get_mut(&index) else {
            return Vec::new();
        };
        block.text.push_str(&text);
        (block.item_id.clone(), block.provider_item_id.clone())
    };
    vec![content_delta_event(
        state,
        turn_id,
        item_id,
        provider_item_id,
        text,
    )]
}

fn content_block_stop(state: &Arc<Mutex<ClaudeSessionState>>, index: u64) -> Vec<HarnessEvent> {
    let Some(turn_id) = current_turn(state) else {
        return Vec::new();
    };
    if native_turn_is_completed(state, &turn_id) {
        return Vec::new();
    }
    let Some(block) = lock(state).blocks.remove(&index) else {
        return Vec::new();
    };
    lock(state)
        .completed_provider_items
        .insert(block.provider_item_id.0.clone());
    vec![item_completed_event(
        state,
        turn_id,
        block.item_id,
        block.provider_item_id,
        block.kind,
        ChatItemState::Completed,
        vec![ChatContent::Text(TextContent {
            value: block.text.into(),
        })]
        .into(),
    )]
}

fn normalize_assistant(
    state: &Arc<Mutex<ClaudeSessionState>>,
    message: &RawJson,
) -> Result<Vec<HarnessEvent>, HarnessError> {
    let message: AssistantMessage = decode(message, "assistant message")?;
    let mut events = ensure_native_turn(state);
    let Some(turn_id) = current_turn(state) else {
        return Ok(events);
    };
    let turn_completed = native_turn_is_completed(state, &turn_id);
    let message_id = message
        .message
        .id
        .or_else(|| lock(state).current_message_id.clone())
        .unwrap_or_else(|| format!("assistant-{}", ChatItemId::generate().0));
    for (index, raw_block) in message.message.content.into_iter().enumerate() {
        if turn_completed {
            break;
        }
        let block = decode_claude_block(&raw_block)?;
        let provider_item_id = ProviderItemId(format!("{message_id}:{index}"));
        if lock(state)
            .completed_provider_items
            .contains(&provider_item_id.0)
            || lock(state)
                .blocks
                .values()
                .any(|block| block.provider_item_id == provider_item_id)
        {
            continue;
        }
        match block {
            ClaudeBlock::Text { text } => events.extend(complete_text_item(
                state,
                turn_id.clone(),
                provider_item_id,
                ChatItemKind::AssistantMessage,
                text,
            )),
            ClaudeBlock::Thinking { thinking } => events.extend(complete_text_item(
                state,
                turn_id.clone(),
                provider_item_id,
                ChatItemKind::Reasoning,
                thinking,
            )),
            ClaudeBlock::ToolUse { id, name, input }
            | ClaudeBlock::ServerToolUse { id, name, input }
            | ClaudeBlock::McpToolUse { id, name, input } => {
                if lock(state).tools.contains_key(&id) {
                    continue;
                }
                let kind = tool_kind(&name);
                let item_id = ChatItemId::generate();
                let provider_item_id = ProviderItemId(id.clone());
                lock(state).tools.insert(
                    id,
                    ToolState {
                        item_id: item_id.clone(),
                        provider_item_id: provider_item_id.clone(),
                        kind,
                        name,
                        input,
                        partial_input: String::new(),
                    },
                );
                events.push(item_started_event(
                    state,
                    turn_id.clone(),
                    item_id,
                    provider_item_id,
                    kind,
                ));
            }
            ClaudeBlock::RedactedThinking | ClaudeBlock::ToolResult { .. } | ClaudeBlock::Other => {
            }
        }
    }
    if let Some(uuid) = message.uuid {
        let cursor = {
            let mut session = lock(state);
            session.last_assistant_uuid = Some(uuid.clone());
            let cursor = resume_cursor(&session.provider_session_id, Some(&uuid));
            session.info.resume_cursor = Some(cursor.clone());
            cursor
        };
        events.push(event(
            session_references(state, None, None),
            Some(turn_id),
            None,
            None,
            HarnessEventPayload::ResumeCursorUpdated {
                resume_cursor: cursor,
            },
        ));
    }
    Ok(events)
}

fn normalize_user_message(
    state: &Arc<Mutex<ClaudeSessionState>>,
    message: &RawJson,
) -> Result<Vec<HarnessEvent>, HarnessError> {
    let message: NativeUserMessage = decode(message, "user message")?;
    let Some(turn_id) = current_turn(state) else {
        return Ok(Vec::new());
    };
    if native_turn_is_completed(state, &turn_id) {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    let Ok(content) = serde_json::from_str::<Vec<RawJson>>(message.message.content.get()) else {
        return Ok(Vec::new());
    };
    for raw_block in content {
        let block = decode_claude_block(&raw_block)?;
        let ClaudeBlock::ToolResult {
            tool_use_id,
            is_error,
            content,
        } = block
        else {
            continue;
        };
        let tool = {
            let mut session = lock(state);
            session.tool_indices.retain(|_, id| id != &tool_use_id);
            session.tools.remove(&tool_use_id)
        };
        let Some(tool) = tool else {
            continue;
        };
        lock(state)
            .completed_provider_items
            .insert(tool.provider_item_id.0.clone());
        let item_content = tool_content(&tool, is_error, content.as_deref())?;
        events.push(item_completed_event(
            state,
            turn_id.clone(),
            tool.item_id,
            tool.provider_item_id,
            tool.kind,
            if is_error {
                ChatItemState::Failed
            } else {
                ChatItemState::Completed
            },
            item_content,
        ));
    }
    Ok(events)
}

fn normalize_result(
    state: &Arc<Mutex<ClaudeSessionState>>,
    message: &RawJson,
) -> Result<Vec<HarnessEvent>, HarnessError> {
    let message: ResultMessage = decode(message, "result message")?;
    let Some(turn_id) = lock(state).turn_queue.pop_front() else {
        return Ok(Vec::new());
    };
    if lock(state).completed_turns.remove(&turn_id) {
        return Ok(Vec::new());
    }
    let mut events = complete_open_items_for_turn(state, &turn_id, ChatItemState::Failed);
    if let Some(usage) = message.usage {
        events.push(event(
            session_references(state, None, None),
            Some(turn_id.clone()),
            None,
            None,
            HarnessEventPayload::TurnUsageUpdated {
                usage: normalize_usage(&usage, message.model_usage),
                state: ChatUsageState::Settled,
            },
        ));
    }
    let error_text = message.errors.join("; ");
    let lower = error_text.to_ascii_lowercase();
    let turn_state = if lower.contains("interrupt") || lower.contains("aborted") {
        ChatTurnState::Interrupted
    } else if message.subtype == "success" && !message.is_error {
        ChatTurnState::Completed
    } else {
        ChatTurnState::Failed
    };
    let failure = (turn_state == ChatTurnState::Failed).then(|| ChatFailure {
        code: "request_failed".into(),
        message: if error_text.is_empty() {
            format!("Claude Code turn ended with {}", message.subtype).into()
        } else {
            error_text.into()
        },
    });
    clear_active_turn(
        state,
        &turn_id,
        if turn_state == ChatTurnState::Failed {
            SessionState::Failed
        } else {
            SessionState::Ready
        },
    );
    events.push(turn_completed_event(state, turn_id, turn_state, failure));
    Ok(events)
}

fn normalize_system(
    state: &Arc<Mutex<ClaudeSessionState>>,
    message: &RawJson,
) -> Result<Vec<HarnessEvent>, HarnessError> {
    let message: SystemMessage = decode(message, "system message")?;
    match message.subtype.as_str() {
        "init" => {
            let (turn_id, cursor, model) = {
                let mut session = lock(state);
                if let Some(session_id) = message.session_id {
                    session.provider_session_id = ProviderSessionId(session_id);
                }
                let model = message.model.map(|model| ChatModelSelection {
                    model: model.into(),
                    options: session
                        .info
                        .model
                        .as_ref()
                        .map_or_else(ArcVec::new, |selection| selection.options.clone()),
                });
                let changed = model
                    .as_ref()
                    .filter(|model| session.info.model.as_ref() != Some(model))
                    .cloned();
                if model.is_some() {
                    session.info.model = model;
                }
                let cursor = resume_cursor(
                    &session.provider_session_id,
                    session.last_assistant_uuid.as_deref(),
                );
                session.info.resume_cursor = Some(cursor.clone());
                (session.active_turn.clone(), cursor, changed)
            };
            let mut events = vec![event(
                session_references(state, None, None),
                turn_id.clone(),
                None,
                None,
                HarnessEventPayload::ResumeCursorUpdated {
                    resume_cursor: cursor,
                },
            )];
            if let Some(model) = model {
                events.push(event(
                    session_references(state, None, None),
                    turn_id,
                    None,
                    None,
                    HarnessEventPayload::ModelChanged { model },
                ));
            }
            Ok(events)
        }
        "compact_boundary" => Ok(vec![base_warning(
            state,
            "context_compacted",
            "Claude Code compacted the session context".to_owned(),
        )]),
        "api_retry" => Ok(vec![base_warning(
            state,
            "api_retry",
            message.error.map_or_else(
                || "Claude Code is retrying an API request".to_owned(),
                |error| format!("Claude Code is retrying after {error}"),
            ),
        )]),
        _ => Ok(Vec::new()),
    }
}

fn complete_text_item(
    state: &Arc<Mutex<ClaudeSessionState>>,
    turn_id: ChatTurnId,
    provider_item_id: ProviderItemId,
    kind: ChatItemKind,
    text: String,
) -> Vec<HarnessEvent> {
    let item_id = ChatItemId::generate();
    lock(state)
        .completed_provider_items
        .insert(provider_item_id.0.clone());
    let mut events = vec![item_started_event(
        state,
        turn_id.clone(),
        item_id.clone(),
        provider_item_id.clone(),
        kind,
    )];
    if !text.is_empty() {
        events.push(content_delta_event(
            state,
            turn_id.clone(),
            item_id.clone(),
            provider_item_id.clone(),
            text.clone(),
        ));
    }
    events.push(item_completed_event(
        state,
        turn_id,
        item_id,
        provider_item_id,
        kind,
        ChatItemState::Completed,
        vec![ChatContent::Text(TextContent { value: text.into() })].into(),
    ));
    events
}

fn ensure_native_turn(state: &Arc<Mutex<ClaudeSessionState>>) -> Vec<HarnessEvent> {
    if current_turn(state).is_some() {
        return Vec::new();
    }
    let turn_id = ChatTurnId::generate();
    {
        let mut session = lock(state);
        session.turn_queue.push_back(turn_id.clone());
        session.active_turn = Some(turn_id.clone());
        session.info.active_turn_id = Some(turn_id.clone());
        session.info.state = SessionState::Running;
    }
    vec![event(
        session_references(state, None, None),
        Some(turn_id),
        None,
        None,
        HarnessEventPayload::TurnStarted,
    )]
}

fn complete_open_items(
    state: &Arc<Mutex<ClaudeSessionState>>,
    item_state: ChatItemState,
) -> Vec<HarnessEvent> {
    let Some(turn_id) = current_turn(state) else {
        return Vec::new();
    };
    complete_open_items_for_turn(state, &turn_id, item_state)
}

fn complete_open_items_for_turn(
    state: &Arc<Mutex<ClaudeSessionState>>,
    turn_id: &ChatTurnId,
    item_state: ChatItemState,
) -> Vec<HarnessEvent> {
    let (blocks, tools) = {
        let mut session = lock(state);
        let blocks = session
            .blocks
            .drain()
            .map(|(_, block)| block)
            .collect::<Vec<_>>();
        let tools = session
            .tools
            .drain()
            .map(|(_, tool)| tool)
            .collect::<Vec<_>>();
        session.tool_indices.clear();
        (blocks, tools)
    };
    let mut events = Vec::new();
    for block in blocks {
        lock(state)
            .completed_provider_items
            .insert(block.provider_item_id.0.clone());
        events.push(item_completed_event(
            state,
            turn_id.clone(),
            block.item_id,
            block.provider_item_id,
            block.kind,
            item_state,
            vec![ChatContent::Text(TextContent {
                value: block.text.into(),
            })]
            .into(),
        ));
    }
    for tool in tools {
        lock(state)
            .completed_provider_items
            .insert(tool.provider_item_id.0.clone());
        let item_content = tool_content(&tool, true, None).unwrap_or_else(|_| {
            vec![ChatContent::Text(TextContent {
                value: tool.name.clone().into(),
            })]
            .into()
        });
        events.push(item_completed_event(
            state,
            turn_id.clone(),
            tool.item_id,
            tool.provider_item_id,
            tool.kind,
            item_state,
            item_content,
        ));
    }
    events
}

fn tool_content(
    tool: &ToolState,
    is_error: bool,
    result: Option<&RawValue>,
) -> Result<ArcVec<ChatContent>, HarnessError> {
    if tool.kind == ChatItemKind::Plan
        && let Ok(input) = serde_json::from_str::<PlanToolInput>(tool.input.get())
        && let Some(plan) = input.plan.or(input.content)
    {
        return Ok(vec![ChatContent::Text(TextContent { value: plan.into() })].into());
    }
    let value = serde_json::to_value(StructuredToolContent {
        tool: &tool.name,
        input: tool.input.as_ref(),
        result: StructuredToolResult {
            is_error,
            content: result,
        },
    })
    .map_err(|error| {
        harness_error(
            HarnessErrorKind::Internal,
            format!("failed to encode Claude Code tool content: {error}"),
        )
    })?;
    Ok(vec![ChatContent::Structured(StructuredContent { value })].into())
}

fn item_started_event(
    state: &Arc<Mutex<ClaudeSessionState>>,
    turn_id: ChatTurnId,
    item_id: ChatItemId,
    provider_item_id: ProviderItemId,
    kind: ChatItemKind,
) -> HarnessEvent {
    event(
        session_references(state, Some(provider_item_id), None),
        Some(turn_id),
        Some(item_id),
        None,
        HarnessEventPayload::ItemStarted { kind },
    )
}

fn content_delta_event(
    state: &Arc<Mutex<ClaudeSessionState>>,
    turn_id: ChatTurnId,
    item_id: ChatItemId,
    provider_item_id: ProviderItemId,
    delta: String,
) -> HarnessEvent {
    event(
        session_references(state, Some(provider_item_id), None),
        Some(turn_id),
        Some(item_id.clone()),
        None,
        HarnessEventPayload::ChatItemContentAppended(ChatItemContentAppended {
            item_id,
            delta: delta.into(),
        }),
    )
}

fn item_completed_event(
    state: &Arc<Mutex<ClaudeSessionState>>,
    turn_id: ChatTurnId,
    item_id: ChatItemId,
    provider_item_id: ProviderItemId,
    kind: ChatItemKind,
    item_state: ChatItemState,
    content: ArcVec<ChatContent>,
) -> HarnessEvent {
    event(
        session_references(state, Some(provider_item_id), None),
        Some(turn_id),
        Some(item_id),
        None,
        HarnessEventPayload::ItemCompleted {
            kind,
            state: item_state,
            content,
        },
    )
}

fn turn_completed_event(
    state: &Arc<Mutex<ClaudeSessionState>>,
    turn_id: ChatTurnId,
    turn_state: ChatTurnState,
    failure: Option<ChatFailure>,
) -> HarnessEvent {
    event(
        session_references(state, None, None),
        Some(turn_id),
        None,
        None,
        HarnessEventPayload::TurnCompleted {
            state: turn_state,
            error: failure,
        },
    )
}

fn finish_turn(
    server: &ClaudeProcess,
    state: &Arc<Mutex<ClaudeSessionState>>,
    turn_id: ChatTurnId,
    turn_state: ChatTurnState,
    failure: Option<ChatFailure>,
) {
    clear_active_turn(
        state,
        &turn_id,
        if turn_state == ChatTurnState::Failed {
            SessionState::Failed
        } else {
            SessionState::Ready
        },
    );
    server.emit(vec![turn_completed_event(
        state, turn_id, turn_state, failure,
    )]);
}

fn clear_active_turn(
    state: &Arc<Mutex<ClaudeSessionState>>,
    turn_id: &ChatTurnId,
    session_state: SessionState,
) {
    let mut state = lock(state);
    if state.active_turn.as_ref() == Some(turn_id) {
        state.active_turn = None;
        state.info.active_turn_id = None;
        state.info.state = session_state;
    }
}

fn event(
    provider_references: ProviderEventReferences,
    turn_id: Option<ChatTurnId>,
    item_id: Option<ChatItemId>,
    request_id: Option<ChatRequestId>,
    payload: HarnessEventPayload,
) -> HarnessEvent {
    HarnessEvent {
        occurred_at: Timestamp::now(),
        turn_id,
        item_id,
        request_id,
        provider_references,
        payload,
    }
}

fn session_references(
    state: &Arc<Mutex<ClaudeSessionState>>,
    provider_item_id: Option<ProviderItemId>,
    provider_request_id: Option<ProviderRequestId>,
) -> ProviderEventReferences {
    ProviderEventReferences {
        provider_session_id: Some(lock(state).provider_session_id.clone()),
        provider_turn_id: None,
        provider_item_id,
        provider_request_id,
    }
}

fn current_turn(state: &Arc<Mutex<ClaudeSessionState>>) -> Option<ChatTurnId> {
    lock(state).turn_queue.front().cloned()
}

fn native_turn_is_completed(state: &Arc<Mutex<ClaudeSessionState>>, turn_id: &ChatTurnId) -> bool {
    lock(state).completed_turns.contains(turn_id)
}

fn base_warning(
    state: &Arc<Mutex<ClaudeSessionState>>,
    code: &str,
    message: String,
) -> HarnessEvent {
    event(
        session_references(state, None, None),
        current_turn(state),
        None,
        None,
        HarnessEventPayload::Warning {
            code: code.to_owned(),
            message,
        },
    )
}

fn unknown_event(
    state: &Arc<Mutex<ClaudeSessionState>>,
    native_type: String,
    message: &RawJson,
) -> HarnessEvent {
    event(
        session_references(state, None, None),
        current_turn(state),
        None,
        None,
        HarnessEventPayload::Unknown {
            native_type,
            payload: format!("{} byte JSON payload", message.get().len()),
        },
    )
}

fn unexpected_exit(state: &Arc<Mutex<ClaudeSessionState>>) -> HarnessEvent {
    mark_exited(state, true);
    session_exited(
        state,
        Some(harness_error(
            HarnessErrorKind::ProcessExited,
            "the Claude Code process message stream ended unexpectedly",
        )),
    )
}

fn session_exited(
    state: &Arc<Mutex<ClaudeSessionState>>,
    failure: Option<HarnessError>,
) -> HarnessEvent {
    event(
        session_references(state, None, None),
        None,
        None,
        None,
        HarnessEventPayload::SessionExited { error: failure },
    )
}

fn mark_exited(state: &Arc<Mutex<ClaudeSessionState>>, failed: bool) {
    let mut state = lock(state);
    state.info.state = if failed {
        SessionState::Failed
    } else {
        SessionState::Stopped
    };
    state.info.active_turn_id = None;
    state.active_turn = None;
}

fn ensure_running(state: &ClaudeSessionState) -> Result<(), HarnessError> {
    if state.stopped {
        Err(harness_error(
            HarnessErrorKind::SessionNotFound,
            "the Claude Code session has stopped",
        ))
    } else {
        Ok(())
    }
}

fn content_block_provider_id(state: &Arc<Mutex<ClaudeSessionState>>, index: u64) -> String {
    lock(state)
        .current_message_id
        .as_ref()
        .map_or_else(|| format!("content:{index}"), |id| format!("{id}:{index}"))
}

fn tool_kind(name: &str) -> ChatItemKind {
    match name {
        "Bash" => ChatItemKind::CommandExecution,
        "Edit" | "MultiEdit" | "Write" => ChatItemKind::FileChange,
        "WebSearch" | "WebFetch" => ChatItemKind::WebSearch,
        "Task" => ChatItemKind::Subagent,
        _ => ChatItemKind::ToolCall,
    }
}

fn canonical_user_content(
    text: Option<ArcStr>,
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

async fn prompt_content(
    text: Option<ArcStr>,
    attachments: &[HarnessPromptAttachment],
) -> Result<Vec<UserContent>, HarnessError> {
    let files = attachments
        .iter()
        .filter(|attachment| !attachment.media_type.starts_with("image/"))
        .collect::<Vec<_>>();
    let mut prompt_text = text.map_or_else(String::new, |text| text.to_string());
    if !files.is_empty() {
        if !prompt_text.is_empty() {
            prompt_text.push_str("\n\n");
        }
        prompt_text.push_str("<tascarrel_attachments>\n");
        for attachment in files {
            writeln!(prompt_text, "- {}: {}", attachment.name, attachment.path)
                .expect("writing to a String cannot fail");
        }
        prompt_text.push_str("</tascarrel_attachments>");
    }
    let mut content = Vec::new();
    if !prompt_text.is_empty() {
        content.push(UserContent::Text { text: prompt_text });
    }
    for attachment in attachments
        .iter()
        .filter(|attachment| attachment.media_type.starts_with("image/"))
    {
        if !matches!(
            attachment.media_type.as_ref(),
            "image/gif" | "image/jpeg" | "image/png" | "image/webp"
        ) {
            return Err(harness_error(
                HarnessErrorKind::RequestFailed,
                format!(
                    "Claude Code does not support image attachment type {}",
                    attachment.media_type
                ),
            ));
        }
        let bytes = tokio::fs::read(attachment.source_path.as_ref())
            .await
            .map_err(|error| {
                harness_error(
                    HarnessErrorKind::RequestFailed,
                    format!(
                        "failed to read Claude Code attachment {}: {error}",
                        attachment.name
                    ),
                )
            })?;
        content.push(UserContent::Image {
            source: ImageSource {
                kind: "base64",
                media_type: attachment.media_type.clone(),
                data: BASE64.encode(bytes),
            },
        });
    }
    if content.is_empty() {
        return Err(harness_error(
            HarnessErrorKind::RequestFailed,
            "a Claude Code prompt must contain text or an attachment",
        ));
    }
    Ok(content)
}

fn normalize_model(model: NativeModel) -> Option<ChatModel> {
    if model.value == "default" {
        return None;
    }
    let id = model.resolved_model.unwrap_or(model.value);
    let display_name = model.display_name.unwrap_or_else(|| id.clone());
    let options = if model.supported_effort_levels.is_empty() {
        ArcVec::new()
    } else {
        vec![ChatModelOptionDescriptor::Select(
            ChatModelSelectOptionDescriptor {
                id: "effort".into(),
                label: "Effort".into(),
                description: Some("How much effort Claude should spend on the task.".into()),
                choices: model
                    .supported_effort_levels
                    .into_iter()
                    .map(|effort| ChatModelOptionChoice {
                        is_default: effort == "high",
                        label: effort_label(&effort).into(),
                        description: None,
                        id: effort.into(),
                    })
                    .collect::<Vec<_>>()
                    .into(),
            },
        )]
        .into()
    };
    Some(ChatModel {
        short_name: model_short_name(&display_name, &id).map(Into::into),
        id: id.into(),
        display_name: display_name.into(),
        is_custom: false,
        options,
        pricing: None,
    })
}

fn curate_models(version: &str, native_models: Vec<ChatModel>) -> Vec<ChatModel> {
    let mut models = CURATED_MODELS
        .iter()
        .filter(|model| {
            model
                .minimum_version
                .is_none_or(|minimum| version_at_least(version, minimum))
        })
        .map(curated_model)
        .collect::<Vec<_>>();
    models.extend(native_models.into_iter().filter(|native| {
        !CURATED_MODELS
            .iter()
            .any(|curated| model_matches(native.id.as_ref(), curated.id))
    }));
    models
}

fn curated_model(model: &CuratedClaudeModel) -> ChatModel {
    let mut options = Vec::new();
    if !model.efforts.is_empty() {
        options.push(ChatModelOptionDescriptor::Select(
            ChatModelSelectOptionDescriptor {
                id: "effort".into(),
                label: "Effort".into(),
                description: Some("How much effort Claude should spend on the task.".into()),
                choices: model
                    .efforts
                    .iter()
                    .map(|effort| ChatModelOptionChoice {
                        id: (*effort).into(),
                        label: effort_label(effort).into(),
                        description: None,
                        is_default: model.default_effort == Some(*effort),
                    })
                    .collect::<Vec<_>>()
                    .into(),
            },
        ));
    }
    if model.context_window {
        options.push(ChatModelOptionDescriptor::Select(
            ChatModelSelectOptionDescriptor {
                id: "contextWindow".into(),
                label: "Context Window".into(),
                description: Some("Maximum context made available to Claude Code.".into()),
                choices: vec![
                    ChatModelOptionChoice {
                        id: "200k".into(),
                        label: "200k".into(),
                        description: None,
                        is_default: true,
                    },
                    ChatModelOptionChoice {
                        id: "1m".into(),
                        label: "1M".into(),
                        description: None,
                        is_default: false,
                    },
                ]
                .into(),
            },
        ));
    }
    ChatModel {
        id: model.id.into(),
        display_name: model.display_name.into(),
        short_name: Some(model.short_name.into()),
        is_custom: false,
        options: options.into(),
        pricing: None,
    }
}

fn model_matches(candidate: &str, curated: &str) -> bool {
    let candidate = candidate.split_once('[').map_or(candidate, |(id, _)| id);
    candidate == curated
        || (curated == "claude-haiku-4-5"
            && candidate.strip_prefix(curated).is_some_and(|suffix| {
                suffix.strip_prefix('-').is_some_and(|suffix| {
                    suffix.chars().all(|character| character.is_ascii_digit())
                })
            }))
}

fn version_at_least(version: &str, minimum: (u64, u64, u64)) -> bool {
    let mut parts = version
        .split_once('-')
        .map_or(version, |(version, _)| version)
        .split('.');
    let Some(version) = (|| {
        Some((
            parts.next()?.parse::<u64>().ok()?,
            parts.next()?.parse::<u64>().ok()?,
            parts.next()?.parse::<u64>().ok()?,
        ))
    })() else {
        return false;
    };
    version >= minimum
}

fn model_short_name(display_name: &str, model: &str) -> Option<String> {
    let model = model.strip_prefix("claude-")?;
    let model = model.split_once('[').map_or(model, |(model, _)| model);
    let mut parts = model.split('-');
    parts.next()?;
    let major = parts.next()?.parse::<u16>().ok()?;
    let minor = parts
        .next()
        .filter(|part| part.len() <= 2)
        .and_then(|part| part.parse::<u16>().ok());
    Some(minor.map_or_else(
        || format!("{display_name} {major}"),
        |minor| format!("{display_name} {major}.{minor}"),
    ))
}

fn effort_label(effort: &str) -> String {
    match effort {
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        _ => effort,
    }
    .to_owned()
}

fn normalize_usage(
    usage: &NativeUsage,
    model_usage: HashMap<String, NativeUsage>,
) -> ChatUsageSnapshot {
    let tokens = token_usage(usage);
    let models = model_usage
        .into_iter()
        .map(|(model, usage)| ChatModelUsage {
            model: ChatModelSelection {
                model: model.into(),
                options: ArcVec::new(),
            },
            tokens: token_usage(&usage),
            pricing: None,
            provider_estimated_cost: None,
        })
        .collect::<Vec<_>>()
        .into();
    ChatUsageSnapshot {
        coverage: ChatUsageCoverage::ExecutionTree,
        tokens,
        models,
        provider_estimated_cost: None,
    }
}

fn token_usage(usage: &NativeUsage) -> ChatTokenUsage {
    ChatTokenUsage {
        input_tokens: usage
            .input_tokens
            .saturating_add(usage.cache_read_input_tokens)
            .saturating_add(usage.cache_creation_input_tokens),
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: Some(usage.cache_read_input_tokens),
        cache_write_input_tokens: Some(usage.cache_creation_input_tokens),
        cache_writes_by_ttl: ArcVec::new(),
        reasoning_output_tokens: None,
    }
}

/// Reads only the provider identifiers needed to resume a Claude Code session.
///
/// Resume cursors are intentionally opaque. The adaptor accepts historical
/// provider spellings and ignores every field that is not needed at launch.
fn parse_resume_cursor(cursor: &ResumeCursor) -> Result<ResumeState, HarnessError> {
    let provider_session_id = cursor
        .0
        .pointer("/session_id")
        .or_else(|| cursor.0.pointer("/sessionId"))
        .or_else(|| cursor.0.pointer("/resume"))
        .and_then(JsonValue::as_str)
        .ok_or_else(|| {
            harness_error(
                HarnessErrorKind::InvalidResumeCursor,
                "the Claude Code resume cursor contains no session id",
            )
        })?;
    Uuid::parse_str(provider_session_id).map_err(|_| {
        harness_error(
            HarnessErrorKind::InvalidResumeCursor,
            "the Claude Code resume cursor session id is not a UUID",
        )
    })?;
    let resume_session_at = cursor
        .0
        .pointer("/resume_session_at")
        .or_else(|| cursor.0.pointer("/resumeSessionAt"))
        .and_then(JsonValue::as_str)
        .map(str::to_owned);
    if let Some(message_id) = &resume_session_at {
        Uuid::parse_str(message_id).map_err(|_| {
            harness_error(
                HarnessErrorKind::InvalidResumeCursor,
                "the Claude Code resume cursor message id is not a UUID",
            )
        })?;
    }
    Ok(ResumeState {
        provider_session_id: ProviderSessionId(provider_session_id.to_owned()),
        resume_session_at,
    })
}

fn resume_cursor(
    provider_session_id: &ProviderSessionId,
    resume_session_at: Option<&str>,
) -> ResumeCursor {
    ResumeCursor(JsonValue::Object(serde_json::Map::from_iter([
        (
            "session_id".to_owned(),
            JsonValue::String(provider_session_id.0.clone()),
        ),
        (
            "resume_session_at".to_owned(),
            resume_session_at.map_or(JsonValue::Null, |message_id| {
                JsonValue::String(message_id.to_owned())
            }),
        ),
    ])))
}

fn selected_string_option<'a>(selection: &'a ChatModelSelection, id: &str) -> Option<&'a str> {
    selection.options.iter().find_map(|option| {
        if option.id.as_ref() != id {
            return None;
        }
        match &option.value {
            ChatModelOptionValue::String(value) => Some(value.as_ref()),
            ChatModelOptionValue::Boolean(_) => None,
        }
    })
}

fn native_model_id(selection: &ChatModelSelection) -> String {
    let model = selection.model.as_ref();
    if selected_string_option(selection, "contextWindow") == Some("1m") && !model.ends_with("[1m]")
    {
        format!("{model}[1m]")
    } else {
        model.to_owned()
    }
}

type RawJson = Box<RawValue>;
type PendingResponses = Arc<Mutex<HashMap<String, oneshot::Sender<Result<RawJson, HarnessError>>>>>;

fn empty_raw_json() -> RawJson {
    serde_json::from_str("{}").expect("an empty object is valid JSON")
}

fn null_raw_json() -> RawJson {
    serde_json::from_str("null").expect("null is valid JSON")
}

struct LaunchOptions {
    model: Option<ChatModelSelection>,
    provider_session_id: ProviderSessionId,
    resume_session_at: Option<String>,
    resuming: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            model: None,
            provider_session_id: ProviderSessionId(String::new()),
            resume_session_at: None,
            resuming: false,
        }
    }
}

struct ResumeState {
    provider_session_id: ProviderSessionId,
    resume_session_at: Option<String>,
}

struct StartedClaudeProcess {
    control: Arc<ClaudeProcess>,
    messages: ClaudeMessages,
}

struct ClaudeMessages {
    receiver: mpsc::UnboundedReceiver<ClaudeMessage>,
}

enum ClaudeMessage {
    Native(RawJson),
    Events(Vec<HarnessEvent>),
    Exited(Option<HarnessError>),
}

struct ClaudeProcess {
    process: Arc<dyn HarnessProcessControl>,
    pending: PendingResponses,
    messages: mpsc::UnboundedSender<ClaudeMessage>,
    next_request_id: AtomicU64,
}

impl ClaudeProcess {
    async fn request<P, R>(&self, request: P) -> Result<R, HarnessError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let request_id = format!(
            "{REQUEST_ID_PREFIX}{}",
            self.next_request_id.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        lock_pending(&self.pending).insert(request_id.clone(), sender);
        if let Err(error) = self
            .write_message(&ControlRequestEnvelope {
                kind: "control_request",
                request_id: &request_id,
                request,
            })
            .await
        {
            lock_pending(&self.pending).remove(&request_id);
            return Err(error);
        }
        let response = receiver.await.map_err(|_| {
            harness_error(
                HarnessErrorKind::ProcessExited,
                "Claude Code exited before replying to a control request",
            )
        })??;
        serde_json::from_str(response.get()).map_err(|error| {
            harness_error(
                HarnessErrorKind::Protocol,
                format!("failed to decode a Claude Code control response: {error}"),
            )
        })
    }

    async fn respond<R>(&self, request_id: &str, response: R) -> Result<(), HarnessError>
    where
        R: Serialize,
    {
        self.write_message(&ControlResponseEnvelope {
            kind: "control_response",
            response: ControlResponseSuccess {
                subtype: "success",
                request_id,
                response,
            },
        })
        .await
    }

    async fn respond_error(&self, request_id: &str, message: String) -> Result<(), HarnessError> {
        self.write_message(&ControlResponseEnvelope {
            kind: "control_response",
            response: ControlResponseFailure {
                subtype: "error",
                request_id,
                error: message,
            },
        })
        .await
    }

    async fn write_message<T: Serialize>(&self, message: &T) -> Result<(), HarnessError> {
        let mut bytes = serde_json::to_vec(message).map_err(|error| {
            harness_error(
                HarnessErrorKind::Internal,
                format!("failed to encode a Claude Code message: {error}"),
            )
        })?;
        bytes.push(b'\n');
        self.process.write(bytes).await
    }

    fn emit(&self, events: Vec<HarnessEvent>) {
        if !events.is_empty() && self.messages.send(ClaudeMessage::Events(events)).is_err() {
            tracing::debug!("Claude Code event receiver closed before event delivery");
        }
    }

    async fn stop(&self) -> Result<(), HarnessError> {
        self.process.stop().await
    }
}

async fn start_server(
    executable: PathBuf,
    launcher: Arc<dyn HarnessProcessLauncher>,
    options: &LaunchOptions,
    process_environment: Option<&dyn ProcessEnvironment>,
    working_directory: PathBuf,
) -> Result<StartedClaudeProcess, HarnessError> {
    let mut environment = process_environment
        .map(ProcessEnvironment::variables)
        .transpose()
        .map_err(|error| {
            harness_error(
                HarnessErrorKind::ProcessStart,
                format!("failed to configure the Claude Code environment: {error}"),
            )
        })?
        .unwrap_or_default();
    environment
        .entry("CLAUDE_CODE_ENTRYPOINT".to_owned())
        .or_insert_with(|| "sdk-ts".to_owned());
    environment.remove("NODE_OPTIONS");
    let mut arguments = vec![
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--verbose".to_owned(),
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--permission-prompt-tool".to_owned(),
        "stdio".to_owned(),
        "--permission-mode".to_owned(),
        "bypassPermissions".to_owned(),
        "--allow-dangerously-skip-permissions".to_owned(),
        "--include-partial-messages".to_owned(),
        format!("--setting-sources={SETTING_SOURCES}"),
    ];
    if let Some(model) = &options.model {
        arguments.extend(["--model".to_owned(), native_model_id(model)]);
        if let Some(effort) = selected_string_option(model, "effort") {
            arguments.extend(["--effort".to_owned(), effort.to_owned()]);
        }
    }
    if options.resuming {
        arguments.extend(["--resume".to_owned(), options.provider_session_id.0.clone()]);
        if let Some(message_id) = &options.resume_session_at {
            arguments.extend(["--resume-session-at".to_owned(), message_id.clone()]);
        }
    } else if !options.provider_session_id.0.is_empty() {
        arguments.extend([
            "--session-id".to_owned(),
            options.provider_session_id.0.clone(),
        ]);
    }
    let process = launcher
        .launch(HarnessProcessSpec {
            title: "Claude Code chat harness".to_owned(),
            executable,
            arguments,
            environment,
            working_directory,
        })
        .await?;
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(read_messages(
        process.stdout,
        Arc::clone(&pending),
        sender.clone(),
    ));
    Ok(StartedClaudeProcess {
        control: Arc::new(ClaudeProcess {
            process: process.control,
            pending,
            messages: sender,
            next_request_id: AtomicU64::new(1),
        }),
        messages: ClaudeMessages { receiver },
    })
}

async fn read_messages(
    stdout: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    pending: PendingResponses,
    sender: mpsc::UnboundedSender<ClaudeMessage>,
) {
    let mut lines = BufReader::new(stdout).lines();
    let exit_error = loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break None,
            Err(error) => {
                break Some(harness_error(
                    HarnessErrorKind::ProcessExited,
                    format!("Claude Code communication failed: {error}"),
                ));
            }
        };
        let raw: RawJson = match serde_json::from_str(&line) {
            Ok(raw) => raw,
            Err(error) => {
                break Some(harness_error(
                    HarnessErrorKind::Protocol,
                    format!("Claude Code emitted invalid JSON: {error}"),
                ));
            }
        };
        let header: MessageHeader = match decode(&raw, "message header") {
            Ok(header) => header,
            Err(error) => break Some(error),
        };
        if header.kind == "control_response" {
            let response: IncomingControlResponse = match decode(&raw, "control response") {
                Ok(response) => response,
                Err(error) => break Some(error),
            };
            let Some(sender) = lock_pending(&pending).remove(&response.response.request_id) else {
                continue;
            };
            let result = if response.response.subtype == "success" {
                Ok(response.response.response.unwrap_or_else(null_raw_json))
            } else {
                Err(harness_error(
                    HarnessErrorKind::RequestFailed,
                    response
                        .response
                        .error
                        .unwrap_or_else(|| "Claude Code control request failed".to_owned()),
                ))
            };
            if sender.send(result).is_err() {
                tracing::debug!("Claude Code response receiver closed before delivery");
            }
        } else if sender.send(ClaudeMessage::Native(raw)).is_err() {
            tracing::debug!("Claude Code event receiver closed before native event delivery");
            return;
        }
    };
    let pending_error = exit_error.clone().unwrap_or_else(|| {
        harness_error(
            HarnessErrorKind::ProcessExited,
            "Claude Code exited while control requests were pending",
        )
    });
    for (_, response) in lock_pending(&pending).drain() {
        if response.send(Err(pending_error.clone())).is_err() {
            tracing::debug!("Claude Code response receiver closed during process shutdown");
        }
    }
    if sender.send(ClaudeMessage::Exited(exit_error)).is_err() {
        tracing::debug!("Claude Code event receiver closed before the exit event");
    }
}

fn decode<T: DeserializeOwned>(raw: &RawJson, context: &str) -> Result<T, HarnessError> {
    serde_json::from_str(raw.get()).map_err(|error| {
        harness_error(
            HarnessErrorKind::Protocol,
            format!("failed to decode Claude Code {context}: {error}"),
        )
    })
}

fn decode_claude_block(raw: &RawJson) -> Result<ClaudeBlock, HarnessError> {
    let header: ClaudeBlockHeader = decode(raw, "content block header")?;
    match header.kind.as_str() {
        "text" => decode::<TextBlock>(raw, "text content block")
            .map(|block| ClaudeBlock::Text { text: block.text }),
        "thinking" => decode::<ThinkingBlock>(raw, "thinking content block").map(|block| {
            ClaudeBlock::Thinking {
                thinking: block.thinking,
            }
        }),
        "redacted_thinking" => Ok(ClaudeBlock::RedactedThinking),
        "tool_use" => decode::<ToolUseBlock>(raw, "tool-use content block").map(|block| {
            ClaudeBlock::ToolUse {
                id: block.id,
                name: block.name,
                input: block.input,
            }
        }),
        "server_tool_use" => {
            decode::<ToolUseBlock>(raw, "server-tool-use content block").map(|block| {
                ClaudeBlock::ServerToolUse {
                    id: block.id,
                    name: block.name,
                    input: block.input,
                }
            })
        }
        "mcp_tool_use" => decode::<ToolUseBlock>(raw, "MCP-tool-use content block").map(|block| {
            ClaudeBlock::McpToolUse {
                id: block.id,
                name: block.name,
                input: block.input,
            }
        }),
        "tool_result" => decode::<ToolResultBlock>(raw, "tool-result content block").map(|block| {
            ClaudeBlock::ToolResult {
                tool_use_id: block.tool_use_id,
                is_error: block.is_error,
                content: block.content,
            }
        }),
        _ => Ok(ClaudeBlock::Other),
    }
}

fn harness_error(kind: HarnessErrorKind, message: impl Into<String>) -> HarnessError {
    HarnessError {
        kind,
        message: message.into(),
        retryable: false,
    }
}

fn lock(state: &Arc<Mutex<ClaudeSessionState>>) -> MutexGuard<'_, ClaudeSessionState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_pending(
    pending: &PendingResponses,
) -> MutexGuard<'_, HashMap<String, oneshot::Sender<Result<RawJson, HarnessError>>>> {
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Deserialize)]
struct MessageHeader {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
enum ControlCommand {
    Initialize,
    SetModel { model: Option<String> },
    Interrupt,
}

#[derive(Serialize)]
struct ControlRequestEnvelope<'a, P> {
    #[serde(rename = "type")]
    kind: &'static str,
    request_id: &'a str,
    request: P,
}

#[derive(Serialize)]
struct ControlResponseEnvelope<R> {
    #[serde(rename = "type")]
    kind: &'static str,
    response: R,
}

#[derive(Serialize)]
struct ControlResponseSuccess<'a, R> {
    subtype: &'static str,
    request_id: &'a str,
    response: R,
}

#[derive(Serialize)]
struct ControlResponseFailure<'a> {
    subtype: &'static str,
    request_id: &'a str,
    error: String,
}

#[derive(Deserialize)]
struct IncomingControlResponse {
    response: IncomingControlResponseBody,
}

#[derive(Deserialize)]
struct IncomingControlResponseBody {
    subtype: String,
    request_id: String,
    response: Option<RawJson>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct InitializeResponse {
    #[serde(default)]
    models: Vec<NativeModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeModel {
    value: String,
    resolved_model: Option<String>,
    display_name: Option<String>,
    #[serde(default)]
    supported_effort_levels: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolPermissionResponse<'a, T> {
    behavior: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "toolUseID")]
    tool_use_id: Option<&'a str>,
    updated_input: T,
}

#[derive(Deserialize)]
struct ControlRequestMessage {
    request_id: String,
    request: NativeControlRequest,
}

#[derive(Deserialize)]
struct NativeControlRequest {
    subtype: String,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    input: RawJson,
}

#[derive(Deserialize)]
struct PlanToolInput {
    plan: Option<String>,
    content: Option<String>,
}

#[derive(Serialize)]
struct StructuredToolContent<'a> {
    tool: &'a str,
    input: &'a RawValue,
    result: StructuredToolResult<'a>,
}

#[derive(Serialize)]
struct StructuredToolResult<'a> {
    is_error: bool,
    content: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct ControlCancelMessage {
    request_id: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeQuestion {
    question: String,
    header: Option<String>,
    #[serde(default)]
    options: Vec<NativeQuestionOption>,
    #[serde(default)]
    multi_select: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct NativeQuestionOption {
    label: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct AskUserQuestionInput {
    questions: Vec<NativeQuestion>,
}

#[derive(Serialize)]
struct AskUserQuestionResponse<'a> {
    questions: &'a [NativeQuestion],
    answers: HashMap<String, NativeAnswer>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum NativeAnswer {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct StreamEventMessage {
    event: RawJson,
}

#[derive(Deserialize)]
struct StreamEventHeader {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct MessageStartEvent {
    message: MessageStartBody,
}

#[derive(Deserialize)]
struct ContentBlockStartEvent {
    index: u64,
    content_block: RawJson,
}

#[derive(Deserialize)]
struct ContentBlockDeltaEvent {
    index: u64,
    delta: ClaudeDelta,
}

#[derive(Deserialize)]
struct ContentBlockStopEvent {
    index: u64,
}

#[derive(Deserialize)]
struct MessageStartBody {
    id: String,
}

enum ClaudeBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    RedactedThinking,
    ToolUse {
        id: String,
        name: String,
        input: RawJson,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: RawJson,
    },
    McpToolUse {
        id: String,
        name: String,
        input: RawJson,
    },
    ToolResult {
        tool_use_id: String,
        is_error: bool,
        content: Option<RawJson>,
    },
    Other,
}

#[derive(Deserialize)]
struct ClaudeBlockHeader {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct TextBlock {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct ThinkingBlock {
    #[serde(default)]
    thinking: String,
}

#[derive(Deserialize)]
struct ToolUseBlock {
    id: String,
    name: String,
    #[serde(default = "empty_raw_json")]
    input: RawJson,
}

#[derive(Deserialize)]
struct ToolResultBlock {
    tool_use_id: String,
    #[serde(default)]
    is_error: bool,
    content: Option<RawJson>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClaudeDelta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    InputJsonDelta {
        #[serde(default)]
        partial_json: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct AssistantMessage {
    message: AssistantMessageBody,
    uuid: Option<String>,
}

#[derive(Deserialize)]
struct AssistantMessageBody {
    id: Option<String>,
    #[serde(default)]
    content: Vec<RawJson>,
}

#[derive(Deserialize)]
struct NativeUserMessage {
    message: NativeUserMessageBody,
}

#[derive(Deserialize)]
struct NativeUserMessageBody {
    #[serde(default = "null_raw_json")]
    /// Provider-owned synthetic messages can use scalar or otherwise opaque
    /// content.
    content: RawJson,
}

#[derive(Deserialize)]
struct ResultMessage {
    subtype: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    errors: Vec<String>,
    usage: Option<NativeUsage>,
    #[serde(rename = "modelUsage", default)]
    model_usage: HashMap<String, NativeUsage>,
}

#[derive(Clone, Deserialize)]
#[allow(clippy::struct_field_names)] // Field names mirror Claude's token-usage schema.
struct NativeUsage {
    #[serde(default, alias = "inputTokens")]
    input_tokens: u64,
    #[serde(default, alias = "outputTokens")]
    output_tokens: u64,
    #[serde(default, alias = "cacheReadInputTokens")]
    cache_read_input_tokens: u64,
    #[serde(default, alias = "cacheCreationInputTokens")]
    cache_creation_input_tokens: u64,
}

#[derive(Deserialize)]
struct SystemMessage {
    subtype: String,
    session_id: Option<String>,
    model: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct UserMessage {
    #[serde(rename = "type")]
    kind: &'static str,
    session_id: &'static str,
    parent_tool_use_id: Option<&'static str>,
    message: UserMessageBody,
}

#[derive(Serialize)]
struct UserMessageBody {
    role: &'static str,
    content: Vec<UserContent>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UserContent {
    Text { text: String },
    Image { source: ImageSource },
}

#[derive(Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: ArcStr,
    data: String,
}

#[cfg(test)]
mod tests {
    use tascarrel_api::types::chats::ChatModelOptionSelection;

    use super::*;

    /// Confirms that the pinned catalog supplies missing models in its curated
    /// order, gates new entries by version, and maps the context option to the
    /// native Claude model spelling.
    #[test]
    fn curates_versioned_models_and_native_context_selection() {
        let native = vec![
            model("claude-sonnet-5"),
            model("claude-opus-5"),
            model("claude-opus-4-8"),
            model("claude-haiku-4-5-20251001"),
            model("claude-future-6"),
        ];
        let current = curate_models("2.1.220", native.clone());
        let ids = current
            .iter()
            .map(|model| model.id.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "claude-opus-5",
                "claude-fable-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-opus-4-6",
                "claude-opus-4-5",
                "claude-sonnet-5",
                "claude-sonnet-4-6",
                "claude-haiku-4-5",
                "claude-future-6",
            ]
        );
        assert!(
            curate_models("2.1.168", native.clone())
                .iter()
                .all(|model| model.id.as_ref() != "claude-fable-5")
        );
        assert!(
            curate_models("2.1.218", native)
                .iter()
                .all(|model| model.id.as_ref() != "claude-opus-5")
        );

        let selection = ChatModelSelection {
            model: "claude-opus-5".into(),
            options: vec![ChatModelOptionSelection {
                id: "contextWindow".into(),
                value: ChatModelOptionValue::String("1m".into()),
            }]
            .into(),
        };
        assert_eq!(native_model_id(&selection), "claude-opus-5[1m]");
    }

    fn model(id: &str) -> ChatModel {
        ChatModel {
            id: id.into(),
            display_name: id.into(),
            short_name: None,
            is_custom: false,
            options: ArcVec::new(),
            pricing: None,
        }
    }
}
