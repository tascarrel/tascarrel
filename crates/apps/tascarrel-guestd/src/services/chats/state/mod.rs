//! Durable chat state and streaming interfaces.
//!
//! This layer consumes normalized harness events but never starts or controls a
//! harness itself. It persists compacted chat state and harness resumption
//! data, while subscription replay uses a fixed-capacity in-memory event buffer
//! scoped to one running state instance.
//!
//! Turns and timeline entries retain the binding identifier of the harness
//! session that produced them. Unfinished state is current only while that
//! binding remains current; once the binding ends, unfinished turns and items
//! are considered failed without rewriting their durable representation.
//! Harness bindings and prompt queues are runtime state owned by the engine and
//! are not persisted.

use std::num::NonZeroUsize;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use jiff::Timestamp;
use reportify::Report;
use tascarrel_api::ArcVec;
use tascarrel_api::ids::ChatActivityId;
use tascarrel_api::ids::ChatBindingId;
use tascarrel_api::ids::ChatId;
use tascarrel_api::ids::ChatItemId;
use tascarrel_api::ids::ChatRequestId;
use tascarrel_api::ids::ChatTurnId;
use tascarrel_api::is_valid_chat_cost_center_id;
use tascarrel_api::types::chats;
use tascarrel_api::types::chats::Chat;
use tascarrel_api::types::chats::ChatActivity;
use tascarrel_api::types::chats::ChatActivityKind;
use tascarrel_api::types::chats::ChatAgentStatus;
use tascarrel_api::types::chats::ChatBinding;
use tascarrel_api::types::chats::ChatBindingError;
use tascarrel_api::types::chats::ChatContent;
use tascarrel_api::types::chats::ChatCostCenterId;
use tascarrel_api::types::chats::ChatItem;
use tascarrel_api::types::chats::ChatItemCompleted;
use tascarrel_api::types::chats::ChatItemState;
use tascarrel_api::types::chats::ChatList;
use tascarrel_api::types::chats::ChatListChangedSubscription;
use tascarrel_api::types::chats::ChatListMutation;
use tascarrel_api::types::chats::ChatModelSelection;
use tascarrel_api::types::chats::ChatMutation;
use tascarrel_api::types::chats::ChatPromptAttachment;
use tascarrel_api::types::chats::ChatPromptQueue;
use tascarrel_api::types::chats::ChatQueuedPrompt;
use tascarrel_api::types::chats::ChatRequest;
use tascarrel_api::types::chats::ChatSubscription;
use tascarrel_api::types::chats::ChatSummary;
use tascarrel_api::types::chats::ChatTimelineEntry;
use tascarrel_api::types::chats::ChatTurn;
use tascarrel_api::types::chats::ChatTurnState;
use tascarrel_api::types::chats::ChatTurnUsage;
use tascarrel_api::types::chats::ChatUsageReport;
use tascarrel_api::types::chats::ChatUsageState;
use tascarrel_api::types::chats::TextContent;
use tokio::sync::Mutex as AsyncMutex;
use tokio_rusqlite::Connection;

use crate::services::chats::harness::protocol::HarnessError;
use crate::services::chats::harness::protocol::HarnessEvent;
use crate::services::chats::harness::protocol::HarnessEventPayload;
use crate::services::chats::harness::protocol::ResumeCursor;
use crate::services::chats::state::layer::StateChatUpdate;
use crate::services::chats::state::layer::StateLayer;
use crate::services::chats::state::layer::StateLayerError;
use crate::services::chats::state::layer::StateUpdate;
use crate::services::chats::state::protocol::ChatStateError;
use crate::services::chats::state::protocol::ChatStateErrorKind;
use crate::services::chats::state::protocol::CreateChatRequest;
use crate::services::chats::state::protocol::CreateChatResult;
use crate::services::chats::state::protocol::HarnessResumption;
use crate::services::chats::state::protocol::IngestHarnessEventRequest;
use crate::services::chats::state::protocol::IngestHarnessEventResult;
use crate::services::chats::state::storage::Storage;

mod cost;
mod layer;
pub mod protocol;
mod storage;
mod usage;

pub(crate) use layer::ChatListStoreSubscription;
pub(crate) use layer::ChatStoreSubscription;
pub(crate) use usage::UsageReportSubscription;

/// Durable chat state consumed by the binding-aware engine.
pub struct ChatState {
    layer: Arc<StateLayer>,
    operations: AsyncMutex<()>,
}

