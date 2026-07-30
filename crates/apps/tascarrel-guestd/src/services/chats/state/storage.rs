//! `SQLite` storage used by the durable state layer.

use jiff::Timestamp;
use reportify::Report;
use reportify::ResultExt as _;
use tascarrel_api::ids::ChatId;
use tascarrel_api::types::chats::ChatAgentStatus;
use tascarrel_api::types::chats::ChatCostCenterId;
use tascarrel_api::types::chats::ChatHarnessKind;
use tascarrel_api::types::chats::ChatModelSelection;
use tascarrel_api::types::chats::ChatPromptAttachment;
use tascarrel_api::types::chats::ChatSummary;
use tascarrel_api::types::chats::ChatTimelineEntry;
use tascarrel_api::types::chats::ChatTurn;
use tascarrel_api::types::chats::ChatUsageReport;
use tokio_rusqlite::Connection;
use tokio_rusqlite::rusqlite;
use tokio_rusqlite::rusqlite::OptionalExtension as _;

use crate::services::chats::harness::protocol::ResumeCursor;
use crate::services::chats::state::protocol::HarnessResumption;
use crate::services::chats::state::usage;
use crate::services::chats::state::usage::UsageRecord;

reportify::new_whatever_type! {
    /// Failure while accessing chat state in SQLite.
    pub StorageError
}

/// Serialized access to the state layer's `SQLite` database.
#[derive(Clone)]
pub struct Storage {
    database: Connection,
}

impl Storage {
    /// Opens chat storage on the migrated guest database connection.
    pub fn open(database: Connection) -> Self {
        Self { database }
    }

