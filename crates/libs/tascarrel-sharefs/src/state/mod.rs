//! Transactional namespace index for private upper state.
//!
//! [`State`] owns the internal `SQLite` connection and provides atomic node,
//! entry, whiteout, rename, and garbage-collection transitions to the
//! filesystem mechanics.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::ffi::OsStringExt as _;
use std::path::Path;

use rusqlite::Connection;
use rusqlite::OptionalExtension as _;
use rusqlite::Transaction;
use rusqlite::params;

use crate::ContentDigest;
use crate::EntryKind;
use crate::EntryVersion;
use crate::FileTime;
use crate::LowerLease;
use crate::ShareFsError;
use crate::ShareFsResult;

pub(crate) const ROOT_NODE_ID: i64 = 1;

const SCHEMA_VERSION: i64 = 1;
const ENTRY_PRESENT: i64 = 1;
const ENTRY_WHITEOUT: i64 = 2;

/// Durable namespace state for one private upper.
pub(crate) struct State {
    connection: Connection,
}

impl State {
    /// Opens and validates one namespace database.
    pub(crate) fn open(path: &Path) -> ShareFsResult<Self> {
        let connection = Connection::open(path)
            .map_err(|source| database_error("open the share namespace database", source))?;
        connection
            .execute_batch(
                "
                PRAGMA foreign_keys = ON;
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = FULL;
                PRAGMA trusted_schema = OFF;
                ",
            )
            .map_err(|source| database_error("configure the share namespace database", source))?;
        initialize_schema(&connection)?;
        validate_schema(&connection)?;
        Ok(Self { connection })
    }

    /// Returns one durable node.
    pub(crate) fn node(&self, id: i64) -> ShareFsResult<NodeRecord> {
        self.connection
            .query_row(
                "
                SELECT id, kind, object_name, symlink_target, mode,
                       modified_seconds, modified_nanoseconds, merge_lower,
                       opaque, metadata_changed
                FROM share_nodes
                WHERE id = ?1
                ",
                [id],
                decode_node,
            )
            .optional()
            .map_err(|source| database_error("read an upper node", source))?
            .ok_or_else(|| reportify::Report::new(ShareFsError::CorruptState))
    }

    /// Returns one durable directory-entry override.
    pub(crate) fn entry(&self, parent: i64, name: &OsStr) -> ShareFsResult<Option<EntryRecord>> {
        self.connection
            .query_row(
                "
                SELECT parent, name, state, node, base_kind, base_size,
                       base_mode, base_digest, base_mtime_seconds,
                       base_mtime_nanoseconds, base_ctime_seconds,
                       base_ctime_nanoseconds, base_device, base_inode
                FROM share_entries
                WHERE parent = ?1 AND name = ?2
                ",
                params![parent, name.as_bytes()],
                decode_entry,
            )
            .optional()
            .map_err(|source| database_error("read an upper directory entry", source))
    }