impl ChatState {
    /// Opens the durable state layer over the guest daemon's `SQLite`
    /// connection.
    pub fn new(database: Connection) -> BoxFuture<'static, Result<Self, ChatStateError>> {
        Box::pin(async move {
            let storage = Storage::open(database);
            let layer = StateLayer::open(EVENT_CAPACITY, storage)
                .await
                .map_err(state_layer_error)?;
            Ok(Self {
                layer: Arc::new(layer),
                operations: AsyncMutex::new(()),
            })
        })
    }

    /// Returns a consistent snapshot of all chats.
    pub fn chats(&self) -> BoxFuture<'_, Result<ChatList, ChatStateError>> {
        Box::pin(std::future::ready(Ok(self.layer.chats())))
    }

    /// Returns a consistent snapshot of one chat, or `None` when it does not
    /// exist.
    pub fn chat(
        &self,
        chat_id: chats::ChatId,
    ) -> BoxFuture<'_, Result<Option<Chat>, ChatStateError>> {
        Box::pin(async move { self.layer.chat(&chat_id).await.map_err(state_layer_error) })
    }

    /// Permanently removes a chat from active state.
    pub fn archive_chat(
        &self,
        chat_id: chats::ChatId,
    ) -> BoxFuture<'_, Result<(), ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            if !self
                .layer
                .archive_chat(&chat_id)
                .await
                .map_err(state_layer_error)?
            {
                return Err(invalid_input("cannot archive an unknown chat"));
            }
            Ok(())
        })
    }

    /// Clears the durable attention flag after a user views the chat.
    pub fn acknowledge_attention(
        &self,
        chat_id: ChatId,
    ) -> BoxFuture<'_, Result<(), ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            let mut summary = self.summary(&chat_id).await?;
            if !summary.attention_required {
                return Ok(());
            }
            summary.attention_required = false;
            self.apply_summary(summary).await
        })
    }

    /// Subscribes to changes to the chat list.
    ///
    /// A cursor that can be satisfied by the current instance's bounded event
    /// buffer is replayed before live delivery. A missing,
    /// foreign-instance, or evicted cursor starts with a fresh snapshot.
    pub fn subscribe_chats(
        &self,
        subscription: &ChatListChangedSubscription,
    ) -> Result<ChatListStoreSubscription, ChatStateError> {
        let cursor = subscription
            .cursor
            .as_ref()
            .map(runtime_stamp)
            .transpose()?;
        Ok(self.layer.subscribe_chats(cursor))
    }

    /// Subscribes to changes to one chat.
    ///
    /// A cursor that can be satisfied by the current instance's bounded event
    /// buffer is replayed before live delivery. A missing,
    /// foreign-instance, or evicted cursor starts with a fresh snapshot.
    pub fn subscribe_chat(
        &self,
        subscription: ChatSubscription,
    ) -> BoxFuture<'_, Result<ChatStoreSubscription, ChatStateError>> {
        let layer = Arc::clone(&self.layer);
        Box::pin(async move {
            let cursor = subscription
                .cursor
                .as_ref()
                .map(runtime_stamp)
                .transpose()?;
            layer
                .subscribe_chat(subscription.chat_id, cursor)
                .await
                .map_err(state_layer_error)?
                .ok_or_else(|| invalid_input("cannot subscribe to an unknown chat"))
        })
    }

    /// Returns attributed durable usage for one half-open interval.
    pub fn usage_report(
        &self,
        from: Timestamp,
        until: Timestamp,
    ) -> BoxFuture<'_, Result<ChatUsageReport, ChatStateError>> {
        let layer = Arc::clone(&self.layer);
        Box::pin(async move {
            validate_usage_interval(from, until)?;
            layer
                .usage_report(from, until)
                .await
                .map_err(state_layer_error)
        })
    }

    /// Subscribes to attributed durable usage for one half-open interval.
    pub fn subscribe_usage_report(
        &self,
        from: Timestamp,
        until: Timestamp,
    ) -> Result<UsageReportSubscription, ChatStateError> {
        validate_usage_interval(from, until)?;
        Ok(self.layer.subscribe_usage_report(from, until))
    }

    /// Creates a chat durably associated with one harness implementation.
    pub fn create_chat(
        &self,
        request: CreateChatRequest,
    ) -> BoxFuture<'_, Result<CreateChatResult, ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            validate_cost_center_id(request.cost_center_id.as_ref())?;
            let chat_id = ChatId::generate();
            let now = Timestamp::now();
            let summary = ChatSummary {
                chat_id: chat_id.clone(),
                pod_id: request.pod_id,
                binding: None,
                last_binding_error: None,
                agent_status: ChatAgentStatus::Idle,
                attention_required: false,
                harness: request.harness,
                purpose: request.purpose,
                model: request.model,
                cost_center_id: request.cost_center_id,
                title: request.title.into(),
                created_at: now,
                updated_at: now,
            };
            self.layer
                .create_chat(summary)
                .await
                .map_err(state_layer_error)?;
            Ok(CreateChatResult { chat_id })
        })
    }

    /// Reattributes every turn belonging to one active chat.
    pub fn set_cost_center(
        &self,
        chat_id: ChatId,
        cost_center_id: Option<ChatCostCenterId>,
    ) -> BoxFuture<'_, Result<(), ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            validate_cost_center_id(cost_center_id.as_ref())?;
            let mut summary = self.summary(&chat_id).await?;
            if summary.cost_center_id == cost_center_id {
                return Ok(());
            }
            summary.cost_center_id = cost_center_id;
            self.apply_summary(summary).await
        })
    }

    /// Applies one normalized harness event to a chat.
    ///
    /// The resulting state change is published immediately and its compacted
    /// durable portion is checkpointed before this method returns.
    pub fn ingest_harness_event(
        &self,
        request: IngestHarnessEventRequest,
    ) -> BoxFuture<'_, Result<IngestHarnessEventResult, ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            let snapshot = self
                .layer
                .chat(&request.chat_id)
                .await
                .map_err(state_layer_error)?
                .ok_or_else(|| invalid_input("cannot ingest an event for an unknown chat"))?;
            require_current_binding(&snapshot, &request.binding_id)?;
            let occurred_at = request.event.occurred_at;
            let reconciled =
                reconcile_harness_event(&snapshot, &request.binding_id, request.event)?;
            if let Some(resume_cursor) = reconciled.resume_cursor {
                self.layer
                    .store_resume_cursor(request.chat_id.clone(), resume_cursor)
                    .await
                    .map_err(state_layer_error)?;
            }
            if let Some(update) = reconciled.update {
                self.layer
                    .apply_update(update)
                    .await
                    .map_err(state_layer_error)?;
            }
            if reconciled.touch_summary {
                self.touch_summary(&request.chat_id, occurred_at, reconciled.attention_required)
                    .await?;
            }
            Ok(IngestHarnessEventResult {})
        })
    }

    /// Returns the harness-owned state needed to open the chat's next native
    /// session.
    pub fn harness_resumption(
        &self,
        chat_id: ChatId,
    ) -> BoxFuture<'_, Result<Option<HarnessResumption>, ChatStateError>> {
        Box::pin(async move {
            self.layer
                .harness_resumption(&chat_id)
                .await
                .map_err(state_layer_error)
        })
    }

    /// Replaces the runtime binding information exposed for one chat.
    pub fn set_binding(
        &self,
        chat_id: ChatId,
        binding: Option<ChatBinding>,
        last_error: Option<ChatBindingError>,
    ) -> BoxFuture<'_, Result<(), ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            let mut summary = self.summary(&chat_id).await?;
            if binding
                .as_ref()
                .zip(summary.binding.as_ref())
                .is_none_or(|(next, current)| next.binding_id != current.binding_id)
            {
                summary.agent_status = ChatAgentStatus::Idle;
            }
            summary.binding = binding;
            summary.last_binding_error = last_error;
            self.apply_summary(summary).await
        })
    }

    /// Replaces the runtime prompt queue exposed for one chat.
    pub fn set_prompt_queue(
        &self,
        chat_id: ChatId,
        prompts: ArcVec<ChatQueuedPrompt>,
    ) -> BoxFuture<'_, Result<(), ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            if self
                .layer
                .chat(&chat_id)
                .await
                .map_err(state_layer_error)?
                .is_none()
            {
                return Err(invalid_input("cannot update an unknown chat prompt queue"));
            }
            self.layer
                .apply_update(StateUpdate {
                    list: None,
                    chat: Some(StateChatUpdate {
                        chat_id,
                        payload: ChatMutation::ReplacePromptQueue(ChatPromptQueue { prompts }),
                    }),
                })
                .await
                .map_err(state_layer_error)?;
            Ok(())
        })
    }

    /// Adds attachment metadata to a chat's durable attachment collection.
    pub fn upsert_attachment(
        &self,
        chat_id: ChatId,
        attachment: ChatPromptAttachment,
    ) -> BoxFuture<'_, Result<(), ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            if self
                .layer
                .chat(&chat_id)
                .await
                .map_err(state_layer_error)?
                .is_none()
            {
                return Err(invalid_input("cannot add an attachment to an unknown chat"));
            }
            self.layer
                .apply_update(StateUpdate {
                    list: None,
                    chat: Some(StateChatUpdate {
                        chat_id,
                        payload: ChatMutation::UpsertAttachment(attachment),
                    }),
                })
                .await
                .map_err(state_layer_error)
        })
    }

    /// Replaces the effective model selected for a chat.
    pub fn set_model(
        &self,
        chat_id: ChatId,
        model: ChatModelSelection,
    ) -> BoxFuture<'_, Result<(), ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            let mut summary = self.summary(&chat_id).await?;
            summary.model = Some(model);
            summary.updated_at = Timestamp::now();
            self.apply_summary(summary).await
        })
    }

    /// Replaces a chat title when it still matches the expected value.
    ///
    /// The comparison and update are serialized with other state operations.
    /// This allows a background title generator to avoid overwriting a
    /// newer user- or client-supplied title.
    pub fn replace_title(
        &self,
        chat_id: ChatId,
        expected: String,
        replacement: String,
    ) -> BoxFuture<'_, Result<bool, ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            let mut summary = self.summary(&chat_id).await?;
            if summary.title.as_ref() != expected {
                return Ok(false);
            }
            if replacement == expected {
                return Ok(false);
            }
            summary.title = replacement.into();
            summary.updated_at = Timestamp::now();
            self.apply_summary(summary).await?;
            Ok(true)
        })
    }

    /// Checkpoints state produced by a binding before removing its runtime
    /// attachment.
    pub fn finish_binding(
        &self,
        chat_id: ChatId,
        binding_id: ChatBindingId,
        last_error: Option<ChatBindingError>,
    ) -> BoxFuture<'_, Result<(), ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            let snapshot = self
                .layer
                .chat(&chat_id)
                .await
                .map_err(state_layer_error)?
                .ok_or_else(|| invalid_input("cannot finish a binding for an unknown chat"))?;
            if snapshot
                .summary
                .binding
                .as_ref()
                .is_none_or(|binding| binding.binding_id != binding_id)
            {
                return Ok(());
            }
            let attention_required =
                has_inflight_state(&snapshot, &binding_id) && snapshot.queued_prompts.is_empty();
            let mut summary = snapshot.summary;
            summary.binding = None;
            summary.last_binding_error = last_error;
            summary.agent_status = ChatAgentStatus::Idle;
            if attention_required {
                summary.attention_required = true;
                summary.updated_at = Timestamp::now();
            }
            let summary_result = self.apply_summary(summary).await;
            let checkpoint_result = self
                .layer
                .checkpoint_chat(&chat_id)
                .await
                .map_err(state_layer_error);
            summary_result.and(checkpoint_result.map(|_| ()))
        })
    }

    /// Checkpoints every materialized chat before graceful engine shutdown
    /// completes.
    pub fn checkpoint(&self) -> BoxFuture<'_, Result<(), ChatStateError>> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            self.layer.checkpoint().await.map_err(state_layer_error)
        })
    }

    async fn summary(&self, chat_id: &ChatId) -> Result<ChatSummary, ChatStateError> {
        self.layer
            .chat(chat_id)
            .await
            .map_err(state_layer_error)?
            .map(|snapshot| snapshot.summary)
            .ok_or_else(|| invalid_input("chat does not exist"))
    }

    async fn apply_summary(&self, summary: ChatSummary) -> Result<(), ChatStateError> {
        self.layer
            .apply_update(summary_update(summary))
            .await
            .map_err(state_layer_error)?;
        Ok(())
    }

    async fn touch_summary(
        &self,
        chat_id: &ChatId,
        occurred_at: Timestamp,
        attention_required: bool,
    ) -> Result<(), ChatStateError> {
        let snapshot = self
            .layer
            .chat(chat_id)
            .await
            .map_err(state_layer_error)?
            .ok_or_else(|| invalid_input("chat does not exist"))?;
        let status = agent_status(&snapshot);
        let mut summary = snapshot.summary;
        summary.agent_status = status;
        summary.attention_required |= attention_required;
        if occurred_at > summary.updated_at {
            summary.updated_at = occurred_at;
        }
        self.apply_summary(summary).await
    }
}