    /// Inserts a newly created durable chat.
    pub async fn create_chat(&self, summary: ChatSummary) -> Result<(), Report<StorageError>> {
        let diagnostic_chat_id = summary.chat_id.0.to_string();
        self.database
            .call_raw(move |database| -> Result<_, Report<StorageError>> {
                let harness = encode_json(&summary.harness, "unable to serialize chat harness")?;
                let model = encode_optional_json(
                    summary.model.as_ref(),
                    "unable to serialize initial chat model",
                )?;
                database
                    .execute(
                        "INSERT INTO chats (
                             chat_id, pod_id, title, harness_jsonb, model_jsonb,
                             resume_cursor_jsonb, archived, attention_required, cost_center_id,
                             created_at, updated_at
                         ) VALUES (
                             ?1, ?2, ?3, jsonb(?4), jsonb(?5), NULL, 0, ?6, ?7, ?8, ?9
                         )",
                        (
                            summary.chat_id.0.as_ref(),
                            summary.pod_id.0.as_ref(),
                            summary.title.as_ref(),
                            harness,
                            model,
                            summary.attention_required,
                            summary
                                .cost_center_id
                                .as_ref()
                                .map(ChatCostCenterId::as_str),
                            summary.created_at.to_string(),
                            summary.updated_at.to_string(),
                        ),
                    )
                    .whatever("unable to insert durable chat")?;
                Ok(())
            })
            .await
            .whatever("unable to access the SQLite connection")?
            .message("unable to create durable chat")
            .field("chat_id", diagnostic_chat_id)
    }

    /// Loads the lightweight chat summaries used to initialize the chat list.
    pub async fn load_chat_summaries(&self) -> Result<Vec<ChatSummary>, Report<StorageError>> {
        self.database
            .call_raw(|database| load_chat_summaries(database))
            .await
            .whatever("unable to access the SQLite connection")?
            .message("unable to load durable chat summaries")
    }

    /// Marks an active chat as archived.
    pub async fn archive_chat(&self, chat_id: &ChatId) -> Result<bool, Report<StorageError>> {
        let chat_id = chat_id.clone();
        let diagnostic_chat_id = chat_id.0.to_string();
        self.database
            .call_raw(move |database| -> Result<_, Report<StorageError>> {
                let changed = database
                    .execute(
                        "UPDATE chats SET archived = 1 WHERE chat_id = ?1 AND archived = 0",
                        [chat_id.0.as_ref()],
                    )
                    .whatever("unable to mark the chat as archived")?;
                Ok(changed == 1)
            })
            .await
            .whatever("unable to access the SQLite connection")?
            .message("unable to archive durable chat")
            .field("chat_id", diagnostic_chat_id)
    }

    /// Loads the durable turns and timeline entries for one chat.
    pub async fn load_chat(
        &self,
        chat_id: &ChatId,
    ) -> Result<Option<StoredChat>, Report<StorageError>> {
        let chat_id = chat_id.clone();
        let diagnostic_chat_id = chat_id.0.to_string();
        self.database
            .call_raw(
                move |database| -> Result<Option<StoredChat>, Report<StorageError>> {
                    let exists = database
                        .query_row(
                            "SELECT EXISTS (
                                 SELECT 1 FROM chats WHERE chat_id = ?1 AND archived = 0
                             )",
                            [chat_id.0.as_ref()],
                            |row| row.get::<_, bool>(0),
                        )
                        .whatever("unable to determine whether a chat exists")?;
                    if !exists {
                        return Ok(None);
                    }

                    let turns = load_turns(database, &chat_id)?;
                    let timeline = load_timeline(database, &chat_id)?;
                    let attachments = load_attachments(database, &chat_id)?;
                    Ok(Some(StoredChat {
                        turns,
                        timeline,
                        attachments,
                    }))
                },
            )
            .await
            .whatever("unable to access the SQLite connection")?
            .message("unable to load durable chat contents")
            .field("chat_id", diagnostic_chat_id)
    }

    /// Loads the harness-owned state needed to attach a chat.
    pub async fn load_resumption(
        &self,
        chat_id: &ChatId,
    ) -> Result<Option<HarnessResumption>, Report<StorageError>> {
        let chat_id = chat_id.clone();
        let diagnostic_chat_id = chat_id.0.to_string();
        self.database
            .call_raw(move |database| -> Result<_, Report<StorageError>> {
                database
                    .query_row(
                        "SELECT json(harness_jsonb), json(model_jsonb), json(resume_cursor_jsonb)
                         FROM chats
                         WHERE chat_id = ?1 AND archived = 0",
                        [chat_id.0.as_ref()],
                        |row| {
                            let harness = parse_json::<ChatHarnessKind>(row.get_ref(0)?.as_str()?)?;
                            let model = row
                                .get::<_, Option<String>>(1)?
                                .map(|value| parse_json::<ChatModelSelection>(&value))
                                .transpose()?;
                            let resume_cursor = row
                                .get::<_, Option<String>>(2)?
                                .map(|value| parse_json::<ResumeCursor>(&value))
                                .transpose()?;
                            Ok(HarnessResumption {
                                harness,
                                model,
                                resume_cursor,
                            })
                        },
                    )
                    .optional()
                    .whatever("unable to query harness resumption data")
            })
            .await
            .whatever("unable to access the SQLite connection")?
            .message("unable to load harness resumption data")
            .field("chat_id", diagnostic_chat_id)
    }

    /// Aggregates durable chat usage observed during one half-open interval.
    pub async fn usage_report(
        &self,
        from: Timestamp,
        until: Timestamp,
    ) -> Result<ChatUsageReport, Report<StorageError>> {
        self.database
            .call_raw(move |database| -> Result<_, Report<StorageError>> {
                let mut statement = database
                    .prepare(
                        "SELECT chats.chat_id, chats.cost_center_id, json(chat_turns.turn_jsonb)
                         FROM chat_turns
                         JOIN chats USING (chat_id)
                         ORDER BY chats.cost_center_id, chats.chat_id, chat_turns.turn_index",
                    )
                    .whatever("unable to prepare the durable chat-usage query")?;
                let rows = statement
                    .query_map([], |row| {
                        let chat_id = parse_id(row.get_ref(0)?.as_str()?)?;
                        let cost_center_id = row
                            .get::<_, Option<String>>(1)?
                            .map(|value| parse_id::<ChatCostCenterId>(&value))
                            .transpose()?;
                        let turn = parse_json::<ChatTurn>(row.get_ref(2)?.as_str()?)?;
                        Ok((chat_id, cost_center_id, turn))
                    })
                    .whatever("unable to query durable chat usage")?;
                let mut records = Vec::new();
                for row in rows {
                    let (chat_id, cost_center_id, turn) =
                        row.whatever("unable to decode durable chat usage")?;
                    let Some(usage) = turn.usage else {
                        continue;
                    };
                    if usage.observed_at < from || usage.observed_at >= until {
                        continue;
                    }
                    records.push(UsageRecord {
                        chat_id,
                        cost_center_id,
                        usage,
                    });
                }
                usage::build_report(from, until, records)
                    .whatever("unable to aggregate durable chat usage")
            })
            .await
            .whatever("unable to access the SQLite connection")?
            .message("unable to load durable chat usage")
    }

    /// Stores one compacted durable state change.
    pub async fn store_update(&self, update: DurableUpdate) -> Result<(), Report<StorageError>> {
        if update.is_empty() {
            return Ok(());
        }

        self.database
            .call_raw(move |database| -> Result<_, Report<StorageError>> {
                let transaction = database
                    .transaction()
                    .whatever("unable to begin compacted-state transaction")?;
                apply_durable_update(&transaction, update)?;
                transaction
                    .commit()
                    .whatever("unable to commit compacted chat state")
            })
            .await
            .whatever("unable to access the SQLite connection")?
            .message("unable to store compacted chat state")
    }

    /// Atomically checkpoints all supplied in-memory chats.
    pub async fn checkpoint(
        &self,
        checkpoints: Vec<StoredChatCheckpoint>,
    ) -> Result<(), Report<StorageError>> {
        if checkpoints.is_empty() {
            return Ok(());
        }
        self.database
            .call_raw(move |database| -> Result<_, Report<StorageError>> {
                let transaction = database
                    .transaction()
                    .whatever("unable to begin chat checkpoint transaction")?;
                for checkpoint in checkpoints {
                    apply_durable_update(&transaction, checkpoint.into_update())?;
                }
                transaction
                    .commit()
                    .whatever("unable to commit chat checkpoint")
            })
            .await
            .whatever("unable to access the SQLite connection")?
            .message("unable to checkpoint in-memory chat state")
    }
}

