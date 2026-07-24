//! SQLite-backed image inventory and immutable generation linkage.

use std::str::FromStr as _;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use serde::Deserialize as _;
use tascarrel_api::ArcStr;
use tascarrel_api::types::images as api;
use thiserror::Error;
use tokio_rusqlite::rusqlite;
use tokio_rusqlite::rusqlite::TransactionBehavior;

use crate::Database;
use crate::runtime::pod::ImageId as RuntimeImageId;

const INTERRUPTED_MESSAGE: &str = "image generation was interrupted by a guest restart";

/// Durable data required to recover one image inventory entry.
#[derive(Clone, Debug)]
pub(crate) struct ImageRecord {
    /// API image state retained across guest restarts.
    pub(crate) image: api::Image,
    /// Immutable runtime generation linked to a successful build.
    pub(crate) generation: Option<RuntimeImageId>,
}

/// Persistent image records stored on the guest database connection.
#[derive(Clone)]
pub(crate) struct ImageStateRepository {
    database: Database,
}

impl ImageStateRepository {
    /// Binds image state access to the migrated guest database.
    pub(crate) fn new(database: Database) -> Self {
        Self { database }
    }

    /// Inserts one newly admitted generating image.
    pub(crate) async fn create(&self, record: ImageRecord) -> Result<(), Report<ImageStateError>> {
        self.call("create image record", move |database| {
            if !matches!(record.image.state, api::ImageState::Generating)
                || record.generation.is_some()
            {
                return Err(invalid_state("new image is not generating"));
            }
            insert_record(database, &record)
        })
        .await
    }

    /// Persists one terminal generation outcome.
    pub(crate) async fn finish(&self, record: ImageRecord) -> Result<(), Report<ImageStateError>> {
        self.call("finish image record", move |database| {
            let TerminalColumns {
                status,
                generation,
                failure_message,
                failed_at,
            } = terminal_columns(&record)?;
            let changed = database
                .execute(
                    r"
UPDATE images
SET status = ?2,
    runtime_generation_id = ?3,
    failure_message = ?4,
    failed_at = ?5
WHERE id = ?1 AND status = 'generating'
",
                    rusqlite::params![
                        record.image.id.0.as_ref(),
                        status,
                        generation,
                        failure_message,
                        failed_at,
                    ],
                )
                .map_err(|source| database_failure("update image record", source))?;
            if changed == 1 {
                Ok(())
            } else {
                Err(invalid_state("generating image record is missing")
                    .field("image_id", record.image.id.0.as_ref())
                    .field_display("changed_rows", changed))
            }
        })
        .await
    }

    /// Fails interrupted generations and loads the complete inventory.
    pub(crate) async fn recover(&self) -> Result<Vec<ImageRecord>, Report<ImageStateError>> {
        let failed_at = Timestamp::now().to_string();
        self.call("recover image records", move |database| {
            let transaction = database
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|source| database_failure("begin image recovery", source))?;
            transaction
                .execute(
                    r"
UPDATE images
SET status = 'failed', failure_message = ?1, failed_at = ?2
WHERE status = 'generating'
",
                    rusqlite::params![INTERRUPTED_MESSAGE, failed_at],
                )
                .map_err(|source| database_failure("fail interrupted image records", source))?;
            let records = query_records(&transaction)?;
            transaction
                .commit()
                .map_err(|source| database_failure("commit image recovery", source))?;
            Ok(records)
        })
        .await
    }

    /// Persists whether generation-linked records still have immutable storage.
    pub(crate) async fn reconcile(
        &self,
        availability: Vec<(api::ImageId, bool)>,
    ) -> Result<(), Report<ImageStateError>> {
        self.call("reconcile image records", move |database| {
            let transaction = database
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|source| database_failure("begin image reconciliation", source))?;
            for (image_id, available) in availability {
                let status = if available { "generated" } else { "orphaned" };
                let changed = transaction
                    .execute(
                        "UPDATE images SET status = ?2
                         WHERE id = ?1 AND status IN ('generated', 'orphaned')",
                        rusqlite::params![image_id.0.as_ref(), status],
                    )
                    .map_err(|source| database_failure("reconcile image record", source))?;
                if changed != 1 {
                    return Err(invalid_state("generation-linked image record is missing")
                        .field("image_id", image_id.0.as_ref())
                        .field_display("changed_rows", changed));
                }
            }
            transaction
                .commit()
                .map_err(|source| database_failure("commit image reconciliation", source))?;
            Ok(())
        })
        .await
    }

    /// Executes one repository operation on the serialized database connection.
    async fn call<T, F>(
        &self,
        operation: &'static str,
        function: F,
    ) -> Result<T, Report<ImageStateError>>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, Report<ImageStateError>> + Send + 'static,
    {
        self.database
            .connection()
            .call_raw(function)
            .await
            .map_err(|source| database_failure(operation, source))?
    }
}

