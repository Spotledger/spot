//! The DDL of this crate's tables, its indexes, and the idempotent `prepare`.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_graph_edge` | one row per edge: its type id, owner and two ends, and its properties as `jsonb`; the primary key is the whole edge identity, so an edge is written whole |
//! | `spot_edge_drift_report` | one row per verify run: when it ran, how many documents it checked, how much drift it found, and whether the tenant was clean |
//! | `spot_edge_drift_entry` | one row per drift the run found, keyed by `(report_key, ordinal)`; `expected` is the edge the documents imply and `found` the edge the store holds |
//! | `spot_edge_repair_audit` | one row per repair batch: what the batch added, removed and updated, and when |
//!
//! Indexes (ADR-045): `spot_graph_edge_owner_idx` serves the bounded read the hot path uses
//! (`scan_by_source`, and the verify scan's per-page read); the `(report_key, ordinal)` primary
//! key serves reading a report's entries in order.
//!
//! These tables are this crate's own. The document tables, and the graph traversal over the
//! link columns (WP-STO-07), are the storage backend's; the edge store mirrors the link set so
//! the invariant "edge set equals link set" can be checked and repaired.

//! Every table starts with `spot_`, the prefix `spot_pg_migrate` accepts as the platform's own.
//! Databases created before the prefix existed hold the four tables under their old names
//! (`graph_edge`, `edge_drift_report`, `edge_drift_entry`, `edge_repair_audit`); [`prepare`]
//! renames them, with their index and constraint names, in the one transaction that also creates
//! anything missing, keeps every row, and is a no-op once done. A database that holds an old and
//! a new table of the same pair is refused with both names and nothing is dropped.
//!
//! One database is one site (ADR-34): the tables carry no site column, and the tenant boundary is
//! the database the transaction belongs to.

use tokio_postgres::{Client, Transaction};

use crate::error::{EdgePgError, Result};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x4544_4745_0001;

/// The idempotent DDL. Every statement is `IF NOT EXISTS`.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_graph_edge (
    edge_type    text NOT NULL,
    owner_entity text NOT NULL,
    owner_id     uuid NOT NULL,
    from_entity  text NOT NULL,
    from_id      uuid NOT NULL,
    to_entity    text NOT NULL,
    to_id        uuid NOT NULL,
    properties   jsonb NOT NULL DEFAULT '{}'::jsonb,
    PRIMARY KEY (edge_type, owner_entity, owner_id, from_entity, from_id, to_entity, to_id)
);

CREATE INDEX IF NOT EXISTS spot_graph_edge_owner_idx
    ON spot_graph_edge (owner_entity, owner_id);

CREATE TABLE IF NOT EXISTS spot_edge_drift_report (
    report_key        text PRIMARY KEY,
    generated_at      timestamptz NOT NULL,
    checked_documents bigint NOT NULL CHECK (checked_documents >= 0),
    drift_count       bigint NOT NULL CHECK (drift_count >= 0),
    clean             boolean NOT NULL
);

CREATE TABLE IF NOT EXISTS spot_edge_drift_entry (
    report_key   text NOT NULL REFERENCES spot_edge_drift_report (report_key) ON DELETE CASCADE,
    ordinal      integer NOT NULL CHECK (ordinal >= 0),
    kind         text NOT NULL CHECK (kind IN ('missing', 'extra', 'mismatch')),
    edge_type    text NOT NULL,
    owner_entity text NOT NULL,
    owner_id     uuid NOT NULL,
    from_entity  text NOT NULL,
    from_id      uuid NOT NULL,
    to_entity    text NOT NULL,
    to_id        uuid NOT NULL,
    expected     jsonb,
    found        jsonb,
    PRIMARY KEY (report_key, ordinal)
);

CREATE TABLE IF NOT EXISTS spot_edge_repair_audit (
    report_key  text NOT NULL,
    batch_no    integer NOT NULL CHECK (batch_no >= 0),
    repaired_at timestamptz NOT NULL,
    added       integer NOT NULL CHECK (added >= 0),
    removed     integer NOT NULL CHECK (removed >= 0),
    updated     integer NOT NULL CHECK (updated >= 0),
    PRIMARY KEY (report_key, batch_no)
);
"#;

/// The tables this crate used to create without the prefix, each with the table that replaces it
/// and the columns the old table must have to be recognised as this crate's own.
const RENAMES: [(&str, &str, &[&str]); 4] = [
    (
        "graph_edge",
        "spot_graph_edge",
        &[
            "edge_type",
            "from_entity",
            "from_id",
            "owner_entity",
            "owner_id",
            "properties",
            "to_entity",
            "to_id",
        ],
    ),
    (
        "edge_drift_report",
        "spot_edge_drift_report",
        &[
            "checked_documents",
            "clean",
            "drift_count",
            "generated_at",
            "report_key",
        ],
    ),
    (
        "edge_drift_entry",
        "spot_edge_drift_entry",
        &[
            "expected",
            "edge_type",
            "found",
            "from_entity",
            "from_id",
            "kind",
            "ordinal",
            "owner_entity",
            "owner_id",
            "report_key",
            "to_entity",
            "to_id",
        ],
    ),
    (
        "edge_repair_audit",
        "spot_edge_repair_audit",
        &[
            "added",
            "batch_no",
            "removed",
            "repaired_at",
            "report_key",
            "updated",
        ],
    ),
];