/// Compacted durable changes produced by one in-memory state update.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DurableUpdate {
    /// Chat receiving the durable change.
    pub chat_id: Option<ChatId>,
    /// Durable chat metadata to replace.
    pub summary: Option<ChatSummary>,
    /// Latest provider resume cursor to replace.
    pub resume_cursor: Option<ResumeCursor>,
    /// Turns to write to durable storage.
    pub turns: Vec<StoredTurn>,
    /// Timeline entries to write to durable storage.
    pub timeline: Vec<StoredTimelineEntry>,
    /// Prompt attachments to associate durably with the chat.
    pub attachments: Vec<StoredAttachment>,
}

impl DurableUpdate {
    fn is_empty(&self) -> bool {
        self.summary.is_none()
            && self.resume_cursor.is_none()
            && self.turns.is_empty()
            && self.timeline.is_empty()
            && self.attachments.is_empty()
    }
}

/// One turn together with its stable position in the chat.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredTurn {
    /// Chat containing the turn.
    pub chat_id: ChatId,
    /// Stable zero-based position in the turn list.
    pub turn_index: usize,
    /// Complete turn representation.
    pub turn: ChatTurn,
}

/// One timeline entry together with its stable position in the chat.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredTimelineEntry {
    /// Chat containing the entry.
    pub chat_id: ChatId,
    /// Stable zero-based position in the chat timeline.
    pub entry_index: usize,
    /// Complete timeline entry.
    pub entry: ChatTimelineEntry,
}

