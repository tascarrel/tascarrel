//! Shared `SQLite` database for durable guest state.
//!
//! [`Database`] owns the guest daemon's serialized `SQLite` connection. Opening
//! it configures the connection, validates the migration ledger, and applies
//! every pending schema migration before feature storage becomes available.

mod migrations;

use std::path::Path;

use migrations::MIGRATIONS;
use migrations::Migration;
use reportify::ErrorExt as _;
use reportify::Report;
use sha2::Digest as _;
use sha2::Sha256;
use thiserror::Error;
use tokio_rusqlite::Connection;
use tokio_rusqlite::rusqlite;
use tokio_rusqlite::rusqlite::Transaction;
use tokio_rusqlite::rusqlite::TransactionBehavior;

/// A configured, migrated connection to the guest daemon's `SQLite` database.
#[derive(Clone)]
pub struct Database {
    connection: Connection,
}

impl Database {
    /// Opens a serialized `SQLite` connection and applies pending migrations.
    ///
    /// # Errors
    ///
    /// Returns [`DatabaseError`] when the database is unavailable, its
    /// migration ledger is incompatible with this binary, or initialization
    /// fails.
    #[tracing::instrument(level = "debug", skip_all, fields(path = %path.as_ref().display()))]
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Report<DatabaseError>> {
        let path = path.as_ref().to_owned();
        let connection = Connection::open(&path)
            .await
            .map_err(|source| unavailable(&path, source))?;

        connection
            .call_raw(|connection| initialize(connection, MIGRATIONS))
            .await
            .map_err(|source| unavailable(&path, source))??;

        Ok(Self { connection })
    }

    /// Returns the configured serialized connection for feature storage.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
}

/// Caller-relevant failure categories for the guest database.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum DatabaseError {
    /// The database connection could not be opened or accessed.
    #[error("guest database is unavailable")]
    Unavailable,
    /// The applied migration history does not match this binary.
    #[error("guest database schema is incompatible with this binary")]
    IncompatibleSchema,
    /// Connection configuration or schema migration failed.
    #[error("failed to initialize the guest database")]
    Initialization,
}

/// Connection-local settings applied before schema inspection or feature use.
const CONNECTION_CONFIGURATION: &str = r"
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
PRAGMA trusted_schema = OFF;
";

/// The migration ledger is the only bootstrap schema outside the ordered
/// migrations.
const MIGRATION_LEDGER_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    name TEXT NOT NULL UNIQUE,
    sql_sha256 BLOB NOT NULL CHECK (length(sql_sha256) = 32),
    applied_at TEXT NOT NULL CHECK (length(applied_at) > 0)
) STRICT;
";

/// Validated position of the database within the ordered migration list.
struct MigrationState {
    applied: usize,
    next_version: i64,
}

/// Configures one connection and brings its schema to the current version.
fn initialize(
    database: &mut rusqlite::Connection,
    migrations: &[Migration],
) -> Result<(), Report<DatabaseError>> {
    database
        .execute_batch(CONNECTION_CONFIGURATION)
        .map_err(|source| initialization_failure("configure connection", source))?;
    database
        .execute_batch(MIGRATION_LEDGER_SCHEMA)
        .map_err(|source| initialization_failure("install migration ledger", source))?;

    loop {
        let transaction = database
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| initialization_failure("begin migration transaction", source))?;
        let state = inspect_migrations(&transaction, migrations)?;
        let Some(migration) = migrations.get(state.applied) else {
            transaction
                .commit()
                .map_err(|source| initialization_failure("commit schema validation", source))?;
            return Ok(());
        };

        apply_migration(&transaction, migration, state.next_version)?;
        transaction.commit().map_err(|source| {
            migration_failure("commit transaction", migration, state.next_version, source)
        })?;
    }
}

/// Validates every ledger row and returns the next migration position.
fn inspect_migrations(
    database: &rusqlite::Connection,
    migrations: &[Migration],
) -> Result<MigrationState, Report<DatabaseError>> {
    let mut statement = database
        .prepare("SELECT version, name, sql_sha256 FROM schema_migrations ORDER BY version")
        .map_err(|source| initialization_failure("prepare migration ledger query", source))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(|source| initialization_failure("query migration ledger", source))?;

    let mut applied = 0;
    let mut next_version = 1_i64;
    for row in rows {
        let (version, name, stored_hash) =
            row.map_err(|source| initialization_failure("decode migration ledger row", source))?;
        let Some(migration) = migrations.get(applied) else {
            return Err(
                incompatible_schema("database contains an unknown migration")
                    .field("migration_version", version)
                    .field("migration", name),
            );
        };
        if version != next_version {
            return Err(
                incompatible_schema("database migration versions are not contiguous")
                    .field("migration_version", version)
                    .field("expected_version", next_version),
            );
        }
        if name != migration.name {
            return Err(
                incompatible_schema("database migration name does not match this binary")
                    .field("migration_version", version)
                    .field("migration", name)
                    .field("expected_migration", migration.name),
            );
        }
        if stored_hash.as_slice() != migration_hash(migration.sql) {
            return Err(
                incompatible_schema("database migration SQL does not match this binary")
                    .field("migration_version", version)
                    .field("migration", migration.name),
            );
        }

        applied += 1;
        next_version = next_version.checked_add(1).ok_or_else(|| {
            incompatible_schema("database migration version exceeds the supported range")
        })?;
    }

    Ok(MigrationState {
        applied,
        next_version,
    })
}