/// The old table that carried [`INDEX_RENAME`].
const INDEX_RENAME_TABLE: &str = "graph_edge";

/// The index the old `graph_edge` table carried, and its new name.
const INDEX_RENAME: (&str, &str) = ("graph_edge_owner_idx", "spot_graph_edge_owner_idx");

/// Creates the tables and indexes if they are missing. Idempotent and safe from several nodes at
/// once (a transaction-scoped advisory lock serialises them).
///
/// # Errors
/// `edge_pg.migration_failed` when the database cannot apply the DDL.
pub async fn prepare(client: &mut Client) -> Result<()> {
    let failed = |e: tokio_postgres::Error| EdgePgError::MigrationFailed {
        message: e.to_string(),
    };
    let tx = client.transaction().await.map_err(failed)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(failed)?;
    rename_old_tables(&tx).await?;
    tx.batch_execute(DDL).await.map_err(failed)?;
    tx.commit().await.map_err(failed)
}

/// Whether `name` is a relation of the current schema (the schema `CREATE TABLE` writes to).
async fn exists(tx: &Transaction<'_>, name: &str) -> Result<bool> {
    let row = tx
        .query_one(
            "SELECT to_regclass(quote_ident(current_schema()) || '.' || quote_ident($1)) IS NOT NULL",
            &[&name],
        )
        .await
        .map_err(migration_failed)?;
    Ok(row.get(0))
}

fn migration_failed(e: tokio_postgres::Error) -> EdgePgError {
    EdgePgError::MigrationFailed {
        message: e.to_string(),
    }
}

/// Renames the pre-prefix tables, their index and their constraints, all or nothing: the caller's
/// transaction commits or rolls back every rename together with the DDL after it.
async fn rename_old_tables(tx: &Transaction<'_>) -> Result<()> {
    // Check every pair first, so a refusal happens before the first rename.
    let mut to_rename = Vec::new();
    for (old, new, columns) in RENAMES {
        if !exists(tx, old).await? {
            continue;
        }
        if exists(tx, new).await? {
            return Err(EdgePgError::MigrationFailed {
                message: format!(
                    "both `{old}` and `{new}` exist; nothing was renamed or dropped, resolve it by hand"
                ),
            });
        }
        let found: Vec<String> = tx
            .query(
                "SELECT column_name::text FROM information_schema.columns \
                 WHERE table_schema = current_schema() AND table_name = $1 \
                 ORDER BY column_name",
                &[&old],
            )
            .await
            .map_err(migration_failed)?
            .iter()
            .map(|r| r.get(0))
            .collect();
        let mut wanted: Vec<&str> = columns.to_vec();
        wanted.sort_unstable();
        if found.iter().map(String::as_str).ne(wanted.iter().copied()) {
            return Err(EdgePgError::MigrationFailed {
                message: format!(
                    "the table `{old}` does not have the columns of this crate's table, so it was not renamed to `{new}`"
                ),
            });
        }
        to_rename.push((old, new));
    }
    let renames_edge_table = to_rename.iter().any(|(old, _)| *old == INDEX_RENAME_TABLE);
    for (old, new) in to_rename {
        tx.batch_execute(&format!(
            "ALTER TABLE {} RENAME TO {}",
            quote(old),
            quote(new)
        ))
        .await
        .map_err(migration_failed)?;
        // Constraint names (and the primary key's index) carry the old table name as a prefix.
        let pattern = format!("{}\\_%", old.replace('_', "\\_"));
        let names: Vec<String> = tx
            .query(
                "SELECT conname::text FROM pg_constraint \
                 WHERE conrelid = to_regclass(quote_ident(current_schema()) || '.' || quote_ident($1)) \
                 AND conname LIKE $2 ORDER BY conname",
                &[&new, &pattern],
            )
            .await
            .map_err(migration_failed)?
            .iter()
            .map(|r| r.get(0))
            .collect();
        for name in names {
            let Some(rest) = name.strip_prefix(old) else {
                continue;
            };
            let renamed = format!("{new}{rest}");
            tx.batch_execute(&format!(
                "ALTER TABLE {} RENAME CONSTRAINT {} TO {}",
                quote(new),
                quote(&name),
                quote(&renamed)
            ))
            .await
            .map_err(migration_failed)?;
        }
    }
    let (old_index, new_index) = INDEX_RENAME;
    if renames_edge_table && exists(tx, old_index).await? && !exists(tx, new_index).await? {
        tx.batch_execute(&format!(
            "ALTER INDEX {} RENAME TO {}",
            quote(old_index),
            quote(new_index)
        ))
        .await
        .map_err(migration_failed)?;
    }
    Ok(())
}

/// A validated storage name as a quoted identifier. Names are `[a-z][a-z0-9_]*`, so quoting is
/// safe; the escape is defensive.
#[must_use]
pub fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
