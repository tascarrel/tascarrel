//! Durable append-only storage for native agent sessions.
//!
//! [`AgentSessionStore`] owns a versioned JSONL journal. The first record
//! identifies the session and working directory. Later entry records retain
//! every original [`SessionEntry`](crate::SessionEntry), and commit records
//! make each successful operation atomic during crash recovery. Opening a
//! journal rebuilds and validates the complete [`AgentSession`].

use std::io;
use std::path::Path;
use std::path::PathBuf;

use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tokio::fs::File;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt as _;

use crate::AgentSession;
use crate::SessionEntry;
use crate::SessionEntryId;

const CURRENT_SESSION_FORMAT_VERSION: u32 = 1;
const MAX_SESSION_ID_BYTES: usize = 128;

/// Append-only JSONL journal for one native agent session.
#[derive(Debug)]
pub struct AgentSessionStore {
    file: File,
    path: PathBuf,
    persisted_entries: usize,
}

impl AgentSessionStore {
    /// Creates and durably initializes a new journal.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is unsafe, the journal already
    /// exists, or its directory or file cannot be initialized.
    #[tracing::instrument(level = "debug", skip_all, fields(session_id))]
    pub async fn create(
        directory: impl AsRef<Path>,
        session_id: &str,
        working_directory: &str,
    ) -> AgentSessionStoreResult<Self> {
        let directory = directory.as_ref();
        let path = session_path(directory, session_id)?;
        prepare_directory(directory).await?;
        let mut file = match OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(&path)
            .await
        {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Report::new(AgentSessionStoreError::AlreadyExists));
            }
            Err(source) => return Err(io_error("create", &path, source)),
        };
        set_private_file_permissions(&path).await?;
        let header = SessionFileRecord::Session {
            version: CURRENT_SESSION_FORMAT_VERSION,
            session_id: session_id.to_owned(),
            working_directory: working_directory.to_owned(),
        };
        let mut encoded = encode_json(&header)?;
        encoded.push(b'\n');
        file.write_all(&encoded)
            .await
            .map_err(|source| io_error("write", &path, source))?;
        file.sync_data()
            .await
            .map_err(|source| io_error("synchronize", &path, source))?;
        Ok(Self {
            file,
            path,
            persisted_entries: 0,
        })
    }

    /// Opens, recovers, and validates an existing journal.
    ///
    /// An incomplete final JSONL record is discarded. Complete malformed
    /// records and unsupported versions fail closed.
    ///
    /// # Errors
    ///
    /// Returns an error when the journal is absent, unsafe, corrupt, belongs
    /// to another session or working directory, or cannot be opened.
    #[tracing::instrument(level = "debug", skip_all, fields(session_id))]
    pub async fn open(
        directory: impl AsRef<Path>,
        session_id: &str,
        working_directory: &str,
    ) -> AgentSessionStoreResult<(Self, AgentSession)> {
        let directory = directory.as_ref();
        let path = session_path(directory, session_id)?;
        validate_directory(directory).await?;
        let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                Report::new(AgentSessionStoreError::NotFound)
            } else {
                io_error("inspect", &path, source)
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Report::new(AgentSessionStoreError::UnsafePath { path }));
        }
        let contents = tokio::fs::read(&path)
            .await
            .map_err(|source| io_error("read", &path, source))?;
        let decoded = decode_session_file(&contents, session_id, working_directory)?;
        let session = AgentSession::from_entries(decoded.entries)
            .map_err(|error| error.escalate(AgentSessionStoreError::InvalidSession))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|source| io_error("open", &path, source))?;
        if decoded.complete_bytes != contents.len() {
            file.set_len(usize_to_u64(decoded.complete_bytes))
                .await
                .map_err(|source| io_error("recover", &path, source))?;
            file.sync_data()
                .await
                .map_err(|source| io_error("synchronize", &path, source))?;
        }
        let persisted_entries = session.entries().len();
        Ok((
            Self {
                file,
                path,
                persisted_entries,
            },
            session,
        ))
    }

    /// Durably appends entries added since the preceding successful write.
    ///
    /// # Errors
    ///
    /// Returns an error when the supplied session no longer extends this
    /// journal or when encoding or synchronizing its new entries fails.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(path = %self.path.display(), entry_count = session.entries().len())
    )]
    pub async fn persist(&mut self, session: &AgentSession) -> AgentSessionStoreResult<()> {
        let entries = session.entries();
        if entries.len() < self.persisted_entries
            || entries
                .get(self.persisted_entries)
                .is_some_and(|entry| entry.id != entry_id(self.persisted_entries))
        {
            return Err(Report::new(AgentSessionStoreError::Diverged));
        }
        let mut encoded = Vec::new();
        for entry in &entries[self.persisted_entries..] {
            encoded.extend(encode_json(&SessionFileRecord::Entry {
                entry: entry.clone(),
            })?);
            encoded.push(b'\n');
        }
        if encoded.is_empty() {
            return Ok(());
        }
        encoded.extend(encode_json(&SessionFileRecord::Commit {
            entry_count: usize_to_u64(entries.len()),
        })?);
        encoded.push(b'\n');
        self.file
            .write_all(&encoded)
            .await
            .map_err(|source| io_error("append to", &self.path, source))?;
        self.file
            .sync_data()
            .await
            .map_err(|source| io_error("synchronize", &self.path, source))?;
        self.persisted_entries = entries.len();
        Ok(())
    }

    /// Returns the journal pathname.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Failure while creating, opening, or extending an agent session journal.
