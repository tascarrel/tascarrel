//! Binding-aware chat API orchestration.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use futures_util::future::BoxFuture;
use jiff::Timestamp;
use reportify::Report;
use tascarrel_api::ArcVec;
use tascarrel_api::ids::ChatBindingId;
use tascarrel_api::ids::ChatId;
use tascarrel_api::ids::ChatQueuedPromptId;
use tascarrel_api::ids::ChatTurnId;
use tascarrel_api::types::chats::AcknowledgeChatAttentionAction;
use tascarrel_api::types::chats::AcknowledgeChatAttentionOutput;
use tascarrel_api::types::chats::ArchiveChatAction;
use tascarrel_api::types::chats::ArchiveChatOutput;
use tascarrel_api::types::chats::AttachChatBindingAction;
use tascarrel_api::types::chats::AttachChatBindingOutput;
use tascarrel_api::types::chats::ChatBinding;
use tascarrel_api::types::chats::ChatBindingError;
use tascarrel_api::types::chats::ChatBindingStatus;
use tascarrel_api::types::chats::ChatHarness;
use tascarrel_api::types::chats::ChatHarnessKind;
use tascarrel_api::types::chats::ChatListChangedSubscription;
use tascarrel_api::types::chats::ChatModel;
use tascarrel_api::types::chats::ChatModelSelection;
use tascarrel_api::types::chats::ChatPrompt;
use tascarrel_api::types::chats::ChatPromptAttachment;
use tascarrel_api::types::chats::ChatPromptDelivery;
use tascarrel_api::types::chats::ChatPromptMode;
use tascarrel_api::types::chats::ChatPromptQueued;
use tascarrel_api::types::chats::ChatPromptStarted;
use tascarrel_api::types::chats::ChatQueuedPrompt;
use tascarrel_api::types::chats::ChatSubscription;
use tascarrel_api::types::chats::ChatTimelineEntry;
use tascarrel_api::types::chats::CompactChatContextAction;
use tascarrel_api::types::chats::CompactChatContextOutput;
use tascarrel_api::types::chats::CreateChatAction;
use tascarrel_api::types::chats::CreateChatOutput;
use tascarrel_api::types::chats::DetachChatBindingAction;
use tascarrel_api::types::chats::DetachChatBindingOutput;
use tascarrel_api::types::chats::FlushChatPromptQueueAction;
use tascarrel_api::types::chats::FlushChatPromptQueueOutput;
use tascarrel_api::types::chats::GetChatUsageReportAction;
use tascarrel_api::types::chats::GetChatUsageReportOutput;
use tascarrel_api::types::chats::GetPodChatsOutput;
use tascarrel_api::types::chats::InterruptChatAction;
use tascarrel_api::types::chats::InterruptChatOutput;
use tascarrel_api::types::chats::RemoveChatQueuedPromptAction;
use tascarrel_api::types::chats::RemoveChatQueuedPromptOutput;
use tascarrel_api::types::chats::ResolveChatRequestAction;
use tascarrel_api::types::chats::ResolveChatRequestOutput;
use tascarrel_api::types::chats::SendChatPromptAction;
use tascarrel_api::types::chats::SendChatPromptOutput;
use tascarrel_api::types::chats::SetChatCostCenterAction;
use tascarrel_api::types::chats::SetChatCostCenterOutput;
use tascarrel_api::types::pods::PodId;
use tascarrel_protocol::MAX_CHAT_ATTACHMENT_BYTES;
use tokio::fs::File;
use tokio::io::AsyncRead;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::GuestNetworkService;
use crate::ProcessSupervisor;
use crate::services::chats::attachment::AttachmentStore;
use crate::services::chats::attachment::StoreChatAttachmentRequest;
use crate::services::chats::binding::AttachHarnessBindingRequest;
use crate::services::chats::binding::BindingProvider;
use crate::services::chats::binding::HarnessBinding;
use crate::services::chats::binding::HarnessBindingControl;
use crate::services::chats::binding::HarnessBindingError;
use crate::services::chats::harness::protocol::HarnessCommand;
use crate::services::chats::harness::protocol::HarnessCommandResult;
use crate::services::chats::harness::protocol::HarnessError;
use crate::services::chats::harness::protocol::HarnessEvent;
use crate::services::chats::harness::protocol::HarnessEventPayload;
use crate::services::chats::harness::protocol::HarnessPrompt;
use crate::services::chats::harness::protocol::HarnessPromptAttachment;
use crate::services::chats::state::ChatListStoreSubscription;
use crate::services::chats::state::ChatState;
use crate::services::chats::state::ChatStoreSubscription;
use crate::services::chats::state::UsageReportSubscription;
use crate::services::chats::state::protocol::ChatStateError;
use crate::services::chats::state::protocol::CreateChatRequest;
use crate::services::chats::state::protocol::IngestHarnessEventRequest;
use crate::services::chats::title::GenerateTitleRequest;
use crate::services::chats::title::TitleGenerationService;
use crate::services::chats::title::fallback_title;
use crate::services::pods::PodService;
use crate::services::pods::PodServiceError;

/// Binding-aware API for durable chats.
pub struct ChatEngine {
    inner: Arc<EngineInner>,
}

/// Runtime resource policy for harness bindings owned by a chat engine.
#[derive(Clone, Debug)]
pub struct ChatEngineOptions {
    /// Time an attached binding may remain idle before it becomes eligible for
    /// detachment.
    pub binding_idle_timeout: Duration,
    /// Number of attached bindings retained even after they have exceeded the
    /// idle timeout.
    pub max_active_bindings: usize,
    /// Directory in which uploaded prompt attachments are stored.
    ///
    /// Attachments are rejected when this is `None`.
    pub attachment_store_path: Option<PathBuf>,
    /// Store path as observed by harness processes inside their pods.
    pub attachment_binding_path: Option<PathBuf>,
    /// VM user and group that own the pod-visible attachment tree.
    pub attachment_owner: Option<(u32, u32)>,
    /// Maximum number of bytes accepted for one uploaded attachment.
    pub max_attachment_bytes: u64,
    /// Maximum number of attachments accepted on one prompt.
    pub max_prompt_attachments: usize,
}

impl Default for ChatEngineOptions {
    fn default() -> Self {
        Self {
            binding_idle_timeout: Duration::from_mins(5),
            max_active_bindings: 4,
            attachment_store_path: None,
            attachment_binding_path: None,
            attachment_owner: None,
            max_attachment_bytes: MAX_CHAT_ATTACHMENT_BYTES,
            max_prompt_attachments: 10,
        }
    }
}

impl ChatEngine {
    /// Creates an engine with explicit binding policy and optional title
    /// generation.
    #[must_use]
    pub fn with_options_and_title_generator(
        state: Arc<ChatState>,
        binding_provider: Arc<dyn BindingProvider>,
        options: ChatEngineOptions,
        title_generator: Option<Arc<dyn TitleGenerationService>>,
    ) -> Self {
        let attachment_store = options.attachment_store_path.clone().map(|path| {
            AttachmentStore::new(
                path,
                options.max_attachment_bytes,
                options.attachment_owner,
                options.attachment_binding_path.clone(),
            )
        });
        let inner = Arc::new(EngineInner {
            state,
            binding_provider,
            title_generator,
            attachment_store,
            options,
            runtime: Mutex::new(EngineRuntime::default()),
            shutting_down: AtomicBool::new(false),
            shutdown_lock: AsyncMutex::new(()),
            shutdown_notify: Notify::new(),
            shutdown_result: Mutex::new(None),
        });
        inner.start_idle_reaper();
        Self { inner }
    }