struct ReconciledEvent {
    update: Option<StateUpdate>,
    resume_cursor: Option<ResumeCursor>,
    touch_summary: bool,
    attention_required: bool,
}

const EVENT_CAPACITY: NonZeroUsize =
    NonZeroUsize::new(16_384).expect("chat event capacity is non-zero");

#[allow(clippy::too_many_lines)] // One exhaustive reducer keeps event invariants visible.
fn reconcile_harness_event(
    snapshot: &Chat,
    binding_id: &ChatBindingId,
    event: HarnessEvent,
) -> Result<ReconciledEvent, ChatStateError> {
    let HarnessEvent {
        occurred_at,
        turn_id,
        item_id,
        request_id,
        payload,
        ..
    } = event;
    let chat_id = snapshot.summary.chat_id.clone();
    let attention_required = snapshot.queued_prompts.is_empty()
        && matches!(
            &payload,
            HarnessEventPayload::TurnCompleted { state, .. } if *state != ChatTurnState::Running
        );
    let (payload, resume_cursor, touch_summary) = match payload {
        HarnessEventPayload::SessionStarted
        | HarnessEventPayload::SessionStateChanged { .. }
        | HarnessEventPayload::SessionExited { error: None } => (None, None, false),
        HarnessEventPayload::SessionExited { error: Some(error) }
        | HarnessEventPayload::Error(error) => (
            Some(activity_payload(
                binding_id,
                occurred_at,
                error_activity(error),
            )),
            None,
            true,
        ),
        HarnessEventPayload::ResumeCursorUpdated { resume_cursor } => {
            (None, Some(resume_cursor), false)
        }
        HarnessEventPayload::ModelChanged { model } => {
            let mut summary = snapshot.summary.clone();
            summary.model = Some(model);
            summary.updated_at = occurred_at;
            return Ok(ReconciledEvent {
                update: Some(summary_update(summary)),
                resume_cursor: None,
                touch_summary: false,
                attention_required: false,
            });
        }
        HarnessEventPayload::TurnStarted => {
            let turn_id = require_turn_id(turn_id)?;
            let turn = snapshot
                .turns
                .iter()
                .find(|turn| turn.turn_id == turn_id)
                .map_or_else(
                    || ChatTurn {
                        turn_id,
                        binding_id: binding_id.clone(),
                        state: ChatTurnState::Running,
                        started_at: Some(occurred_at),
                        completed_at: None,
                        error: None,
                        usage: None,
                    },
                    |turn| {
                        let mut turn = turn.clone();
                        turn.state = ChatTurnState::Running;
                        turn.started_at.get_or_insert(occurred_at);
                        turn
                    },
                );
            require_binding(&turn.binding_id, binding_id, "turn")?;
            (Some(ChatMutation::UpsertTurn(turn)), None, true)
        }
        HarnessEventPayload::TurnUsageUpdated { usage, state } => {
            let turn_id = require_turn_id(turn_id)?;
            let mut turn = snapshot
                .turns
                .iter()
                .find(|turn| turn.turn_id == turn_id)
                .cloned()
                .ok_or_else(|| invalid_event("turn usage requires an existing turn"))?;
            require_binding(&turn.binding_id, binding_id, "turn")?;

            let provisional_after_settlement = state == ChatUsageState::Provisional
                && (turn.state != ChatTurnState::Running
                    || turn
                        .usage
                        .as_ref()
                        .is_some_and(|usage| usage.state == ChatUsageState::Settled));
            if provisional_after_settlement {
                return Ok(ReconciledEvent {
                    update: None,
                    resume_cursor: None,
                    touch_summary: false,
                    attention_required: false,
                });
            }
            turn.usage = Some(ChatTurnUsage {
                state,
                observed_at: occurred_at,
                calculated_cost: cost::calculate_cost(&usage),
                snapshot: usage,
            });
            (Some(ChatMutation::UpsertTurn(turn)), None, false)
        }
        HarnessEventPayload::TurnCompleted { state, error } => {
            if state == ChatTurnState::Running {
                return Err(invalid_event("a completed turn must have a terminal state"));
            }
            let turn_id = require_turn_id(turn_id)?;
            let turn = snapshot
                .turns
                .iter()
                .find(|turn| turn.turn_id == turn_id)
                .map_or_else(
                    || ChatTurn {
                        turn_id,
                        binding_id: binding_id.clone(),
                        state,
                        started_at: None,
                        completed_at: Some(occurred_at),
                        error: error.clone(),
                        usage: None,
                    },
                    |turn| {
                        let mut turn = turn.clone();
                        turn.state = state;
                        turn.completed_at = Some(occurred_at);
                        turn.error.clone_from(&error);
                        if let Some(usage) = &mut turn.usage {
                            usage.state = ChatUsageState::Settled;
                        }
                        turn
                    },
                );
            require_binding(&turn.binding_id, binding_id, "turn")?;
            (Some(ChatMutation::UpsertTurn(turn)), None, true)
        }
        HarnessEventPayload::ItemStarted { kind } => {
            let item_id = require_item_id(item_id)?;
            let item = ChatItem {
                item_id,
                binding_id: binding_id.clone(),
                turn_id: require_turn_id(turn_id)?,
                kind,
                state: ChatItemState::Started,
                completed_at: None,
                content: vec![ChatContent::Text(TextContent { value: "".into() })].into(),
            };
            (
                Some(ChatMutation::UpsertTimelineEntry(ChatTimelineEntry::Item(
                    item,
                ))),
                None,
                false,
            )
        }
        HarnessEventPayload::ChatItemContentAppended(appended) => {
            if item_id.as_ref() != Some(&appended.item_id) {
                return Err(invalid_event(
                    "content append item identifier does not match its event",
                ));
            }
            (Some(ChatMutation::AppendItemContent(appended)), None, false)
        }
        HarnessEventPayload::ItemCompleted {
            kind,
            state,
            content,
        } => {
            if state == ChatItemState::Started {
                return Err(invalid_event("a completed item must have a terminal state"));
            }
            let item_id = require_item_id(item_id)?;
            let turn_id = require_turn_id(turn_id)?;
            let current = find_item(snapshot, &item_id);
            if let Some(current) = current {
                require_binding(&current.binding_id, binding_id, "item")?;
            }
            if state == ChatItemState::Completed
                && current.is_some_and(|item| {
                    item.state == ChatItemState::Started
                        && item.kind == kind
                        && item.content == content
                })
            {
                (
                    Some(ChatMutation::CompleteTimelineItem(ChatItemCompleted {
                        item_id,
                        completed_at: occurred_at,
                    })),
                    None,
                    true,
                )
            } else {
                let item = ChatItem {
                    item_id,
                    binding_id: binding_id.clone(),
                    turn_id,
                    kind,
                    state,
                    completed_at: Some(occurred_at),
                    content,
                };
                (
                    Some(ChatMutation::UpsertTimelineEntry(ChatTimelineEntry::Item(
                        item,
                    ))),
                    None,
                    true,
                )
            }
        }
        HarnessEventPayload::UserInputRequested { questions } => {
            let request = ChatRequest {
                request_id: require_request_id(request_id)?,
                binding_id: binding_id.clone(),
                turn_id,
                item_id,
                resolved: false,
                questions,
            };
            (
                Some(ChatMutation::UpsertTimelineEntry(
                    ChatTimelineEntry::Request(request),
                )),
                None,
                true,
            )
        }
        HarnessEventPayload::RequestResolved => {
            let request_id = require_request_id(request_id)?;
            let mut request = find_request(snapshot, &request_id)
                .cloned()
                .ok_or_else(|| invalid_event("cannot resolve an unknown chat request"))?;
            require_binding(&request.binding_id, binding_id, "request")?;
            request.resolved = true;
            (
                Some(ChatMutation::UpsertTimelineEntry(
                    ChatTimelineEntry::Request(request),
                )),
                None,
                true,
            )
        }
        HarnessEventPayload::Warning { code: _, message } => (
            Some(activity_payload(
                binding_id,
                occurred_at,
                (ChatActivityKind::Warning, message, None),
            )),
            None,
            true,
        ),
        HarnessEventPayload::Unknown { native_type, .. } => (
            Some(activity_payload(
                binding_id,
                occurred_at,
                (
                    ChatActivityKind::Information,
                    format!("Unrecognized harness event: {native_type}"),
                    None,
                ),
            )),
            None,
            true,
        ),
    };
    Ok(ReconciledEvent {
        update: payload.map(|payload| StateUpdate {
            list: None,
            chat: Some(StateChatUpdate { chat_id, payload }),
        }),
        resume_cursor,
        touch_summary,
        attention_required,
    })
}