/// Durable contents for one lazily materialized chat.
pub struct StoredChat {
    /// Turns in chronological order.
    pub turns: Vec<StoredTurn>,
    /// Timeline entries with their stable positions.
    pub timeline: Vec<StoredTimelineEntry>,
    /// Prompt attachments ordered by their stable identifier.
    pub attachments: Vec<ChatPromptAttachment>,
}

/// One prompt attachment associated durably with a chat.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredAttachment {
    /// Chat containing the attachment.
    pub chat_id: ChatId,
    /// Complete attachment metadata.
    pub attachment: ChatPromptAttachment,
}

/// Complete durable representation of one materialized in-memory chat.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredChatCheckpoint {
    /// Durable chat metadata.
    pub summary: ChatSummary,
    /// Turns with stable positions.
    pub turns: Vec<StoredTurn>,
    /// Timeline entries with stable positions.
    pub timeline: Vec<StoredTimelineEntry>,
    /// Prompt attachments ordered by their stable identifier.
    pub attachments: Vec<StoredAttachment>,
}

impl StoredChatCheckpoint {
    fn into_update(self) -> DurableUpdate {
        DurableUpdate {
            chat_id: Some(self.summary.chat_id.clone()),
            summary: Some(self.summary),
            turns: self.turns,
            timeline: self.timeline,
            attachments: self.attachments,
            ..DurableUpdate::default()
        }
    }
}

fn load_chat_summaries(
    database: &rusqlite::Connection,
) -> Result<Vec<ChatSummary>, Report<StorageError>> {
    let mut statement = database
        .prepare(
            "SELECT chat_id, pod_id, title, json(harness_jsonb), json(model_jsonb),
                    attention_required, cost_center_id, created_at, updated_at
             FROM chats
             WHERE archived = 0
             ORDER BY updated_at DESC, chat_id",
        )
        .whatever("unable to prepare the durable chat-summary query")?;
    statement
        .query_map([], |row| {
            let chat_id = parse_id(row.get_ref(0)?.as_str()?)?;
            let pod_id = parse_id(row.get_ref(1)?.as_str()?)?;
            let title = row.get::<_, String>(2)?.into();
            let harness = parse_json::<ChatHarnessKind>(row.get_ref(3)?.as_str()?)?;
            let model = row
                .get::<_, Option<String>>(4)?
                .map(|value| parse_json::<ChatModelSelection>(&value))
                .transpose()?;
            let attention_required = row.get(5)?;
            let cost_center_id = row
                .get::<_, Option<String>>(6)?
                .map(|value| parse_id::<ChatCostCenterId>(&value))
                .transpose()?;
            let created_at = parse_timestamp(row.get_ref(7)?.as_str()?)?;
            let updated_at = parse_timestamp(row.get_ref(8)?.as_str()?)?;
            Ok(ChatSummary {
                chat_id,
                pod_id,
                binding: None,
                last_binding_error: None,
                agent_status: ChatAgentStatus::Idle,
                attention_required,
                harness,
                model,
                cost_center_id,
                title,
                created_at,
                updated_at,
            })
        })
        .whatever("unable to query durable chat summaries")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .whatever("unable to decode durable chat summaries")
}

fn load_turns(
    database: &rusqlite::Connection,
    chat_id: &ChatId,
) -> Result<Vec<StoredTurn>, Report<StorageError>> {
    let mut statement = database
        .prepare(
            "SELECT turn_index, json(turn_jsonb)
             FROM chat_turns
             WHERE chat_id = ?1
             ORDER BY turn_index",
        )
        .whatever("unable to prepare the durable chat-turn query")?;
    statement
        .query_map([chat_id.0.as_ref()], |row| {
            let turn_index = usize::try_from(row.get::<_, i64>(0)?)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, i64::MAX))?;
            let turn = parse_json::<ChatTurn>(row.get_ref(1)?.as_str()?)?;
            Ok(StoredTurn {
                chat_id: chat_id.clone(),
                turn_index,
                turn,
            })
        })
        .whatever("unable to query durable chat turns")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .whatever("unable to decode durable chat turns")
}

