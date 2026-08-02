//! Durable chat state, reducer stores, and subscription resumption.

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::MutexGuard;

use reportify::Report;
use reportify::ResultExt as _;
use tascarrel_api::ArcVec;
use tascarrel_api::ids::ChatActivityId;
use tascarrel_api::ids::ChatId;
use tascarrel_api::ids::ChatItemId;
use tascarrel_api::ids::ChatRequestId;
use tascarrel_api::types::chats::Chat;
use tascarrel_api::types::chats::ChatContent;
use tascarrel_api::types::chats::ChatItemState;
use tascarrel_api::types::chats::ChatList;
use tascarrel_api::types::chats::ChatListMutation;
use tascarrel_api::types::chats::ChatMutation;
use tascarrel_api::types::chats::ChatSummary;
use tascarrel_api::types::chats::ChatTimelineEntry;
use tascarrel_api::types::chats::ChatUsageReport;
use tascarrel_store::Stamp;
use tascarrel_store::Store;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::watch;

use crate::services::chats::harness::protocol::ResumeCursor;
use crate::services::chats::state::protocol::HarnessResumption;
use crate::services::chats::state::storage::DurableUpdate;
use crate::services::chats::state::storage::Storage;
use crate::services::chats::state::storage::StoredAttachment;
use crate::services::chats::state::storage::StoredChat;
use crate::services::chats::state::storage::StoredChatCheckpoint;
use crate::services::chats::state::storage::StoredTimelineEntry;
use crate::services::chats::state::storage::StoredTurn;
use crate::services::chats::state::usage::UsageReportSubscription;

type ChatListStore = Store<ChatList, ChatListMutation>;
type ChatStore = Store<Chat, ChatMutation>;

/// Resumable subscription to the workspace chat list.
pub type ChatListStoreSubscription = tascarrel_store::Subscription<ChatList, ChatListMutation>;

/// Resumable subscription to one chat.
pub type ChatStoreSubscription = tascarrel_store::Subscription<Chat, ChatMutation>;

/// Durable state layer backed by `SQLite` and reducer stores.
pub struct StateLayer {
    storage: Storage,
    list: ChatListStore,
    chats: Mutex<HashMap<ChatId, ChatSlot>>,
    update_lock: AsyncMutex<()>,
    history_limit: NonZeroUsize,
    usage_changes: watch::Sender<u64>,
}

impl StateLayer {
    /// Loads durable summaries and creates fresh reducer-store generations.
    pub async fn open(
        history_limit: NonZeroUsize,
        storage: Storage,
    ) -> Result<Self, Report<StateLayerError>> {
        let summaries = storage
            .load_chat_summaries()
            .await
            .escalate(StateLayerError::Storage)?;
        let mut chats = HashMap::with_capacity(summaries.len());
        for summary in &summaries {
            if chats
                .insert(
                    summary.chat_id.clone(),
                    ChatSlot {
                        summary: summary.clone(),
                        store: None,
                    },
                )
                .is_some()
            {
                return Err(reconciliation_error(
                    "durable chat summaries contain a duplicate chat",
                )
                .escalate(StateLayerError::Reconciliation));
            }
        }
        let list = Store::new(
            ChatList {
                chats: summaries.into(),
            },
            reduce_chat_list,
            history_limit,
        );
        let (usage_changes, _) = watch::channel(0);
        Ok(Self {
            storage,
            list,
            chats: Mutex::new(chats),
            update_lock: AsyncMutex::new(()),
            history_limit,
            usage_changes,
        })
    }

    /// Returns the current workspace chat list.
    pub fn chats(&self) -> ChatList {
        (*self.list.snapshot().value).clone()
    }