fn summary_update(summary: ChatSummary) -> StateUpdate {
    StateUpdate {
        list: Some(ChatListMutation::Upsert(summary.clone())),
        chat: Some(StateChatUpdate {
            chat_id: summary.chat_id.clone(),
            payload: ChatMutation::UpdateSummary(summary),
        }),
    }
}

fn validate_usage_interval(from: Timestamp, until: Timestamp) -> Result<(), ChatStateError> {
    if from >= until {
        return Err(invalid_input(
            "usage report end must be later than its beginning",
        ));
    }
    Ok(())
}

fn validate_cost_center_id(
    cost_center_id: Option<&ChatCostCenterId>,
) -> Result<(), ChatStateError> {
    if cost_center_id
        .is_some_and(|cost_center_id| !is_valid_chat_cost_center_id(cost_center_id.as_str()))
    {
        return Err(invalid_input(
            "cost-center identifier must contain 1-64 ASCII letters, digits, hyphens, or underscores",
        ));
    }
    Ok(())
}

fn agent_status(snapshot: &Chat) -> ChatAgentStatus {
    let Some(binding_id) = snapshot
        .summary
        .binding
        .as_ref()
        .map(|binding| &binding.binding_id)
    else {
        return ChatAgentStatus::Idle;
    };

    if snapshot.timeline.iter().any(|entry| {
        matches!(
            entry,
            ChatTimelineEntry::Request(request)
                if &request.binding_id == binding_id && !request.resolved
        )
    }) {
        return ChatAgentStatus::UserInputRequired;
    }

    if snapshot
        .turns
        .iter()
        .any(|turn| &turn.binding_id == binding_id && turn.state == ChatTurnState::Running)
    {
        ChatAgentStatus::Working
    } else {
        ChatAgentStatus::Idle
    }
}

