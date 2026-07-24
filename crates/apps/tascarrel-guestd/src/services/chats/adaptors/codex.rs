//! Codex app-server adaptor.
//!
//! Each Tascarrel harness session owns an independent typed JSON-RPC connection
//! to one `codex app-server` process.
//!
//! Provider wire types in this module are intentionally minimal. They contain
//! only fields consumed by the adaptor, and Serde ignores every other field.
//! Do not expand them merely to mirror the complete provider protocol.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

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
use crate::services::chats::harness::protocol::HarnessPromptAttachment;
use crate::services::chats::harness::protocol::HarnessSessionInfo;
use crate::services::chats::harness::protocol::ProviderEventReferences;
use crate::services::chats::harness::protocol::ProviderItemId;
use crate::services::chats::harness::protocol::ProviderRequestId;
use crate::services::chats::harness::protocol::ProviderSessionId;
use crate::services::chats::harness::protocol::ProviderTurnId;
use crate::services::chats::harness::protocol::ResumeCursor;
use crate::services::chats::harness::protocol::SessionState;
use crate::services::chats::harness::protocol::StartSessionRequest;
use crate::services::chats::process::HarnessProcessControl;
use crate::services::chats::process::HarnessProcessLauncher;
use crate::services::chats::process::HarnessProcessSpec;
use crate::services::chats::process::ProcessEnvironment;

const MISSING_BUBBLEWRAP_WARNING: &str = "Codex could not find bubblewrap on PATH.";
const BUNDLED_BUBBLEWRAP_FALLBACK: &str = "Codex will use the bundled bubblewrap";

/// Codex harness adaptor backed by one app-server connection per session.
#[derive(Clone)]
pub struct CodexAdaptor {
    executable: PathBuf,
    launcher: Arc<dyn HarnessProcessLauncher>,
    process_environment: Option<Arc<dyn ProcessEnvironment>>,
    working_directory: PathBuf,
}

impl CodexAdaptor {
    /// Creates an adaptor for one Codex executable.
    #[must_use]
    pub(crate) fn new(executable: PathBuf, launcher: Arc<dyn HarnessProcessLauncher>) -> Self {
        Self {
            executable,
            launcher,
            process_environment: None,
            working_directory: PathBuf::from("/workspace"),
        }
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

    /// Overrides the working directory used by Codex.
    #[must_use]
    pub(crate) fn with_working_directory(mut self, working_directory: PathBuf) -> Self {
        self.working_directory = working_directory;
        self
    }
}

impl Harness for CodexAdaptor {
    fn models(&self) -> BoxFuture<'_, Result<ArcVec<ChatModel>, HarnessError>> {
        let executable = self.executable.clone();
        let launcher = Arc::clone(&self.launcher);
        let environment = self.process_environment.clone();
        let working_directory = self.working_directory.clone();
        Box::pin(async move {
            let server = start_server(
                executable,
                launcher,
                environment.as_deref(),
                working_directory,
            )
            .await?;
            let result = async {
                initialize(&server.control).await?;
                list_models(&server.control).await
            }
            .await;
            let stopped = server.control.stop().await;
            match (result, stopped) {
                (Ok(models), Ok(())) => Ok(models),
                (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            }
        })
    }