    /// Creates a durable chat and publishes it to the workspace list.
    pub async fn create_chat(&self, summary: ChatSummary) -> Result<(), Report<StateLayerError>> {
        let _guard = self.update_lock.lock().await;
        if lock(&self.chats).contains_key(&summary.chat_id) {
            return Err(reconciliation_error("cannot create a duplicate chat")
                .escalate(StateLayerError::Reconciliation));
        }
        self.storage
            .create_chat(summary.clone())
            .await
            .escalate(StateLayerError::Storage)?;
        let store = Store::new(empty_chat(summary.clone()), reduce_chat, self.history_limit);
        lock(&self.chats).insert(
            summary.chat_id.clone(),
            ChatSlot {
                summary: summary.clone(),
                store: Some(store),
            },
        );
        self.list.apply(ChatListMutation::Upsert(summary));
        Ok(())
    }

    /// Archives a durable chat and removes it from active reducer state.
    pub async fn archive_chat(&self, chat_id: &ChatId) -> Result<bool, Report<StateLayerError>> {
        let _guard = self.update_lock.lock().await;
        if !lock(&self.chats).contains_key(chat_id) {
            return Ok(false);
        }
        if !self
            .storage
            .archive_chat(chat_id)
            .await
            .escalate(StateLayerError::Storage)?
        {
            return Ok(false);
        }
        lock(&self.chats).remove(chat_id);
        self.list.apply(ChatListMutation::Remove(chat_id.clone()));
        Ok(true)
    }

    /// Returns the current complete state of one chat.
    pub async fn chat(&self, chat_id: &ChatId) -> Result<Option<Chat>, Report<StateLayerError>> {
        let _guard = self.update_lock.lock().await;
        if !self.materialize_chat_locked(chat_id).await? {
            return Ok(None);
        }
        Ok(self
            .chat_store(chat_id)
            .map(|store| (*store.snapshot().value).clone()))
    }

    /// Applies one validated state change and persists its durable projection.
    pub async fn apply_update(&self, update: StateUpdate) -> Result<(), Report<StateLayerError>> {
        let _guard = self.update_lock.lock().await;
        validate_update_shape(&update)?;

        if let Some(chat_update) = update.chat.as_ref() {
            if !self.materialize_chat_locked(&chat_update.chat_id).await? {
                return Err(reconciliation_error("cannot update an unknown chat")
                    .escalate(StateLayerError::Reconciliation));
            }
            let store = self.chat_store(&chat_update.chat_id).ok_or_else(|| {
                reconciliation_error("materialized chat has no reducer store")
                    .escalate(StateLayerError::Reconciliation)
            })?;
            let current_chat = store.snapshot().value;
            let mut next_chat = (*current_chat).clone();
            apply_chat_mutation(&mut next_chat, &chat_update.payload)
                .escalate(StateLayerError::Reconciliation)?;
            let durable = durable_update(
                &chat_update.chat_id,
                &chat_update.payload,
                &current_chat,
                &next_chat,
            )
            .escalate(StateLayerError::Reconciliation)?;
            let usage_changed = durable.turns.iter().any(|turn| turn.turn.usage.is_some())
                || matches!(
                    &chat_update.payload,
                    ChatMutation::UpdateSummary(summary)
                        if summary.cost_center_id != current_chat.summary.cost_center_id
                );
            self.storage
                .store_update(durable)
                .await
                .escalate(StateLayerError::Storage)?;
            store.apply(chat_update.payload.clone());
            if let ChatMutation::UpdateSummary(summary) = &chat_update.payload
                && let Some(slot) = lock(&self.chats).get_mut(&chat_update.chat_id)
            {
                slot.summary = summary.clone();
            }
            if usage_changed {
                self.usage_changes.send_modify(|revision| {
                    *revision = revision.wrapping_add(1);
                });
            }
        }

        if let Some(list_mutation) = update.list {
            self.list.apply(list_mutation);
        }
        Ok(())
    }

    /// Loads the provider-owned state needed to attach a harness session.
    pub async fn harness_resumption(
        &self,
        chat_id: &ChatId,
    ) -> Result<Option<HarnessResumption>, Report<StateLayerError>> {
        self.storage
            .load_resumption(chat_id)
            .await
            .escalate(StateLayerError::Storage)
    }