fn activity_payload(
    binding_id: &ChatBindingId,
    occurred_at: Timestamp,
    activity: (
        ChatActivityKind,
        String,
        Option<tascarrel_api::types::common::JsonValue>,
    ),
) -> ChatMutation {
    let (kind, message, detail) = activity;
    ChatMutation::UpsertTimelineEntry(ChatTimelineEntry::Activity(ChatActivity {
        activity_id: ChatActivityId::generate(),
        binding_id: binding_id.clone(),
        occurred_at,
        kind,
        message: message.into(),
        detail,
    }))
}

fn error_activity(
    error: HarnessError,
) -> (
    ChatActivityKind,
    String,
    Option<tascarrel_api::types::common::JsonValue>,
) {
    (ChatActivityKind::Error, error.message, None)
}

fn find_item<'a>(snapshot: &'a Chat, item_id: &ChatItemId) -> Option<&'a ChatItem> {
    snapshot.timeline.iter().find_map(|entry| match entry {
        ChatTimelineEntry::Item(item) if &item.item_id == item_id => Some(item),
        _ => None,
    })
}

fn find_request<'a>(snapshot: &'a Chat, request_id: &ChatRequestId) -> Option<&'a ChatRequest> {
    snapshot.timeline.iter().find_map(|entry| match entry {
        ChatTimelineEntry::Request(request) if &request.request_id == request_id => Some(request),
        _ => None,
    })
}