    /// Returns the current active chats belonging to one pod.
    pub(crate) fn get_pod_chats(
        &self,
        pod_id: PodId,
    ) -> BoxFuture<'_, Result<GetPodChatsOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            let chats = self
                .inner
                .state
                .chats()
                .await
                .map_err(state_api_error)?
                .chats
                .iter()
                .filter(|chat| chat.pod_id == pod_id)
                .cloned()
                .collect::<Vec<_>>()
                .into();
            Ok(GetPodChatsOutput { chats })
        })
    }

    /// Returns attributed durable chat usage for one half-open interval.
    pub(crate) fn get_usage_report(
        &self,
        action: &GetChatUsageReportAction,
    ) -> BoxFuture<'_, Result<GetChatUsageReportOutput, ChatEngineError>> {
        let from = action.from;
        let until = action.until;
        Box::pin(async move {
            self.inner.ensure_running()?;
            let report = self
                .inner
                .state
                .usage_report(from, until)
                .await
                .map_err(state_api_error)?;
            Ok(GetChatUsageReportOutput { report })
        })
    }

    /// Creates a durable chat and queues its initial prompt while attaching
    /// when provided.
    pub fn create_chat(
        &self,
        action: CreateChatAction,
        processes: ProcessSupervisor,
        pods: PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> BoxFuture<'_, Result<CreateChatOutput, ChatEngineError>> {
        self.create_chat_with_options(action, processes, pods, network_service, false)
    }

    /// Creates a first chat whose generated title is also applied to its pod.
    pub(crate) fn create_pod_chat(
        &self,
        action: CreateChatAction,
        processes: ProcessSupervisor,
        pods: PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> BoxFuture<'_, Result<CreateChatOutput, ChatEngineError>> {
        self.create_chat_with_options(action, processes, pods, network_service, true)
    }

    /// Validates chat creation before a related resource is created.
    pub(crate) async fn validate_chat_creation(
        &self,
        action: &CreateChatAction,
    ) -> Result<(), ChatEngineError> {
        self.validate_chat_selection(&action.harness, action.model.as_ref())
            .await
    }

    /// Validates the harness and model shared by all chat creation actions.
    pub(crate) async fn validate_chat_selection(
        &self,
        harness: &ChatHarnessKind,
        model: Option<&ChatModelSelection>,
    ) -> Result<(), ChatEngineError> {
        self.inner.ensure_running()?;
        let harnesses = self
            .inner
            .binding_provider
            .harnesses()
            .await
            .map_err(binding_api_error)?;
        validate_selection(&harnesses, harness, model)
    }

    fn create_chat_with_options(
        &self,
        action: CreateChatAction,
        processes: ProcessSupervisor,
        pods: PodService,
        network_service: Arc<GuestNetworkService>,
        mirror_pod_title: bool,
    ) -> BoxFuture<'_, Result<CreateChatOutput, ChatEngineError>> {
        Box::pin(async move {
            self.validate_chat_creation(&action).await?;
            let _pod_operation = pods
                .lock_for_chat_creation(&action.pod_id)
                .await
                .map_err(pod_api_error)?;
            let initial_prompt = action.initial_prompt;
            let auto_attach = action.auto_attach.unwrap_or(true);
            let explicit_title = action
                .title
                .filter(|title| !title.as_ref().trim().is_empty());
            let generate_title = explicit_title.is_none() && self.inner.title_generator.is_some();
            let title_prompt = explicit_title
                .is_none()
                .then(|| initial_prompt.clone())
                .flatten();
            let title_harness = action.harness.clone();
            let pod_title_mirror = mirror_pod_title.then(|| PodTitleMirror {
                pods: pods.clone(),
                pod_id: action.pod_id.clone(),
            });
            let initial_title = explicit_title.map_or_else(
                || {
                    initial_prompt
                        .as_ref()
                        .map_or_else(|| "New chat".to_owned(), fallback_title)
                },
                |title| title.as_ref().trim().to_owned(),
            );
            let result = self
                .inner
                .state
                .create_chat(CreateChatRequest {
                    title: initial_title.clone(),
                    pod_id: action.pod_id,
                    cost_center_id: action.cost_center_id,
                    harness: action.harness,
                    purpose: action.purpose,
                    model: action.model,
                })
                .await
                .map_err(state_api_error)?;
            if let Some(prompt) = title_prompt {
                self.inner.schedule_title_generation(
                    result.chat_id.clone(),
                    initial_title,
                    title_harness,
                    prompt,
                    pod_title_mirror,
                );
            } else if generate_title {
                lock(&self.inner.runtime)
                    .chat_mut(&result.chat_id)
                    .title_generation_pending = true;
            }
            if let Some(prompt) = initial_prompt {
                self.inner
                    .send_prompt(
                        SendChatPromptAction {
                            chat_id: result.chat_id.clone(),
                            prompt,
                            mode: ChatPromptMode::Immediate,
                        },
                        processes,
                        pods,
                        network_service,
                    )
                    .await?;
            } else if auto_attach {
                self.inner
                    .schedule_attach(result.chat_id.clone(), processes, pods, network_service)
                    .await?;
            }
            Ok(CreateChatOutput {
                chat_id: result.chat_id,
            })
        })
    }

    /// Schedules attachment of a harness binding to a chat.
    pub fn attach_chat_binding(
        &self,
        action: AttachChatBindingAction,
        processes: ProcessSupervisor,
        pods: PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> BoxFuture<'_, Result<AttachChatBindingOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            self.inner
                .schedule_attach(action.chat_id, processes, pods, network_service)
                .await?;
            Ok(AttachChatBindingOutput {})
        })
    }

    /// Schedules detachment of a chat's harness binding.
    pub fn detach_chat_binding(
        &self,
        action: DetachChatBindingAction,
    ) -> BoxFuture<'_, Result<DetachChatBindingOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            self.inner.begin_detach(action.chat_id).await?;
            Ok(DetachChatBindingOutput {})
        })
    }

    /// Stops the chat's binding and permanently removes the chat from active
    /// state.
    pub fn archive_chat(
        &self,
        action: ArchiveChatAction,
    ) -> BoxFuture<'_, Result<ArchiveChatOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            self.inner.archive_chat(action.chat_id).await?;
            Ok(ArchiveChatOutput {})
        })
    }

    /// Reattributes every turn belonging to one active chat.
    pub fn set_cost_center(
        &self,
        action: SetChatCostCenterAction,
    ) -> BoxFuture<'_, Result<SetChatCostCenterOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            self.inner
                .state
                .set_cost_center(action.chat_id, action.cost_center_id)
                .await
                .map_err(state_api_error)?;
            Ok(SetChatCostCenterOutput {})
        })
    }

    /// Archives every active chat owned by one pod.
    pub(crate) async fn archive_pod_chats(&self, pod_id: &PodId) -> Result<(), ChatEngineError> {
        self.inner.ensure_running()?;
        let chat_ids = self
            .inner
            .state
            .chats()
            .await
            .map_err(state_api_error)?
            .chats
            .iter()
            .filter(|chat| &chat.pod_id == pod_id)
            .map(|chat| chat.chat_id.clone())
            .collect::<Vec<_>>();
        for chat_id in chat_ids {
            self.inner.archive_chat_if_active(chat_id).await?;
        }
        Ok(())
    }

    /// Clears the attention flag after the chat has been viewed.
    pub fn acknowledge_chat_attention(
        &self,
        action: AcknowledgeChatAttentionAction,
    ) -> BoxFuture<'_, Result<AcknowledgeChatAttentionOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            self.inner
                .state
                .acknowledge_attention(action.chat_id)
                .await
                .map_err(state_api_error)?;
            Ok(AcknowledgeChatAttentionOutput {})
        })
    }

    /// Sends, queues, or interrupts and sends user input according to its
    /// delivery mode.
    ///
    /// A prompt submitted to a detached chat is queued while a new binding is
    /// attached.
    pub fn send_chat_prompt(
        &self,
        action: SendChatPromptAction,
        processes: ProcessSupervisor,
        pods: PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> BoxFuture<'_, Result<SendChatPromptOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            let chat_id = action.chat_id.clone();
            let title_prompt = action.prompt.clone();
            let delivery = self
                .inner
                .send_prompt(action, processes, pods, network_service)
                .await?;
            self.inner
                .schedule_pending_title_generation(chat_id, title_prompt)
                .await;
            Ok(SendChatPromptOutput { delivery })
        })
    }

    /// Interrupts the active turn and sends all queued prompts as one prompt.
    pub fn flush_chat_prompt_queue(
        &self,
        action: FlushChatPromptQueueAction,
    ) -> BoxFuture<'_, Result<FlushChatPromptQueueOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            self.inner.flush_prompt_queue(action.chat_id).await
        })
    }

    /// Removes one prompt waiting for a chat to become idle.
    pub fn remove_chat_queued_prompt(
        &self,
        action: RemoveChatQueuedPromptAction,
    ) -> BoxFuture<'_, Result<RemoveChatQueuedPromptOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            self.inner
                .remove_queued_prompt(action.chat_id, action.queued_prompt_id)
                .await?;
            Ok(RemoveChatQueuedPromptOutput {})
        })
    }

    /// Interrupts a chat's active turn.
    pub fn interrupt_chat(
        &self,
        action: InterruptChatAction,
    ) -> BoxFuture<'_, Result<InterruptChatOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            self.inner
                .apply_simple_command(action.chat_id, HarnessCommand::Interrupt)
                .await?;
            Ok(InterruptChatOutput {})
        })
    }

    /// Requests compaction of the model context associated with a chat.
    pub fn compact_chat_context(
        &self,
        action: CompactChatContextAction,
    ) -> BoxFuture<'_, Result<CompactChatContextOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            self.inner
                .apply_simple_command(action.chat_id, HarnessCommand::CompactContext)
                .await?;
            Ok(CompactChatContextOutput {})
        })
    }

    /// Resolves a structured user-input request owned by the current binding.
    pub fn resolve_chat_request(
        &self,
        action: ResolveChatRequestAction,
    ) -> BoxFuture<'_, Result<ResolveChatRequestOutput, ChatEngineError>> {
        Box::pin(async move {
            self.inner.ensure_running()?;
            self.inner.resolve_request(action).await?;
            Ok(ResolveChatRequestOutput {})
        })
    }

    /// Subscribes to snapshots and ordered changes for the chat list.
    pub fn subscribe_chats(
        &self,
        subscription: &ChatListChangedSubscription,
    ) -> Result<ChatListStoreSubscription, ChatEngineError> {
        self.inner
            .state
            .subscribe_chats(subscription)
            .map_err(state_api_error)
    }

    /// Subscribes to attributed durable usage for one half-open interval.
    pub fn subscribe_usage_report(
        &self,
        from: Timestamp,
        until: Timestamp,
    ) -> Result<UsageReportSubscription, ChatEngineError> {
        self.inner.ensure_running()?;
        self.inner
            .state
            .subscribe_usage_report(from, until)
            .map_err(state_api_error)
    }

    /// Returns the pod which owns one active chat.
    pub async fn chat_pod_id(&self, chat_id: &ChatId) -> Result<Option<PodId>, ChatEngineError> {
        self.inner
            .state
            .chat(chat_id.clone())
            .await
            .map(|chat| chat.map(|chat| chat.summary.pod_id))
            .map_err(state_api_error)
    }

    /// Returns the durable harness and model used to prepare an attachment.
    pub(crate) async fn chat_selection(
        &self,
        chat_id: &ChatId,
    ) -> Result<Option<(ChatHarnessKind, Option<ChatModelSelection>)>, ChatEngineError> {
        self.inner
            .state
            .harness_resumption(chat_id.clone())
            .await
            .map(|resumption| resumption.map(|resumption| (resumption.harness, resumption.model)))
            .map_err(state_api_error)
    }

    /// Subscribes to snapshots and ordered changes for one chat.
    pub fn subscribe_chat(
        &self,
        subscription: ChatSubscription,
    ) -> BoxFuture<'_, Result<ChatStoreSubscription, ChatEngineError>> {
        Box::pin(async move {
            self.inner
                .state
                .subscribe_chat(subscription)
                .await
                .map_err(state_api_error)
        })
    }

    /// Gracefully stops bindings, drains their events, and checkpoints durable
    /// chat state.
    ///
    /// The operation is idempotent. Once shutdown begins, new actions are
    /// rejected.
    pub fn shutdown(&self) -> BoxFuture<'_, Result<(), ChatEngineError>> {
        Box::pin(self.inner.shutdown())
    }

    /// Streams a new prompt attachment into the configured engine store.
    ///
    /// The returned metadata is safe to include in a [`ChatPrompt`]. The
    /// prompt-facing record does not expose the engine path; that path is
    /// resolved only when the prompt is handed to a harness binding.
    pub async fn store_chat_attachment<R>(
        &self,
        request: StoreChatAttachmentRequest,
        reader: R,
    ) -> Result<ChatPromptAttachment, ChatEngineError>
    where
        R: AsyncRead + Unpin,
    {
        self.inner.ensure_running()?;
        let store = self
            .inner
            .attachment_store
            .as_ref()
            .ok_or_else(|| api_error("chat attachments are not configured"))?;
        store
            .store(request, reader)
            .await
            .map_err(|error| api_error(error.to_string()))
    }

    /// Opens one immutable attachment for streaming outside the typed API.
    ///
    /// # Errors
    ///
    /// Returns a client-safe error when the attachment is unavailable or its
    /// durable representation is invalid.
    pub async fn open_chat_attachment(
        &self,
        attachment_id: &tascarrel_api::ids::ChatAttachmentId,
    ) -> Result<(ChatPromptAttachment, File), ChatEngineError> {
        self.inner.ensure_running()?;
        let store = self
            .inner
            .attachment_store
            .as_ref()
            .ok_or_else(|| api_error("chat attachments are not configured"))?;
        let resolved = store.resolve(attachment_id).await.map_err(|error| {
            if matches!(
                error,
                crate::services::chats::attachment::AttachmentStoreError::NotFound
            ) {
                ChatEngineError {
                    code: "not_found".to_owned(),
                    message: "attachment does not exist".to_owned(),
                }
            } else {
                api_error(error.to_string())
            }
        })?;
        let file = File::open(&resolved.source_path)
            .await
            .map_err(|error| api_error(format!("failed to open attachment content: {error}")))?;
        Ok((resolved.attachment, file))
    }
}