    /// Stores a newer provider resume cursor without publishing a mutation.
    pub async fn store_resume_cursor(
        &self,
        chat_id: ChatId,
        resume_cursor: ResumeCursor,
    ) -> Result<(), Report<StateLayerError>> {
        self.storage
            .store_update(DurableUpdate {
                chat_id: Some(chat_id),
                resume_cursor: Some(resume_cursor),
                ..DurableUpdate::default()
            })
            .await
            .escalate(StateLayerError::Storage)
    }

    /// Checkpoints all state for one materialized chat.
    pub async fn checkpoint_chat(&self, chat_id: &ChatId) -> Result<bool, Report<StateLayerError>> {
        let _guard = self.update_lock.lock().await;
        if !self.materialize_chat_locked(chat_id).await? {
            return Ok(false);
        }
        let store = self.chat_store(chat_id).ok_or_else(|| {
            reconciliation_error("materialized chat has no reducer store")
                .escalate(StateLayerError::Reconciliation)
        })?;
        self.storage
            .checkpoint(vec![checkpoint(&store.snapshot().value)])
            .await
            .escalate(StateLayerError::Storage)?;
        Ok(true)
    }

    /// Checkpoints every materialized chat.
    pub async fn checkpoint(&self) -> Result<(), Report<StateLayerError>> {
        let _guard = self.update_lock.lock().await;
        let stores = lock(&self.chats)
            .values()
            .filter_map(|slot| slot.store.clone())
            .collect::<Vec<_>>();
        let checkpoints = stores
            .iter()
            .map(|store| checkpoint(&store.snapshot().value))
            .collect();
        self.storage
            .checkpoint(checkpoints)
            .await
            .escalate(StateLayerError::Storage)
    }

    /// Subscribes to the workspace chat-list reducer store.
    pub fn subscribe_chats(&self, after: Option<Stamp>) -> ChatListStoreSubscription {
        self.list.subscribe(after)
    }

    /// Returns attributed durable usage for one half-open interval.
    pub async fn usage_report(
        &self,
        from: jiff::Timestamp,
        until: jiff::Timestamp,
    ) -> Result<ChatUsageReport, Report<StateLayerError>> {
        self.storage
            .usage_report(from, until)
            .await
            .escalate(StateLayerError::Storage)
    }

    /// Subscribes to attributed usage for one half-open interval.
    pub fn subscribe_usage_report(
        &self,
        from: jiff::Timestamp,
        until: jiff::Timestamp,
    ) -> UsageReportSubscription {
        UsageReportSubscription::new(
            self.storage.clone(),
            from,
            until,
            self.usage_changes.subscribe(),
        )
    }

    /// Subscribes to one chat's reducer store.
    pub async fn subscribe_chat(
        &self,
        chat_id: ChatId,
        after: Option<Stamp>,
    ) -> Result<Option<ChatStoreSubscription>, Report<StateLayerError>> {
        let _guard = self.update_lock.lock().await;
        if !self.materialize_chat_locked(&chat_id).await? {
            return Ok(None);
        }
        Ok(self
            .chat_store(&chat_id)
            .map(|store| store.subscribe(after)))
    }

    async fn materialize_chat_locked(
        &self,
        chat_id: &ChatId,
    ) -> Result<bool, Report<StateLayerError>> {
        let summary = {
            let chats = lock(&self.chats);
            let Some(slot) = chats.get(chat_id) else {
                return Ok(false);
            };
            if slot.store.is_some() {
                return Ok(true);
            }
            slot.summary.clone()
        };
        let stored = self
            .storage
            .load_chat(chat_id)
            .await
            .escalate(StateLayerError::Storage)?
            .ok_or_else(|| {
                reconciliation_error("a known chat is missing from durable storage")
                    .escalate(StateLayerError::Reconciliation)
            })?;
        let chat = materialize_chat(summary, stored)?;
        let store = Store::new(chat, reduce_chat, self.history_limit);
        let mut chats = lock(&self.chats);
        let Some(slot) = chats.get_mut(chat_id) else {
            return Ok(false);
        };
        slot.store = Some(store);
        Ok(true)
    }

