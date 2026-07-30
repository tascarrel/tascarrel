//! Ordered storage-owned guest database schema migrations.
//!
//! Each applied entry is recorded in `schema_migrations` with its position,
//! stable name, the SHA-256 hash of its exact SQL bytes, and its application
//! time. Entries are append-only so a released migration identity never
//! changes.

/// Domain schema changes in installation order.
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        name: "create_pod_states",
        sql: include_str!("migrations/0001_create_pod_states.sql"),
    },
    Migration {
        name: "create_image_states",
        sql: include_str!("migrations/0002_create_image_states.sql"),
    },
    Migration {
        name: "create_chat_states",
        sql: include_str!("migrations/0003_create_chat_states.sql"),
    },
    Migration {
        name: "add_chat_attention",
        sql: include_str!("migrations/0004_add_chat_attention.sql"),
    },
    Migration {
        name: "add_chat_cost_centers",
        sql: include_str!("migrations/0005_add_chat_cost_centers.sql"),
    },
    Migration {
        name: "add_chat_purpose",
        sql: include_str!("migrations/0006_add_chat_purpose.sql"),
    },
];

/// One schema change applied in an immediate transaction.
pub(crate) struct Migration {
    /// Stable diagnostic name recorded in the migration ledger.
    pub(crate) name: &'static str,
    /// SQL applied atomically before the ledger row is inserted.
    pub(crate) sql: &'static str,
}
