//! The table: one per tenant database, created by the migrator and checked at start-up.
//!
//! The tenant is the database, not a column: the table carries no tenant id and no statement of
//! this crate names one. A host that keeps several tenants in one database gives each its own
//! schema and points the pool's `search_path` at it (see the README, "Where the tenant comes
//! from").
//!
//! | Column | Holds |
//! | --- | --- |
//! | `entry` | the opaque entry id the adapter derives (tenant and salted subject hash) |
//! | `state` | `live` or `destroyed` |
//! | `key_id`, `key_version` | the key-encryption key that wrapped the subject key (a reference, not key material) |
//! | `wrapped` | the wrapped subject key, as the adapter gave it; null once destroyed |
//! | `created_at`, `destroyed_at` | the database's clock when the row was made and when it was destroyed |
//!
//! A check constraint ties the columns together: a live row has all of `key_id`, `key_version`
//! and `wrapped` and no `destroyed_at`; a destroyed row has none of the three and a
//! `destroyed_at`. A tombstone is never deleted: it is what keeps an old ciphertext from getting
//! a second key.
//!
//! This crate creates nothing at run time. The host passes [`extra_table`] to the migrator's
//! `PlanOptions::extra_tables`, where the table becomes a planned, reported, locked, audited step
//! (`extra_table:spot_journal_subject_key`, additive); at start-up it calls [`ensure_present`],
//! which only reads the catalogue.

use spot_pg_bridge::Bridge;
use spot_pg_migrate::{ExtraTable, Refusal};

use crate::error::{TableError, table_missing, table_read_failed};

/// The table.
pub const TABLE: &str = "spot_journal_subject_key";

/// The DDL the migrator runs: one idempotent statement, with no semicolon (the hook accepts none).
/// Names are unqualified: it runs under the connection's `search_path`. Every constraint is named,
/// after the table.
pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_journal_subject_key (\
    entry text NOT NULL, \
    state text NOT NULL, \
    key_id text, \
    key_version bigint, \
    wrapped bytea, \
    created_at timestamptz NOT NULL DEFAULT now(), \
    destroyed_at timestamptz, \
    CONSTRAINT spot_journal_subject_key_pkey PRIMARY KEY (entry), \
    CONSTRAINT spot_journal_subject_key_state_check \
        CHECK (state IN ('live', 'destroyed')), \
    CONSTRAINT spot_journal_subject_key_version_check \
        CHECK (key_version IS NULL OR (key_version >= 0 AND key_version <= 4294967295)), \
    CONSTRAINT spot_journal_subject_key_shape_check CHECK (\
        (state = 'live' \
            AND key_id IS NOT NULL AND key_version IS NOT NULL AND wrapped IS NOT NULL \
            AND destroyed_at IS NULL) \
        OR (state = 'destroyed' \
            AND key_id IS NULL AND key_version IS NULL AND wrapped IS NULL \
            AND destroyed_at IS NOT NULL)))";

/// The columns [`ensure_present`] expects, with their `information_schema` data type.
const COLUMNS: [(&str, &str); 7] = [
    ("entry", "text"),
    ("state", "text"),
    ("key_id", "text"),
    ("key_version", "bigint"),
    ("wrapped", "bytea"),
    ("created_at", "timestamp with time zone"),
    ("destroyed_at", "timestamp with time zone"),
];

/// The catalogue query: the name and type of every column of the table the `search_path`
/// resolves, or no row when the table is missing. It reads no row of the table.
const COLUMNS_SQL: &str = "\
SELECT c.column_name::text, c.data_type::text \
FROM information_schema.columns c \
WHERE (c.table_schema, c.table_name) = ( \
    SELECT n.nspname, r.relname FROM pg_class r \
    JOIN pg_namespace n ON n.oid = r.relnamespace \
    WHERE r.oid = to_regclass($1::text))";

/// The entry for the migrator's `PlanOptions::extra_tables`: this table, as one statement.
///
/// # Errors
/// The migrator's refusal if it rejects [`DDL`]. A test proves it does not, so a host never sees
/// one; the function returns it rather than panicking.
pub fn extra_table() -> Result<ExtraTable, Refusal> {
    ExtraTable::new(TABLE, vec![DDL.to_owned()])
}

/// Checks that the table exists with the expected columns. It creates nothing, takes no lock
/// and reads no row: one catalogue query.
///
/// # Errors
/// `journal_pii_secrets_pg.table_missing` when the table or one of its columns (by name and type)
/// is not there; `journal_pii_secrets_pg.store_failed` when the catalogue could not be read. The
/// message carries the bridge code or the SQLSTATE, never a value.
pub fn ensure_present(bridge: &Bridge) -> Result<(), TableError> {
    let found: Vec<(String, String)> = bridge
        .run_default(|conn| {
            Box::pin(async move {
                let rows = conn.query(COLUMNS_SQL, &[&TABLE]).await?;
                Ok(rows
                    .iter()
                    .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
                    .collect())
            })
        })
        .map_err(table_read_failed)?;
    let complete = COLUMNS
        .iter()
        .all(|(name, ty)| found.iter().any(|(n, t)| n == name && t == ty));
    if complete {
        Ok(())
    } else {
        Err(table_missing())
    }
}