impl Drop for ChatEngine {
    fn drop(&mut self) {
        self.inner.shutting_down.store(true, Ordering::Release);
        self.inner.shutdown_notify.notify_waiters();
        let mut runtime = lock(&self.inner.runtime);
        for task in runtime.tasks.drain(..) {
            task.abort();
        }
        runtime.chats.clear();
    }
}

struct EngineInner {
    state: Arc<ChatState>,
    binding_provider: Arc<dyn BindingProvider>,
    title_generator: Option<Arc<dyn TitleGenerationService>>,
    attachment_store: Option<AttachmentStore>,
    options: ChatEngineOptions,
    runtime: Mutex<EngineRuntime>,
    shutting_down: AtomicBool,
    shutdown_lock: AsyncMutex<()>,
    shutdown_notify: Notify,
    shutdown_result: Mutex<Option<Result<(), ChatEngineError>>>,
}

/// Pod title target paired with the chat's initial generated-title candidate.
struct PodTitleMirror {
    pods: PodService,
    pod_id: PodId,
}

impl EngineInner {
    fn ensure_running(&self) -> Result<(), ChatEngineError> {
        if self.shutting_down.load(Ordering::Acquire) {
            Err(api_error("the chat engine is shutting down"))
        } else {
            Ok(())
        }
    }

    async fn prepare_prompt(
        &self,
        chat_id: &ChatId,
        mut prompt: ChatPrompt,
    ) -> Result<PreparedPrompt, ChatEngineError> {
        if prompt
            .text
            .as_deref()
            .is_none_or(|text| text.trim().is_empty())
            && prompt.attachments.is_empty()
        {
            return Err(api_error(
                "a chat prompt must contain text or an attachment",
            ));
        }
        if prompt.attachments.len() > self.options.max_prompt_attachments {
            return Err(api_error(format!(
                "a chat prompt may contain at most {} attachments",
                self.options.max_prompt_attachments,
            )));
        }

        let mut harness_attachments = Vec::with_capacity(prompt.attachments.len());
        if !prompt.attachments.is_empty() {
            self.state
                .chat(chat_id.clone())
                .await
                .map_err(state_api_error)?
                .ok_or_else(|| api_error("cannot attach a file to an unknown chat"))?;
            let store = self
                .attachment_store
                .as_ref()
                .ok_or_else(|| api_error("chat attachments are not configured"))?;
            let mut seen = HashSet::with_capacity(prompt.attachments.len());
            let mut canonical = Vec::with_capacity(prompt.attachments.len());
            for requested in &prompt.attachments {
                if !seen.insert(requested.attachment_id.clone()) {
                    return Err(api_error(
                        "a prompt cannot contain the same attachment twice",
                    ));
                }
                let resolved = store
                    .resolve(&requested.attachment_id)
                    .await
                    .map_err(|error| api_error(error.to_string()))?;
                let path = resolved.path.to_str().ok_or_else(|| {
                    api_error("the configured attachment binding path is not valid UTF-8")
                })?;
                let source_path = resolved.source_path.to_str().ok_or_else(|| {
                    api_error("the configured attachment source path is not valid UTF-8")
                })?;
                harness_attachments.push(HarnessPromptAttachment {
                    attachment_id: resolved.attachment.attachment_id.clone(),
                    name: resolved.attachment.name.clone(),
                    media_type: resolved.attachment.media_type.clone(),
                    size: resolved.attachment.size,
                    digest: resolved.attachment.digest.clone(),
                    source_path: source_path.into(),
                    path: path.into(),
                });
                canonical.push(resolved.attachment);
            }
            for attachment in &canonical {
                store
                    .associate_with_chat(chat_id, &attachment.attachment_id)
                    .await
                    .map_err(|error| api_error(error.to_string()))?;
                self.state
                    .upsert_attachment(chat_id.clone(), attachment.clone())
                    .await
                    .map_err(state_api_error)?;
            }
            prompt.attachments = canonical.into();
        }

        let harness_prompt = HarnessPrompt {
            text: prompt.text.clone(),
            attachments: harness_attachments.into(),
            model: prompt.model.clone(),
        };
        Ok(PreparedPrompt {
            prompt,
            harness_prompt,
        })
    }