    fn chat_store(&self, chat_id: &ChatId) -> Option<ChatStore> {
        lock(&self.chats)
            .get(chat_id)
            .and_then(|slot| slot.store.clone())
    }
}

/// One state change applied under the state layer's operation lock.
pub struct StateUpdate {
    /// Optional workspace chat-list mutation.
    pub list: Option<ChatListMutation>,
    /// Optional mutation for one materialized chat.
    pub chat: Option<StateChatUpdate>,
}

/// Chat-specific part of one state change.
pub struct StateChatUpdate {
    /// Chat receiving the mutation.
    pub chat_id: ChatId,
    /// Mutation applied to the chat reducer store.
    pub payload: ChatMutation,
}

/// Failure while opening or updating durable chat state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StateLayerError {
    /// In-memory reconciliation failed.
    Reconciliation,
    /// `SQLite` access failed.
    Storage,
}

impl fmt::Display for StateLayerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reconciliation => formatter.write_str("chat state reconciliation failed"),
            Self::Storage => formatter.write_str("chat state storage failed"),
        }
    }
}

impl std::error::Error for StateLayerError {}

reportify::new_whatever_type! {
    /// Invalid durable state or reducer mutation.
    pub ReconciliationError
}

struct ChatSlot {
    summary: ChatSummary,
    store: Option<ChatStore>,
}

fn empty_chat(summary: ChatSummary) -> Chat {
    Chat {
        summary,
        turns: ArcVec::new(),
        timeline: ArcVec::new(),
        attachments: ArcVec::new(),
        queued_prompts: ArcVec::new(),
    }
}

fn materialize_chat(
    summary: ChatSummary,
    stored: StoredChat,
) -> Result<Chat, Report<StateLayerError>> {
    for (expected, turn) in stored.turns.iter().enumerate() {
        if turn.turn_index != expected {
            return Err(
                reconciliation_error("durable chat turns are not contiguous")
                    .escalate(StateLayerError::Reconciliation),
            );
        }
    }
    for (expected, entry) in stored.timeline.iter().enumerate() {
        if entry.entry_index != expected {
            return Err(
                reconciliation_error("durable chat timeline is not contiguous")
                    .escalate(StateLayerError::Reconciliation),
            );
        }
    }
    Ok(Chat {
        summary,
        turns: stored.turns.into_iter().map(|stored| stored.turn).collect(),
        timeline: stored
            .timeline
            .into_iter()
            .map(|stored| stored.entry)
            .collect(),
        attachments: stored.attachments.into(),
        queued_prompts: ArcVec::new(),
    })
}

fn validate_update_shape(update: &StateUpdate) -> Result<(), Report<StateLayerError>> {
    match (&update.list, &update.chat) {
        (
            Some(ChatListMutation::Upsert(list_summary)),
            Some(StateChatUpdate {
                chat_id,
                payload: ChatMutation::UpdateSummary(chat_summary),
            }),
        ) if &list_summary.chat_id == chat_id && list_summary == chat_summary => Ok(()),
        (Some(ChatListMutation::Upsert(_)), _) => Err(reconciliation_error(
            "a chat-list upsert must include the same chat summary mutation",
        )
        .escalate(StateLayerError::Reconciliation)),
        (Some(ChatListMutation::Remove(_)), Some(_)) => Err(reconciliation_error(
            "a chat-list removal cannot include a chat mutation",
        )
        .escalate(StateLayerError::Reconciliation)),
        (
            None,
            Some(StateChatUpdate {
                payload: ChatMutation::UpdateSummary(_),
                ..
            }),
        ) => Err(
            reconciliation_error("a summary mutation must include the same chat-list upsert")
                .escalate(StateLayerError::Reconciliation),
        ),
        (Some(ChatListMutation::Remove(_)), None) | (None, Some(_)) => Ok(()),
        (None, None) => Err(
            reconciliation_error("a state update must contain a mutation")
                .escalate(StateLayerError::Reconciliation),
        ),
    }
}