    /// Returns every override directly below a durable directory node.
    pub(crate) fn entries(&self, parent: i64) -> ShareFsResult<Vec<EntryRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT parent, name, state, node, base_kind, base_size,
                       base_mode, base_digest, base_mtime_seconds,
                       base_mtime_nanoseconds, base_ctime_seconds,
                       base_ctime_nanoseconds, base_device, base_inode
                FROM share_entries
                WHERE parent = ?1
                ORDER BY name
                ",
            )
            .map_err(|source| database_error("prepare an upper directory enumeration", source))?;
        statement
            .query_map([parent], decode_entry)
            .map_err(|source| database_error("enumerate upper directory entries", source))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| database_error("decode upper directory entries", source))
    }

    /// Installs a new node at one overridden directory entry.
    pub(crate) fn install_node(
        &mut self,
        parent: i64,
        name: &OsStr,
        node: &NewNode,
        base: Option<&BaseRecord>,
    ) -> ShareFsResult<NodeRecord> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|source| database_error("start an upper-node transaction", source))?;
        let id = insert_node(&transaction, node)?;
        put_present_entry(&transaction, parent, name, id, base)?;
        transaction
            .commit()
            .map_err(|source| database_error("commit an upper-node transaction", source))?;
        self.node(id)
    }

    /// Replaces an entry with a whiteout retaining its original base lease.
    pub(crate) fn remove_entry(
        &mut self,
        parent: i64,
        name: &OsStr,
        base: Option<&BaseRecord>,
    ) -> ShareFsResult<()> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|source| database_error("start an upper-removal transaction", source))?;
        match base {
            Some(base) => put_whiteout(&transaction, parent, name, base)?,
            None => {
                transaction
                    .execute(
                        "DELETE FROM share_entries WHERE parent = ?1 AND name = ?2",
                        params![parent, name.as_bytes()],
                    )
                    .map_err(|source| database_error("remove a transient upper entry", source))?;
            }
        }
        transaction
            .commit()
            .map_err(|source| database_error("commit an upper-removal transaction", source))
    }

    /// Moves one present override and preserves path-specific base leases.
    pub(crate) fn rename_entry(
        &mut self,
        source_parent: i64,
        source_name: &OsStr,
        destination_parent: i64,
        destination_name: &OsStr,
        destination_base: Option<&BaseRecord>,
    ) -> ShareFsResult<()> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|source| database_error("start an upper-rename transaction", source))?;
        let source = query_entry(&transaction, source_parent, source_name)?
            .ok_or_else(|| reportify::Report::new(ShareFsError::CorruptState))?;
        let node_id = match source.state {
            EntryState::Present(node_id) => node_id,
            EntryState::Whiteout => {
                return Err(reportify::Report::new(ShareFsError::CorruptState));
            }
        };
        match source.base.as_ref() {
            Some(base) => put_whiteout(&transaction, source_parent, source_name, base)?,
            None => {
                transaction
                    .execute(
                        "DELETE FROM share_entries WHERE parent = ?1 AND name = ?2",
                        params![source_parent, source_name.as_bytes()],
                    )
                    .map_err(|source| database_error("remove a transient rename source", source))?;
            }
        }
        put_present_entry(
            &transaction,
            destination_parent,
            destination_name,
            node_id,
            destination_base,
        )?;
        transaction
            .commit()
            .map_err(|source| database_error("commit an upper-rename transaction", source))
    }

    /// Marks a regular node as content-modified.
    pub(crate) fn mark_content_changed(
        &self,
        node: i64,
        modified_at: FileTime,
    ) -> ShareFsResult<()> {
        let changed = self
            .connection
            .execute(
                "
                UPDATE share_nodes
                SET modified_seconds = ?2,
                    modified_nanoseconds = ?3
                WHERE id = ?1 AND kind = ?4
                ",
                params![
                    node,
                    modified_at.seconds,
                    i64::from(modified_at.nanoseconds),
                    encode_kind(EntryKind::File)
                ],
            )
            .map_err(|source| database_error("mark upper file content changed", source))?;
        if changed != 1 {
            return Err(reportify::Report::new(ShareFsError::CorruptState));
        }
        Ok(())
    }

    /// Changes the logical mode retained for one upper node.
    pub(crate) fn set_mode(
        &self,
        node: i64,
        mode: u32,
        modified_at: FileTime,
    ) -> ShareFsResult<()> {
        let changed = self
            .connection
            .execute(
                "
                UPDATE share_nodes
                SET mode = ?2,
                    metadata_changed = 1,
                    modified_seconds = ?3,
                    modified_nanoseconds = ?4
                WHERE id = ?1
                ",
                params![
                    node,
                    i64::from(mode),
                    modified_at.seconds,
                    i64::from(modified_at.nanoseconds)
                ],
            )
            .map_err(|source| database_error("change upper node metadata", source))?;
        if changed != 1 {
            return Err(reportify::Report::new(ShareFsError::CorruptState));
        }
        Ok(())
    }

    /// Removes every retained override and restores the dynamic lower merge.
    pub(crate) fn clear(&mut self) -> ShareFsResult<()> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|source| database_error("start upper-state reset", source))?;
        transaction
            .execute("DELETE FROM share_entries", [])
            .map_err(|source| database_error("remove upper directory entries", source))?;
        transaction
            .execute("DELETE FROM share_nodes WHERE id != ?1", [ROOT_NODE_ID])
            .map_err(|source| database_error("remove upper nodes", source))?;
        transaction
            .execute(
                "
                UPDATE share_nodes
                SET mode = 493,
                    modified_seconds = 0,
                    modified_nanoseconds = 0,
                    merge_lower = 1,
                    opaque = 0,
                    metadata_changed = 0
                WHERE id = ?1
                ",
                [ROOT_NODE_ID],
            )
            .map_err(|source| database_error("restore upper root node", source))?;
        transaction
            .commit()
            .map_err(|source| database_error("commit upper-state reset", source))
    }

    /// Removes unreachable namespace nodes and returns their object names.
    pub(crate) fn collect_unreachable_objects(&mut self) -> ShareFsResult<Vec<String>> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|source| database_error("start upper-state collection", source))?;
        let unreachable = {
            let mut statement = transaction
                .prepare(
                    "
                    WITH RECURSIVE reachable(id) AS (
                        SELECT ?1
                        UNION
                        SELECT entry.node
                        FROM share_entries AS entry
                        JOIN reachable ON entry.parent = reachable.id
                        WHERE entry.state = ?2 AND entry.node IS NOT NULL
                    )
                    SELECT id, object_name
                    FROM share_nodes
                    WHERE id NOT IN (SELECT id FROM reachable)
                    ",
                )
                .map_err(|source| database_error("prepare upper-state collection", source))?;
            statement
                .query_map(params![ROOT_NODE_ID, ENTRY_PRESENT], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .map_err(|source| database_error("find unreachable upper nodes", source))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|source| database_error("decode unreachable upper nodes", source))?
        };
        for (id, _) in &unreachable {
            transaction
                .execute("DELETE FROM share_entries WHERE parent = ?1", [id])
                .map_err(|source| database_error("remove unreachable upper entries", source))?;
        }
        for (id, _) in &unreachable {
            transaction
                .execute("DELETE FROM share_nodes WHERE id = ?1", [id])
                .map_err(|source| database_error("remove an unreachable upper node", source))?;
        }
        transaction
            .commit()
            .map_err(|source| database_error("commit upper-state collection", source))?;
        Ok(unreachable
            .into_iter()
            .filter_map(|(_, object_name)| object_name)
            .collect())
    }

    /// Returns every regular-file object referenced by durable nodes.
    pub(crate) fn referenced_objects(&self) -> ShareFsResult<BTreeSet<String>> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT object_name
                FROM share_nodes
                WHERE object_name IS NOT NULL
                ORDER BY object_name
                ",
            )
            .map_err(|source| database_error("prepare an upper-object inventory", source))?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|source| database_error("inventory upper objects", source))?
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(|source| database_error("decode the upper-object inventory", source))
    }

    /// Checkpoints committed namespace state into the main database file.
    pub(crate) fn checkpoint(&self) -> ShareFsResult<()> {
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|source| database_error("checkpoint the share namespace database", source))
    }
}

