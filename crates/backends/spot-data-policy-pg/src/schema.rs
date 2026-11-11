//! The tables, created idempotently in the tenant's own schema.
//!
//! Every statement runs with the tenant's schema as the **only** entry of `search_path`, set
//! transaction-locally from a name derived from the tenant id (never from a tenant-chosen string),
//! so the SQL carries no schema name and a statement can only ever see the tenant it was built
//! for. The bridge resets nothing between calls, which is why the setting is `set_config(.., true)`
//! inside a transaction and never a session `SET`.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_retention_outcome` | one row per (run, record): status, attempt and the full outcome document; the primary key is what makes an item apply once |
//! | `spot_retention_checkpoint` | one row per run: the checkpoint document, replaced on save |
//! | `spot_deletion_proof` | one row per deleted record: the proof document; written once, never updated or deleted, and not tied to the record's lifetime |
//!
//! No extension is required.

use deadpool_postgres::Transaction;
use spot_pg_bridge::PooledConn;

use crate::error::OpError;

/// The tables of one tenant schema, in creation order.
pub const TABLES: [&str; 3] = [
    "spot_retention_outcome",
    "spot_retention_checkpoint",
    "spot_deletion_proof",
];

/// Advisory lock key held (transaction-scoped) while the DDL runs, so two processes preparing at
/// once do not race on `CREATE TABLE IF NOT EXISTS`.
const PREPARE_LOCK: i64 = 0x5350_4452_5001;

/// The idempotent DDL. Names are unqualified: it runs under the tenant's `search_path`.
pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_retention_outcome (\
    run_id text NOT NULL, \
    record_id text NOT NULL, \
    status text NOT NULL CHECK (status IN ('applied', 'would_apply', 'skipped', 'failed')), \
    attempt bigint NOT NULL CHECK (attempt >= 0), \
    outcome jsonb NOT NULL, \
    PRIMARY KEY (run_id, record_id)); \
CREATE TABLE IF NOT EXISTS spot_retention_checkpoint (\
    run_id text PRIMARY KEY, \
    checkpoint jsonb NOT NULL); \
CREATE TABLE IF NOT EXISTS spot_deletion_proof (\
    record_id text PRIMARY KEY, \
    proof jsonb NOT NULL);";

/// Starts a transaction whose `search_path` is `schema` alone.
pub(crate) async fn scoped<'c>(
    conn: &'c mut PooledConn,
    schema: &str,
) -> Result<Transaction<'c>, OpError> {
    let tx = conn.transaction().await?;
    tx.query_one("SELECT set_config('search_path', $1, true)", &[&schema])
        .await?;
    Ok(tx)
}

/// Creates the tables under the advisory lock.
pub(crate) async fn prepare(conn: &mut PooledConn, schema: &str) -> Result<(), OpError> {
    let tx = scoped(conn, schema).await?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await?;
    tx.batch_execute(DDL).await?;
    tx.commit().await?;
    Ok(())
}