/// Failures while reading or updating persistent image state.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ImageStateError {
    /// The database connection or an SQLite operation failed.
    #[error("image state database is unavailable")]
    Unavailable,
    /// A stored image record violates the feature's invariants.
    #[error("image state database contains invalid data")]
    InvalidState,
}

/// SQLite representation decoded before domain validation.
struct RawImageRecord {
    id: String,
    input_sha256: String,
    input_modified_at: String,
    status: String,
    generation: Option<String>,
    failure_message: Option<String>,
    failed_at: Option<String>,
    created_at: String,
}

/// Database columns selected from one terminal record.
struct TerminalColumns {
    status: &'static str,
    generation: Option<String>,
    failure_message: Option<String>,
    failed_at: Option<String>,
}

/// Inserts one validated image record.
fn insert_record(
    database: &rusqlite::Connection,
    record: &ImageRecord,
) -> Result<(), Report<ImageStateError>> {
    let input_sha256: ArcStr = record.image.input.sha256.clone().into();
    database
        .execute(
            r"
INSERT INTO images (
    id,
    input_sha256,
    input_modified_at,
    status,
    runtime_generation_id,
    failure_message,
    failed_at,
    created_at
) VALUES (?1, ?2, ?3, 'generating', NULL, NULL, NULL, ?4)
",
            rusqlite::params![
                record.image.id.0.as_ref(),
                input_sha256.as_ref(),
                record.image.input.modified_at.to_string(),
                record.image.created_at.to_string(),
            ],
        )
        .map_err(|source| database_failure("insert image record", source))?;
    Ok(())
}

/// Loads every image in stable API order.
fn query_records(
    database: &rusqlite::Connection,
) -> Result<Vec<ImageRecord>, Report<ImageStateError>> {
    let mut statement = database
        .prepare(
            r"
SELECT
    id,
    input_sha256,
    input_modified_at,
    status,
    runtime_generation_id,
    failure_message,
    failed_at,
    created_at
FROM images
ORDER BY created_at, id
",
        )
        .map_err(|source| database_failure("prepare image query", source))?;
    let rows = statement
        .query_map([], decode_raw_record)
        .map_err(|source| database_failure("query image records", source))?;
    rows.map(|row| {
        row.map_err(|source| database_failure("decode image record", source))
            .and_then(decode_record)
    })
    .collect()
}

/// Reads one raw row without interpreting domain values inside rusqlite.
fn decode_raw_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawImageRecord> {
    Ok(RawImageRecord {
        id: row.get(0)?,
        input_sha256: row.get(1)?,
        input_modified_at: row.get(2)?,
        status: row.get(3)?,
        generation: row.get(4)?,
        failure_message: row.get(5)?,
        failed_at: row.get(6)?,
        created_at: row.get(7)?,
    })
}

/// Validates and converts one raw database row into feature state.
fn decode_record(raw: RawImageRecord) -> Result<ImageRecord, Report<ImageStateError>> {
    let id = api::ImageId::from_str(&raw.id)
        .map_err(|source| invalid_column("id", source.to_string()))?;
    let sha256 = decode_sha256(raw.input_sha256)?;
    let modified_at = Timestamp::from_str(&raw.input_modified_at)
        .map_err(|source| invalid_column("input_modified_at", source.to_string()))?;
    let created_at = Timestamp::from_str(&raw.created_at)
        .map_err(|source| invalid_column("created_at", source.to_string()))?;
    let (status, generation) = decode_status(
        &raw.status,
        raw.generation,
        raw.failure_message,
        raw.failed_at,
    )?;
    Ok(ImageRecord {
        image: api::Image {
            id,
            input: api::ImageInput {
                sha256,
                modified_at,
            },
            state: status,
            created_at,
        },
        generation,
    })
}

/// Validates one persisted SHA-256 and creates its API wrapper.
fn decode_sha256(value: String) -> Result<api::ImageInputSha256, Report<ImageStateError>> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_column(
            "input_sha256",
            "image input SHA-256 is invalid",
        ));
    }
    let deserializer = serde::de::value::StringDeserializer::<serde::de::value::Error>::new(value);
    api::ImageInputSha256::deserialize(deserializer)
        .map_err(|source| invalid_column("input_sha256", source.to_string()))
}