/// One durable upper node.
#[derive(Clone, Debug)]
pub(crate) struct NodeRecord {
    pub(crate) id: i64,
    pub(crate) kind: EntryKind,
    pub(crate) object_name: Option<String>,
    pub(crate) symlink_target: Option<OsString>,
    pub(crate) mode: u32,
    pub(crate) modified_at: FileTime,
    pub(crate) merge_lower: bool,
    pub(crate) opaque: bool,
    pub(crate) metadata_changed: bool,
}

/// Values used to create one durable upper node.
pub(crate) struct NewNode {
    pub(crate) kind: EntryKind,
    pub(crate) object_name: Option<String>,
    pub(crate) symlink_target: Option<OsString>,
    pub(crate) mode: u32,
    pub(crate) modified_at: FileTime,
    pub(crate) merge_lower: bool,
    pub(crate) opaque: bool,
    pub(crate) metadata_changed: bool,
}

/// One durable directory-entry override.
#[derive(Clone, Debug)]
pub(crate) struct EntryRecord {
    pub(crate) _parent: i64,
    pub(crate) name: OsString,
    pub(crate) state: EntryState,
    pub(crate) base: Option<BaseRecord>,
}

/// Durable disposition of an overridden name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryState {
    /// Name resolves to an upper node.
    Present(i64),
    /// Name is hidden even when it exists in the lower directory.
    Whiteout,
}

