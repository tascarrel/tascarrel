//! Bounded wire projection for resumable chat-store subscriptions.
//!
//! [`ChatEventSource`] maps authoritative store snapshots to transactional
//! collection ranges and forwards later mutations without changing their
//! stamps.

use std::collections::VecDeque;

use async_trait::async_trait;
use reportify::ErrorExt as _;
use reportify::Report;
use serde::Serialize;
use tascarrel_api::ArcVec;
use tascarrel_api::types::chats as api;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::store as store_api;
use tascarrel_protocol::DEFAULT_MAX_FRAME_LEN;

use super::operation_error_details;
use super::operations::EventSource;
use super::operations::store_stamp;
use crate::services::chats::ChatStoreSubscription;

/// Projects one reducer-store subscription into bounded chat wire events.
pub(crate) struct ChatEventSource {
    subscription: ChatStoreSubscription,
    pending: VecDeque<api::ChatEvent>,
}

impl ChatEventSource {
    /// Wraps one authoritative chat-store subscription.
    pub(crate) fn new(subscription: ChatStoreSubscription) -> Self {
        Self {
            subscription,
            pending: VecDeque::new(),
        }
    }
}

#[async_trait]
impl EventSource for ChatEventSource {
    type Event = api::ChatEvent;

    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            let Some(change) = self.subscription.recv().await else {
                return Ok(None);
            };
            match change {
                tascarrel_store::StoreEvent::Snapshot(snapshot) => {
                    self.pending = bootstrap_events(snapshot)?;
                }
                tascarrel_store::StoreEvent::Mutation(mutation) => {
                    return checked_chat_event(api::ChatChange::Mutation(
                        api::StampedChatMutation {
                            stamp: store_stamp(mutation.stamp),
                            mutation: (*mutation.mutation).clone(),
                        },
                    ))
                    .map(Some);
                }
            }
        }
    }
}

/// Preferred encoded payload size for one bootstrap collection range.
const CHAT_BOOTSTRAP_CHUNK_TARGET_BYTES: usize = 256 * 1024;

/// Leaves room for the control-plane subscription envelope around an event.
const MAX_CHAT_EVENT_BYTES: usize = DEFAULT_MAX_FRAME_LEN - 64 * 1024;

/// Projects one complete store snapshot as a transactional bootstrap.
fn bootstrap_events(
    snapshot: tascarrel_store::Snapshot<api::Chat>,
) -> Result<VecDeque<api::ChatEvent>, Report<wire::OperationError>> {
    let stamp = store_stamp(snapshot.stamp);
    let chat = snapshot.value;
    let mut events = VecDeque::new();
    push_chat_event(
        &mut events,
        api::ChatChange::BootstrapStarted(api::ChatBootstrapStarted {
            stamp: stamp.clone(),
            summary: chat.summary.clone(),
            turn_count: collection_len(chat.turns.len())?,
            timeline_count: collection_len(chat.timeline.len())?,
            attachment_count: collection_len(chat.attachments.len())?,
            queued_prompt_count: collection_len(chat.queued_prompts.len())?,
        }),
    )?;
    push_bootstrap_ranges(&mut events, &stamp, &chat.turns, |stamp, offset, turns| {
        api::ChatChange::BootstrapTurns(api::ChatBootstrapTurns {
            stamp,
            offset,
            turns,
        })
    })?;
    push_bootstrap_ranges(
        &mut events,
        &stamp,
        &chat.timeline,
        |stamp, offset, entries| {
            api::ChatChange::BootstrapTimeline(api::ChatBootstrapTimeline {
                stamp,
                offset,
                entries,
            })
        },
    )?;
    push_bootstrap_ranges(
        &mut events,
        &stamp,
        &chat.attachments,
        |stamp, offset, attachments| {
            api::ChatChange::BootstrapAttachments(api::ChatBootstrapAttachments {
                stamp,
                offset,
                attachments,
            })
        },
    )?;
    push_bootstrap_ranges(
        &mut events,
        &stamp,
        &chat.queued_prompts,
        |stamp, offset, prompts| {
            api::ChatChange::BootstrapPromptQueue(api::ChatBootstrapPromptQueue {
                stamp,
                offset,
                prompts,
            })
        },
    )?;
    push_chat_event(
        &mut events,
        api::ChatChange::BootstrapCompleted(api::ChatBootstrapCompleted { stamp }),
    )?;
    Ok(events)
}

fn push_bootstrap_ranges<T, F>(
    events: &mut VecDeque<api::ChatEvent>,
    stamp: &store_api::Stamp,
    values: &[T],
    change: F,
) -> Result<(), Report<wire::OperationError>>
where
    T: Clone + Serialize,
    F: Fn(store_api::Stamp, u32, ArcVec<T>) -> api::ChatChange,
{
    for (offset, values) in bootstrap_ranges(values)? {
        push_chat_event(events, change(stamp.clone(), offset, values))?;
    }
    Ok(())
}

/// Partitions one collection by encoded byte size while preserving its order.
fn bootstrap_ranges<T>(values: &[T]) -> Result<Vec<(u32, ArcVec<T>)>, Report<wire::OperationError>>
where
    T: Clone + Serialize,
{
    let mut ranges = Vec::new();
    let mut range = Vec::new();
    let mut range_bytes = 0usize;
    let mut range_offset = 0usize;
    for (index, value) in values.iter().enumerate() {
        let value_bytes = serde_json::to_vec(value)
            .map_err(|error| chat_event_encoding_error(error, "measure chat bootstrap value"))?
            .len();
        if !range.is_empty()
            && range_bytes.saturating_add(value_bytes) > CHAT_BOOTSTRAP_CHUNK_TARGET_BYTES
        {
            ranges.push((
                collection_len(range_offset)?,
                std::mem::take(&mut range).into(),
            ));
            range_offset = index;
            range_bytes = 0;
        }
        range.push(value.clone());
        range_bytes = range_bytes.saturating_add(value_bytes);
    }
    if !range.is_empty() {
        ranges.push((collection_len(range_offset)?, range.into()));
    }
    Ok(ranges)
}

fn push_chat_event(
    events: &mut VecDeque<api::ChatEvent>,
    change: api::ChatChange,
) -> Result<(), Report<wire::OperationError>> {
    events.push_back(checked_chat_event(change)?);
    Ok(())
}

/// Rejects an event before the control-plane envelope could exceed its frame.
fn checked_chat_event(
    change: api::ChatChange,
) -> Result<api::ChatEvent, Report<wire::OperationError>> {
    let event = api::ChatEvent { change };
    let encoded = serde_json::to_vec(&event)
        .map_err(|error| chat_event_encoding_error(error, "encode chat subscription event"))?;
    if encoded.len() > MAX_CHAT_EVENT_BYTES {
        return Err(
            wire::OperationError::Internal(operation_error_details(format!(
                "chat subscription event is {} bytes, exceeding the {} byte safety limit",
                encoded.len(),
                MAX_CHAT_EVENT_BYTES,
            )))
            .report(),
        );
    }
    Ok(event)
}

fn collection_len(length: usize) -> Result<u32, Report<wire::OperationError>> {
    u32::try_from(length).map_err(|error| {
        error
            .report()
            .escalate(wire::OperationError::Internal(operation_error_details(
                "chat collection exceeds the supported bootstrap length",
            )))
    })
}

fn chat_event_encoding_error(
    error: serde_json::Error,
    operation: &'static str,
) -> Report<wire::OperationError> {
    error
        .report()
        .escalate(wire::OperationError::Internal(operation_error_details(
            format!("failed to {operation}"),
        )))
}