fn load_timeline(
    database: &rusqlite::Connection,
    chat_id: &ChatId,
) -> Result<Vec<StoredTimelineEntry>, Report<StorageError>> {
    let mut statement = database
        .prepare(
            "SELECT entry_index, json(entry_jsonb)
             FROM chat_timeline_entries
             WHERE chat_id = ?1
             ORDER BY entry_index",
        )
        .whatever("unable to prepare the durable chat-timeline query")?;
    statement
        .query_map([chat_id.0.as_ref()], |row| {
            let entry_index = usize::try_from(row.get::<_, i64>(0)?)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, i64::MAX))?;
            let entry = parse_json::<ChatTimelineEntry>(row.get_ref(1)?.as_str()?)?;
            Ok(StoredTimelineEntry {
                chat_id: chat_id.clone(),
                entry_index,
                entry,
            })
        })
        .whatever("unable to query the durable chat timeline")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .whatever("unable to decode the durable chat timeline")
}

fn load_attachments(
    database: &rusqlite::Connection,
    chat_id: &ChatId,
) -> Result<Vec<ChatPromptAttachment>, Report<StorageError>> {
    let mut statement = database
        .prepare(
            "SELECT json(attachment_jsonb)
             FROM chat_attachments
             WHERE chat_id = ?1
             ORDER BY attachment_id",
        )
        .whatever("unable to prepare the durable chat-attachment query")?;
    statement
        .query_map([chat_id.0.as_ref()], |row| {
            parse_json::<ChatPromptAttachment>(row.get_ref(0)?.as_str()?)
        })
        .whatever("unable to query durable chat attachments")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .whatever("unable to decode durable chat attachments")
}

fn timeline_entry_identity(entry: &ChatTimelineEntry) -> (&str, &'static str) {
    match entry {
        ChatTimelineEntry::Item(item) => (item.item_id.0.as_ref(), "item"),
        ChatTimelineEntry::Request(request) => (request.request_id.0.as_ref(), "request"),
        ChatTimelineEntry::Activity(activity) => (activity.activity_id.0.as_ref(), "activity"),
    }
}

fn apply_durable_update(
    transaction: &rusqlite::Transaction<'_>,
    update: DurableUpdate,
) -> Result<(), Report<StorageError>> {
    let chat_id = update
        .chat_id
        .clone()
        .or_else(|| {
            update
                .summary
                .as_ref()
                .map(|summary| summary.chat_id.clone())
        })
        .or_else(|| update.turns.first().map(|turn| turn.chat_id.clone()))
        .or_else(|| update.timeline.first().map(|entry| entry.chat_id.clone()))
        .or_else(|| {
            update
                .attachments
                .first()
                .map(|attachment| attachment.chat_id.clone())
        });
    if let Some(summary) = update.summary {
        store_summary(transaction, &summary)?;
    }
    if let Some(resume_cursor) = update.resume_cursor {
        let chat_id = chat_id
            .as_ref()
            .ok_or_else(|| Report::whatever("resume cursor update has no chat identifier"))?;
        store_resume_cursor(transaction, chat_id, &resume_cursor)?;
    }
    for turn in update.turns {
        store_turn(transaction, &turn)?;
    }
    for entry in update.timeline {
        store_timeline_entry(transaction, &entry)?;
    }
    for attachment in update.attachments {
        store_attachment(transaction, &attachment)?;
    }
    Ok(())
}