/// Internal name for a captured public lower lease.
pub(crate) type BaseRecord = LowerLease;

fn initialize_schema(connection: &Connection) -> ShareFsResult<()> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|source| database_error("read the share namespace schema version", source))?;
    if version == 0 {
        connection
            .execute_batch(
                "
                BEGIN IMMEDIATE;
                CREATE TABLE share_nodes (
                    id INTEGER PRIMARY KEY,
                    kind INTEGER NOT NULL,
                    object_name TEXT UNIQUE,
                    symlink_target BLOB,
                    mode INTEGER NOT NULL,
                    modified_seconds INTEGER NOT NULL,
                    modified_nanoseconds INTEGER NOT NULL,
                    merge_lower INTEGER NOT NULL,
                    opaque INTEGER NOT NULL,
                    metadata_changed INTEGER NOT NULL
                );
                CREATE TABLE share_entries (
                    parent INTEGER NOT NULL REFERENCES share_nodes(id),
                    name BLOB NOT NULL,
                    state INTEGER NOT NULL,
                    node INTEGER REFERENCES share_nodes(id),
                    base_kind INTEGER,
                    base_size BLOB,
                    base_mode INTEGER,
                    base_digest BLOB,
                    base_mtime_seconds INTEGER,
                    base_mtime_nanoseconds INTEGER,
                    base_ctime_seconds INTEGER,
                    base_ctime_nanoseconds INTEGER,
                    base_device BLOB,
                    base_inode BLOB,
                    PRIMARY KEY (parent, name),
                    CHECK (
                        (state = 1 AND node IS NOT NULL)
                        OR (state = 2 AND node IS NULL)
                    )
                ) WITHOUT ROWID;
                INSERT INTO share_nodes (
                    id, kind, object_name, symlink_target, mode,
                    modified_seconds, modified_nanoseconds, merge_lower,
                    opaque, metadata_changed
                ) VALUES (1, 2, NULL, NULL, 493, 0, 0, 1, 0, 0);
                PRAGMA user_version = 1;
                COMMIT;
                ",
            )
            .map_err(|source| database_error("initialize the share namespace schema", source))?;
    } else if version != SCHEMA_VERSION {
        return Err(reportify::Report::new(ShareFsError::CorruptState));
    }
    Ok(())
}

fn validate_schema(connection: &Connection) -> ShareFsResult<()> {
    let integrity = connection
        .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
        .map_err(|source| database_error("check the share namespace database", source))?;
    if integrity != "ok" {
        return Err(reportify::Report::new(ShareFsError::CorruptState));
    }
    let root_kind = connection
        .query_row(
            "SELECT kind FROM share_nodes WHERE id = ?1",
            [ROOT_NODE_ID],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|source| database_error("validate the share namespace root", source))?;
    if root_kind != Some(encode_kind(EntryKind::Directory)) {
        return Err(reportify::Report::new(ShareFsError::CorruptState));
    }
    let foreign_key_error = connection
        .query_row("PRAGMA foreign_key_check", [], |_| Ok(()))
        .optional()
        .map_err(|source| database_error("validate share namespace references", source))?;
    if foreign_key_error.is_some() {
        return Err(reportify::Report::new(ShareFsError::CorruptState));
    }
    Ok(())
}