#[derive(Debug, Error)]
pub enum AgentSessionStoreError {
    /// The external session identifier cannot be used as one filename.
    #[error("Tasci session identifier is invalid")]
    InvalidSessionId,
    /// A new journal unexpectedly collides with an existing session.
    #[error("Tasci session already exists")]
    AlreadyExists,
    /// The requested journal is absent.
    #[error("Tasci session does not exist")]
    NotFound,
    /// The final resolved storage object is not a regular, non-symlink file or
    /// directory.
    #[error("Tasci session storage path is unsafe: {path}")]
    UnsafePath {
        /// Unexpected storage object.
        path: PathBuf,
    },
    /// The journal header is absent or has the wrong record kind.
    #[error("Tasci session journal header is invalid")]
    InvalidHeader,
    /// The journal uses a format this implementation cannot safely interpret.
    #[error("Tasci session journal version {found} is unsupported")]
    UnsupportedVersion {
        /// Version read from the journal.
        found: u32,
    },
    /// The cursor and journal refer to different native sessions.
    #[error("Tasci session journal identifier does not match the resume cursor")]
    SessionIdMismatch,
    /// The session is being resumed in a different workspace path.
    #[error("Tasci session belongs to a different working directory")]
    WorkingDirectoryMismatch,
    /// One complete JSONL record cannot be decoded.
    #[error("Tasci session journal contains invalid JSON")]
    InvalidJson {
        /// Underlying decoder failure.
        #[source]
        source: serde_json::Error,
    },
    /// A journal record could not be encoded.
    #[error("failed to encode a Tasci session journal record")]
    EncodeJson {
        /// Underlying encoder failure.
        #[source]
        source: serde_json::Error,
    },
    /// An entry transaction has a missing or inconsistent commit record.
    #[error("Tasci session journal has an invalid entry commit")]
    InvalidCommit,
    /// Persisted entries violate native session invariants.
    #[error("persisted Tasci session is invalid")]
    InvalidSession,
    /// A caller supplied a session that does not extend the current journal.
    #[error("Tasci session no longer extends its persisted journal")]
    Diverged,
    /// A filesystem operation failed.
    #[error("failed to {operation} Tasci session storage {path}: {source}")]
    Io {
        /// Attempted filesystem operation.
        operation: &'static str,
        /// Affected path.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },
}

/// Result returned by durable agent session operations.
pub type AgentSessionStoreResult<T> = Result<T, Report<AgentSessionStoreError>>;

#[derive(Deserialize, Serialize)]
#[serde(tag = "record", rename_all = "snake_case")]
enum SessionFileRecord {
    Session {
        version: u32,
        session_id: String,
        working_directory: String,
    },
    Entry {
        entry: SessionEntry,
    },
    Commit {
        entry_count: u64,
    },
}

struct DecodedSessionFile {
    entries: Vec<SessionEntry>,
    complete_bytes: usize,
}

/// Creates and restricts the journal directory.
async fn prepare_directory(directory: &Path) -> AgentSessionStoreResult<()> {
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|source| io_error("create", directory, source))?;
    validate_directory(directory).await?;
    set_private_directory_permissions(directory).await
}

