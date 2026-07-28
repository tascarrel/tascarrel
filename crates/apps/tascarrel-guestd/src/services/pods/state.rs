//! SQLite-backed pod lifecycle records and identity allocation.

use std::str::FromStr as _;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::pods as api;
use thiserror::Error;
use tokio_rusqlite::rusqlite;
use tokio_rusqlite::rusqlite::TransactionBehavior;

use super::runc::MAX_IDENTITY_SLOT;
use crate::Database;
use crate::runtime::pod::ImageId;

/// Durable data required to manage and recover one pod.
#[derive(Clone, Debug)]
pub(crate) struct PodRecord {
    /// API representation whose runtime status may be published independently.
    pub(crate) pod: api::Pod,
    /// Immutable image generation used by the pod storage.
    pub(crate) image: ImageId,
    /// Whether recovery must automatically remove the pod.
    pub(crate) ephemeral: bool,
    /// Stable host identity-mapping slot allocated to the pod.
    pub(crate) slot: u32,
    /// Durable lifecycle checkpoint used during recovery.
    pub(crate) persistent_state: PersistentPodState,
}

/// Recovery-relevant pod lifecycle persisted independently of runtime status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistentPodState {
    /// Database admission completed but storage creation may be incomplete.
    Creating,
    /// Persistent storage is complete and recoverable as stopped.
    Ready,
    /// Destruction began but persistent resources may remain.
    Destroying,
    /// The record is archived and no persistent resources should remain.
    Destroyed,
}

/// Persistent pod records stored on the guest database connection.
#[derive(Clone)]
pub(crate) struct PodStateRepository {
    database: Database,
}

impl PodStateRepository {
    /// Binds pod state access to the migrated guest database.
    pub(crate) fn new(database: Database) -> Self {
        Self { database }
    }

    /// Atomically allocates an identity slot and inserts one active pod.
    pub(crate) async fn create(
        &self,
        pod: api::Pod,
        image: ImageId,
        ephemeral: bool,
    ) -> Result<PodRecord, Report<PodStateError>> {
        self.call("create pod record", move |database| {
            let transaction = database
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|source| database_failure("begin pod creation", source))?;
            let slot = allocate_slot(&transaction)?;

            let record = PodRecord {
                pod,
                image,
                ephemeral,
                slot,
                persistent_state: PersistentPodState::Creating,
            };
            insert_record(&transaction, &record)?;
            transaction
                .commit()
                .map_err(|source| database_failure("commit pod creation", source))?;
            Ok(record)
        })
        .await
    }

    /// Saves the recovery state and title of one active pod.
    pub(crate) async fn save(&self, record: PodRecord) -> Result<(), Report<PodStateError>> {
        self.call("save pod record", move |database| {
            let status = encode_persistent_state(record.persistent_state);
            let changed = database
                .execute(
                    r"
UPDATE pods
SET title = ?2, status = ?3
WHERE id = ?1 AND status <> 'destroyed'
",
                    rusqlite::params![record.pod.id.0.as_ref(), record.pod.title.as_ref(), status,],
                )
                .map_err(|source| database_failure("update pod record", source))?;
            require_one_active_row(changed, &record.pod.id)
        })
        .await
    }

    /// Marks one active pod as archived while retaining its lifecycle history.
    pub(crate) async fn archive(
        &self,
        pod_id: api::PodId,
        archived_at: Timestamp,
    ) -> Result<(), Report<PodStateError>> {
        self.call("archive pod record", move |database| {
            let changed = database
                .execute(
                    "UPDATE pods SET status = 'destroyed', archived_at = ?2
                     WHERE id = ?1 AND status <> 'destroyed'",
                    rusqlite::params![pod_id.0.as_ref(), archived_at.to_string()],
                )
                .map_err(|source| database_failure("archive pod record", source))?;
            require_one_active_row(changed, &pod_id)
        })
        .await
    }

    /// Loads every non-archived pod in stable creation order.
    pub(crate) async fn active(&self) -> Result<Vec<PodRecord>, Report<PodStateError>> {
        self.records(
            "load active pod records",
            "SELECT id, title, status, created_at, image_id, ephemeral, identity_slot, archived_at
             FROM pods WHERE status <> 'destroyed' ORDER BY created_at, id",
        )
        .await
    }

    /// Loads every archived pod which may still have orphaned runtime files.
    pub(crate) async fn archived(&self) -> Result<Vec<PodRecord>, Report<PodStateError>> {
        self.records(
            "load archived pod records",
            "SELECT id, title, status, created_at, image_id, ephemeral, identity_slot, archived_at
             FROM pods WHERE status = 'destroyed' ORDER BY archived_at, id",
        )
        .await
    }

    /// Executes one repository operation on the serialized database connection.
    async fn call<T, F>(
        &self,
        operation: &'static str,
        function: F,
    ) -> Result<T, Report<PodStateError>>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, Report<PodStateError>> + Send + 'static,
    {
        self.database
            .connection()
            .call_raw(function)
            .await
            .map_err(|source| database_failure(operation, source))?
    }

    /// Loads and validates rows selected by one fixed repository query.
    async fn records(
        &self,
        operation: &'static str,
        query: &'static str,
    ) -> Result<Vec<PodRecord>, Report<PodStateError>> {
        self.call(operation, move |database| {
            let mut statement = database
                .prepare(query)
                .map_err(|source| database_failure("prepare pod query", source))?;
            let rows = statement
                .query_map([], decode_raw_record)
                .map_err(|source| database_failure("query pod records", source))?;
            rows.map(|row| {
                row.map_err(|source| database_failure("decode pod record", source))
                    .and_then(decode_record)
            })
            .collect()
        })
        .await
    }
}