fn insert_node(transaction: &Transaction<'_>, node: &NewNode) -> ShareFsResult<i64> {
    transaction
        .execute(
            "
            INSERT INTO share_nodes (
                kind, object_name, symlink_target, mode,
                modified_seconds, modified_nanoseconds, merge_lower,
                opaque, metadata_changed
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ",
            params![
                encode_kind(node.kind),
                node.object_name,
                node.symlink_target
                    .as_ref()
                    .map(|target| target.as_bytes().to_vec()),
                i64::from(node.mode),
                node.modified_at.seconds,
                i64::from(node.modified_at.nanoseconds),
                node.merge_lower,
                node.opaque,
                node.metadata_changed
            ],
        )
        .map_err(|source| database_error("insert an upper node", source))?;
    Ok(transaction.last_insert_rowid())
}

fn put_present_entry(
    transaction: &Transaction<'_>,
    parent: i64,
    name: &OsStr,
    node: i64,
    base: Option<&BaseRecord>,
) -> ShareFsResult<()> {
    put_entry(transaction, parent, name, ENTRY_PRESENT, Some(node), base)
}

fn put_whiteout(
    transaction: &Transaction<'_>,
    parent: i64,
    name: &OsStr,
    base: &BaseRecord,
) -> ShareFsResult<()> {
    put_entry(transaction, parent, name, ENTRY_WHITEOUT, None, Some(base))
}

#[expect(
    clippy::similar_names,
    reason = "The mtime and ctime bindings mirror distinct lease columns."
)]
fn put_entry(
    transaction: &Transaction<'_>,
    parent: i64,
    name: &OsStr,
    state: i64,
    node: Option<i64>,
    base: Option<&BaseRecord>,
) -> ShareFsResult<()> {
    let (
        base_kind,
        base_size,
        base_mode,
        base_digest,
        base_mtime_seconds,
        base_mtime_nanoseconds,
        base_ctime_seconds,
        base_ctime_nanoseconds,
        base_device,
        base_inode,
    ) = match base {
        Some(base) => (
            Some(encode_kind(base.version.kind)),
            Some(encode_u64(base.version.size).to_vec()),
            Some(i64::from(base.version.mode)),
            base.version.content_digest.map(|digest| digest.0.to_vec()),
            Some(base.modified_at.seconds),
            Some(i64::from(base.modified_at.nanoseconds)),
            Some(base.changed_at.seconds),
            Some(i64::from(base.changed_at.nanoseconds)),
            Some(encode_u64(base.device).to_vec()),
            Some(encode_u64(base.inode).to_vec()),
        ),
        None => (None, None, None, None, None, None, None, None, None, None),
    };
    transaction
        .execute(
            "
            INSERT INTO share_entries (
                parent, name, state, node, base_kind, base_size,
                base_mode, base_digest, base_mtime_seconds,
                base_mtime_nanoseconds, base_ctime_seconds,
                base_ctime_nanoseconds, base_device, base_inode
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
            )
            ON CONFLICT (parent, name) DO UPDATE SET
                state = excluded.state,
                node = excluded.node,
                base_kind = excluded.base_kind,
                base_size = excluded.base_size,
                base_mode = excluded.base_mode,
                base_digest = excluded.base_digest,
                base_mtime_seconds = excluded.base_mtime_seconds,
                base_mtime_nanoseconds = excluded.base_mtime_nanoseconds,
                base_ctime_seconds = excluded.base_ctime_seconds,
                base_ctime_nanoseconds = excluded.base_ctime_nanoseconds,
                base_device = excluded.base_device,
                base_inode = excluded.base_inode
            ",
            params![
                parent,
                name.as_bytes(),
                state,
                node,
                base_kind,
                base_size,
                base_mode,
                base_digest,
                base_mtime_seconds,
                base_mtime_nanoseconds,
                base_ctime_seconds,
                base_ctime_nanoseconds,
                base_device,
                base_inode
            ],
        )
        .map_err(|source| database_error("write an upper directory entry", source))?;
    Ok(())
}

fn query_entry(
    transaction: &Transaction<'_>,
    parent: i64,
    name: &OsStr,
) -> ShareFsResult<Option<EntryRecord>> {
    transaction
        .query_row(
            "
            SELECT parent, name, state, node, base_kind, base_size,
                   base_mode, base_digest, base_mtime_seconds,
                   base_mtime_nanoseconds, base_ctime_seconds,
                   base_ctime_nanoseconds, base_device, base_inode
            FROM share_entries
            WHERE parent = ?1 AND name = ?2
            ",
            params![parent, name.as_bytes()],
            decode_entry,
        )
        .optional()
        .map_err(|source| database_error("read an upper directory entry", source))
}

fn decode_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRecord> {
    let kind = decode_kind(row.get(1)?)?;
    let symlink_target = row.get::<_, Option<Vec<u8>>>(3)?.map(OsString::from_vec);
    Ok(NodeRecord {
        id: row.get(0)?,
        kind,
        object_name: row.get(2)?,
        symlink_target,
        mode: decode_u32(row.get(4)?)?,
        modified_at: FileTime {
            seconds: row.get(5)?,
            nanoseconds: decode_u32(row.get(6)?)?,
        },
        merge_lower: row.get(7)?,
        opaque: row.get(8)?,
        metadata_changed: row.get(9)?,
    })
}