/// Rejects a journal root that is not a real directory.
async fn validate_directory(directory: &Path) -> AgentSessionStoreResult<()> {
    let metadata = tokio::fs::symlink_metadata(directory)
        .await
        .map_err(|source| io_error("inspect", directory, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Report::new(AgentSessionStoreError::UnsafePath {
            path: directory.to_owned(),
        }));
    }
    Ok(())
}

/// Converts a safe opaque identifier into its journal pathname.
fn session_path(directory: &Path, session_id: &str) -> AgentSessionStoreResult<PathBuf> {
    if !valid_session_id(session_id) {
        return Err(Report::new(AgentSessionStoreError::InvalidSessionId));
    }
    Ok(directory.join(format!("{session_id}.jsonl")))
}

fn valid_session_id(session_id: &str) -> bool {
    let bytes = session_id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_SESSION_ID_BYTES
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Decodes committed transactions and locates the last recovery boundary.
fn decode_session_file(
    contents: &[u8],
    expected_session_id: &str,
    expected_working_directory: &str,
) -> AgentSessionStoreResult<DecodedSessionFile> {
    let complete_input_bytes = contents
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .ok_or_else(|| Report::new(AgentSessionStoreError::InvalidHeader))?;
    let mut records = contents[..complete_input_bytes].split_inclusive(|byte| *byte == b'\n');
    let header_record = records
        .next()
        .ok_or_else(|| Report::new(AgentSessionStoreError::InvalidHeader))
        .and_then(decode_record)?;
    let SessionFileRecord::Session {
        version,
        session_id,
        working_directory,
    } = header_record
    else {
        return Err(Report::new(AgentSessionStoreError::InvalidHeader));
    };
    if version != CURRENT_SESSION_FORMAT_VERSION {
        return Err(Report::new(AgentSessionStoreError::UnsupportedVersion {
            found: version,
        }));
    }
    if session_id != expected_session_id {
        return Err(Report::new(AgentSessionStoreError::SessionIdMismatch));
    }
    if working_directory != expected_working_directory {
        return Err(Report::new(
            AgentSessionStoreError::WorkingDirectoryMismatch,
        ));
    }
    let mut entries = Vec::new();
    let mut pending_entries = Vec::new();
    let mut complete_bytes = contents[..complete_input_bytes]
        .split_inclusive(|byte| *byte == b'\n')
        .next()
        .map_or(0, <[u8]>::len);
    let mut record_end = complete_bytes;
    for encoded in records {
        record_end = record_end.saturating_add(encoded.len());
        match decode_record(encoded)? {
            SessionFileRecord::Entry { entry } => pending_entries.push(entry),
            SessionFileRecord::Commit { entry_count } => {
                let committed_count = entries.len().saturating_add(pending_entries.len());
                if pending_entries.is_empty() || entry_count != usize_to_u64(committed_count) {
                    return Err(Report::new(AgentSessionStoreError::InvalidCommit));
                }
                entries.append(&mut pending_entries);
                complete_bytes = record_end;
            }
            SessionFileRecord::Session { .. } => {
                return Err(Report::new(AgentSessionStoreError::InvalidHeader));
            }
        }
    }
    Ok(DecodedSessionFile {
        entries,
        complete_bytes,
    })
}

fn encode_json<T: Serialize>(value: &T) -> AgentSessionStoreResult<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|source| Report::new(AgentSessionStoreError::EncodeJson { source }))
}

fn decode_record(encoded: &[u8]) -> AgentSessionStoreResult<SessionFileRecord> {
    let encoded = encoded.strip_suffix(b"\n").unwrap_or(encoded);
    serde_json::from_slice(encoded)
        .map_err(|source| Report::new(AgentSessionStoreError::InvalidJson { source }))
}

fn io_error(
    operation: &'static str,
    path: &Path,
    source: io::Error,
) -> Report<AgentSessionStoreError> {
    Report::new(AgentSessionStoreError::Io {
        operation,
        path: path.to_owned(),
        source,
    })
}

fn entry_id(index: usize) -> SessionEntryId {
    SessionEntryId(usize_to_u64(index))
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(unix)]
async fn set_private_directory_permissions(directory: &Path) -> AgentSessionStoreResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|source| io_error("set permissions on", directory, source))
}

#[cfg(not(unix))]
async fn set_private_directory_permissions(_directory: &Path) -> AgentSessionStoreResult<()> {
    Ok(())
}