/// Reconstructs one image state and its immutable generation linkage.
fn decode_status(
    status: &str,
    generation: Option<String>,
    failure_message: Option<String>,
    failed_at: Option<String>,
) -> Result<(api::ImageState, Option<RuntimeImageId>), Report<ImageStateError>> {
    match (status, generation, failure_message, failed_at) {
        ("generating", None, None, None) => Ok((api::ImageState::Generating, None)),
        ("generated", Some(generation), None, None) => {
            let generation = RuntimeImageId::from_str(&generation)
                .map_err(|source| invalid_column("runtime_generation_id", source.to_string()))?;
            Ok((api::ImageState::Generated, Some(generation)))
        }
        ("orphaned", Some(generation), None, None) => {
            let generation = RuntimeImageId::from_str(&generation)
                .map_err(|source| invalid_column("runtime_generation_id", source.to_string()))?;
            Ok((api::ImageState::Orphaned, Some(generation)))
        }
        ("failed", None, Some(message), Some(failed_at)) => {
            let failed_at = Timestamp::from_str(&failed_at)
                .map_err(|source| invalid_column("failed_at", source.to_string()))?;
            Ok((
                api::ImageState::Failed(api::ImageGenerationFailure {
                    message: message.into(),
                    failed_at,
                }),
                None,
            ))
        }
        _ => Err(invalid_state("image has inconsistent generation state")),
    }
}

/// Converts one terminal record into nullable database columns.
fn terminal_columns(record: &ImageRecord) -> Result<TerminalColumns, Report<ImageStateError>> {
    match (&record.image.state, &record.generation) {
        (api::ImageState::Generated | api::ImageState::Available, Some(generation)) => {
            Ok(TerminalColumns {
                status: "generated",
                generation: Some(generation.as_str().to_owned()),
                failure_message: None,
                failed_at: None,
            })
        }
        (api::ImageState::Failed(failure), None) if !failure.message.is_empty() => {
            Ok(TerminalColumns {
                status: "failed",
                generation: None,
                failure_message: Some(failure.message.to_string()),
                failed_at: Some(failure.failed_at.to_string()),
            })
        }
        _ => Err(invalid_state(
            "finished image does not have a terminal state",
        )),
    }
}

/// Creates a database-access report with the failed operation attached.
fn database_failure(
    operation: &'static str,
    source: impl std::fmt::Display,
) -> Report<ImageStateError> {
    ImageStateError::Unavailable
        .report()
        .message(source.to_string())
        .field("operation", operation)
}

/// Creates a report for one invalid durable column.
fn invalid_column(column: &'static str, message: impl Into<String>) -> Report<ImageStateError> {
    invalid_state(message).field("column", column)
}

/// Creates a report for invalid durable image state.
fn invalid_state(message: impl Into<String>) -> Report<ImageStateError> {
    ImageStateError::InvalidState
        .report()
        .message(message.into())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    /// Verifies terminal outcomes survive a database round trip while an
    /// interrupted generation is failed during recovery.
    #[tokio::test]
    async fn recovery_preserves_outcomes_and_fails_interrupted_generations() {
        let temporary = tempdir().expect("temporary directory is created");
        let database = Database::open(temporary.path().join("guest.db"))
            .await
            .expect("database opens");
        let repository = ImageStateRepository::new(database);

        let mut completed = generating_record("2026-01-01T00:00:00Z");
        let completed_id = completed.image.id.clone();
        repository
            .create(completed.clone())
            .await
            .expect("generating image is persisted");
        let generation = RuntimeImageId::new(format!("sha256:{}", "a".repeat(64)))
            .expect("test generation is valid");
        completed.image.state = api::ImageState::Available;
        completed.generation = Some(generation.clone());
        repository
            .finish(completed)
            .await
            .expect("terminal image is persisted");

        let interrupted = generating_record("2026-01-02T00:00:00Z");
        let interrupted_id = interrupted.image.id.clone();
        repository
            .create(interrupted)
            .await
            .expect("second generating image is persisted");

        let recovered = repository.recover().await.expect("images recover");
        let completed = recovered
            .iter()
            .find(|record| record.image.id == completed_id)
            .expect("completed image is recovered");
        assert_eq!(completed.generation.as_ref(), Some(&generation));
        assert!(matches!(completed.image.state, api::ImageState::Generated));
        let interrupted = recovered
            .iter()
            .find(|record| record.image.id == interrupted_id)
            .expect("interrupted image is recovered");
        let api::ImageState::Failed(failure) = &interrupted.image.state else {
            panic!("interrupted image is failed");
        };
        assert_eq!(failure.message, INTERRUPTED_MESSAGE);
        assert!(interrupted.generation.is_none());

        repository
            .reconcile(vec![(completed_id.clone(), false)])
            .await
            .expect("missing generation is persisted as orphaned");
        let recovered = repository.recover().await.expect("images recover again");
        let completed = recovered
            .iter()
            .find(|record| record.image.id == completed_id)
            .expect("orphaned image is recovered");
        assert!(matches!(completed.image.state, api::ImageState::Orphaned));
    }

    fn generating_record(created_at: &str) -> ImageRecord {
        let created_at = Timestamp::from_str(created_at).expect("test timestamp is valid");
        ImageRecord {
            image: api::Image {
                id: api::ImageId::generate(),
                input: api::ImageInput {
                    sha256: decode_sha256("b".repeat(64)).expect("test digest is valid"),
                    modified_at: created_at,
                },
                state: api::ImageState::Generating,
                created_at,
            },
            generation: None,
        }
    }
}