fn require_current_binding(
    snapshot: &Chat,
    binding_id: &ChatBindingId,
) -> Result<(), ChatStateError> {
    if snapshot
        .summary
        .binding
        .as_ref()
        .is_some_and(|binding| &binding.binding_id == binding_id)
    {
        Ok(())
    } else {
        Err(invalid_event("harness event belongs to a stale binding"))
    }
}

fn require_binding(
    actual: &ChatBindingId,
    expected: &ChatBindingId,
    kind: &'static str,
) -> Result<(), ChatStateError> {
    if actual == expected {
        Ok(())
    } else {
        Err(invalid_event(format!(
            "harness event attempted to replace a {kind} from another binding"
        )))
    }
}

fn require_turn_id(turn_id: Option<ChatTurnId>) -> Result<ChatTurnId, ChatStateError> {
    turn_id.ok_or_else(|| invalid_event("harness event requires a turn identifier"))
}

fn require_item_id(item_id: Option<ChatItemId>) -> Result<ChatItemId, ChatStateError> {
    item_id.ok_or_else(|| invalid_event("harness event requires an item identifier"))
}

fn require_request_id(request_id: Option<ChatRequestId>) -> Result<ChatRequestId, ChatStateError> {
    request_id.ok_or_else(|| invalid_event("harness event requires a request identifier"))
}