    fn start_session(
        &self,
        request: StartSessionRequest,
    ) -> BoxFuture<'_, Result<HarnessSession, HarnessError>> {
        let executable = self.executable.clone();
        let launcher = Arc::clone(&self.launcher);
        let environment = self.process_environment.clone();
        let working_directory = self.working_directory.clone();
        Box::pin(async move {
            let server = start_server(
                executable,
                launcher,
                environment.as_deref(),
                working_directory,
            )
            .await?;
            if let Err(error) = initialize(&server.control).await {
                if let Err(stop_error) = server.control.stop().await {
                    tracing::warn!(
                        message = %stop_error.message,
                        "failed to stop Codex after initialization failed"
                    );
                }
                return Err(error);
            }
            let opened = match open_thread(&server.control, &request).await {
                Ok(opened) => opened,
                Err(error) => {
                    if let Err(stop_error) = server.control.stop().await {
                        tracing::warn!(
                            message = %stop_error.message,
                            "failed to stop Codex after opening its thread failed"
                        );
                    }
                    return Err(error);
                }
            };
            let cursor = resume_cursor(&opened.provider_session_id);
            let state = Arc::new(Mutex::new(CodexSessionState {
                info: HarnessSessionInfo {
                    state: SessionState::Ready,
                    model: opened.model.or(request.model),
                    active_turn_id: None,
                    resume_cursor: Some(cursor.clone()),
                },
                provider_session_id: opened.provider_session_id.clone(),
                provider_active_turn_id: None,
                active_turn_usage: None,
                turns: HashMap::new(),
                items: HashMap::new(),
                pending_requests: HashMap::new(),
                provider_requests: HashMap::new(),
                pending_prompts: HashMap::new(),
                stopped: false,
            }));
            let pending = VecDeque::from([
                event(
                    references(Some(opened.provider_session_id.clone()), None, None, None),
                    None,
                    None,
                    None,
                    HarnessEventPayload::SessionStarted,
                ),
                event(
                    references(Some(opened.provider_session_id.clone()), None, None, None),
                    None,
                    None,
                    None,
                    HarnessEventPayload::ResumeCursorUpdated {
                        resume_cursor: cursor,
                    },
                ),
                event(
                    references(Some(opened.provider_session_id), None, None, None),
                    None,
                    None,
                    None,
                    HarnessEventPayload::SessionStateChanged {
                        state: SessionState::Ready,
                        reason: None,
                    },
                ),
            ]);
            let control = Arc::new(CodexControl {
                server: Arc::clone(&server.control),
                state: Arc::clone(&state),
                command_lock: AsyncMutex::new(()),
            });
            Ok(HarnessSession {
                control,
                events: Box::new(CodexEvents {
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

struct CodexControl {
    server: Arc<CodexAppServer>,
    state: Arc<Mutex<CodexSessionState>>,
    command_lock: AsyncMutex<()>,
}

impl HarnessControl for CodexControl {
    fn apply(
        &self,
        command: HarnessCommand,
    ) -> BoxFuture<'_, Result<HarnessCommandResult, HarnessError>> {
        Box::pin(async move {
            let _guard = self.command_lock.lock().await;
            match command {
                HarnessCommand::SendPrompt(prompt) => self.send_prompt(prompt).await,
                HarnessCommand::InterruptAndSend(prompt) => {
                    if lock(&self.state).provider_active_turn_id.is_some() {
                        self.interrupt().await?;
                    }
                    self.send_prompt(prompt).await
                }
                HarnessCommand::Interrupt => self.interrupt().await,
                HarnessCommand::CompactContext => self.compact().await,
                HarnessCommand::ResolveUserInput {
                    request_id,
                    answers,
                } => self.resolve_user_input(request_id, answers).await,
                HarnessCommand::Stop => self.stop().await,
            }
        })
    }
}

impl CodexControl {
    async fn send_prompt(
        &self,
        prompt: crate::services::chats::harness::protocol::HarnessPrompt,
    ) -> Result<HarnessCommandResult, HarnessError> {
        let input = codex_input(prompt.text.clone(), &prompt.attachments)?;
        let pending_prompt = PendingPrompt {
            text: prompt.text,
            attachments: prompt.attachments,
        };
        let (provider_session_id, active_turn, current_model) = {
            let state = lock(&self.state);
            ensure_running(&state)?;
            (
                state.provider_session_id.clone(),
                state
                    .provider_active_turn_id
                    .clone()
                    .zip(state.info.active_turn_id.clone()),
                state.info.model.clone(),
            )
        };
        if let Some((provider_turn_id, turn_id)) = active_turn {
            if prompt
                .model
                .as_ref()
                .is_some_and(|model| current_model.as_ref() != Some(model))
            {
                return Err(harness_error(
                    HarnessErrorKind::UnsupportedOperation,
                    "Codex cannot change models while steering an active turn",
                ));
            }
            let client_message_id = register_pending_prompt(&self.state, pending_prompt);
            let result: TurnSteerResult = self
                .server
                .request(
                    "turn/steer",
                    TurnSteerParams {
                        thread_id: provider_session_id.0,
                        expected_turn_id: provider_turn_id.0.clone(),
                        client_user_message_id: client_message_id.clone(),
                        input,
                    },
                )
                .await
                .inspect_err(|_| discard_pending_prompt(&self.state, &client_message_id))?;
            if result.turn_id != provider_turn_id.0 {
                discard_pending_prompt(&self.state, &client_message_id);
                return Err(harness_error(
                    HarnessErrorKind::Protocol,
                    "Codex returned a different turn while steering",
                ));
            }
            return Ok(HarnessCommandResult::PromptAccepted {
                turn_id,
                provider_turn_id: Some(provider_turn_id),
            });
        }

        let model = prompt.model.or(current_model);
        let client_message_id = register_pending_prompt(&self.state, pending_prompt);
        let params = TurnStartParams {
            thread_id: provider_session_id.0,
            input,
            summary: "detailed",
            model: model.as_ref().map(|model| model.model.to_string()),
            effort: model
                .as_ref()
                .and_then(|model| selected_string_option(model, "reasoningEffort"))
                .map(str::to_owned),
            service_tier: model
                .as_ref()
                .and_then(|model| selected_string_option(model, "serviceTier"))
                .map(str::to_owned),
            client_user_message_id: client_message_id.clone(),
        };
        let result: TurnStartResult = self
            .server
            .request("turn/start", params)
            .await
            .inspect_err(|_| discard_pending_prompt(&self.state, &client_message_id))?;
        let provider_turn_id = ProviderTurnId(result.turn.id.clone());
        let turn_id = {
            let mut state = lock(&self.state);
            let turn_id = tascarrel_turn_id(&mut state, &result.turn.id);
            state.provider_active_turn_id = Some(provider_turn_id.clone());
            state.info.active_turn_id = Some(turn_id.clone());
            state.info.state = SessionState::Running;
            state.active_turn_usage = None;
            if model.is_some() {
                state.info.model = model;
            }
            turn_id
        };
        Ok(HarnessCommandResult::PromptAccepted {
            turn_id,
            provider_turn_id: Some(provider_turn_id),
        })
    }

    async fn interrupt(&self) -> Result<HarnessCommandResult, HarnessError> {
        let (thread_id, turn_id) = {
            let state = lock(&self.state);
            ensure_running(&state)?;
            let turn_id = state.provider_active_turn_id.clone().ok_or_else(|| {
                harness_error(
                    HarnessErrorKind::TurnNotFound,
                    "the Codex session has no active turn",
                )
            })?;
            (state.provider_session_id.0.clone(), turn_id)
        };
        let _: IgnoredAny = self
            .server
            .request(
                "turn/interrupt",
                TurnReferenceParams {
                    thread_id,
                    turn_id: turn_id.0.clone(),
                },
            )
            .await?;
        let mut state = lock(&self.state);
        if state.provider_active_turn_id.as_ref() == Some(&turn_id) {
            clear_active_turn(&mut state, SessionState::Ready);
        }
        Ok(HarnessCommandResult::Accepted)
    }

    async fn compact(&self) -> Result<HarnessCommandResult, HarnessError> {
        let thread_id = {
            let state = lock(&self.state);
            ensure_running(&state)?;
            state.provider_session_id.0.clone()
        };
        let _: IgnoredAny = self
            .server
            .request("thread/compact/start", ThreadReferenceParams { thread_id })
            .await?;
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
                "the Codex user-input request is not pending",
            )
        })?;
        let mut native_answers = HashMap::new();
        for answer in answers {
            let native_question_id =
                pending.questions.get(&answer.question_id).ok_or_else(|| {
                    harness_error(
                        HarnessErrorKind::RequestFailed,
                        "an answer refers to an unknown Codex question",
                    )
                })?;
            native_answers.insert(
                native_question_id.clone(),
                NativeQuestionAnswer {
                    answers: answer.answers.iter().map(ToString::to_string).collect(),
                },
            );
        }
        self.server
            .respond(
                pending.rpc_id.clone(),
                UserInputResponse {
                    answers: native_answers,
                },
            )
            .await?;
        lock(&self.state).pending_requests.remove(&request_id);
        Ok(HarnessCommandResult::Accepted)
    }

    async fn stop(&self) -> Result<HarnessCommandResult, HarnessError> {
        {
            let mut state = lock(&self.state);
            state.stopped = true;
            clear_active_turn(&mut state, SessionState::Stopped);
        }
        self.server.stop().await?;
        Ok(HarnessCommandResult::Stopped)
    }
}

struct CodexEvents {
    server: Arc<CodexAppServer>,
    messages: CodexMessages,
    state: Arc<Mutex<CodexSessionState>>,
    pending: VecDeque<HarnessEvent>,
    ended: bool,
}

impl HarnessEventStream for CodexEvents {
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
                    let stopped = lock(&self.state).stopped;
                    let failure = (!stopped).then(|| {
                        harness_error(
                            HarnessErrorKind::ProcessExited,
                            "the Codex app-server message stream ended unexpectedly",
                        )
                    });
                    mark_exited(&self.state, failure.is_some());
                    return Ok(Some(session_exited(&self.state, failure)));
                };
                match message {
                    CodexMessage::Notification { method, params } => {
                        if let Some(event) = normalize_notification(&self.state, &method, params)? {
                            return Ok(Some(event));
                        }
                    }
                    CodexMessage::Request { id, method, params } => {
                        if method == "item/tool/requestUserInput" {
                            match normalize_user_input_request(&self.state, id.clone(), &params) {
                                Ok(event) => return Ok(Some(event)),
                                Err(error) => {
                                    self.server
                                        .respond_error(id, -32602, error.message.clone())
                                        .await?;
                                    return Err(error);
                                }
                            }
                        }
                        self.server
                            .respond_error(
                                id,
                                -32601,
                                format!("unsupported Codex server request: {method}"),
                            )
                            .await?;
                        return Ok(Some(unknown_event(&self.state, method, &params)));
                    }
                    CodexMessage::Exited(exit_error) => {
                        self.ended = true;
                        let stopped = lock(&self.state).stopped;
                        let failure = if stopped {
                            None
                        } else {
                            Some(exit_error.unwrap_or_else(|| {
                                harness_error(
                                    HarnessErrorKind::ProcessExited,
                                    "the Codex app-server exited unexpectedly",
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
    rpc_id: RpcId,
    questions: HashMap<ChatQuestionId, String>,
}

struct PendingPrompt {
    text: Option<ArcStr>,
    attachments: ArcVec<HarnessPromptAttachment>,
}

struct CodexSessionState {
    info: HarnessSessionInfo,
    provider_session_id: ProviderSessionId,
    provider_active_turn_id: Option<ProviderTurnId>,
    active_turn_usage: Option<ChatTokenUsage>,
    turns: HashMap<String, ChatTurnId>,
    items: HashMap<String, ChatItemId>,
    pending_requests: HashMap<ChatRequestId, PendingUserInput>,
    provider_requests: HashMap<RpcId, ChatRequestId>,
    pending_prompts: HashMap<ArcStr, PendingPrompt>,
    stopped: bool,
}

struct OpenedThread {
    provider_session_id: ProviderSessionId,
    model: Option<ChatModelSelection>,
}

async fn initialize(server: &CodexAppServer) -> Result<(), HarnessError> {
    let _: IgnoredAny = server
        .request(
            "initialize",
            InitializeParams {
                client_info: ClientInfo {
                    name: "tascarrel_chat_engine",
                    title: "Tascarrel Chat Engine",
                    version: env!("CARGO_PKG_VERSION"),
                },
                capabilities: ClientCapabilities {
                    experimental_api: true,
                },
            },
        )
        .await?;
    server.notify("initialized").await
}

async fn open_thread(
    server: &CodexAppServer,
    request: &StartSessionRequest,
) -> Result<OpenedThread, HarnessError> {
    let model = request.model.as_ref().map(|model| model.model.to_string());
    let service_tier = request
        .model
        .as_ref()
        .and_then(|model| selected_string_option(model, "serviceTier"))
        .map(str::to_owned);
    let resume_thread_id = request
        .resume_cursor
        .as_ref()
        .map(parse_resume_cursor)
        .transpose()?
        .map(|thread_id| thread_id.0);
    let result: OpenThreadResult = if let Some(thread_id) = resume_thread_id {
        match server
            .request(
                "thread/resume",
                OpenThreadParams {
                    approval_policy: "never",
                    sandbox: "danger-full-access",
                    model: model.clone(),
                    service_tier: service_tier.clone(),
                    thread_id: Some(thread_id.clone()),
                },
            )
            .await
        {
            Ok(result) => result,
            Err(error) if is_recoverable_thread_resume_error(&error) => {
                tracing::warn!(
                    thread_id,
                    message = %error.message,
                    "Codex thread resume failed; starting a fresh thread"
                );
                server
                    .request(
                        "thread/start",
                        OpenThreadParams {
                            approval_policy: "never",
                            sandbox: "danger-full-access",
                            model,
                            service_tier,
                            thread_id: None,
                        },
                    )
                    .await?
            }
            Err(error) => return Err(error),
        }
    } else {
        server
            .request(
                "thread/start",
                OpenThreadParams {
                    approval_policy: "never",
                    sandbox: "danger-full-access",
                    model,
                    service_tier,
                    thread_id: None,
                },
            )
            .await?
    };
    let provider_session_id = ProviderSessionId(result.thread.id);
    let selected_model = result.model.map(|model| ChatModelSelection {
        options: request
            .model
            .as_ref()
            .filter(|selection| selection.model.as_ref() == model)
            .map_or_else(ArcVec::new, |selection| selection.options.clone()),
        model: model.into(),
    });
    Ok(OpenedThread {
        provider_session_id,
        model: selected_model,
    })
}

async fn list_models(server: &CodexAppServer) -> Result<ArcVec<ChatModel>, HarnessError> {
    let mut models = Vec::new();
    let mut cursor = None;
    loop {
        let result: ModelListResult = server
            .request(
                "model/list",
                ModelListParams {
                    cursor: cursor.clone(),
                },
            )
            .await?;
        models.extend(result.data.into_iter().filter_map(normalize_model));
        cursor = result.next_cursor;
        if cursor.is_none() {
            return Ok(models.into());
        }
    }
}

fn normalize_notification(
    state: &Arc<Mutex<CodexSessionState>>,
    method: &str,
    params: RawJson,
) -> Result<Option<HarnessEvent>, HarnessError> {
    if !belongs_to_session(state, &params)? {
        return Ok(None);
    }
    match method {
        "thread/started" => {
            let params: ThreadStartedParams = decode(&params, method)?;
            let provider_session_id = ProviderSessionId(params.thread.id);
            if provider_session_id != lock(state).provider_session_id {
                return Ok(None);
            }
            Ok(Some(event(
                references(Some(provider_session_id.clone()), None, None, None),
                None,
                None,
                None,
                HarnessEventPayload::ResumeCursorUpdated {
                    resume_cursor: resume_cursor(&provider_session_id),
                },
            )))
        }
        "turn/started" => turn_started(state, decode(&params, method)?).map(Some),
        "turn/completed" => turn_completed(state, decode(&params, method)?).map(Some),
        "thread/tokenUsage/updated" => token_usage_updated(state, decode(&params, method)?),
        "item/started" => item_started(state, decode(&params, method)?).map(Some),
        "item/completed" => item_completed(state, decode(&params, method)?).map(Some),
        "item/agentMessage/delta"
        | "item/reasoning/textDelta"
        | "item/reasoning/summaryTextDelta"
        | "item/plan/delta"
        | "item/commandExecution/outputDelta"
        | "item/fileChange/outputDelta" => content_delta(state, decode(&params, method)?).map(Some),
        "serverRequest/resolved" => request_resolved(state, decode(&params, method)?).map(Some),
        "warning" | "configWarning" => {
            let warning: WarningParams = decode(&params, method)?;
            let message = warning
                .message
                .or(warning.summary)
                .unwrap_or_else(|| "Codex emitted a warning".to_owned());
            // Tascarrel supplies the pod isolation boundary and explicitly asks
            // Codex for `danger-full-access`. Its bundled-bubblewrap fallback
            // is therefore neither required nor actionable for this session.
            if message.starts_with(MISSING_BUBBLEWRAP_WARNING)
                && message.contains(BUNDLED_BUBBLEWRAP_FALLBACK)
            {
                return Ok(None);
            }
            Ok(Some(base_event(
                state,
                HarnessEventPayload::Warning {
                    code: method.to_owned(),
                    message,
                },
            )))
        }
        "error" => {
            let error_params: ErrorNotificationParams = decode(&params, method)?;
            let message = error_params
                .error
                .map(|error| error.message)
                .or(error_params.message)
                .unwrap_or_else(|| "Codex emitted an error".to_owned());
            if error_params.will_retry == Some(false) {
                lock(state).info.state = SessionState::Failed;
            }
            Ok(Some(base_event(
                state,
                HarnessEventPayload::Error(harness_error(HarnessErrorKind::RequestFailed, message)),
            )))
        }
        "model/rerouted" => {
            let params: ModelReroutedParams = decode(&params, method)?;
            let model = params.to_model.or(params.model).ok_or_else(|| {
                harness_error(HarnessErrorKind::Protocol, "model change has no model")
            })?;
            let selection = ChatModelSelection {
                model: model.into(),
                options: ArcVec::new(),
            };
            lock(state).info.model = Some(selection.clone());
            Ok(Some(base_event(
                state,
                HarnessEventPayload::ModelChanged { model: selection },
            )))
        }
        _ => Ok(Some(unknown_event(state, method.to_owned(), &params))),
    }
}

fn belongs_to_session(
    state: &Arc<Mutex<CodexSessionState>>,
    params: &RawJson,
) -> Result<bool, HarnessError> {
    let route: MessageRoute = decode(params, "message route")?;
    let thread_id = route
        .thread_id
        .or_else(|| route.thread.map(|thread| thread.id));
    Ok(thread_id.is_none_or(|thread_id| lock(state).provider_session_id.0 == thread_id))
}

fn normalize_user_input_request(
    state: &Arc<Mutex<CodexSessionState>>,
    rpc_id: RpcId,
    params: &RawJson,
) -> Result<HarnessEvent, HarnessError> {
    let params: UserInputRequestParams = decode(params, "item/tool/requestUserInput")?;
    if params.questions.is_empty() {
        return Err(harness_error(
            HarnessErrorKind::Protocol,
            "user-input request has no questions",
        ));
    }
    let provider_request_id = ProviderRequestId(rpc_id.to_string());
    let request_id = ChatRequestId::generate();
    let mut question_ids = HashMap::new();
    let mut questions = Vec::with_capacity(params.questions.len());
    for question in params.questions {
        let question_id = ChatQuestionId::generate();
        question_ids.insert(question_id.clone(), question.id);
        questions.push(ChatQuestion {
            question_id,
            header: question.header.unwrap_or_default().into(),
            prompt: question.question.into(),
            options: question
                .options
                .into_iter()
                .map(|option| ChatQuestionOption {
                    label: option.label.into(),
                    description: option.description.map(Into::into),
                })
                .collect::<Vec<_>>()
                .into(),
            multiple: false,
        });
    }
    let mut session = lock(state);
    let turn_id = tascarrel_turn_id(&mut session, &params.turn_id);
    let item_id = tascarrel_item_id(&mut session, &params.item_id);
    session.pending_requests.insert(
        request_id.clone(),
        PendingUserInput {
            rpc_id: rpc_id.clone(),
            questions: question_ids,
        },
    );
    session.provider_requests.insert(rpc_id, request_id.clone());
    session.info.state = SessionState::WaitingForInput;
    let provider_session_id = session.provider_session_id.clone();
    drop(session);
    Ok(event(
        references(
            Some(provider_session_id),
            Some(ProviderTurnId(params.turn_id)),
            Some(ProviderItemId(params.item_id)),
            Some(provider_request_id),
        ),
        Some(turn_id),
        Some(item_id),
        Some(request_id),
        HarnessEventPayload::UserInputRequested {
            questions: questions.into(),
        },
    ))
}

fn turn_started(
    state: &Arc<Mutex<CodexSessionState>>,
    params: TurnLifecycleParams,
) -> Result<HarnessEvent, HarnessError> {
    let provider_turn_id = params.turn.id;
    let mut session = lock(state);
    let turn_id = tascarrel_turn_id(&mut session, &provider_turn_id);
    if session
        .provider_active_turn_id
        .as_ref()
        .is_none_or(|active| active.0 != provider_turn_id)
    {
        session.active_turn_usage = None;
    }
    session.provider_active_turn_id = Some(ProviderTurnId(provider_turn_id.clone()));
    session.info.active_turn_id = Some(turn_id.clone());
    session.info.state = SessionState::Running;
    let provider_session_id = session.provider_session_id.clone();
    drop(session);
    Ok(event(
        references(
            Some(provider_session_id),
            Some(ProviderTurnId(provider_turn_id)),
            None,
            None,
        ),
        Some(turn_id),
        None,
        None,
        HarnessEventPayload::TurnStarted,
    ))
}

fn turn_completed(
    state: &Arc<Mutex<CodexSessionState>>,
    params: TurnLifecycleParams,
) -> Result<HarnessEvent, HarnessError> {
    let provider_turn_id = params.turn.id;
    let turn_state = match params.turn.status.as_deref() {
        Some("completed") => ChatTurnState::Completed,
        Some("interrupted") => ChatTurnState::Interrupted,
        Some("failed") => ChatTurnState::Failed,
        _ => ChatTurnState::Running,
    };
    let failure = params.turn.error.map(|error| ChatFailure {
        code: "request_failed".into(),
        message: error.message.into(),
    });
    let mut session = lock(state);
    let turn_id = tascarrel_turn_id(&mut session, &provider_turn_id);
    if session
        .provider_active_turn_id
        .as_ref()
        .is_some_and(|active| active.0 == provider_turn_id)
    {
        clear_active_turn(
            &mut session,
            if turn_state == ChatTurnState::Failed {
                SessionState::Failed
            } else {
                SessionState::Ready
            },
        );
    }
    let provider_session_id = session.provider_session_id.clone();
    drop(session);
    Ok(event(
        references(
            Some(provider_session_id),
            Some(ProviderTurnId(provider_turn_id)),
            None,
            None,
        ),
        Some(turn_id),
        None,
        None,
        HarnessEventPayload::TurnCompleted {
            state: turn_state,
            error: failure,
        },
    ))
}

fn token_usage_updated(
    state: &Arc<Mutex<CodexSessionState>>,
    params: TokenUsageParams,
) -> Result<Option<HarnessEvent>, HarnessError> {
    let latest = params.token_usage.last;
    let latest = ChatTokenUsage {
        input_tokens: latest.input_tokens,
        output_tokens: latest.output_tokens,
        cache_read_input_tokens: Some(latest.cached_input_tokens),
        cache_write_input_tokens: None,
        cache_writes_by_ttl: ArcVec::new(),
        reasoning_output_tokens: Some(latest.reasoning_output_tokens),
    };
    let mut session = lock(state);
    if session
        .provider_active_turn_id
        .as_ref()
        .is_none_or(|active| active.0 != params.turn_id)
    {
        return Ok(None);
    }
    let turn_id = tascarrel_turn_id(&mut session, &params.turn_id);
    let tokens = session.active_turn_usage.get_or_insert_with(empty_usage);
    accumulate_token_usage(tokens, &latest);
    let tokens = tokens.clone();
    let models = session
        .info
        .model
        .clone()
        .map(|model| ChatModelUsage {
            model,
            tokens: tokens.clone(),
            pricing: None,
            provider_estimated_cost: None,
        })
        .into_iter()
        .collect::<Vec<_>>()
        .into();
    let provider_session_id = session.provider_session_id.clone();
    drop(session);
    Ok(Some(event(
        references(
            Some(provider_session_id),
            Some(ProviderTurnId(params.turn_id)),
            None,
            None,
        ),
        Some(turn_id),
        None,
        None,
        HarnessEventPayload::TurnUsageUpdated {
            usage: ChatUsageSnapshot {
                coverage: ChatUsageCoverage::ExecutionTree,
                tokens,
                models,
                provider_estimated_cost: None,
            },
            state: ChatUsageState::Provisional,
        },
    )))
}

fn item_started(
    state: &Arc<Mutex<CodexSessionState>>,
    params: ItemLifecycleParams,
) -> Result<HarnessEvent, HarnessError> {
    let item: CodexItem = decode(&params.item, "started item")?;
    let mut session = lock(state);
    let turn_id = tascarrel_turn_id(&mut session, &params.turn_id);
    let item_id = tascarrel_item_id(&mut session, &item.id);
    let provider_session_id = session.provider_session_id.clone();
    drop(session);
    Ok(event(
        references(
            Some(provider_session_id),
            Some(ProviderTurnId(params.turn_id)),
            Some(ProviderItemId(item.id.clone())),
            None,
        ),
        Some(turn_id),
        Some(item_id),
        None,
        HarnessEventPayload::ItemStarted {
            kind: item_kind(&item.kind),
        },
    ))
}

fn item_completed(
    state: &Arc<Mutex<CodexSessionState>>,
    params: ItemLifecycleParams,
) -> Result<HarnessEvent, HarnessError> {
    let item: CodexItem = decode(&params.item, "completed item")?;
    let provider_item_id = item.id.clone();
    let mut session = lock(state);
    let turn_id = tascarrel_turn_id(&mut session, &params.turn_id);
    let item_id = tascarrel_item_id(&mut session, &provider_item_id);
    let pending_prompt = item
        .client_id
        .as_deref()
        .and_then(|client_id| session.pending_prompts.remove(client_id));
    let provider_session_id = session.provider_session_id.clone();
    drop(session);
    let kind = item_kind(&item.kind);
    let state = item_state(item.status.as_deref());
    let content = item_content(item, pending_prompt, &params.item)?;
    Ok(event(
        references(
            Some(provider_session_id),
            Some(ProviderTurnId(params.turn_id)),
            Some(ProviderItemId(provider_item_id)),
            None,
        ),
        Some(turn_id),
        Some(item_id),
        None,
        HarnessEventPayload::ItemCompleted {
            kind,
            state,
            content,
        },
    ))
}

fn content_delta(
    state: &Arc<Mutex<CodexSessionState>>,
    params: ContentDeltaParams,
) -> Result<HarnessEvent, HarnessError> {
    let mut session = lock(state);
    let turn_id = tascarrel_turn_id(&mut session, &params.turn_id);
    let item_id = tascarrel_item_id(&mut session, &params.item_id);
    let provider_session_id = session.provider_session_id.clone();
    drop(session);
    Ok(event(
        references(
            Some(provider_session_id),
            Some(ProviderTurnId(params.turn_id)),
            Some(ProviderItemId(params.item_id)),
            None,
        ),
        Some(turn_id),
        Some(item_id.clone()),
        None,
        HarnessEventPayload::ChatItemContentAppended(ChatItemContentAppended {
            item_id,
            delta: params.delta.into(),
        }),
    ))
}

fn request_resolved(
    state: &Arc<Mutex<CodexSessionState>>,
    params: RequestResolvedParams,
) -> Result<HarnessEvent, HarnessError> {
    let mut session = lock(state);
    let Some(request_id) = session.provider_requests.remove(&params.request_id) else {
        drop(session);
        return Ok(base_event(
            state,
            HarnessEventPayload::Warning {
                code: "unknown_resolved_request".to_owned(),
                message: "Codex resolved an unknown user-input request".to_owned(),
            },
        ));
    };
    session.pending_requests.remove(&request_id);
    session.info.state = SessionState::Running;
    let turn_id = session.info.active_turn_id.clone();
    let provider_turn_id = session.provider_active_turn_id.clone();
    let provider_session_id = session.provider_session_id.clone();
    drop(session);
    Ok(event(
        references(
            Some(provider_session_id),
            provider_turn_id,
            None,
            Some(ProviderRequestId(params.request_id.to_string())),
        ),
        turn_id,
        None,
        Some(request_id),
        HarnessEventPayload::RequestResolved,
    ))
}

fn item_content(
    item: CodexItem,
    pending_prompt: Option<PendingPrompt>,
    raw_item: &RawJson,
) -> Result<ArcVec<ChatContent>, HarnessError> {
    let content = match item.kind.as_str() {
        "userMessage" => pending_prompt.map_or_else(
            || provider_user_message_content(item.content),
            canonical_user_message_content,
        ),
        "agentMessage" | "plan" => item
            .text
            .map(|value| {
                ChatContent::Text(TextContent {
                    value: value.into(),
                })
            })
            .into_iter()
            .collect::<Vec<_>>()
            .into(),
        "reasoning" => item
            .summary
            .into_iter()
            .chain(
                item.content
                    .into_iter()
                    .filter_map(|content| match content {
                        CodexContent::Text { text } => Some(text),
                        CodexContent::Mention { .. }
                        | CodexContent::LocalImage { .. }
                        | CodexContent::Image { .. }
                        | CodexContent::Other => None,
                    }),
            )
            .map(|value| {
                ChatContent::Text(TextContent {
                    value: value.into(),
                })
            })
            .collect::<Vec<_>>()
            .into(),
        "contextCompaction" => ArcVec::new(),
        _ => {
            let value = serde_json::to_value(raw_item.as_ref()).map_err(|error| {
                harness_error(
                    HarnessErrorKind::Internal,
                    format!("failed to preserve a structured Codex item: {error}"),
                )
            })?;
            vec![ChatContent::Structured(StructuredContent { value })].into()
        }
    };
    Ok(content)
}

fn canonical_user_message_content(prompt: PendingPrompt) -> ArcVec<ChatContent> {
    let mut content = Vec::new();
    if let Some(text) = prompt.text {
        content.push(ChatContent::Text(TextContent { value: text }));
    }
    content.extend(prompt.attachments.iter().map(attachment_content));
    content.into()
}

fn provider_user_message_content(content: Vec<CodexContent>) -> ArcVec<ChatContent> {
    content
        .into_iter()
        .filter_map(|content| match content {
            CodexContent::Text { text } => {
                Some(ChatContent::Text(TextContent { value: text.into() }))
            }
            CodexContent::Mention { name, .. } => Some(ChatContent::Text(TextContent {
                value: format!("Attached file: {name}").into(),
            })),
            CodexContent::LocalImage { .. } | CodexContent::Image { .. } => {
                Some(ChatContent::Text(TextContent {
                    value: "Attached image".into(),
                }))
            }
            CodexContent::Other => None,
        })
        .collect::<Vec<_>>()
        .into()
}

fn attachment_content(attachment: &HarnessPromptAttachment) -> ChatContent {
    ChatContent::Attachment(ChatPromptAttachment {
        attachment_id: attachment.attachment_id.clone(),
        name: attachment.name.clone(),
        media_type: attachment.media_type.clone(),
        size: attachment.size,
        digest: attachment.digest.clone(),
    })
}

fn codex_input(
    text: Option<ArcStr>,
    attachments: &[HarnessPromptAttachment],
) -> Result<Vec<CodexInput>, HarnessError> {
    let files = attachments
        .iter()
        .filter(|attachment| !attachment.media_type.starts_with("image/"))
        .map(|attachment| AttachmentManifestEntry {
            name: attachment.name.as_ref(),
            media_type: attachment.media_type.as_ref(),
            size: attachment.size,
            path: attachment.path.as_ref(),
        })
        .collect::<Vec<_>>();
    let mut input = Vec::new();
    if let Some(text) = text {
        input.push(CodexInput::Text { text });
    }
    if !files.is_empty() {
        let manifest = serde_json::to_string_pretty(&files).map_err(|error| {
            harness_error(
                HarnessErrorKind::Internal,
                format!("failed to describe Codex prompt attachments: {error}"),
            )
        })?;
        input.push(CodexInput::Text {
            text: format!(
                "# Files attached by the user\n\nThe following attachment paths are readable in this environment. Inspect the relevant files when answering the user's prompt.\n\n{manifest}"
            )
            .into(),
        });
    }
    for attachment in attachments {
        if attachment.media_type.starts_with("image/") {
            input.push(CodexInput::LocalImage {
                path: attachment.path.clone(),
            });
        } else {
            input.push(CodexInput::Mention {
                name: attachment.name.clone(),
                path: attachment.path.clone(),
            });
        }
    }
    if input.is_empty() {
        return Err(harness_error(
            HarnessErrorKind::RequestFailed,
            "a Codex prompt must contain text or an attachment",
        ));
    }
    Ok(input)
}

fn normalize_model(model: NativeModel) -> Option<ChatModel> {
    if model.hidden {
        return None;
    }
    let mut options = Vec::new();
    if !model.supported_reasoning_efforts.is_empty() {
        options.push(ChatModelOptionDescriptor::Select(
            ChatModelSelectOptionDescriptor {
                id: "reasoningEffort".into(),
                label: "Reasoning".into(),
                description: None,
                choices: model
                    .supported_reasoning_efforts
                    .into_iter()
                    .map(|effort| ChatModelOptionChoice {
                        is_default: model.default_reasoning_effort.as_ref()
                            == Some(&effort.reasoning_effort),
                        label: reasoning_label(&effort.reasoning_effort).into(),
                        description: effort.description.map(Into::into),
                        id: effort.reasoning_effort.into(),
                    })
                    .collect::<Vec<_>>()
                    .into(),
            },
        ));
    }
    if !model.service_tiers.is_empty() {
        let default = model
            .default_service_tier
            .unwrap_or_else(|| "default".to_owned());
        let mut choices = vec![ChatModelOptionChoice {
            id: "default".into(),
            label: "Standard".into(),
            description: None,
            is_default: default == "default",
        }];
        choices.extend(
            model
                .service_tiers
                .into_iter()
                .map(|tier| ChatModelOptionChoice {
                    is_default: default == tier.id,
                    label: tier.name.unwrap_or_else(|| tier.id.clone()).into(),
                    description: tier.description.map(Into::into),
                    id: tier.id.into(),
                }),
        );
        options.push(ChatModelOptionDescriptor::Select(
            ChatModelSelectOptionDescriptor {
                id: "serviceTier".into(),
                label: "Service Tier".into(),
                description: None,
                choices: choices.into(),
            },
        ));
    }
    Some(ChatModel {
        display_name: model
            .display_name
            .unwrap_or_else(|| model.model.clone())
            .into(),
        id: model.model.into(),
        short_name: None,
        is_custom: false,
        options: options.into(),
        pricing: None,
    })
}

fn item_kind(kind: &str) -> ChatItemKind {
    match kind {
        "userMessage" => ChatItemKind::UserMessage,
        "agentMessage" => ChatItemKind::AssistantMessage,
        "reasoning" => ChatItemKind::Reasoning,
        "plan" => ChatItemKind::Plan,
        "commandExecution" => ChatItemKind::CommandExecution,
        "fileChange" => ChatItemKind::FileChange,
        "mcpToolCall" | "dynamicToolCall" => ChatItemKind::ToolCall,
        "webSearch" => ChatItemKind::WebSearch,
        "collabAgentToolCall" => ChatItemKind::Subagent,
        "contextCompaction" => ChatItemKind::ContextCompaction,
        "error" => ChatItemKind::Error,
        _ => ChatItemKind::Unknown,
    }
}

fn item_state(status: Option<&str>) -> ChatItemState {
    match status {
        Some("failed" | "declined") => ChatItemState::Failed,
        Some("inProgress") => ChatItemState::Started,
        _ => ChatItemState::Completed,
    }
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

fn is_recoverable_thread_resume_error(error: &HarnessError) -> bool {
    let message = error.message.to_ascii_lowercase();
    message.contains("thread")
        && [
            "not found",
            "missing thread",
            "no such thread",
            "unknown thread",
            "does not exist",
        ]
        .iter()
        .any(|snippet| message.contains(snippet))
}

/// Reads only the provider identifier needed to resume a Codex thread.
///
/// Resume cursors are intentionally opaque. The adaptor must not interpret
/// or validate fields that it does not need to launch the harness.
fn parse_resume_cursor(cursor: &ResumeCursor) -> Result<ProviderSessionId, HarnessError> {
    cursor
        .0
        .pointer("/threadId")
        .and_then(JsonValue::as_str)
        .filter(|thread_id| !thread_id.is_empty())
        .map(|thread_id| ProviderSessionId(thread_id.to_owned()))
        .ok_or_else(|| {
            harness_error(
                HarnessErrorKind::InvalidResumeCursor,
                "a Codex resume cursor must contain a non-empty string threadId",
            )
        })
}

fn resume_cursor(provider_session_id: &ProviderSessionId) -> ResumeCursor {
    ResumeCursor(JsonValue::Object(serde_json::Map::from_iter([(
        "threadId".to_owned(),
        JsonValue::String(provider_session_id.0.clone()),
    )])))
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

fn references(
    provider_session_id: Option<ProviderSessionId>,
    provider_turn_id: Option<ProviderTurnId>,
    provider_item_id: Option<ProviderItemId>,
    provider_request_id: Option<ProviderRequestId>,
) -> ProviderEventReferences {
    ProviderEventReferences {
        provider_session_id,
        provider_turn_id,
        provider_item_id,
        provider_request_id,
    }
}

fn base_event(state: &Arc<Mutex<CodexSessionState>>, payload: HarnessEventPayload) -> HarnessEvent {
    let state = lock(state);
    event(
        references(
            Some(state.provider_session_id.clone()),
            state.provider_active_turn_id.clone(),
            None,
            None,
        ),
        state.info.active_turn_id.clone(),
        None,
        None,
        payload,
    )
}

fn unknown_event(
    state: &Arc<Mutex<CodexSessionState>>,
    method: String,
    params: &RawJson,
) -> HarnessEvent {
    base_event(
        state,
        HarnessEventPayload::Unknown {
            native_type: method,
            payload: format!("{} byte JSON payload", params.get().len()),
        },
    )
}

fn session_exited(
    state: &Arc<Mutex<CodexSessionState>>,
    failure: Option<HarnessError>,
) -> HarnessEvent {
    event(
        references(
            Some(lock(state).provider_session_id.clone()),
            None,
            None,
            None,
        ),
        None,
        None,
        None,
        HarnessEventPayload::SessionExited { error: failure },
    )
}

fn tascarrel_turn_id(state: &mut CodexSessionState, provider_id: &str) -> ChatTurnId {
    state
        .turns
        .entry(provider_id.to_owned())
        .or_insert_with(ChatTurnId::generate)
        .clone()
}

fn tascarrel_item_id(state: &mut CodexSessionState, provider_id: &str) -> ChatItemId {
    state
        .items
        .entry(provider_id.to_owned())
        .or_insert_with(ChatItemId::generate)
        .clone()
}

fn clear_active_turn(state: &mut CodexSessionState, session_state: SessionState) {
    state.provider_active_turn_id = None;
    state.active_turn_usage = None;
    state.info.active_turn_id = None;
    state.info.state = session_state;
}

fn mark_exited(state: &Arc<Mutex<CodexSessionState>>, failed: bool) {
    let mut state = lock(state);
    state.stopped = true;
    clear_active_turn(
        &mut state,
        if failed {
            SessionState::Failed
        } else {
            SessionState::Stopped
        },
    );
}

fn ensure_running(state: &CodexSessionState) -> Result<(), HarnessError> {
    if state.stopped {
        Err(harness_error(
            HarnessErrorKind::SessionNotFound,
            "the Codex session has stopped",
        ))
    } else {
        Ok(())
    }
}

fn register_pending_prompt(state: &Arc<Mutex<CodexSessionState>>, prompt: PendingPrompt) -> ArcStr {
    let id = ChatItemId::generate().0;
    lock(state).pending_prompts.insert(id.clone(), prompt);
    id
}

fn discard_pending_prompt(state: &Arc<Mutex<CodexSessionState>>, id: &str) {
    lock(state).pending_prompts.remove(id);
}

fn empty_usage() -> ChatTokenUsage {
    ChatTokenUsage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: Some(0),
        cache_write_input_tokens: None,
        cache_writes_by_ttl: ArcVec::new(),
        reasoning_output_tokens: Some(0),
    }
}

fn accumulate_token_usage(total: &mut ChatTokenUsage, latest: &ChatTokenUsage) {
    total.input_tokens = total.input_tokens.saturating_add(latest.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(latest.output_tokens);
    total.cache_read_input_tokens = add_optional(
        total.cache_read_input_tokens,
        latest.cache_read_input_tokens,
    );
    total.reasoning_output_tokens = add_optional(
        total.reasoning_output_tokens,
        latest.reasoning_output_tokens,
    );
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn reasoning_label(effort: &str) -> String {
    match effort {
        "none" => "None",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        "ultra" => "Ultra",
        _ => effort,
    }
    .to_owned()
}

type RawJson = Box<RawValue>;
type PendingResponses = Arc<Mutex<HashMap<RpcId, oneshot::Sender<Result<RawJson, HarnessError>>>>>;

struct StartedCodexAppServer {
    control: Arc<CodexAppServer>,
    messages: CodexMessages,
}

struct CodexMessages {
    receiver: mpsc::UnboundedReceiver<CodexMessage>,
}

struct CodexAppServer {
    process: Arc<dyn HarnessProcessControl>,
    pending: PendingResponses,
    messages: mpsc::UnboundedSender<CodexMessage>,
    next_request_id: AtomicU64,
}

impl CodexAppServer {
    async fn request<P, R>(&self, method: &'static str, params: P) -> Result<R, HarnessError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = RpcId::Number(self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = oneshot::channel();
        lock_pending(&self.pending).insert(id.clone(), sender);
        let message = RpcRequest {
            id: id.clone(),
            method,
            params,
        };
        if let Err(error) = self.write_message(&message).await {
            lock_pending(&self.pending).remove(&id);
            return Err(error);
        }
        let response = receiver.await.map_err(|_| {
            harness_error(
                HarnessErrorKind::ProcessExited,
                "the Codex app-server exited before replying",
            )
        })??;
        serde_json::from_str(response.get()).map_err(|error| {
            harness_error(
                HarnessErrorKind::Protocol,
                format!("failed to decode the Codex response to {method}: {error}"),
            )
        })
    }

    async fn notify(&self, method: &'static str) -> Result<(), HarnessError> {
        self.write_message(&RpcNotification { method }).await
    }

    async fn respond<R>(&self, id: RpcId, result: R) -> Result<(), HarnessError>
    where
        R: Serialize,
    {
        self.write_message(&RpcResponse { id, result }).await
    }

    async fn respond_error(
        &self,
        id: RpcId,
        code: i64,
        message: String,
    ) -> Result<(), HarnessError> {
        self.write_message(&RpcErrorResponse {
            id,
            error: RpcError { code, message },
        })
        .await
    }

    async fn write_message<T: Serialize>(&self, message: &T) -> Result<(), HarnessError> {
        let mut bytes = serde_json::to_vec(message).map_err(|error| {
            harness_error(
                HarnessErrorKind::Internal,
                format!("failed to encode a Codex app-server message: {error}"),
            )
        })?;
        bytes.push(b'\n');
        self.process.write(bytes).await
    }

    async fn stop(&self) -> Result<(), HarnessError> {
        self.process.stop().await?;
        if self.messages.send(CodexMessage::Exited(None)).is_err() {
            tracing::debug!("Codex event receiver was already closed while stopping");
        }
        Ok(())
    }
}

async fn start_server(
    executable: PathBuf,
    launcher: Arc<dyn HarnessProcessLauncher>,
    process_environment: Option<&dyn ProcessEnvironment>,
    working_directory: PathBuf,
) -> Result<StartedCodexAppServer, HarnessError> {
    let environment = process_environment
        .map(ProcessEnvironment::variables)
        .transpose()
        .map_err(|error| {
            harness_error(
                HarnessErrorKind::ProcessStart,
                format!("failed to configure the Codex environment: {error}"),
            )
        })?
        .unwrap_or_default();
    let process = launcher
        .launch(HarnessProcessSpec {
            title: "Codex chat harness".to_owned(),
            executable,
            arguments: vec!["app-server".to_owned()],
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
    Ok(StartedCodexAppServer {
        control: Arc::new(CodexAppServer {
            process: process.control,
            pending,
            messages: sender,
            next_request_id: AtomicU64::new(1),
        }),
        messages: CodexMessages { receiver },
    })
}

async fn read_messages(
    stdout: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    pending: PendingResponses,
    sender: mpsc::UnboundedSender<CodexMessage>,
) {
    let mut lines = BufReader::new(stdout).lines();
    let exit_error = loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break None,
            Err(error) => {
                break Some(harness_error(
                    HarnessErrorKind::ProcessExited,
                    format!("Codex app-server communication failed: {error}"),
                ));
            }
        };
        let incoming: IncomingMessage = match serde_json::from_str(&line) {
            Ok(incoming) => incoming,
            Err(error) => {
                break Some(harness_error(
                    HarnessErrorKind::Protocol,
                    format!("Codex emitted an invalid message: {error}"),
                ));
            }
        };
        if let Some(method) = incoming.method {
            let Some(params) = incoming.params else {
                break Some(harness_error(
                    HarnessErrorKind::Protocol,
                    "Codex emitted a method without parameters",
                ));
            };
            let message = match incoming.id {
                Some(id) => CodexMessage::Request { id, method, params },
                None => CodexMessage::Notification { method, params },
            };
            if sender.send(message).is_err() {
                tracing::debug!("Codex event receiver closed before event delivery");
                return;
            }
            continue;
        }
        let Some(id) = incoming.id else {
            break Some(harness_error(
                HarnessErrorKind::Protocol,
                "Codex emitted a message with neither a method nor an id",
            ));
        };
        let Some(response) = lock_pending(&pending).remove(&id) else {
            continue;
        };
        let result = match (incoming.result, incoming.error) {
            (Some(result), None) => Ok(result),
            (_, Some(error)) => Err(harness_error(
                HarnessErrorKind::RequestFailed,
                error.message,
            )),
            _ => Err(harness_error(
                HarnessErrorKind::Protocol,
                "Codex response has neither a result nor an error",
            )),
        };
        if response.send(result).is_err() {
            tracing::debug!("Codex response receiver closed before delivery");
        }
    };
    let pending_error = exit_error.clone().unwrap_or_else(|| {
        harness_error(
            HarnessErrorKind::ProcessExited,
            "the Codex app-server exited before replying",
        )
    });
    for (_, response) in lock_pending(&pending).drain() {
        if response.send(Err(pending_error.clone())).is_err() {
            tracing::debug!("Codex response receiver closed during process shutdown");
        }
    }
    if sender.send(CodexMessage::Exited(exit_error)).is_err() {
        tracing::debug!("Codex event receiver closed before the exit event");
    }
}

fn decode<T: DeserializeOwned>(raw: &RawJson, context: &str) -> Result<T, HarnessError> {
    serde_json::from_str(raw.get()).map_err(|error| {
        harness_error(
            HarnessErrorKind::Protocol,
            format!("failed to decode Codex {context}: {error}"),
        )
    })
}

fn harness_error(kind: HarnessErrorKind, message: impl Into<String>) -> HarnessError {
    HarnessError {
        kind,
        message: message.into(),
        retryable: false,
    }
}

fn lock(state: &Arc<Mutex<CodexSessionState>>) -> MutexGuard<'_, CodexSessionState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_pending(
    pending: &PendingResponses,
) -> MutexGuard<'_, HashMap<RpcId, oneshot::Sender<Result<RawJson, HarnessError>>>> {
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

enum CodexMessage {
    Notification {
        method: String,
        params: RawJson,
    },
    Request {
        id: RpcId,
        method: String,
        params: RawJson,
    },
    Exited(Option<HarnessError>),
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
enum RpcId {
    Number(u64),
    String(String),
}

impl fmt::Display for RpcId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => value.fmt(formatter),
            Self::String(value) => value.fmt(formatter),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncomingMessage {
    id: Option<RpcId>,
    method: Option<String>,
    params: Option<RawJson>,
    result: Option<RawJson>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageRoute {
    thread_id: Option<String>,
    thread: Option<IdObject>,
}

#[derive(Deserialize, Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Serialize)]
struct RpcRequest<P> {
    id: RpcId,
    method: &'static str,
    params: P,
}

#[derive(Serialize)]
struct RpcNotification {
    method: &'static str,
}

#[derive(Serialize)]
struct RpcResponse<R> {
    id: RpcId,
    result: R,
}

#[derive(Serialize)]
struct RpcErrorResponse {
    id: RpcId,
    error: RpcError,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    client_info: ClientInfo,
    capabilities: ClientCapabilities,
}

#[derive(Serialize)]
struct ClientInfo {
    name: &'static str,
    title: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientCapabilities {
    experimental_api: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenThreadParams {
    approval_policy: &'static str,
    sandbox: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
}

#[derive(Deserialize)]
struct OpenThreadResult {
    thread: IdObject,
    model: Option<String>,
}

#[derive(Clone, Deserialize)]
struct IdObject {
    id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelListResult {
    data: Vec<NativeModel>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeModel {
    model: String,
    display_name: Option<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    supported_reasoning_efforts: Vec<NativeReasoningEffort>,
    default_reasoning_effort: Option<String>,
    #[serde(default)]
    service_tiers: Vec<NativeServiceTier>,
    default_service_tier: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeReasoningEffort {
    reasoning_effort: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct NativeServiceTier {
    id: String,
    name: Option<String>,
    description: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum CodexInput {
    Text { text: ArcStr },
    LocalImage { path: ArcStr },
    Mention { name: ArcStr, path: ArcStr },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentManifestEntry<'a> {
    name: &'a str,
    media_type: &'a str,
    size: u64,
    path: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnStartParams {
    thread_id: String,
    input: Vec<CodexInput>,
    summary: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    client_user_message_id: ArcStr,
}

#[derive(Deserialize)]
struct TurnStartResult {
    turn: IdObject,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnSteerParams {
    thread_id: String,
    expected_turn_id: String,
    client_user_message_id: ArcStr,
    input: Vec<CodexInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnSteerResult {
    turn_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnReferenceParams {
    thread_id: String,
    turn_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadReferenceParams {
    thread_id: String,
}

#[derive(Serialize)]
struct NativeQuestionAnswer {
    answers: Vec<String>,
}

#[derive(Serialize)]
struct UserInputResponse {
    answers: HashMap<String, NativeQuestionAnswer>,
}

#[derive(Deserialize)]
struct ThreadStartedParams {
    thread: IdObject,
}

#[derive(Deserialize)]
struct TurnLifecycleParams {
    turn: NativeTurn,
}

#[derive(Deserialize)]
struct NativeTurn {
    id: String,
    status: Option<String>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsageParams {
    turn_id: String,
    token_usage: NativeTokenUsage,
}

#[derive(Deserialize)]
struct NativeTokenUsage {
    last: NativeTokenCounts,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeTokenCounts {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemLifecycleParams {
    turn_id: String,
    item: RawJson,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexItem {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    status: Option<String>,
    text: Option<String>,
    #[serde(default)]
    summary: Vec<String>,
    #[serde(default)]
    content: Vec<CodexContent>,
    client_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum CodexContent {
    Text {
        text: String,
    },
    Mention {
        name: String,
        #[serde(rename = "path")]
        _path: IgnoredAny,
    },
    LocalImage {
        #[serde(rename = "path")]
        _path: IgnoredAny,
    },
    Image {
        #[serde(rename = "path")]
        _path: IgnoredAny,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentDeltaParams {
    turn_id: String,
    item_id: String,
    delta: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestResolvedParams {
    request_id: RpcId,
}

#[derive(Deserialize)]
struct WarningParams {
    message: Option<String>,
    summary: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorNotificationParams {
    error: Option<RpcError>,
    message: Option<String>,
    will_retry: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelReroutedParams {
    to_model: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserInputRequestParams {
    turn_id: String,
    item_id: String,
    questions: Vec<NativeQuestion>,
}

#[derive(Deserialize)]
struct NativeQuestion {
    id: String,
    header: Option<String>,
    question: String,
    #[serde(default)]
    options: Vec<NativeQuestionOption>,
}

#[derive(Deserialize)]
struct NativeQuestionOption {
    label: String,
    description: Option<String>,
}