/// Projects one reducer mutation into compacted durable state.
///
/// Every turn and timeline entry is persisted when it is first appended,
/// regardless of its lifecycle state. This keeps durable positional indexes
/// dense even when a later entry becomes terminal first. Later high-frequency
/// updates may remain in memory until the entry becomes terminal or the chat is
/// checkpointed.
fn durable_update(
    chat_id: &ChatId,
    mutation: &ChatMutation,
    current_chat: &Chat,
    next_chat: &Chat,
) -> Result<DurableUpdate, Report<ReconciliationError>> {
    let mut durable = DurableUpdate {
        chat_id: Some(chat_id.clone()),
        ..DurableUpdate::default()
    };
    match mutation {
        ChatMutation::UpdateSummary(summary) => durable.summary = Some(summary.clone()),
        ChatMutation::UpsertTurn(turn)
            if turn.state != tascarrel_api::types::chats::ChatTurnState::Running
                || !current_chat
                    .turns
                    .iter()
                    .any(|candidate| candidate.turn_id == turn.turn_id) =>
        {
            let turn_index = next_chat
                .turns
                .iter()
                .position(|candidate| candidate.turn_id == turn.turn_id)
                .ok_or_else(|| reconciliation_error("upserted turn is missing after reduction"))?;
            durable.turns.push(StoredTurn {
                chat_id: chat_id.clone(),
                turn_index,
                turn: turn.clone(),
            });
        }
        ChatMutation::UpsertTimelineEntry(entry)
            if timeline_entry_is_terminal(entry)
                || !current_chat.timeline.iter().any(|candidate| {
                    timeline_entry_key(candidate) == timeline_entry_key(entry)
                }) =>
        {
            let entry_index = next_chat
                .timeline
                .iter()
                .position(|candidate| timeline_entry_key(candidate) == timeline_entry_key(entry))
                .ok_or_else(|| {
                    reconciliation_error("upserted timeline entry is missing after reduction")
                })?;
            durable.timeline.push(StoredTimelineEntry {
                chat_id: chat_id.clone(),
                entry_index,
                entry: entry.clone(),
            });
        }
        ChatMutation::UpsertTurn(_)
        | ChatMutation::AppendItemContent(_)
        | ChatMutation::ReplacePromptQueue(_)
        | ChatMutation::UpsertTimelineEntry(_) => {}
        ChatMutation::CompleteTimelineItem(completion) => {
            let entry_index = next_chat
                .timeline
                .iter()
                .position(|entry| matches!(entry, ChatTimelineEntry::Item(item) if item.item_id == completion.item_id))
                .ok_or_else(|| reconciliation_error("completed item is missing after reduction"))?;
            durable.timeline.push(StoredTimelineEntry {
                chat_id: chat_id.clone(),
                entry_index,
                entry: next_chat.timeline[entry_index].clone(),
            });
        }
        ChatMutation::UpsertAttachment(attachment) => {
            durable.attachments.push(StoredAttachment {
                chat_id: chat_id.clone(),
                attachment: attachment.clone(),
            });
        }
    }
    Ok(durable)
}

fn checkpoint(chat: &Chat) -> StoredChatCheckpoint {
    let chat_id = chat.summary.chat_id.clone();
    StoredChatCheckpoint {
        summary: chat.summary.clone(),
        turns: chat
            .turns
            .iter()
            .cloned()
            .enumerate()
            .map(|(turn_index, turn)| StoredTurn {
                chat_id: chat_id.clone(),
                turn_index,
                turn,
            })
            .collect(),
        timeline: chat
            .timeline
            .iter()
            .cloned()
            .enumerate()
            .map(|(entry_index, entry)| StoredTimelineEntry {
                chat_id: chat_id.clone(),
                entry_index,
                entry,
            })
            .collect(),
        attachments: chat
            .attachments
            .iter()
            .cloned()
            .map(|attachment| StoredAttachment {
                chat_id: chat_id.clone(),
                attachment,
            })
            .collect(),
    }
}