fn decode_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRecord> {
    let state_value = row.get::<_, i64>(2)?;
    let node = row.get::<_, Option<i64>>(3)?;
    let state = match (state_value, node) {
        (ENTRY_PRESENT, Some(node)) => EntryState::Present(node),
        (ENTRY_WHITEOUT, None) => EntryState::Whiteout,
        _ => return Err(invalid_database_value(2, "invalid upper entry state")),
    };
    let base_kind = row.get::<_, Option<i64>>(4)?;
    let base = match base_kind {
        Some(kind) => {
            let digest = decode_digest(row.get::<_, Option<Vec<u8>>>(7)?)?;
            Some(BaseRecord {
                version: EntryVersion {
                    kind: decode_kind(kind)?,
                    size: decode_u64_blob(row.get::<_, Option<Vec<u8>>>(5)?, 5)?,
                    mode: decode_optional_u32(row.get::<_, Option<i64>>(6)?, 6)?,
                    content_digest: digest,
                },
                modified_at: FileTime {
                    seconds: decode_required(row.get(8)?, 8)?,
                    nanoseconds: decode_optional_u32(row.get(9)?, 9)?,
                },
                changed_at: FileTime {
                    seconds: decode_required(row.get(10)?, 10)?,
                    nanoseconds: decode_optional_u32(row.get(11)?, 11)?,
                },
                device: decode_u64_blob(row.get::<_, Option<Vec<u8>>>(12)?, 12)?,
                inode: decode_u64_blob(row.get::<_, Option<Vec<u8>>>(13)?, 13)?,
            })
        }
        None => None,
    };
    Ok(EntryRecord {
        _parent: row.get(0)?,
        name: OsString::from_vec(row.get(1)?),
        state,
        base,
    })
}

const fn encode_kind(kind: EntryKind) -> i64 {
    match kind {
        EntryKind::File => 1,
        EntryKind::Directory => 2,
        EntryKind::Symlink => 3,
    }
}

fn decode_kind(value: i64) -> rusqlite::Result<EntryKind> {
    match value {
        1 => Ok(EntryKind::File),
        2 => Ok(EntryKind::Directory),
        3 => Ok(EntryKind::Symlink),
        _ => Err(invalid_database_value(0, "invalid upper node kind")),
    }
}

const fn encode_u64(value: u64) -> [u8; 8] {
    value.to_be_bytes()
}

fn decode_u64_blob(value: Option<Vec<u8>>, column: usize) -> rusqlite::Result<u64> {
    let value = value.ok_or_else(|| invalid_database_value(column, "missing unsigned value"))?;
    let bytes: [u8; 8] = value
        .try_into()
        .map_err(|_| invalid_database_value(column, "invalid unsigned value"))?;
    Ok(u64::from_be_bytes(bytes))
}

fn decode_digest(value: Option<Vec<u8>>) -> rusqlite::Result<Option<ContentDigest>> {
    value
        .map(|value| {
            value
                .try_into()
                .map(ContentDigest)
                .map_err(|_| invalid_database_value(7, "invalid content digest"))
        })
        .transpose()
}

fn decode_u32(value: i64) -> rusqlite::Result<u32> {
    u32::try_from(value).map_err(|_| invalid_database_value(0, "integer does not fit in u32"))
}

fn decode_optional_u32(value: Option<i64>, column: usize) -> rusqlite::Result<u32> {
    value
        .ok_or_else(|| invalid_database_value(column, "missing integer value"))
        .and_then(|value| {
            u32::try_from(value)
                .map_err(|_| invalid_database_value(column, "integer does not fit in u32"))
        })
}

fn decode_required<T>(value: Option<T>, column: usize) -> rusqlite::Result<T> {
    value.ok_or_else(|| invalid_database_value(column, "missing required value"))
}

fn invalid_database_value(column: usize, message: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Blob,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    )
}

fn database_error(
    action: &'static str,
    source: rusqlite::Error,
) -> reportify::Report<ShareFsError> {
    reportify::Report::new(ShareFsError::Database { action, source })
}