fn has_inflight_state(snapshot: &Chat, binding_id: &ChatBindingId) -> bool {
    snapshot
        .turns
        .iter()
        .any(|turn| &turn.binding_id == binding_id && turn.state == ChatTurnState::Running)
        || snapshot.timeline.iter().any(|entry| match entry {
            ChatTimelineEntry::Item(item) => {
                &item.binding_id == binding_id && item.state == ChatItemState::Started
            }
            ChatTimelineEntry::Request(request) => {
                &request.binding_id == binding_id && !request.resolved
            }
            ChatTimelineEntry::Activity(_) => false,
        })
}

#[allow(clippy::needless_pass_by_value)] // This signature is used directly with Result::map_err.
fn state_layer_error(error: Report<StateLayerError>) -> ChatStateError {
    let kind = match error.error() {
        StateLayerError::Reconciliation => ChatStateErrorKind::Internal,
        StateLayerError::Storage => ChatStateErrorKind::Storage,
    };
    ChatStateError {
        kind,
        message: error.to_string(),
    }
}

fn invalid_input(message: impl Into<String>) -> ChatStateError {
    ChatStateError {
        kind: ChatStateErrorKind::InvalidInput,
        message: message.into(),
    }
}

fn invalid_event(message: impl Into<String>) -> ChatStateError {
    ChatStateError {
        kind: ChatStateErrorKind::InvalidHarnessEvent,
        message: message.into(),
    }
}

