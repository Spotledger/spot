//! The DDL of the replay table and its index.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_sso_replay` | one row per claimed id: the namespace, the namespaced id and when it expires. The primary key is `(namespace, id)`, which is what makes the claim's `ON CONFLICT` resolve a race inside the database |
//!
//! Indexes:
//!
//! - `spot_sso_replay_pkey (namespace, id)`: the primary key, serving the claim's conflict check and
//!   the prune's delete-by-key.
//! - `spot_sso_replay_expires_idx (expires_at)`: serves [`prune`](crate::PgReplayStore::prune), which
//!   selects the expired rows in bounded batches.
//!
//! Constraints: the key size caps (`spot_sso_replay_keys`: namespace 1..=128 bytes, id 1..=1024 bytes)
//! keep a mis-built key from becoming an unbounded text index entry; an over-long key is refused
//! (fail closed) rather than truncated, because a truncated key would collide with a different id.
//!
//! The DDL is created by [`prepare`], which is idempotent and serialised across nodes by a
//! transaction-scoped advisory lock, so several nodes starting at once is safe.
//!
//! An earlier version created the table as `sso_replay` (no `spot_` prefix, which
//! `spot_pg_migrate` refuses). [`prepare`] renames it, its primary key, its constraints and its
//! index in the same transaction and keeps every row: the table is never dropped or recreated, so
//! the primary key that refuses a second use of an assertion id is in force at every instant.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;
use tokio_postgres::Transaction;

use crate::error::{migration, migration_pool, migration_refused};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x5353_4f50_0001;

/// The idempotent DDL. Every statement can run again.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_sso_replay (
    namespace  text NOT NULL,
    id         text NOT NULL,
    expires_at bigint NOT NULL,
    PRIMARY KEY (namespace, id),
    CONSTRAINT spot_sso_replay_keys CHECK (
        octet_length(namespace) BETWEEN 1 AND 128
        AND octet_length(id) BETWEEN 1 AND 1024)
);

CREATE INDEX IF NOT EXISTS spot_sso_replay_expires_idx ON spot_sso_replay (expires_at);
"#;

/// The table's name, with the `spot_` prefix `spot_pg_migrate` requires.
const NEW_TABLE: &str = "spot_sso_replay";
/// The unprefixed name an earlier version of this crate created.
const OLD_TABLE: &str = "sso_replay";
/// The index the old table carried, and its new name.
const OLD_INDEX: &str = "sso_replay_expires_idx";
const NEW_INDEX: &str = "spot_sso_replay_expires_idx";
/// The columns (`name:type`, sorted) the old table must have to be recognised as this crate's own.
const COLUMNS: [&str; 3] = ["expires_at:bigint", "id:text", "namespace:text"];

/// Whether `name` is a relation of the current schema (the schema `CREATE TABLE` writes to).
async fn exists(tx: &Transaction<'_>, name: &str) -> Result<bool, tokio_postgres::Error> {
    let row = tx
        .query_one(
            "SELECT to_regclass(quote_ident(current_schema()) || '.' || quote_ident($1)) IS NOT NULL",
            &[&name],
        )
        .await?;
    Ok(row.get(0))
}

/// A constant identifier as a quoted one (the names above are fixed ASCII; the escape is defensive).
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Renames the old table, its constraints (the primary key, the key-size check and, on
/// PostgreSQL 18, the named NOT NULL constraints) and its index, keeping every row. Runs inside
/// `prepare`'s transaction under its lock, so a failure leaves the old table exactly as it was.
async fn rename_old_table(tx: &Transaction<'_>) -> AppResult<()> {
    if !exists(tx, OLD_TABLE).await.map_err(|e| migration(&e))? {
        return Ok(());
    }
    if exists(tx, NEW_TABLE).await.map_err(|e| migration(&e))? {
        return Err(migration_refused(format!(
            "both `{OLD_TABLE}` and `{NEW_TABLE}` exist; nothing was renamed or dropped, resolve it by hand"
        )));
    }
    let found: Vec<String> = tx
        .query(
            "SELECT column_name::text || ':' || data_type::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 \
             ORDER BY column_name::text",
            &[&OLD_TABLE],
        )
        .await
        .map_err(|e| migration(&e))?
        .iter()
        .map(|r| r.get(0))
        .collect();
    if found.iter().map(String::as_str).ne(COLUMNS) {
        return Err(migration_refused(format!(
            "the table `{OLD_TABLE}` does not have the columns of this crate's table, so it was not renamed to `{NEW_TABLE}`"
        )));
    }
    let index_to_rename = exists(tx, OLD_INDEX).await.map_err(|e| migration(&e))?
        && !exists(tx, NEW_INDEX).await.map_err(|e| migration(&e))?;
    tx.batch_execute(&format!(
        "ALTER TABLE {} RENAME TO {}",
        quote(OLD_TABLE),
        quote(NEW_TABLE)
    ))
    .await
    .map_err(|e| migration(&e))?;
    // Constraint names carry the old table name as a prefix (the primary key's index follows its
    // constraint).
    let pattern = format!("{}\\_%", OLD_TABLE.replace('_', "\\_"));
    let names: Vec<String> = tx
        .query(
            "SELECT conname::text FROM pg_constraint \
             WHERE conrelid = to_regclass(quote_ident(current_schema()) || '.' || quote_ident($1)) \
             AND conname LIKE $2 ORDER BY conname",
            &[&NEW_TABLE, &pattern],
        )
        .await
        .map_err(|e| migration(&e))?
        .iter()
        .map(|r| r.get(0))
        .collect();
    for name in names {
        let Some(rest) = name.strip_prefix(OLD_TABLE) else {
            continue;
        };
        let renamed = format!("{NEW_TABLE}{rest}");
        tx.batch_execute(&format!(
            "ALTER TABLE {} RENAME CONSTRAINT {} TO {}",
            quote(NEW_TABLE),
            quote(&name),
            quote(&renamed)
        ))
        .await
        .map_err(|e| migration(&e))?;
    }
    if index_to_rename {
        tx.batch_execute(&format!(
            "ALTER INDEX {} RENAME TO {}",
            quote(OLD_INDEX),
            quote(NEW_INDEX)
        ))
        .await
        .map_err(|e| migration(&e))?;
    }
    Ok(())
}

/// Creates the table and its index if they are missing. Idempotent and safe to run from several
/// nodes at once (a transaction-scoped advisory lock serialises them).
///
/// A database that still holds the table as `sso_replay` is upgraded in the same transaction:
/// the table, its constraints and its index are renamed and every row is kept, so a restart across
/// the upgrade does not reopen the replay window. A second call finds only the new name and
/// changes nothing.
///
/// # Errors
/// `sso_pg.migration_failed` when the database cannot apply the DDL, when both the old and the new
/// table exist, or when the old table does not have this crate's columns (nothing is touched).
/// The messages carry names and a SQLSTATE, never a row value.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(|e| migration_pool(&e))?;
    let tx = conn.transaction().await.map_err(|e| migration(&e))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(|e| migration(&e))?;
    rename_old_table(&tx).await?;
    tx.batch_execute(DDL).await.map_err(|e| migration(&e))?;
    tx.commit().await.map_err(|e| migration(&e))
}