fn store_summary(
    transaction: &rusqlite::Transaction<'_>,
    summary: &ChatSummary,
) -> Result<(), Report<StorageError>> {
    let harness = encode_json(&summary.harness, "unable to serialize chat harness")?;
    let model = encode_optional_json(
        summary.model.as_ref(),
        "unable to serialize effective chat model",
    )?;
    transaction
        .execute(
            "UPDATE chats SET
                 pod_id = ?2,
                 title = ?3,
                 harness_jsonb = jsonb(?4),
                 model_jsonb = jsonb(?5),
                 attention_required = ?6,
                 cost_center_id = ?7,
                 updated_at = ?8
             WHERE chat_id = ?1",
            (
                summary.chat_id.0.as_ref(),
                summary.pod_id.0.as_ref(),
                summary.title.as_ref(),
                harness,
                model,
                summary.attention_required,
                summary
                    .cost_center_id
                    .as_ref()
                    .map(ChatCostCenterId::as_str),
                summary.updated_at.to_string(),
            ),
        )
        .whatever("unable to update durable chat metadata")
        .field("chat_id", summary.chat_id.0.to_string())?;
    Ok(())
}

fn store_resume_cursor(
    transaction: &rusqlite::Transaction<'_>,
    chat_id: &ChatId,
    resume_cursor: &ResumeCursor,
) -> Result<(), Report<StorageError>> {
    let resume_cursor = encode_json(resume_cursor, "unable to serialize harness resume cursor")?;
    transaction
        .execute(
            "UPDATE chats SET resume_cursor_jsonb = jsonb(?2) WHERE chat_id = ?1",
            (chat_id.0.as_ref(), resume_cursor),
        )
        .whatever("unable to update harness resume cursor")
        .field("chat_id", chat_id.0.to_string())?;
    Ok(())
}

fn store_turn(
    transaction: &rusqlite::Transaction<'_>,
    stored: &StoredTurn,
) -> Result<(), Report<StorageError>> {
    let turn_index = i64::try_from(stored.turn_index)
        .whatever("turn index does not fit in an SQLite INTEGER")?;
    let turn_json = encode_json(&stored.turn, "unable to serialize compacted chat turn")?;
    transaction
        .execute(
            "INSERT INTO chat_turns (chat_id, turn_id, turn_index, turn_jsonb)
             VALUES (?1, ?2, ?3, jsonb(?4))
             ON CONFLICT (chat_id, turn_id) DO UPDATE SET
                 turn_index = excluded.turn_index,
                 turn_jsonb = excluded.turn_jsonb",
            (
                stored.chat_id.0.as_ref(),
                stored.turn.turn_id.0.as_ref(),
                turn_index,
                turn_json,
            ),
        )
        .whatever("unable to upsert compacted chat turn")
        .field("chat_id", stored.chat_id.0.to_string())
        .field("turn_id", stored.turn.turn_id.0.to_string())?;
    Ok(())
}

fn store_timeline_entry(
    transaction: &rusqlite::Transaction<'_>,
    stored: &StoredTimelineEntry,
) -> Result<(), Report<StorageError>> {
    let entry_index = i64::try_from(stored.entry_index)
        .whatever("timeline entry index does not fit in an SQLite INTEGER")?;
    let (entry_id, entry_kind) = timeline_entry_identity(&stored.entry);
    let entry_id = entry_id.to_owned();
    let entry_json = encode_json(
        &stored.entry,
        "unable to serialize compacted chat timeline entry",
    )?;
    transaction
        .execute(
            "INSERT INTO chat_timeline_entries (
                 chat_id, entry_id, entry_index, entry_kind, entry_jsonb
             ) VALUES (?1, ?2, ?3, ?4, jsonb(?5))
             ON CONFLICT (chat_id, entry_id) DO UPDATE SET
                 entry_index = excluded.entry_index,
                 entry_kind = excluded.entry_kind,
                 entry_jsonb = excluded.entry_jsonb",
            (
                stored.chat_id.0.as_ref(),
                &entry_id,
                entry_index,
                entry_kind,
                entry_json,
            ),
        )
        .whatever("unable to upsert compacted chat timeline entry")
        .field("chat_id", stored.chat_id.0.to_string())
        .field("entry_id", entry_id)?;
    Ok(())
}