    fn schedule_title_generation(
        self: &Arc<Self>,
        chat_id: ChatId,
        expected_title: String,
        harness: ChatHarnessKind,
        prompt: ChatPrompt,
        pod_title_mirror: Option<PodTitleMirror>,
    ) {
        let Some(generator) = self.title_generator.as_ref().map(Arc::clone) else {
            return;
        };
        let state = Arc::clone(&self.state);
        self.spawn(async move {
            let generated = match generator
                .generate_title(GenerateTitleRequest { harness, prompt })
                .await
            {
                Ok(generated) => generated,
                Err(error) => {
                    tracing::warn!(
                        chat_id = %chat_id.0,
                        code = %error.code,
                        message = %error.message,
                        "chat title generation failed"
                    );
                    return;
                }
            };
            let generated_title = generated.title;
            match state
                .replace_title(
                    chat_id.clone(),
                    expected_title.clone(),
                    generated_title.clone(),
                )
                .await
            {
                Ok(true) => {
                    if let Some(pod_title_mirror) = pod_title_mirror
                        && let Err(error) = pod_title_mirror
                            .pods
                            .replace_title(
                                &pod_title_mirror.pod_id,
                                &expected_title,
                                generated_title.into(),
                            )
                            .await
                    {
                        tracing::warn!(
                            chat_id = %chat_id.0,
                            pod_id = %pod_title_mirror.pod_id.0,
                            %error,
                            "failed to store a generated pod title"
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        chat_id = %chat_id.0,
                        message = %error.message,
                        "failed to store a generated chat title"
                    );
                }
            }
        });
    }

    async fn schedule_pending_title_generation(
        self: &Arc<Self>,
        chat_id: ChatId,
        prompt: ChatPrompt,
    ) {
        let pending = {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            let pending = chat.title_generation_pending;
            chat.title_generation_pending = false;
            pending
        };
        if !pending {
            return;
        }
        let fallback = fallback_title(&prompt);
        if let Err(error) = self
            .state
            .replace_title(chat_id.clone(), "New chat".to_owned(), fallback.clone())
            .await
        {
            tracing::warn!(
                chat_id = %chat_id.0,
                message = %error.message,
                "unable to store a prompt-derived fallback chat title"
            );
        }
        let harness = match self.state.chat(chat_id.clone()).await {
            Ok(Some(chat)) => chat.summary.harness,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    chat_id = %chat_id.0,
                    message = %error.message,
                    "unable to determine the chat harness for title generation"
                );
                return;
            }
        };
        self.schedule_title_generation(chat_id, fallback, harness, prompt, None);
    }

    async fn schedule_attach(
        self: &Arc<Self>,
        chat_id: ChatId,
        processes: ProcessSupervisor,
        pods: PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> Result<(), ChatEngineError> {
        let operation_lock = self.chat_operation_lock(&chat_id);
        let _guard = operation_lock.lock().await;
        let snapshot = self
            .state
            .chat(chat_id.clone())
            .await
            .map_err(state_api_error)?
            .ok_or_else(|| api_error("cannot attach an unknown chat"))?;
        let resumption = self
            .state
            .harness_resumption(chat_id.clone())
            .await
            .map_err(state_api_error)?
            .ok_or_else(|| api_error("cannot attach an unknown chat"))?;

        let binding_id = {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            match chat.binding {
                BindingSlot::Attaching { .. } | BindingSlot::Attached { .. } => return Ok(()),
                BindingSlot::Detaching { .. } => {
                    return Err(api_error("the chat binding is still detaching"));
                }
                BindingSlot::Detached => {}
            }
            let binding_id = ChatBindingId::generate();
            chat.binding = BindingSlot::Attaching {
                binding_id: binding_id.clone(),
            };
            binding_id
        };

        if let Err(error) = self
            .state
            .set_binding(
                chat_id.clone(),
                Some(chat_binding(
                    binding_id.clone(),
                    ChatBindingStatus::Attaching,
                )),
                None,
            )
            .await
        {
            lock(&self.runtime).chat_mut(&chat_id).binding = BindingSlot::Detached;
            return Err(state_api_error(error));
        }

        let request = AttachHarnessBindingRequest {
            binding_id: binding_id.clone(),
            chat_id: chat_id.clone(),
            pod_id: snapshot.summary.pod_id,
            resumption,
        };
        let inner = Arc::clone(self);
        self.spawn(async move {
            inner
                .run_binding(request, processes, pods, network_service)
                .await;
        });
        Ok(())
    }

    async fn run_binding(
        self: Arc<Self>,
        request: AttachHarnessBindingRequest,
        processes: ProcessSupervisor,
        pods: PodService,
        network_service: Arc<GuestNetworkService>,
    ) {
        let chat_id = request.chat_id.clone();
        let binding_id = request.binding_id.clone();
        let models = match self.binding_provider.harnesses().await {
            Ok(harnesses) => harnesses
                .iter()
                .find(|harness| harness.kind == request.resumption.harness)
                .map(|harness| harness.models.clone()),
            Err(error) => {
                self.finish_binding(
                    chat_id,
                    binding_id.clone(),
                    Some(binding_error(binding_id.clone(), error)),
                )
                .await;
                return;
            }
        };
        let Some(models) = models else {
            self.finish_binding(
                chat_id,
                binding_id.clone(),
                Some(binding_error(
                    binding_id,
                    HarnessBindingError {
                        code: "unsupported_harness".into(),
                        message: "the chat's harness is no longer available in this workspace"
                            .into(),
                    },
                )),
            )
            .await;
            return;
        };
        let binding = self
            .binding_provider
            .attach(request, processes, pods, network_service)
            .await;
        let binding = match binding {
            Ok(binding) => binding,
            Err(error) => {
                self.finish_binding(
                    chat_id,
                    binding_id.clone(),
                    Some(binding_error(binding_id.clone(), error)),
                )
                .await;
                return;
            }
        };
        self.consume_binding(chat_id, binding_id, models, binding)
            .await;
    }

    #[allow(clippy::too_many_lines)] // Binding consumption keeps ordered lifecycle cleanup together.
    async fn consume_binding(
        self: &Arc<Self>,
        chat_id: ChatId,
        binding_id: ChatBindingId,
        models: ArcVec<ChatModel>,
        mut binding: HarnessBinding,
    ) {
        let operation_lock = self.chat_operation_lock(&chat_id);
        let guard = operation_lock.lock().await;
        let activation = {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            match &chat.binding {
                BindingSlot::Attaching {
                    binding_id: current,
                } if current == &binding_id => {
                    chat.binding = BindingSlot::Attached {
                        binding_id: binding_id.clone(),
                        control: Arc::clone(&binding.control),
                    };
                    chat.idle_since = Some(Instant::now());
                    BindingActivation::Attached
                }
                BindingSlot::Detaching {
                    binding_id: current,
                    ..
                } if current == &binding_id => {
                    chat.binding = BindingSlot::Detaching {
                        binding_id: binding_id.clone(),
                        control: Some(Arc::clone(&binding.control)),
                    };
                    BindingActivation::Detaching
                }
                _ => BindingActivation::Stale,
            }
        };
        if activation == BindingActivation::Stale {
            drop(guard);
            if let Err(error) = binding.control.detach().await {
                tracing::warn!(
                    chat_id = %chat_id.0,
                    binding_id = %binding_id.0,
                    message = %error.message,
                    "failed to stop a stale chat binding"
                );
            }
            return;
        }
        if activation == BindingActivation::Attached
            && let Err(error) = self
                .state
                .set_binding(
                    chat_id.clone(),
                    Some(chat_binding(
                        binding_id.clone(),
                        ChatBindingStatus::Attached,
                    )),
                    None,
                )
                .await
        {
            drop(guard);
            if let Err(detach_error) = binding.control.detach().await {
                tracing::warn!(
                    chat_id = %chat_id.0,
                    binding_id = %binding_id.0,
                    message = %detach_error.message,
                    "failed to stop a chat binding after state activation failed"
                );
            }
            self.finish_binding(
                chat_id,
                binding_id.clone(),
                Some(state_binding_error(binding_id, error)),
            )
            .await;
            return;
        }
        drop(guard);

        if activation == BindingActivation::Attached {
            self.send_next_queued(&chat_id, &binding_id).await;
        } else if let Err(error) = binding.control.detach().await {
            tracing::warn!(
                chat_id = %chat_id.0,
                binding_id = %binding_id.0,
                message = %error.message,
                "failed to finish detaching a chat binding"
            );
        }

        let mut final_error = None;
        loop {
            let mut event = match binding.events.next_event().await {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(error) => {
                    final_error = Some(binding_error(binding_id.clone(), error));
                    break;
                }
            };
            apply_model_pricing(&mut event, &models);
            let signal = EventSignal::from_event(&event);
            // Prompt queue updates use this lock, so attention observes the
            // queue state from the exact turn-completion boundary.
            let completion_operation_lock = matches!(&signal, EventSignal::TurnCompleted(_))
                .then(|| self.chat_operation_lock(&chat_id));
            let completion_guard = match completion_operation_lock.as_ref() {
                Some(operation_lock) => Some(operation_lock.lock().await),
                None => None,
            };
            if let Err(error) = self
                .state
                .ingest_harness_event(IngestHarnessEventRequest {
                    chat_id: chat_id.clone(),
                    binding_id: binding_id.clone(),
                    event,
                })
                .await
            {
                final_error = Some(state_binding_error(binding_id.clone(), error));
                if let Err(detach_error) = binding.control.detach().await {
                    tracing::warn!(
                        chat_id = %chat_id.0,
                        binding_id = %binding_id.0,
                        message = %detach_error.message,
                        "failed to stop a chat binding after event persistence failed"
                    );
                }
                break;
            }
            let send_queued_prompt = match signal {
                EventSignal::TurnStarted(turn_id) => {
                    let operation_lock = self.chat_operation_lock(&chat_id);
                    let _guard = operation_lock.lock().await;
                    let mut runtime = lock(&self.runtime);
                    let chat = runtime.chat_mut(&chat_id);
                    if chat.binding.binding_id() == Some(&binding_id) {
                        chat.active_turn_id = Some(turn_id);
                        chat.idle_since = None;
                    }
                    false
                }
                EventSignal::TurnCompleted(turn_id) => {
                    let mut runtime = lock(&self.runtime);
                    let chat = runtime.chat_mut(&chat_id);
                    if chat.binding.binding_id() == Some(&binding_id)
                        && chat.active_turn_id.as_ref() == Some(&turn_id)
                    {
                        chat.active_turn_id = None;
                        chat.idle_since = Some(Instant::now());
                    }
                    true
                }
                EventSignal::SessionExited(error) => {
                    final_error =
                        error.map(|error| harness_binding_error(binding_id.clone(), error));
                    break;
                }
                EventSignal::Other => false,
            };
            drop(completion_guard);
            if send_queued_prompt {
                self.send_next_queued(&chat_id, &binding_id).await;
            }
        }
        self.finish_binding(chat_id, binding_id, final_error).await;
    }

    async fn begin_detach(self: &Arc<Self>, chat_id: ChatId) -> Result<(), ChatEngineError> {
        let operation_lock = self.chat_operation_lock(&chat_id);
        let _operation_guard = operation_lock.lock().await;
        if self
            .state
            .chat(chat_id.clone())
            .await
            .map_err(state_api_error)?
            .is_none()
        {
            return Err(api_error("cannot detach an unknown chat"));
        }
        let detached = {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            match &chat.binding {
                BindingSlot::Detached | BindingSlot::Detaching { .. } => return Ok(()),
                BindingSlot::Attaching { binding_id } => {
                    let binding_id = binding_id.clone();
                    chat.binding = BindingSlot::Detaching {
                        binding_id: binding_id.clone(),
                        control: None,
                    };
                    (binding_id, None)
                }
                BindingSlot::Attached {
                    binding_id,
                    control,
                } => {
                    let binding_id = binding_id.clone();
                    let control = Arc::clone(control);
                    chat.binding = BindingSlot::Detaching {
                        binding_id: binding_id.clone(),
                        control: Some(Arc::clone(&control)),
                    };
                    (binding_id, Some(control))
                }
            }
        };
        self.state
            .set_binding(
                chat_id.clone(),
                Some(chat_binding(
                    detached.0.clone(),
                    ChatBindingStatus::Detaching,
                )),
                None,
            )
            .await
            .map_err(state_api_error)?;
        if let Some(control) = detached.1 {
            let inner = Arc::clone(self);
            let binding_id = detached.0;
            self.spawn(async move {
                if let Err(error) = control.detach().await {
                    inner
                        .restore_after_detach_failure(chat_id, binding_id, error)
                        .await;
                }
            });
        }
        Ok(())
    }

    async fn archive_chat(&self, chat_id: ChatId) -> Result<(), ChatEngineError> {
        if self.archive_chat_if_active(chat_id).await? {
            Ok(())
        } else {
            Err(api_error("cannot archive an unknown chat"))
        }
    }

    async fn archive_chat_if_active(&self, chat_id: ChatId) -> Result<bool, ChatEngineError> {
        let operation_lock = self.chat_operation_lock(&chat_id);
        let _guard = operation_lock.lock().await;
        if self
            .state
            .chat(chat_id.clone())
            .await
            .map_err(state_api_error)?
            .is_none()
        {
            return Ok(false);
        }
        let control = {
            let runtime = lock(&self.runtime);
            runtime
                .chats
                .get(&chat_id)
                .and_then(|chat| match &chat.binding {
                    BindingSlot::Attached { control, .. } => Some(Arc::clone(control)),
                    BindingSlot::Detached
                    | BindingSlot::Attaching { .. }
                    | BindingSlot::Detaching { .. } => None,
                })
        };
        if let Some(control) = control {
            control.detach().await.map_err(binding_api_error)?;
        }
        self.state
            .archive_chat(chat_id.clone())
            .await
            .map_err(state_api_error)?;
        lock(&self.runtime).chats.remove(&chat_id);
        Ok(true)
    }

    async fn restore_after_detach_failure(
        self: &Arc<Self>,
        chat_id: ChatId,
        binding_id: ChatBindingId,
        error: HarnessBindingError,
    ) {
        let operation_lock = self.chat_operation_lock(&chat_id);
        let _guard = operation_lock.lock().await;
        let restored = {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            if matches!(
                &chat.binding,
                BindingSlot::Detaching {
                    binding_id: current,
                    control: Some(_),
                } if current == &binding_id
            ) {
                let control = chat.binding.control().expect("matched attached control");
                chat.binding = BindingSlot::Attached {
                    binding_id: binding_id.clone(),
                    control,
                };
                true
            } else {
                false
            }
        };
        if restored
            && let Err(state_error) = self
                .state
                .set_binding(
                    chat_id,
                    Some(chat_binding(
                        binding_id.clone(),
                        ChatBindingStatus::Attached,
                    )),
                    Some(binding_error(binding_id.clone(), error)),
                )
                .await
        {
            tracing::warn!(
                binding_id = %binding_id.0,
                message = %state_error.message,
                "failed to restore chat binding state after detach failed"
            );
        }
    }

    async fn send_prompt(
        self: &Arc<Self>,
        action: SendChatPromptAction,
        processes: ProcessSupervisor,
        pods: PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> Result<ChatPromptDelivery, ChatEngineError> {
        let prepared = self.prepare_prompt(&action.chat_id, action.prompt).await?;
        let PreparedPrompt {
            prompt,
            harness_prompt,
        } = prepared;
        let operation_lock = self.chat_operation_lock(&action.chat_id);
        let operation_guard = operation_lock.lock().await;
        let target = {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&action.chat_id);
            match &chat.binding {
                BindingSlot::Attaching { .. } => {
                    let queued_prompt = queued_prompt(prompt);
                    let queued_prompt_id = queued_prompt.queued_prompt_id.clone();
                    chat.queue.push_back(queued_prompt);
                    PromptTarget::Queued {
                        prompts: queued_prompts(chat),
                        attach: false,
                        queued_prompt_id,
                    }
                }
                BindingSlot::Attached { control, .. } => PromptTarget::Attached {
                    control: Arc::clone(control),
                    active_turn_id: chat.active_turn_id.clone(),
                    prompt,
                    harness_prompt,
                },
                BindingSlot::Detached => {
                    let queued_prompt = queued_prompt(prompt);
                    let queued_prompt_id = queued_prompt.queued_prompt_id.clone();
                    chat.queue.push_back(queued_prompt);
                    PromptTarget::Queued {
                        prompts: queued_prompts(chat),
                        attach: true,
                        queued_prompt_id,
                    }
                }
                BindingSlot::Detaching { .. } => {
                    return Err(api_error("the chat binding is detaching"));
                }
            }
        };
        let (control, active_turn_id, prompt, harness_prompt) = match target {
            PromptTarget::Queued {
                prompts,
                attach,
                queued_prompt_id,
            } => {
                self.state
                    .set_prompt_queue(action.chat_id.clone(), prompts)
                    .await
                    .map_err(state_api_error)?;
                drop(operation_guard);
                if attach {
                    self.schedule_attach(action.chat_id, processes, pods, network_service)
                        .await?;
                }
                return Ok(ChatPromptDelivery::Queued(ChatPromptQueued {
                    queued_prompt_id,
                }));
            }
            PromptTarget::Attached {
                control,
                active_turn_id,
                prompt,
                harness_prompt,
            } => (control, active_turn_id, prompt, harness_prompt),
        };
        if action.mode == ChatPromptMode::WhenIdle && active_turn_id.is_some() {
            let prompts = {
                let mut runtime = lock(&self.runtime);
                let chat = runtime.chat_mut(&action.chat_id);
                let queued_prompt = queued_prompt(prompt);
                let queued_prompt_id = queued_prompt.queued_prompt_id.clone();
                chat.queue.push_back(queued_prompt);
                (queued_prompts(chat), queued_prompt_id)
            };
            self.state
                .set_prompt_queue(action.chat_id, prompts.0)
                .await
                .map_err(state_api_error)?;
            return Ok(ChatPromptDelivery::Queued(ChatPromptQueued {
                queued_prompt_id: prompts.1,
            }));
        }

        self.start_prompt(
            action.chat_id,
            action.mode,
            control,
            active_turn_id,
            harness_prompt,
        )
        .await
    }

    async fn start_prompt(
        &self,
        chat_id: ChatId,
        mode: ChatPromptMode,
        control: Arc<dyn HarnessBindingControl>,
        active_turn_id: Option<ChatTurnId>,
        mut harness_prompt: HarnessPrompt,
    ) -> Result<ChatPromptDelivery, ChatEngineError> {
        if active_turn_id.is_some() && mode == ChatPromptMode::Immediate {
            harness_prompt.model = None;
        }
        let model = harness_prompt.model.clone();
        let command = if mode == ChatPromptMode::InterruptAndSend {
            HarnessCommand::InterruptAndSend(harness_prompt)
        } else {
            HarnessCommand::SendPrompt(harness_prompt)
        };
        let result = control.apply(command).await.map_err(binding_api_error)?;
        let turn_id = prompt_turn_id(result)?;
        {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            chat.active_turn_id = Some(turn_id.clone());
            chat.idle_since = None;
        }
        if let Some(model) = model {
            self.state
                .set_model(chat_id, model)
                .await
                .map_err(state_api_error)?;
        }
        Ok(ChatPromptDelivery::Started(ChatPromptStarted { turn_id }))
    }

    async fn send_next_queued(&self, chat_id: &ChatId, binding_id: &ChatBindingId) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        let operation_lock = self.chat_operation_lock(chat_id);
        let _guard = operation_lock.lock().await;
        let (control, prompt) = {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(chat_id);
            let BindingSlot::Attached {
                binding_id: current,
                control,
            } = &chat.binding
            else {
                return;
            };
            if current != binding_id || chat.active_turn_id.is_some() {
                return;
            }
            let Some(prompt) = chat.queue.front().cloned() else {
                return;
            };
            (Arc::clone(control), prompt)
        };
        let prepared = match self.prepare_prompt(chat_id, prompt.prompt).await {
            Ok(prepared) => prepared,
            Err(error) => {
                tracing::warn!(
                    chat_id = %chat_id.0,
                    binding_id = %binding_id.0,
                    message = %error.message,
                    "failed to prepare a queued chat prompt"
                );
                return;
            }
        };
        let model = prepared.harness_prompt.model.clone();
        let result = match control
            .apply(HarnessCommand::SendPrompt(prepared.harness_prompt))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    chat_id = %chat_id.0,
                    binding_id = %binding_id.0,
                    message = %error.message,
                    "failed to deliver a queued chat prompt"
                );
                return;
            }
        };
        let turn_id = match prompt_turn_id(result) {
            Ok(turn_id) => turn_id,
            Err(error) => {
                tracing::warn!(
                    chat_id = %chat_id.0,
                    binding_id = %binding_id.0,
                    message = %error.message,
                    "chat harness returned an invalid queued-prompt result"
                );
                return;
            }
        };
        let prompts = {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(chat_id);
            if chat.binding.binding_id() != Some(binding_id) {
                return;
            }
            chat.queue.pop_front();
            chat.active_turn_id = Some(turn_id);
            chat.idle_since = None;
            queued_prompts(chat)
        };
        if let Err(error) = self.state.set_prompt_queue(chat_id.clone(), prompts).await {
            tracing::warn!(
                chat_id = %chat_id.0,
                binding_id = %binding_id.0,
                message = %error.message,
                "failed to persist the delivered chat prompt queue"
            );
        }
        if let Some(model) = model
            && let Err(error) = self.state.set_model(chat_id.clone(), model).await
        {
            tracing::warn!(
                chat_id = %chat_id.0,
                binding_id = %binding_id.0,
                message = %error.message,
                "failed to persist the model used by a queued chat prompt"
            );
        }
    }

    async fn flush_prompt_queue(
        &self,
        chat_id: ChatId,
    ) -> Result<FlushChatPromptQueueOutput, ChatEngineError> {
        let operation_lock = self.chat_operation_lock(&chat_id);
        let _guard = operation_lock.lock().await;
        if self
            .state
            .chat(chat_id.clone())
            .await
            .map_err(state_api_error)?
            .is_none()
        {
            return Err(api_error("cannot flush an unknown chat prompt queue"));
        }

        let (control, active_turn_id, queued) = {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            if chat.queue.is_empty() {
                return Ok(FlushChatPromptQueueOutput {});
            }
            let BindingSlot::Attached { control, .. } = &chat.binding else {
                return Err(api_error("the chat has no attached harness binding"));
            };
            (
                Arc::clone(control),
                chat.active_turn_id.clone(),
                chat.queue.iter().cloned().collect::<Vec<_>>(),
            )
        };
        let prepared = self
            .prepare_prompt(&chat_id, combine_queued_prompts(&queued))
            .await?;
        let model = prepared.harness_prompt.model.clone();
        let command = if active_turn_id.is_some() {
            HarnessCommand::InterruptAndSend(prepared.harness_prompt)
        } else {
            HarnessCommand::SendPrompt(prepared.harness_prompt)
        };
        let result = control.apply(command).await.map_err(binding_api_error)?;
        let turn_id = prompt_turn_id(result)?;
        {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            chat.queue.clear();
            chat.active_turn_id = Some(turn_id.clone());
            chat.idle_since = None;
        }
        self.state
            .set_prompt_queue(chat_id.clone(), ArcVec::new())
            .await
            .map_err(state_api_error)?;
        if let Some(model) = model {
            self.state
                .set_model(chat_id, model)
                .await
                .map_err(state_api_error)?;
        }
        Ok(FlushChatPromptQueueOutput {})
    }

    async fn remove_queued_prompt(
        &self,
        chat_id: ChatId,
        queued_prompt_id: ChatQueuedPromptId,
    ) -> Result<(), ChatEngineError> {
        let operation_lock = self.chat_operation_lock(&chat_id);
        let _guard = operation_lock.lock().await;
        if self
            .state
            .chat(chat_id.clone())
            .await
            .map_err(state_api_error)?
            .is_none()
        {
            return Err(api_error("cannot remove a prompt from an unknown chat"));
        }
        let prompts = {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            chat.queue
                .retain(|prompt| prompt.queued_prompt_id != queued_prompt_id);
            queued_prompts(chat)
        };
        self.state
            .set_prompt_queue(chat_id, prompts)
            .await
            .map_err(state_api_error)
    }

    async fn apply_simple_command(
        &self,
        chat_id: ChatId,
        command: HarnessCommand,
    ) -> Result<HarnessCommandResult, ChatEngineError> {
        let operation_lock = self.chat_operation_lock(&chat_id);
        let _guard = operation_lock.lock().await;
        let control = {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            match &chat.binding {
                BindingSlot::Attached { control, .. } => Arc::clone(control),
                _ => return Err(api_error("the chat has no attached harness binding")),
            }
        };
        control.apply(command).await.map_err(binding_api_error)
    }

    async fn resolve_request(
        &self,
        action: ResolveChatRequestAction,
    ) -> Result<(), ChatEngineError> {
        let snapshot = self
            .state
            .chat(action.chat_id.clone())
            .await
            .map_err(state_api_error)?
            .ok_or_else(|| api_error("cannot resolve a request for an unknown chat"))?;
        let request = snapshot
            .timeline
            .iter()
            .find_map(|entry| match entry {
                ChatTimelineEntry::Request(request) if request.request_id == action.request_id => {
                    Some(request)
                }
                _ => None,
            })
            .ok_or_else(|| api_error("the chat request does not exist"))?;
        if request.resolved {
            return Err(api_error("the chat request is already resolved"));
        }
        let binding_id = snapshot
            .summary
            .binding
            .as_ref()
            .map(|binding| &binding.binding_id)
            .ok_or_else(|| api_error("the request's harness binding is no longer attached"))?;
        if &request.binding_id != binding_id {
            return Err(api_error(
                "the request belongs to an earlier harness binding",
            ));
        }
        self.apply_simple_command(
            action.chat_id,
            HarnessCommand::ResolveUserInput {
                request_id: action.request_id,
                answers: action.answers,
            },
        )
        .await?;
        Ok(())
    }

    async fn finish_binding(
        &self,
        chat_id: ChatId,
        binding_id: ChatBindingId,
        error: Option<ChatBindingError>,
    ) {
        let operation_lock = self.chat_operation_lock(&chat_id);
        let _guard = operation_lock.lock().await;
        {
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            if chat.binding.binding_id() != Some(&binding_id) {
                return;
            }
            chat.binding = BindingSlot::Detached;
            chat.active_turn_id = None;
            chat.idle_since = None;
        }
        if let Err(state_error) = self
            .state
            .finish_binding(chat_id.clone(), binding_id.clone(), error)
            .await
        {
            tracing::warn!(
                chat_id = %chat_id.0,
                binding_id = %binding_id.0,
                message = %state_error.message,
                "failed to persist a finished chat binding"
            );
        }
    }

    fn start_idle_reaper(self: &Arc<Self>) {
        let inner = Arc::clone(self);
        self.spawn(async move {
            inner.run_idle_reaper().await;
        });
    }

    async fn run_idle_reaper(self: &Arc<Self>) {
        let interval = idle_reaper_interval(self.options.binding_idle_timeout);
        loop {
            if self.shutting_down.load(Ordering::Acquire) {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = self.shutdown_notify.notified() => return,
            }
            self.detach_expired_idle_bindings().await;
        }
    }

    async fn detach_expired_idle_bindings(self: &Arc<Self>) {
        let candidates = {
            let runtime = lock(&self.runtime);
            let attached_count = runtime
                .chats
                .values()
                .filter(|chat| matches!(chat.binding, BindingSlot::Attached { .. }))
                .count();
            let excess = attached_count.saturating_sub(self.options.max_active_bindings);
            let mut candidates = runtime
                .chats
                .iter()
                .filter_map(|(chat_id, chat)| {
                    let BindingSlot::Attached { binding_id, .. } = &chat.binding else {
                        return None;
                    };
                    let idle_since = chat.idle_since?;
                    if chat.active_turn_id.is_some()
                        || !chat.queue.is_empty()
                        || idle_since.elapsed() < self.options.binding_idle_timeout
                    {
                        return None;
                    }
                    Some((idle_since, chat_id.clone(), binding_id.clone()))
                })
                .collect::<Vec<_>>();
            candidates.sort_by_key(|(idle_since, _, _)| *idle_since);
            candidates.truncate(excess);
            candidates
        };
        for (_, chat_id, binding_id) in candidates {
            self.detach_if_expired_idle(chat_id, binding_id).await;
        }
    }

    async fn detach_if_expired_idle(self: &Arc<Self>, chat_id: ChatId, binding_id: ChatBindingId) {
        let operation_lock = self.chat_operation_lock(&chat_id);
        let _guard = operation_lock.lock().await;
        let control = {
            let mut runtime = lock(&self.runtime);
            let attached_count = runtime
                .chats
                .values()
                .filter(|chat| matches!(chat.binding, BindingSlot::Attached { .. }))
                .count();
            if attached_count <= self.options.max_active_bindings {
                return;
            }
            let Some(chat) = runtime.chats.get_mut(&chat_id) else {
                return;
            };
            let BindingSlot::Attached {
                binding_id: current,
                control,
            } = &chat.binding
            else {
                return;
            };
            if current != &binding_id
                || chat.active_turn_id.is_some()
                || !chat.queue.is_empty()
                || !chat.idle_since.is_some_and(|idle_since| {
                    idle_since.elapsed() >= self.options.binding_idle_timeout
                })
            {
                return;
            }
            let control = Arc::clone(control);
            chat.binding = BindingSlot::Detaching {
                binding_id: binding_id.clone(),
                control: Some(Arc::clone(&control)),
            };
            control
        };
        if let Err(error) = self
            .state
            .set_binding(
                chat_id.clone(),
                Some(chat_binding(
                    binding_id.clone(),
                    ChatBindingStatus::Detaching,
                )),
                None,
            )
            .await
        {
            tracing::warn!(
                chat_id = %chat_id.0,
                binding_id = %binding_id.0,
                message = %error.message,
                "failed to publish chat binding detachment"
            );
            let mut runtime = lock(&self.runtime);
            let chat = runtime.chat_mut(&chat_id);
            if matches!(
                &chat.binding,
                BindingSlot::Detaching {
                    binding_id: current,
                    ..
                } if current == &binding_id
            ) {
                chat.binding = BindingSlot::Attached {
                    binding_id,
                    control,
                };
            }
            return;
        }
        let inner = Arc::clone(self);
        self.spawn(async move {
            if let Err(error) = control.detach().await {
                inner
                    .restore_after_detach_failure(chat_id, binding_id, error)
                    .await;
            }
        });
    }

    async fn shutdown(self: &Arc<Self>) -> Result<(), ChatEngineError> {
        let _guard = self.shutdown_lock.lock().await;
        if let Some(result) = lock(&self.shutdown_result).clone() {
            return result;
        }
        self.shutting_down.store(true, Ordering::Release);
        self.shutdown_notify.notify_waiters();
        let chat_ids = {
            let runtime = lock(&self.runtime);
            runtime.chats.keys().cloned().collect::<Vec<_>>()
        };
        let mut first_error = None;
        for chat_id in chat_ids {
            if let Err(error) = self.begin_detach(chat_id).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        loop {
            let tasks = {
                let mut runtime = lock(&self.runtime);
                std::mem::take(&mut runtime.tasks)
            };
            if tasks.is_empty() {
                break;
            }
            for task in tasks {
                if let Err(error) = task.await
                    && !error.is_cancelled()
                    && first_error.is_none()
                {
                    first_error = Some(api_error(format!(
                        "a chat engine task failed during shutdown: {error}"
                    )));
                }
            }
        }
        if let Err(error) = self.state.checkpoint().await
            && first_error.is_none()
        {
            first_error = Some(state_api_error(error));
        }
        let result = first_error.map_or(Ok(()), Err);
        *lock(&self.shutdown_result) = Some(result.clone());
        result
    }

    fn chat_operation_lock(&self, chat_id: &ChatId) -> Arc<AsyncMutex<()>> {
        lock(&self.runtime).chat_mut(chat_id).operation_lock.clone()
    }

    fn spawn<F>(self: &Arc<Self>, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        lock(&self.runtime).tasks.push(tokio::spawn(future));
    }
}

