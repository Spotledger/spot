//! The DDL of the two migration tables [`PgCaseStore::apply_migration`] writes (WP-BPM-09.T8,
//! ADR-082).
//!
//! `spot_bpm_migration` is the migration record the host (`spot-cli`, `process_versions.rs`)
//! creates, leases and advances; `spot_bpm_migration_audit` holds one row per moved Case, keyed by
//! `(tenant, case_id, plan_digest)` so a replay cannot write it twice. A storage crate owns its
//! tables, so the DDL lives here: the host and this crate's own tests call
//! [`prepare_migration_tables`] and nothing else holds a copy.
//!
//! The tables are **opt-in**: [`PgCaseStore::prepare`](crate::PgCaseStore::prepare) does not create
//! them, because a process that never migrates a Case does not need them. A host that migrates
//! calls [`prepare_migration_tables`] at start-up (the `process_versions` module does).
//!
//! `tests/migration_schema.rs` pins the column list and fails when the host's SQL names a column
//! the DDL lacks.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;

use crate::error;

/// The lock key that serialises concurrent `prepare_migration_tables` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x6270_6d70_6703;

/// The idempotent DDL: the two tables and the unique index of the migration idempotency key.
/// Every statement is `IF NOT EXISTS`.
pub const MIGRATION_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_bpm_migration (\
  tenant text NOT NULL, id text NOT NULL, process text NOT NULL, from_version integer NOT NULL, \
  to_version integer NOT NULL, mapping jsonb NOT NULL, state text NOT NULL, \
  cursor text NOT NULL DEFAULT '', moved bigint NOT NULL DEFAULT 0, \
  skipped bigint NOT NULL DEFAULT 0, failed bigint NOT NULL DEFAULT 0, started_by text NOT NULL, \
  idem_key text, started_at timestamptz NOT NULL, ended_at timestamptz, lease_holder text, \
  lease_until timestamptz, PRIMARY KEY (tenant, id));\
CREATE UNIQUE INDEX IF NOT EXISTS spot_bpm_migration_idem_idx \
  ON spot_bpm_migration (tenant, idem_key) WHERE idem_key IS NOT NULL;\
CREATE TABLE IF NOT EXISTS spot_bpm_migration_audit (\
  tenant text NOT NULL, migration_id text NOT NULL, case_id text NOT NULL, \
  plan_digest text NOT NULL, payload text NOT NULL, at timestamptz NOT NULL, \
  PRIMARY KEY (tenant, case_id, plan_digest));";

/// Creates the two migration tables and the idempotency index if missing. Idempotent; safe from
/// several nodes at once (one transaction under an advisory lock).
///
/// # Errors
/// `bpm_pg.migration_failed` when the database cannot apply the DDL.
pub async fn prepare_migration_tables(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(error::migration_pool)?;
    let tx = conn.transaction().await.map_err(error::migration)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(error::migration)?;
    tx.batch_execute(MIGRATION_DDL)
        .await
        .map_err(error::migration)?;
    tx.commit().await.map_err(error::migration)
}