/// Applies one schema change and records its identity in the same transaction.
fn apply_migration(
    transaction: &Transaction<'_>,
    migration: &Migration,
    version: i64,
) -> Result<(), Report<DatabaseError>> {
    transaction
        .execute_batch(migration.sql)
        .map_err(|source| migration_failure("apply schema change", migration, version, source))?;
    let hash = migration_hash(migration.sql);
    transaction
        .execute(
            "INSERT INTO schema_migrations (version, name, sql_sha256, applied_at)
             VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            rusqlite::params![version, migration.name, &hash[..]],
        )
        .map_err(|source| migration_failure("record migration", migration, version, source))?;
    Ok(())
}

/// Hashes the exact migration SQL bytes recorded by the current binary.
fn migration_hash(sql: &str) -> [u8; 32] {
    Sha256::digest(sql.as_bytes()).into()
}

/// Builds a report for a connection that could not be used.
fn unavailable(path: &Path, source: impl std::fmt::Display) -> Report<DatabaseError> {
    DatabaseError::Unavailable
        .report()
        .message(source.to_string())
        .field("path", path)
}

/// Builds a report for an `SQLite` initialization failure.
fn initialization_failure(
    operation: &'static str,
    source: impl std::fmt::Display,
) -> Report<DatabaseError> {
    DatabaseError::Initialization
        .report()
        .message(source.to_string())
        .field("operation", operation)
}

/// Adds migration identity to an initialization failure.
fn migration_failure(
    operation: &'static str,
    migration: &Migration,
    version: i64,
    source: impl std::fmt::Display,
) -> Report<DatabaseError> {
    initialization_failure(operation, source)
        .field("migration", migration.name)
        .field("migration_version", version)
}

/// Builds a report for an incompatible applied migration history.
fn incompatible_schema(message: &'static str) -> Report<DatabaseError> {
    DatabaseError::IncompatibleSchema.report().message(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates only the migration ledger when no domain migrations exist.
    #[test]
    fn empty_migrations_create_only_the_ledger() {
        let mut database = rusqlite::Connection::open_in_memory().unwrap();

        initialize(&mut database, &[]).unwrap();

        let mut statement = database
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap();
        let tables = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(tables, ["schema_migrations"]);
    }

    /// Records each ordered migration with its identity, SQL hash, and
    /// application time.
    #[test]
    fn migrations_record_their_identity_hash_and_time() {
        let migrations = [
            Migration {
                name: "first",
                sql: "SELECT 1;",
            },
            Migration {
                name: "second",
                sql: "SELECT 2;",
            },
        ];
        let mut database = rusqlite::Connection::open_in_memory().unwrap();

        initialize(&mut database, &migrations).unwrap();
        initialize(&mut database, &migrations).unwrap();

        let mut statement = database
            .prepare(
                "SELECT version, name, sql_sha256, applied_at
                 FROM schema_migrations ORDER BY version",
            )
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 1);
        assert_eq!(rows[0].1, "first");
        assert_eq!(rows[0].2, migration_hash("SELECT 1;"));
        assert!(rows[0].3.ends_with('Z'));
        assert_eq!(rows[1].0, 2);
        assert_eq!(rows[1].1, "second");
        assert_eq!(rows[1].2, migration_hash("SELECT 2;"));
        assert!(rows[1].3.ends_with('Z'));
    }

    /// Rejects an applied migration whose released SQL was later changed.
    #[test]
    fn changed_migration_sql_is_rejected() {
        let mut database = rusqlite::Connection::open_in_memory().unwrap();
        initialize(
            &mut database,
            &[Migration {
                name: "first",
                sql: "SELECT 1;",
            }],
        )
        .unwrap();

        let error = initialize(
            &mut database,
            &[Migration {
                name: "first",
                sql: "SELECT 2;",
            }],
        )
        .unwrap_err();

        assert_eq!(error.error(), &DatabaseError::IncompatibleSchema);
    }

    /// Leaves no ledger entry when a migration transaction fails.
    #[test]
    fn failed_migrations_are_not_recorded() {
        let mut database = rusqlite::Connection::open_in_memory().unwrap();

        let error = initialize(
            &mut database,
            &[Migration {
                name: "invalid",
                sql: "NOT VALID SQL;",
            }],
        )
        .unwrap_err();

        assert_eq!(error.error(), &DatabaseError::Initialization);
        let count = database
            .query_row("SELECT count(*) FROM schema_migrations", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }
}