/// Selects the lowest identity slot not held by an active pod.
fn allocate_slot(database: &rusqlite::Connection) -> Result<u32, Report<PodStateError>> {
    let mut statement = database
        .prepare(
            "SELECT identity_slot FROM pods
             WHERE status <> 'destroyed' ORDER BY identity_slot",
        )
        .map_err(|source| database_failure("prepare identity-slot query", source))?;
    let slots = statement
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|source| database_failure("query active identity slots", source))?;
    let mut candidate = 0_u32;
    for slot in slots {
        let slot =
            slot.map_err(|source| database_failure("decode active identity slot", source))?;
        let slot = u32::try_from(slot)
            .map_err(|_| invalid_state("active pod identity slot is out of range"))?;
        if slot > MAX_IDENTITY_SLOT {
            return Err(invalid_state(
                "active pod identity slot exceeds the runtime range",
            ));
        }
        if slot == candidate {
            candidate = candidate
                .checked_add(1)
                .ok_or_else(|| PodStateError::SlotExhausted.report())?;
        } else if slot > candidate {
            break;
        }
    }
    if candidate > MAX_IDENTITY_SLOT {
        return Err(PodStateError::SlotExhausted.report());
    }
    Ok(candidate)
}

/// Failures while reading or updating persistent pod state.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum PodStateError {
    /// The database connection or an `SQLite` operation failed.
    #[error("pod state database is unavailable")]
    Unavailable,
    /// A stored pod record violates the feature's invariants.
    #[error("pod state database contains invalid data")]
    InvalidState,
    /// Every runtime-supported pod identity slot is currently active.
    #[error("pod identity slots are exhausted")]
    SlotExhausted,
}

/// `SQLite` representation decoded before domain validation.
struct RawPodRecord {
    id: String,
    title: String,
    status: String,
    created_at: String,
    image_id: String,
    ephemeral: i64,
    identity_slot: i64,
    archived_at: Option<String>,
}

/// Inserts one newly allocated pod record.
fn insert_record(
    database: &rusqlite::Connection,
    record: &PodRecord,
) -> Result<(), Report<PodStateError>> {
    let status = encode_persistent_state(record.persistent_state);
    database
        .execute(
            r"
INSERT INTO pods (
    id, title, status, created_at, image_id, ephemeral, identity_slot, archived_at
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)
",
            rusqlite::params![
                record.pod.id.0.as_ref(),
                record.pod.title.as_ref(),
                status,
                record.pod.created_at.to_string(),
                record.image.as_str(),
                i64::from(record.ephemeral),
                i64::from(record.slot),
            ],
        )
        .map_err(|source| database_failure("insert pod record", source))?;
    Ok(())
}

/// Converts one recovery state into its database representation.
const fn encode_persistent_state(state: PersistentPodState) -> &'static str {
    match state {
        PersistentPodState::Creating => "creating",
        PersistentPodState::Ready => "ready",
        PersistentPodState::Destroying => "destroying",
        PersistentPodState::Destroyed => "destroyed",
    }
}

