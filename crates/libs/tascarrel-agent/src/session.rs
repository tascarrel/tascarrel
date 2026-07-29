//! Append-only native session state and effective model-context rebuilding.
//!
//! [`AgentSession`] retains every provider-neutral message and compaction
//! record. Compaction changes only how the next model request is projected:
//! the original system prompt, the newest summary checkpoint, and a verbatim
//! suffix are selected from the complete log.

use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use crate::ModelMessage;
use crate::ModelUsage;

/// Complete append-only state for one native agent session.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentSession {
    entries: Vec<SessionEntry>,
}

impl AgentSession {
    /// Creates an empty session.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Restores a session from append-only entries.
    ///
    /// # Errors
    ///
    /// Returns an error when entry identifiers, message ordering, or
    /// compaction boundaries violate session invariants.
    pub fn from_entries(entries: Vec<SessionEntry>) -> SessionResult<Self> {
        let session = Self { entries };
        session.validate()?;
        Ok(session)
    }

    /// Returns every durable entry in append order.
    #[must_use]
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Consumes the session into append-only entries suitable for durable
    /// storage.
    #[must_use]
    pub fn into_entries(self) -> Vec<SessionEntry> {
        self.entries
    }

    /// Returns every original conversation message without compaction
    /// projection.
    pub fn messages(&self) -> impl Iterator<Item = &ModelMessage> {
        self.entries.iter().filter_map(SessionEntry::message)
    }

    /// Rebuilds the context to send on the next model request.
    ///
    /// # Errors
    ///
    /// Returns an error when deserialized session data violates ordering,
    /// system-message, or compaction-reference invariants.
    pub fn effective_messages(&self) -> SessionResult<Vec<ModelMessage>> {
        self.validate()?;
        let Some(system) = self.entries.first().and_then(SessionEntry::message) else {
            return Ok(Vec::new());
        };
        let mut messages = vec![system.clone()];
        let Some((compaction_index, compaction)) = self.latest_compaction() else {
            messages.extend(
                self.entries
                    .iter()
                    .skip(1)
                    .filter_map(SessionEntry::message)
                    .cloned(),
            );
            return Ok(messages);
        };
        messages.push(ModelMessage::ContextSummary {
            content: compaction.summary.clone(),
        });
        let first_kept_index =
            self.entry_index(compaction.first_kept_entry_id)
                .ok_or(Report::new(SessionError::MissingCompactionBoundary {
                    id: compaction.first_kept_entry_id,
                }))?;
        messages.extend(
            self.entries[first_kept_index..]
                .iter()
                .enumerate()
                .filter(|(offset, _)| first_kept_index + offset != compaction_index)
                .filter_map(|(_, entry)| entry.message())
                .filter(|message| !matches!(message, ModelMessage::System { .. }))
                .cloned(),
        );
        Ok(messages)
    }

    /// Returns the newest compaction record, when present.
    #[must_use]
    pub fn compaction(&self) -> Option<&CompactionRecord> {
        self.latest_compaction().map(|(_, record)| record)
    }

    pub(crate) fn append_message(
        &mut self,
        message: ModelMessage,
    ) -> SessionResult<SessionEntryId> {
        if self.entries.is_empty() {
            if !matches!(message, ModelMessage::System { .. }) {
                return Err(Report::new(SessionError::MissingSystemMessage));
            }
        } else if matches!(message, ModelMessage::System { .. }) {
            return Err(Report::new(SessionError::UnexpectedSystemMessage));
        }
        Ok(self.append(SessionEntryValue::Message { message }))
    }

    pub(crate) fn append_compaction(
        &mut self,
        record: CompactionRecord,
    ) -> SessionResult<SessionEntryId> {
        let boundary_index = self
            .entry_index(record.first_kept_entry_id)
            .ok_or(Report::new(SessionError::MissingCompactionBoundary {
                id: record.first_kept_entry_id,
            }))?;
        if boundary_index == 0
            || !matches!(
                self.entries[boundary_index].value,
                SessionEntryValue::Message { .. }
            )
        {
            return Err(Report::new(SessionError::InvalidCompactionBoundary {
                id: record.first_kept_entry_id,
            }));
        }
        Ok(self.append(SessionEntryValue::Compaction(record)))
    }