fn store_attachment(
    transaction: &rusqlite::Transaction<'_>,
    stored: &StoredAttachment,
) -> Result<(), Report<StorageError>> {
    let attachment_json = encode_json(
        &stored.attachment,
        "unable to serialize chat attachment metadata",
    )?;
    transaction
        .execute(
            "INSERT INTO chat_attachments (chat_id, attachment_id, attachment_jsonb)
             VALUES (?1, ?2, jsonb(?3))
             ON CONFLICT (chat_id, attachment_id) DO UPDATE SET
                 attachment_jsonb = excluded.attachment_jsonb",
            (
                stored.chat_id.0.as_ref(),
                stored.attachment.attachment_id.0.as_ref(),
                attachment_json,
            ),
        )
        .whatever("unable to upsert durable chat attachment")
        .field("chat_id", stored.chat_id.0.to_string())
        .field(
            "attachment_id",
            stored.attachment.attachment_id.0.to_string(),
        )?;
    Ok(())
}

fn encode_json<T>(value: &T, message: &'static str) -> Result<String, Report<StorageError>>
where
    T: serde::Serialize,
{
    serde_json::to_string(value).whatever(message)
}

fn encode_optional_json<T>(
    value: Option<&T>,
    message: &'static str,
) -> Result<Option<String>, Report<StorageError>>
where
    T: serde::Serialize,
{
    value.map(|value| encode_json(value, message)).transpose()
}

fn parse_id<T>(value: &str) -> rusqlite::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn parse_json<T>(value: &str) -> rusqlite::Result<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(value).map_err(json_error)
}