/// Reads one raw row without interpreting domain values inside rusqlite.
fn decode_raw_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawPodRecord> {
    Ok(RawPodRecord {
        id: row.get(0)?,
        title: row.get(1)?,
        status: row.get(2)?,
        created_at: row.get(3)?,
        image_id: row.get(4)?,
        ephemeral: row.get(5)?,
        identity_slot: row.get(6)?,
        archived_at: row.get(7)?,
    })
}

/// Validates and converts one raw database row into feature state.
fn decode_record(raw: RawPodRecord) -> Result<PodRecord, Report<PodStateError>> {
    let id = api::PodId::from_str(&raw.id)
        .map_err(|source| invalid_state(source.to_string()).field("column", "id"))?;
    let created_at = Timestamp::from_str(&raw.created_at)
        .map_err(|source| invalid_state(source.to_string()).field("column", "created_at"))?;
    let image = ImageId::from_str(&raw.image_id)
        .map_err(|source| invalid_state(source.to_string()).field("column", "image_id"))?;
    let ephemeral = match raw.ephemeral {
        0 => false,
        1 => true,
        _ => return Err(invalid_state("pod ephemeral flag is invalid")),
    };
    let slot = u32::try_from(raw.identity_slot)
        .map_err(|source| invalid_state(source.to_string()).field("column", "identity_slot"))?;
    if slot > MAX_IDENTITY_SLOT {
        return Err(invalid_state(
            "pod identity slot exceeds the runtime-supported range",
        ));
    }
    if let Some(archived_at) = &raw.archived_at {
        Timestamp::from_str(archived_at)
            .map_err(|source| invalid_state(source.to_string()).field("column", "archived_at"))?;
    }
    let persistent_state = decode_persistent_state(&raw.status)?;
    let status = runtime_status(persistent_state);
    Ok(PodRecord {
        pod: api::Pod {
            id,
            title: raw.title.into(),
            status,
            created_at,
        },
        image,
        ephemeral,
        slot,
        persistent_state,
    })
}

/// Reconstructs a recovery state from its database representation.
fn decode_persistent_state(status: &str) -> Result<PersistentPodState, Report<PodStateError>> {
    let state = match status {
        "creating" => PersistentPodState::Creating,
        "ready" => PersistentPodState::Ready,
        "destroying" => PersistentPodState::Destroying,
        "destroyed" => PersistentPodState::Destroyed,
        _ => return Err(invalid_state("pod has an unknown lifecycle status")),
    };
    Ok(state)
}

/// Chooses the initial API status for one recovered persistent state.
const fn runtime_status(state: PersistentPodState) -> api::PodState {
    match state {
        PersistentPodState::Creating => api::PodState::Creating,
        PersistentPodState::Ready => api::PodState::Stopped,
        PersistentPodState::Destroying | PersistentPodState::Destroyed => api::PodState::Destroying,
    }
}

/// Ensures an update addressed exactly one active pod.
fn require_one_active_row(
    changed: usize,
    pod_id: &api::PodId,
) -> Result<(), Report<PodStateError>> {
    if changed == 1 {
        return Ok(());
    }
    Err(invalid_state("active pod record is missing")
        .field("pod_id", pod_id.0.as_ref())
        .field_display("changed_rows", changed))
}

/// Creates a database-access report with the failed operation attached.
fn database_failure(
    operation: &'static str,
    source: impl std::fmt::Display,
) -> Report<PodStateError> {
    PodStateError::Unavailable
        .report()
        .message(source.to_string())
        .field("operation", operation)
}

/// Creates a report for invalid durable state.
fn invalid_state(message: impl Into<String>) -> Report<PodStateError> {
    PodStateError::InvalidState.report().message(message.into())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    /// Verifies the recovery-cleanup flag survives the durable pod-state
    /// round trip.
    #[tokio::test]
    async fn ephemeral_pod_state_round_trips() {
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path().join("guest.db"))
            .await
            .unwrap();
        let repository = PodStateRepository::new(database);
        let pod = api::Pod {
            id: api::PodId::generate(),
            title: "Image setup".into(),
            status: api::PodState::Creating,
            created_at: Timestamp::now(),
        };
        let image = ImageId::new(format!("sha256:{}", "a".repeat(64))).unwrap();

        let created = repository.create(pod, image, true).await.unwrap();
        let active = repository.active().await.unwrap();

        assert!(created.ephemeral);
        assert_eq!(active.len(), 1);
        assert!(active[0].ephemeral);
        assert_eq!(active[0].slot, created.slot);
    }
}