#[derive(Default)]
struct EngineRuntime {
    chats: HashMap<ChatId, RuntimeChat>,
    tasks: Vec<JoinHandle<()>>,
}

impl EngineRuntime {
    fn chat_mut(&mut self, chat_id: &ChatId) -> &mut RuntimeChat {
        self.chats.entry(chat_id.clone()).or_default()
    }
}

struct RuntimeChat {
    operation_lock: Arc<AsyncMutex<()>>,
    binding: BindingSlot,
    queue: VecDeque<ChatQueuedPrompt>,
    active_turn_id: Option<ChatTurnId>,
    idle_since: Option<Instant>,
    title_generation_pending: bool,
}

impl Default for RuntimeChat {
    fn default() -> Self {
        Self {
            operation_lock: Arc::new(AsyncMutex::new(())),
            binding: BindingSlot::Detached,
            queue: VecDeque::new(),
            active_turn_id: None,
            idle_since: None,
            title_generation_pending: false,
        }
    }
}

enum BindingSlot {
    Detached,
    Attaching {
        binding_id: ChatBindingId,
    },
    Attached {
        binding_id: ChatBindingId,
        control: Arc<dyn HarnessBindingControl>,
    },
    Detaching {
        binding_id: ChatBindingId,
        control: Option<Arc<dyn HarnessBindingControl>>,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum BindingActivation {
    Attached,
    Detaching,
    Stale,
}

enum PromptTarget {
    Queued {
        prompts: ArcVec<ChatQueuedPrompt>,
        attach: bool,
        queued_prompt_id: ChatQueuedPromptId,
    },
    Attached {
        control: Arc<dyn HarnessBindingControl>,
        active_turn_id: Option<ChatTurnId>,
        prompt: ChatPrompt,
        harness_prompt: HarnessPrompt,
    },
}

struct PreparedPrompt {
    prompt: ChatPrompt,
    harness_prompt: HarnessPrompt,
}

impl BindingSlot {
    fn binding_id(&self) -> Option<&ChatBindingId> {
        match self {
            Self::Detached => None,
            Self::Attaching { binding_id }
            | Self::Attached { binding_id, .. }
            | Self::Detaching { binding_id, .. } => Some(binding_id),
        }
    }

    fn control(&self) -> Option<Arc<dyn HarnessBindingControl>> {
        match self {
            Self::Attached { control, .. }
            | Self::Detaching {
                control: Some(control),
                ..
            } => Some(Arc::clone(control)),
            Self::Detached | Self::Attaching { .. } | Self::Detaching { control: None, .. } => None,
        }
    }
}

enum EventSignal {
    TurnStarted(ChatTurnId),
    TurnCompleted(ChatTurnId),
    SessionExited(Option<HarnessError>),
    Other,
}

impl EventSignal {
    fn from_event(event: &HarnessEvent) -> Self {
        match &event.payload {
            HarnessEventPayload::TurnStarted => {
                event.turn_id.clone().map_or(Self::Other, Self::TurnStarted)
            }
            HarnessEventPayload::TurnCompleted { .. } => event
                .turn_id
                .clone()
                .map_or(Self::Other, Self::TurnCompleted),
            HarnessEventPayload::SessionExited { error } => Self::SessionExited(error.clone()),
            _ => Self::Other,
        }
    }
}

fn apply_model_pricing(event: &mut HarnessEvent, models: &[ChatModel]) {
    let HarnessEventPayload::TurnUsageUpdated { usage, .. } = &mut event.payload else {
        return;
    };
    for model_usage in usage.models.iter_mut() {
        model_usage.pricing = models
            .iter()
            .find(|model| model_ids_match(model.id.as_ref(), model_usage.model.model.as_ref()))
            .and_then(|model| model.pricing.clone());
    }
}

fn model_ids_match(left: &str, right: &str) -> bool {
    base_model_id(left) == base_model_id(right)
}

fn base_model_id(model: &str) -> &str {
    model.split_once('[').map_or(model, |(model, _)| model)
}

fn validate_selection(
    harnesses: &[ChatHarness],
    selected_harness: &ChatHarnessKind,
    selection: Option<&ChatModelSelection>,
) -> Result<(), ChatEngineError> {
    let harness = harnesses
        .iter()
        .find(|harness| &harness.kind == selected_harness)
        .ok_or_else(|| api_error("the selected harness is not available in this workspace"))?;
    if let Some(selection) = selection
        && !harness
            .models
            .iter()
            .any(|model| model.id == selection.model)
    {
        return Err(api_error(
            "the selected model is not available for this harness",
        ));
    }
    Ok(())
}

fn queued_prompts(chat: &RuntimeChat) -> ArcVec<ChatQueuedPrompt> {
    chat.queue.iter().cloned().collect::<Vec<_>>().into()
}

fn queued_prompt(prompt: ChatPrompt) -> ChatQueuedPrompt {
    ChatQueuedPrompt {
        queued_prompt_id: ChatQueuedPromptId::generate(),
        prompt,
    }
}

fn combine_queued_prompts(prompts: &[ChatQueuedPrompt]) -> ChatPrompt {
    let text = prompts
        .iter()
        .filter_map(|queued| queued.prompt.text.as_deref())
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let attachments = prompts
        .iter()
        .flat_map(|queued| queued.prompt.attachments.iter().cloned())
        .collect::<Vec<_>>()
        .into();
    let model = prompts
        .iter()
        .rev()
        .find_map(|queued| queued.prompt.model.clone());
    ChatPrompt {
        text: (!text.is_empty()).then(|| text.into()),
        attachments,
        model,
    }
}

fn idle_reaper_interval(timeout: Duration) -> Duration {
    timeout.clamp(Duration::from_millis(10), Duration::from_secs(1))
}

fn prompt_turn_id(result: HarnessCommandResult) -> Result<ChatTurnId, ChatEngineError> {
    match result {
        HarnessCommandResult::PromptAccepted { turn_id, .. } => Ok(turn_id),
        HarnessCommandResult::Accepted | HarnessCommandResult::Stopped => Err(api_error(
            "the harness returned an invalid result for a prompt command",
        )),
    }
}

fn chat_binding(binding_id: ChatBindingId, status: ChatBindingStatus) -> ChatBinding {
    ChatBinding { binding_id, status }
}

fn binding_error(binding_id: ChatBindingId, error: HarnessBindingError) -> ChatBindingError {
    ChatBindingError {
        binding_id,
        code: error.code.into(),
        message: error.message.into(),
        occurred_at: Timestamp::now(),
    }
}

fn harness_binding_error(binding_id: ChatBindingId, error: HarnessError) -> ChatBindingError {
    ChatBindingError {
        binding_id,
        code: format!("{:?}", error.kind).into(),
        message: error.message.into(),
        occurred_at: Timestamp::now(),
    }
}

fn state_binding_error(binding_id: ChatBindingId, error: ChatStateError) -> ChatBindingError {
    ChatBindingError {
        binding_id,
        code: format!("{:?}", error.kind).into(),
        message: error.message.into(),
        occurred_at: Timestamp::now(),
    }
}

fn state_api_error(error: ChatStateError) -> ChatEngineError {
    ChatEngineError {
        code: format!("{:?}", error.kind),
        message: error.message,
    }
}

fn binding_api_error(error: HarnessBindingError) -> ChatEngineError {
    ChatEngineError {
        code: error.code,
        message: error.message,
    }
}

#[allow(clippy::needless_pass_by_value)] // This signature is used directly with Result::map_err.
fn pod_api_error(report: Report<PodServiceError>) -> ChatEngineError {
    match report.error() {
        PodServiceError::InvalidRequest(message) => ChatEngineError {
            code: "invalid_request".to_owned(),
            message: message.clone(),
        },
        PodServiceError::Internal(message) => ChatEngineError {
            code: "internal".to_owned(),
            message: message.clone(),
        },
    }
}

fn api_error(message: impl Into<String>) -> ChatEngineError {
    ChatEngineError {
        code: "invalid_request".to_owned(),
        message: message.into(),
    }
}

/// Failure while executing a chat-engine operation.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatEngineError {
    /// Stable failure category.
    pub code: String,
    /// Human-readable diagnostic safe to expose to the caller.
    pub message: String,
}

impl std::fmt::Display for ChatEngineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ChatEngineError {}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