#[cfg(unix)]
async fn set_private_file_permissions(path: &Path) -> AgentSessionStoreResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|source| io_error("set permissions on", path, source))
}

#[cfg(not(unix))]
async fn set_private_file_permissions(_path: &Path) -> AgentSessionStoreResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AssistantMessage;
    use crate::CompactionRecord;
    use crate::ModelMessage;
    use crate::ModelUsage;

    /// Verifies committed entries survive restart and an interrupted
    /// transaction is removed before the journal continues.
    #[tokio::test]
    async fn session_journal_recovers_an_incomplete_tail_and_continues() {
        let temporary = tempfile::tempdir().unwrap();
        let mut store = AgentSessionStore::create(temporary.path(), "session-1", "/workspace")
            .await
            .unwrap();
        let mut session = AgentSession::new();
        session
            .append_message(ModelMessage::System {
                content: "system".to_owned(),
            })
            .unwrap();
        session
            .append_message(ModelMessage::User {
                content: "first".to_owned(),
            })
            .unwrap();
        session.append_message(assistant("answer")).unwrap();
        store.persist(&session).await.unwrap();
        let path = store.path().to_owned();
        drop(store);
        let mut interrupted_session = session.clone();
        interrupted_session
            .append_message(ModelMessage::User {
                content: "uncommitted".to_owned(),
            })
            .unwrap();
        let uncommitted_entry = interrupted_session.entries().last().unwrap().clone();
        let mut file = OpenOptions::new().append(true).open(&path).await.unwrap();
        let mut encoded = encode_json(&SessionFileRecord::Entry {
            entry: uncommitted_entry,
        })
        .unwrap();
        encoded.push(b'\n');
        file.write_all(&encoded).await.unwrap();
        file.write_all(br#"{"kind":"message","id":"#).await.unwrap();
        file.sync_data().await.unwrap();
        drop(file);

        let (mut store, mut restored) =
            AgentSessionStore::open(temporary.path(), "session-1", "/workspace")
                .await
                .unwrap();
        assert_eq!(restored, session);
        restored
            .append_message(ModelMessage::User {
                content: "second".to_owned(),
            })
            .unwrap();
        store.persist(&restored).await.unwrap();
        drop(store);

        let (_, reopened) = AgentSessionStore::open(temporary.path(), "session-1", "/workspace")
            .await
            .unwrap();
        assert_eq!(reopened, restored);
    }

    /// Verifies a cursor cannot resume a journal created for another working
    /// directory.
    #[tokio::test]
    async fn session_journal_rejects_a_working_directory_change() {
        let temporary = tempfile::tempdir().unwrap();
        AgentSessionStore::create(temporary.path(), "session-1", "/workspace")
            .await
            .unwrap();

        let error = AgentSessionStore::open(temporary.path(), "session-1", "/other")
            .await
            .unwrap_err();

        assert!(matches!(
            error.error(),
            AgentSessionStoreError::WorkingDirectoryMismatch
        ));
    }

    /// Verifies durable recovery retains compaction checkpoints as well as
    /// original messages.
    #[tokio::test]
    async fn session_journal_restores_compaction_records() {
        let temporary = tempfile::tempdir().unwrap();
        let mut store = AgentSessionStore::create(temporary.path(), "session-1", "/workspace")
            .await
            .unwrap();
        let mut session = AgentSession::new();
        session
            .append_message(ModelMessage::System {
                content: "system".to_owned(),
            })
            .unwrap();
        let boundary = session
            .append_message(ModelMessage::User {
                content: "keep".to_owned(),
            })
            .unwrap();
        session
            .append_compaction(CompactionRecord {
                summary: "checkpoint".to_owned(),
                first_kept_entry_id: boundary,
                tokens_before: 42,
                estimated_tokens_after: 12,
                usage: ModelUsage::default(),
                read_files: vec!["README.md".to_owned()],
                modified_files: vec!["src/lib.rs".to_owned()],
            })
            .unwrap();
        store.persist(&session).await.unwrap();
        drop(store);

        let (_, restored) = AgentSessionStore::open(temporary.path(), "session-1", "/workspace")
            .await
            .unwrap();

        assert_eq!(restored, session);
    }

    fn assistant(content: &str) -> ModelMessage {
        ModelMessage::Assistant(AssistantMessage {
            reasoning: String::new(),
            content: content.to_owned(),
            tool_calls: Vec::new(),
            usage: None,
        })
    }
}