fn parse_timestamp(value: &str) -> rusqlite::Result<jiff::Timestamp> {
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn json_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use tascarrel_api::ArcVec;
    use tascarrel_api::ids::ChatBindingId;
    use tascarrel_api::ids::ChatId;
    use tascarrel_api::ids::ChatTurnId;
    use tascarrel_api::types::chats::ChatAgentStatus;
    use tascarrel_api::types::chats::ChatCalculatedCost;
    use tascarrel_api::types::chats::ChatCostCenterId;
    use tascarrel_api::types::chats::ChatHarnessKind;
    use tascarrel_api::types::chats::ChatSummary;
    use tascarrel_api::types::chats::ChatTokenUsage;
    use tascarrel_api::types::chats::ChatTurn;
    use tascarrel_api::types::chats::ChatTurnState;
    use tascarrel_api::types::chats::ChatTurnUsage;
    use tascarrel_api::types::chats::ChatUsageCoverage;
    use tascarrel_api::types::chats::ChatUsageSnapshot;
    use tascarrel_api::types::chats::ChatUsageState;
    use tascarrel_api::types::common::Money;
    use tascarrel_api::types::pods::PodId;

    use super::DurableUpdate;
    use super::Storage;
    use super::StoredTurn;
    use crate::Database;
    use crate::services::chats::harness::protocol::ResumeCursor;

    /// Confirms that durability round-trips provider-owned cursor fields
    /// without interpreting them.
    #[tokio::test]
    async fn resume_cursor_remains_opaque_in_storage() {
        let temporary = tempfile::tempdir().unwrap();
        let database = Database::open(temporary.path().join("state.sqlite3"))
            .await
            .unwrap();
        let storage = Storage::open(database.connection().clone());
        let chat_id = ChatId::generate();
        let now = Timestamp::now();
        storage
            .create_chat(ChatSummary {
                chat_id: chat_id.clone(),
                pod_id: PodId::generate(),
                binding: None,
                last_binding_error: None,
                agent_status: ChatAgentStatus::Idle,
                attention_required: false,
                harness: ChatHarnessKind::Codex,
                model: None,
                cost_center_id: None,
                title: "Opaque cursor".into(),
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        let cursor = ResumeCursor(
            serde_json::from_str(
                r#"{"provider":"future-harness","nested":[1,true,{"extension":"kept"}]}"#,
            )
            .unwrap(),
        );
        storage
            .store_update(DurableUpdate {
                chat_id: Some(chat_id.clone()),
                resume_cursor: Some(cursor.clone()),
                ..DurableUpdate::default()
            })
            .await
            .unwrap();

        let resumption = storage.load_resumption(&chat_id).await.unwrap().unwrap();
        assert_eq!(resumption.resume_cursor, Some(cursor));
    }

    /// Confirms that interval reports read attributed turn usage from the
    /// durable database and continue to include archived chats.
    #[tokio::test]
    async fn usage_report_includes_archived_chats() {
        let temporary = tempfile::tempdir().unwrap();
        let database = Database::open(temporary.path().join("state.sqlite3"))
            .await
            .unwrap();
        let storage = Storage::open(database.connection().clone());
        let chat_id = ChatId::generate();
        let observed_at = timestamp("2026-07-15T12:00:00Z");
        storage
            .create_chat(ChatSummary {
                chat_id: chat_id.clone(),
                pod_id: PodId::generate(),
                binding: None,
                last_binding_error: None,
                agent_status: ChatAgentStatus::Idle,
                attention_required: false,
                harness: ChatHarnessKind::Codex,
                model: None,
                cost_center_id: Some(ChatCostCenterId::new("client_alpha")),
                title: "Attributed usage".into(),
                created_at: observed_at,
                updated_at: observed_at,
            })
            .await
            .unwrap();
        storage
            .store_update(DurableUpdate {
                chat_id: Some(chat_id.clone()),
                turns: vec![StoredTurn {
                    chat_id: chat_id.clone(),
                    turn_index: 0,
                    turn: ChatTurn {
                        turn_id: ChatTurnId::generate(),
                        binding_id: ChatBindingId::generate(),
                        state: ChatTurnState::Completed,
                        started_at: Some(observed_at),
                        completed_at: Some(observed_at),
                        error: None,
                        usage: Some(ChatTurnUsage {
                            state: ChatUsageState::Settled,
                            observed_at,
                            snapshot: ChatUsageSnapshot {
                                coverage: ChatUsageCoverage::ExecutionTree,
                                tokens: ChatTokenUsage {
                                    input_tokens: 120,
                                    output_tokens: 30,
                                    cache_read_input_tokens: Some(20),
                                    cache_write_input_tokens: Some(0),
                                    cache_writes_by_ttl: ArcVec::new(),
                                    reasoning_output_tokens: Some(5),
                                },
                                models: ArcVec::new(),
                                provider_estimated_cost: None,
                            },
                            calculated_cost: Some(ChatCalculatedCost {
                                amount: Money {
                                    currency: "USD".into(),
                                    amount: 4,
                                },
                                pricing_catalog_version: "test".into(),
                            }),
                        }),
                    },
                }],
                ..DurableUpdate::default()
            })
            .await
            .unwrap();
        assert!(storage.archive_chat(&chat_id).await.unwrap());

        let report = storage
            .usage_report(
                timestamp("2026-07-01T00:00:00Z"),
                timestamp("2026-08-01T00:00:00Z"),
            )
            .await
            .unwrap();

        assert_eq!(report.total.tokens.input_tokens, 120);
        assert_eq!(report.total.turn_count, 1);
        assert_eq!(
            report.cost_centers[0]
                .cost_center_id
                .as_ref()
                .map(ChatCostCenterId::as_str),
            Some("client_alpha")
        );
    }

    fn timestamp(value: &str) -> Timestamp {
        value.parse().unwrap()
    }
}