fn reduce_chat_list(list: &mut ChatList, mutation: &ChatListMutation) {
    match mutation {
        ChatListMutation::Upsert(summary) => {
            if let Some(index) = list
                .chats
                .iter()
                .position(|existing| existing.chat_id == summary.chat_id)
            {
                list.chats[index] = summary.clone();
            } else {
                list.chats.push(summary.clone());
            }
            list.chats.sort_by(|left, right| {
                right
                    .updated_at
                    .cmp(&left.updated_at)
                    .then_with(|| left.chat_id.cmp(&right.chat_id))
            });
        }
        ChatListMutation::Remove(chat_id) => {
            if let Some(index) = list
                .chats
                .iter()
                .position(|summary| summary.chat_id == *chat_id)
            {
                list.chats.remove(index);
            }
        }
    }
}

fn reduce_chat(chat: &mut Chat, mutation: &ChatMutation) {
    if let Err(error) = apply_chat_mutation(chat, mutation) {
        tracing::error!(%error, "chat store rejected an internal mutation");
    }
}

fn apply_chat_mutation(
    chat: &mut Chat,
    mutation: &ChatMutation,
) -> Result<(), Report<ReconciliationError>> {
    match mutation {
        ChatMutation::UpdateSummary(summary) => {
            if summary.chat_id != chat.summary.chat_id {
                return Err(reconciliation_error(
                    "summary mutation changed the chat identifier",
                ));
            }
            chat.summary = summary.clone();
        }
        ChatMutation::UpsertTurn(turn) => {
            if let Some(index) = chat
                .turns
                .iter()
                .position(|existing| existing.turn_id == turn.turn_id)
            {
                chat.turns[index] = turn.clone();
            } else {
                chat.turns.push(turn.clone());
            }
        }
        ChatMutation::UpsertTimelineEntry(entry) => {
            let key = timeline_entry_key(entry);
            if let Some(index) = chat
                .timeline
                .iter()
                .position(|existing| timeline_entry_key(existing) == key)
            {
                validate_timeline_replacement(&chat.timeline[index], entry)?;
                chat.timeline[index] = entry.clone();
            } else {
                chat.timeline.push(entry.clone());
            }
        }
        ChatMutation::AppendItemContent(appended) => {
            let item = chat
                .timeline
                .iter_mut()
                .find_map(|entry| match entry {
                    ChatTimelineEntry::Item(item) if item.item_id == appended.item_id => Some(item),
                    _ => None,
                })
                .ok_or_else(|| reconciliation_error("cannot append content to an unknown item"))?;
            if item.state != ChatItemState::Started {
                return Err(reconciliation_error(
                    "cannot append content to a terminal item",
                ));
            }
            let index = text_content_index(&item.content).ok_or_else(|| {
                reconciliation_error("a streaming item must contain exactly one text value")
            })?;
            let ChatContent::Text(text) = &mut item.content[index] else {
                return Err(reconciliation_error("streaming text target changed kind"));
            };
            text.value.push_str(&appended.delta);
        }
        ChatMutation::CompleteTimelineItem(completion) => {
            let item = chat
                .timeline
                .iter_mut()
                .find_map(|entry| match entry {
                    ChatTimelineEntry::Item(item) if item.item_id == completion.item_id => {
                        Some(item)
                    }
                    _ => None,
                })
                .ok_or_else(|| reconciliation_error("cannot complete an unknown item"))?;
            if item.state != ChatItemState::Started {
                return Err(reconciliation_error("cannot complete a terminal item"));
            }
            item.state = ChatItemState::Completed;
            item.completed_at = Some(completion.completed_at);
        }
        ChatMutation::ReplacePromptQueue(queue) => chat.queued_prompts = queue.prompts.clone(),
        ChatMutation::UpsertAttachment(attachment) => {
            if let Some(index) = chat
                .attachments
                .iter()
                .position(|existing| existing.attachment_id == attachment.attachment_id)
            {
                chat.attachments[index] = attachment.clone();
            } else {
                chat.attachments.push(attachment.clone());
                chat.attachments
                    .sort_by(|left, right| left.attachment_id.cmp(&right.attachment_id));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum TimelineEntryKey {
    Item(ChatItemId),
    Request(ChatRequestId),
    Activity(ChatActivityId),
}

fn timeline_entry_key(entry: &ChatTimelineEntry) -> TimelineEntryKey {
    match entry {
        ChatTimelineEntry::Item(item) => TimelineEntryKey::Item(item.item_id.clone()),
        ChatTimelineEntry::Request(request) => {
            TimelineEntryKey::Request(request.request_id.clone())
        }
        ChatTimelineEntry::Activity(activity) => {
            TimelineEntryKey::Activity(activity.activity_id.clone())
        }
    }
}

fn timeline_entry_is_terminal(entry: &ChatTimelineEntry) -> bool {
    match entry {
        ChatTimelineEntry::Item(item) => item.state != ChatItemState::Started,
        ChatTimelineEntry::Request(_) | ChatTimelineEntry::Activity(_) => true,
    }
}

fn validate_timeline_replacement(
    current: &ChatTimelineEntry,
    replacement: &ChatTimelineEntry,
) -> Result<(), Report<ReconciliationError>> {
    match (current, replacement) {
        (ChatTimelineEntry::Item(current), ChatTimelineEntry::Item(replacement)) => {
            if current.state != ChatItemState::Started && replacement.state != current.state {
                Err(reconciliation_error(
                    "a terminal item cannot change its state",
                ))
            } else {
                Ok(())
            }
        }
        (ChatTimelineEntry::Request(current), ChatTimelineEntry::Request(replacement)) => {
            if current.resolved && !replacement.resolved {
                Err(reconciliation_error(
                    "a resolved request cannot become unresolved",
                ))
            } else {
                Ok(())
            }
        }
        (ChatTimelineEntry::Activity(_), ChatTimelineEntry::Activity(_)) => Ok(()),
        _ => Err(reconciliation_error("timeline entry changed its kind")),
    }
}

fn text_content_index(content: &[ChatContent]) -> Option<usize> {
    let mut indices = content
        .iter()
        .enumerate()
        .filter_map(|(index, content)| matches!(content, ChatContent::Text(_)).then_some(index));
    let index = indices.next()?;
    indices.next().is_none().then_some(index)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[track_caller]
fn reconciliation_error(message: &'static str) -> Report<ReconciliationError> {
    Report::whatever(message)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use jiff::Timestamp;
    use tascarrel_api::ids::ChatActivityId;
    use tascarrel_api::ids::ChatBindingId;
    use tascarrel_api::ids::ChatId;
    use tascarrel_api::ids::ChatItemId;
    use tascarrel_api::ids::ChatTurnId;
    use tascarrel_api::types::chats::AutomationChatPurpose;
    use tascarrel_api::types::chats::ChatActivity;
    use tascarrel_api::types::chats::ChatActivityKind;
    use tascarrel_api::types::chats::ChatAgentStatus;
    use tascarrel_api::types::chats::ChatContent;
    use tascarrel_api::types::chats::ChatHarnessKind;
    use tascarrel_api::types::chats::ChatItem;
    use tascarrel_api::types::chats::ChatItemKind;
    use tascarrel_api::types::chats::ChatItemState;
    use tascarrel_api::types::chats::ChatMutation;
    use tascarrel_api::types::chats::ChatPurpose;
    use tascarrel_api::types::chats::ChatSummary;
    use tascarrel_api::types::chats::ChatTimelineEntry;
    use tascarrel_api::types::chats::ChatTurn;
    use tascarrel_api::types::chats::ChatTurnState;
    use tascarrel_api::types::chats::TextContent;
    use tascarrel_api::types::pods::PodId;

    use super::StateChatUpdate;
    use super::StateLayer;
    use super::StateUpdate;
    use super::Storage;
    use crate::Database;

    /// Confirms that newly created in-flight positions remain loadable after
    /// later durable turns and timeline entries are appended.
    #[tokio::test]
    async fn newly_created_positions_survive_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let database = Database::open(temporary.path().join("state.sqlite3"))
            .await
            .unwrap();
        let history_limit = NonZeroUsize::new(16).unwrap();
        let state = StateLayer::open(history_limit, Storage::open(database.connection().clone()))
            .await
            .unwrap();
        let now = Timestamp::now();
        let chat_id = ChatId::generate();
        state
            .create_chat(automation_chat_summary(chat_id.clone(), now))
            .await
            .unwrap();

        let binding_id = ChatBindingId::generate();
        let running_turn_id = ChatTurnId::generate();
        let completed_turn_id = ChatTurnId::generate();
        let started_item_id = ChatItemId::generate();
        let activity_id = ChatActivityId::generate();
        let mutations = [
            ChatMutation::UpsertTurn(ChatTurn {
                turn_id: running_turn_id.clone(),
                binding_id: binding_id.clone(),
                state: ChatTurnState::Running,
                started_at: Some(now),
                completed_at: None,
                error: None,
                usage: None,
            }),
            ChatMutation::UpsertTurn(ChatTurn {
                turn_id: completed_turn_id.clone(),
                binding_id: binding_id.clone(),
                state: ChatTurnState::Completed,
                started_at: None,
                completed_at: Some(now),
                error: None,
                usage: None,
            }),
            ChatMutation::UpsertTimelineEntry(ChatTimelineEntry::Item(ChatItem {
                item_id: started_item_id.clone(),
                binding_id: binding_id.clone(),
                turn_id: running_turn_id.clone(),
                kind: ChatItemKind::AssistantMessage,
                state: ChatItemState::Started,
                completed_at: None,
                content: vec![ChatContent::Text(TextContent { value: "".into() })].into(),
            })),
            ChatMutation::UpsertTimelineEntry(ChatTimelineEntry::Activity(ChatActivity {
                activity_id: activity_id.clone(),
                binding_id,
                occurred_at: now,
                kind: ChatActivityKind::Warning,
                message: "Later durable entry".into(),
                detail: None,
            })),
        ];
        for payload in mutations {
            state
                .apply_update(StateUpdate {
                    list: None,
                    chat: Some(StateChatUpdate {
                        chat_id: chat_id.clone(),
                        payload,
                    }),
                })
                .await
                .unwrap();
        }

        drop(state);
        let reopened =
            StateLayer::open(history_limit, Storage::open(database.connection().clone()))
                .await
                .unwrap();
        let chat = reopened.chat(&chat_id).await.unwrap().unwrap();

        assert!(matches!(
            &chat.summary.purpose,
            Some(ChatPurpose::Automation(purpose))
                if purpose.execution_id.as_ref() == "automation_execution_test"
        ));
        assert_eq!(chat.turns.len(), 2);
        assert_eq!(chat.turns[0].turn_id, running_turn_id);
        assert_eq!(chat.turns[1].turn_id, completed_turn_id);
        assert_eq!(chat.timeline.len(), 2);
        assert!(matches!(
            &chat.timeline[0],
            ChatTimelineEntry::Item(item) if item.item_id == started_item_id
        ));
        assert!(matches!(
            &chat.timeline[1],
            ChatTimelineEntry::Activity(activity) if activity.activity_id == activity_id
        ));
    }

    fn automation_chat_summary(chat_id: ChatId, now: Timestamp) -> ChatSummary {
        ChatSummary {
            chat_id,
            pod_id: PodId::generate(),
            binding: None,
            last_binding_error: None,
            agent_status: ChatAgentStatus::Idle,
            attention_required: false,
            harness: ChatHarnessKind::Codex,
            model: None,
            context_usage: None,
            cost_center_id: None,
            purpose: Some(ChatPurpose::Automation(AutomationChatPurpose {
                execution_id: "automation_execution_test".into(),
            })),
            title: "Durability test".into(),
            created_at: now,
            updated_at: now,
        }
    }
}