    pub(crate) fn latest_compaction(&self) -> Option<(usize, &CompactionRecord)> {
        self.entries
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, entry)| entry.compaction().map(|record| (index, record)))
    }

    pub(crate) fn entry_index(&self, id: SessionEntryId) -> Option<usize> {
        self.entries
            .binary_search_by_key(&id, |entry| entry.id)
            .ok()
    }

    fn append(&mut self, value: SessionEntryValue) -> SessionEntryId {
        let id = self.entries.last().map_or(SessionEntryId(0), |entry| {
            SessionEntryId(entry.id.0.saturating_add(1))
        });
        self.entries.push(SessionEntry { id, value });
        id
    }

    fn validate(&self) -> SessionResult<()> {
        let Some(first) = self.entries.first() else {
            return Ok(());
        };
        if !matches!(
            first.value,
            SessionEntryValue::Message {
                message: ModelMessage::System { .. }
            }
        ) {
            return Err(Report::new(SessionError::MissingSystemMessage));
        }
        for (index, entry) in self.entries.iter().enumerate() {
            let expected = u64::try_from(index).unwrap_or(u64::MAX);
            if entry.id.0 != expected {
                return Err(Report::new(SessionError::NonContiguousEntryId {
                    expected,
                    actual: entry.id.0,
                }));
            }
            if index > 0
                && matches!(
                    entry.value,
                    SessionEntryValue::Message {
                        message: ModelMessage::System { .. }
                    }
                )
            {
                return Err(Report::new(SessionError::UnexpectedSystemMessage));
            }
            if let Some(record) = entry.compaction() {
                let Some(boundary) = self.entry_index(record.first_kept_entry_id) else {
                    return Err(Report::new(SessionError::MissingCompactionBoundary {
                        id: record.first_kept_entry_id,
                    }));
                };
                if boundary == 0 || boundary >= index || self.entries[boundary].message().is_none()
                {
                    return Err(Report::new(SessionError::InvalidCompactionBoundary {
                        id: record.first_kept_entry_id,
                    }));
                }
            }
        }
        Ok(())
    }
}

/// Stable position of one entry in a native session log.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SessionEntryId(pub u64);

/// One append-only native session record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionEntry {
    /// Stable entry position.
    pub id: SessionEntryId,
    /// Durable entry contents.
    #[serde(flatten)]
    pub value: SessionEntryValue,
}

impl SessionEntry {
    /// Returns the original message represented by this entry.
    #[must_use]
    pub const fn message(&self) -> Option<&ModelMessage> {
        match &self.value {
            SessionEntryValue::Message { message } => Some(message),
            SessionEntryValue::Compaction(_) => None,
        }
    }

    /// Returns the compaction record represented by this entry.
    #[must_use]
    pub const fn compaction(&self) -> Option<&CompactionRecord> {
        match &self.value {
            SessionEntryValue::Message { .. } => None,
            SessionEntryValue::Compaction(record) => Some(record),
        }
    }
}

/// Durable contents of one native session entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEntryValue {
    /// One original conversation message.
    Message {
        /// Provider-neutral message.
        message: ModelMessage,
    },
    /// A summary checkpoint that changes subsequent context projection.
    Compaction(CompactionRecord),
}

/// Durable checkpoint produced by one successful compaction.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompactionRecord {
    /// Structured context summary.
    pub summary: String,
    /// First original entry retained verbatim after the summary.
    pub first_kept_entry_id: SessionEntryId,
    /// Estimated or reported context size before compaction.
    pub tokens_before: u64,
    /// Estimated context size after compaction.
    pub estimated_tokens_after: u64,
    /// Usage of model calls that generated the summary.
    pub usage: ModelUsage,
    /// Files only read in summarized history.
    pub read_files: Vec<String>,
    /// Files modified in summarized history.
    pub modified_files: Vec<String>,
}

/// Result of validating or rebuilding a native session.
pub type SessionResult<T> = Result<T, Report<SessionError>>;