fn runtime_stamp(
    stamp: &tascarrel_api::types::store::Stamp,
) -> Result<tascarrel_store::Stamp, ChatStateError> {
    let generation = stamp
        .generation
        .parse::<uuid::Uuid>()
        .map_err(|_| invalid_input("chat subscription cursor generation is invalid"))?;
    Ok(tascarrel_store::Stamp {
        generation,
        version: stamp.version,
    })
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use tascarrel_api::ArcVec;
    use tascarrel_api::ids::ChatBindingId;
    use tascarrel_api::ids::ChatId;
    use tascarrel_api::ids::ChatQueuedPromptId;
    use tascarrel_api::ids::ChatTurnId;
    use tascarrel_api::types::chats::ChatBinding;
    use tascarrel_api::types::chats::ChatBindingStatus;
    use tascarrel_api::types::chats::ChatHarnessKind;
    use tascarrel_api::types::chats::ChatPrompt;
    use tascarrel_api::types::chats::ChatQueuedPrompt;
    use tascarrel_api::types::chats::ChatTurnState;
    use tascarrel_api::types::pods::PodId;

    use super::ChatState;
    use crate::Database;
    use crate::services::chats::harness::protocol::HarnessEvent;
    use crate::services::chats::harness::protocol::HarnessEventPayload;
    use crate::services::chats::harness::protocol::ProviderEventReferences;
    use crate::services::chats::state::protocol::CreateChatRequest;
    use crate::services::chats::state::protocol::IngestHarnessEventRequest;

    /// Confirms that only a completed turn without a queued follow-up requires
    /// durable attention until the user acknowledges the chat.
    #[tokio::test]
    async fn completed_turn_without_follow_up_requires_attention_until_acknowledged() {
        let temporary = tempfile::tempdir().unwrap();
        let database = Database::open(temporary.path().join("state.sqlite3"))
            .await
            .unwrap();
        let state = ChatState::new(database.connection().clone()).await.unwrap();
        let chat_id = state
            .create_chat(CreateChatRequest {
                title: "Attention test".into(),
                pod_id: PodId::generate(),
                cost_center_id: None,
                harness: ChatHarnessKind::Codex,
                model: None,
                purpose: None,
            })
            .await
            .unwrap()
            .chat_id;
        let binding_id = ChatBindingId::generate();
        state
            .set_binding(
                chat_id.clone(),
                Some(ChatBinding {
                    binding_id: binding_id.clone(),
                    status: ChatBindingStatus::Attached,
                }),
                None,
            )
            .await
            .unwrap();
        state
            .set_prompt_queue(
                chat_id.clone(),
                vec![ChatQueuedPrompt {
                    queued_prompt_id: ChatQueuedPromptId::generate(),
                    prompt: ChatPrompt {
                        text: Some("Follow up".into()),
                        attachments: ArcVec::new(),
                        model: None,
                    },
                }]
                .into(),
            )
            .await
            .unwrap();

        ingest_completed_turn(&state, &chat_id, &binding_id).await;
        assert!(
            !state
                .chat(chat_id.clone())
                .await
                .unwrap()
                .unwrap()
                .summary
                .attention_required
        );
        state
            .set_prompt_queue(chat_id.clone(), ArcVec::new())
            .await
            .unwrap();
        ingest_completed_turn(&state, &chat_id, &binding_id).await;
        assert!(
            state
                .chat(chat_id.clone())
                .await
                .unwrap()
                .unwrap()
                .summary
                .attention_required
        );
        drop(state);

        let reopened = ChatState::new(database.connection().clone()).await.unwrap();
        assert!(
            reopened
                .chat(chat_id.clone())
                .await
                .unwrap()
                .unwrap()
                .summary
                .attention_required
        );
        reopened
            .acknowledge_attention(chat_id.clone())
            .await
            .unwrap();
        drop(reopened);

        let acknowledged = ChatState::new(database.connection().clone()).await.unwrap();
        assert!(
            !acknowledged
                .chat(chat_id)
                .await
                .unwrap()
                .unwrap()
                .summary
                .attention_required
        );
    }

    async fn ingest_completed_turn(
        state: &ChatState,
        chat_id: &ChatId,
        binding_id: &ChatBindingId,
    ) {
        let turn_id = ChatTurnId::generate();
        for payload in [
            HarnessEventPayload::TurnStarted,
            HarnessEventPayload::TurnCompleted {
                state: ChatTurnState::Completed,
                error: None,
            },
        ] {
            state
                .ingest_harness_event(IngestHarnessEventRequest {
                    chat_id: chat_id.clone(),
                    binding_id: binding_id.clone(),
                    event: HarnessEvent {
                        occurred_at: Timestamp::now(),
                        turn_id: Some(turn_id.clone()),
                        item_id: None,
                        request_id: None,
                        provider_references: ProviderEventReferences::default(),
                        payload,
                    },
                })
                .await
                .unwrap();
        }
    }
}