/// Invalid native session data.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SessionError {
    /// The original system prompt is absent.
    #[error("session does not start with a system message")]
    MissingSystemMessage,
    /// A later system prompt would make projection ambiguous.
    #[error("session contains a system message after its first entry")]
    UnexpectedSystemMessage,
    /// Entry positions are not append-only and contiguous.
    #[error("session entry identifier is not contiguous: expected {expected}, found {actual}")]
    NonContiguousEntryId {
        /// Expected entry position.
        expected: u64,
        /// Observed entry position.
        actual: u64,
    },
    /// A compaction record refers to an absent entry.
    #[error("compaction boundary entry {id:?} does not exist")]
    MissingCompactionBoundary {
        /// Missing boundary.
        id: SessionEntryId,
    },
    /// A compaction boundary is not an earlier conversation message.
    #[error("compaction boundary entry {id:?} is invalid")]
    InvalidCompactionBoundary {
        /// Invalid boundary.
        id: SessionEntryId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AssistantMessage;

    /// Verifies repeated compactions keep the full original log while only
    /// the latest checkpoint and suffix reach the model.
    #[test]
    fn repeated_compactions_rebuild_context_without_deleting_messages() {
        let mut session = AgentSession::new();
        append(&mut session, system("system"));
        append(&mut session, user("first"));
        append(&mut session, assistant("first answer"));
        let second_user = session.append_message(user("second")).unwrap();
        append(&mut session, assistant("second answer"));
        session
            .append_compaction(record("checkpoint one", second_user))
            .unwrap();
        let third_user = session.append_message(user("third")).unwrap();
        append(&mut session, assistant("third answer"));
        session
            .append_compaction(record("checkpoint two", third_user))
            .unwrap();

        assert_eq!(session.messages().count(), 7);
        assert_eq!(
            session.effective_messages().unwrap(),
            vec![
                system("system"),
                ModelMessage::ContextSummary {
                    content: "checkpoint two".into(),
                },
                user("third"),
                assistant("third answer"),
            ]
        );

        let encoded = serde_json::to_string(&session).unwrap();
        let decoded: AgentSession = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, session);
        assert_eq!(
            AgentSession::from_entries(decoded.clone().into_entries()).unwrap(),
            decoded
        );
    }

    /// Verifies restored session data cannot point a suffix at another
    /// compaction record.
    #[test]
    fn compaction_boundaries_must_reference_original_messages() {
        let mut session = AgentSession::new();
        append(&mut session, system("system"));
        append(&mut session, user("first"));
        let second_user = session.append_message(user("second")).unwrap();
        let first_compaction = session
            .append_compaction(record("checkpoint one", second_user))
            .unwrap();
        let third_user = session.append_message(user("third")).unwrap();
        session
            .append_compaction(record("checkpoint two", third_user))
            .unwrap();
        let Some(SessionEntry {
            value: SessionEntryValue::Compaction(record),
            ..
        }) = session.entries.last_mut()
        else {
            panic!("latest entry should be a compaction");
        };
        record.first_kept_entry_id = first_compaction;

        let error = session.effective_messages().unwrap_err();
        assert!(matches!(
            error.error(),
            SessionError::InvalidCompactionBoundary { id } if *id == first_compaction
        ));
    }

    fn append(session: &mut AgentSession, message: ModelMessage) {
        session.append_message(message).unwrap();
    }

    fn system(content: &str) -> ModelMessage {
        ModelMessage::System {
            content: content.into(),
        }
    }

    fn user(content: &str) -> ModelMessage {
        ModelMessage::User {
            content: content.into(),
        }
    }

    fn assistant(content: &str) -> ModelMessage {
        ModelMessage::Assistant(AssistantMessage {
            reasoning: String::new(),
            content: content.into(),
            tool_calls: Vec::new(),
            usage: None,
        })
    }

    fn record(summary: &str, boundary: SessionEntryId) -> CompactionRecord {
        CompactionRecord {
            summary: summary.into(),
            first_kept_entry_id: boundary,
            tokens_before: 100,
            estimated_tokens_after: 20,
            usage: ModelUsage::default(),
            read_files: Vec::new(),
            modified_files: Vec::new(),
        }
    }
}
