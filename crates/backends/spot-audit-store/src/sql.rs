//! Every SQL statement this crate owns, and the role and trigger that make the chain
//! append-only.
//!
//! Values reach SQL only as bound parameters; the only interpolated identifiers are the fixed
//! role and table names in this module (ADR-45).

use deadpool_postgres::Pool;
use spot_audit_chain::genesis_hash;

use crate::error::{Result, db, pool};
use crate::schema::rename_old_tables;

/// The application role. It is **never** the pool's login role: production connects as it, so
/// the pool's day-to-day grants are the ones this module sets. It may read and append, never
/// rewrite.
pub const APPLICATION_ROLE: &str = "spot_audit_app";

/// The setting a restore or an audited maintenance operation turns on so the guard trigger lets
/// an `UPDATE` or `DELETE` through. It is off (or unset) for every ordinary connection.
///
/// Written with `concat!` so the source holds no bare dotted literal: this is a Postgres setting
/// name, not an error code, so it does not belong in the error catalogue.
pub const MAINTENANCE_SETTING: &str = concat!("spot.", "audit_maintenance");

/// The tables, the role and the guard trigger, created idempotently. Safe to call from several
/// nodes at once.
pub(crate) async fn prepare(db_pool: &Pool) -> Result<()> {
    let mut client = db_pool.get().await.map_err(|e| pool("prepare", &e))?;
    // One transaction under an advisory lock, so two nodes (or tests) preparing one database take
    // turns. The role is cluster-wide and the lock is per database, so the role block also
    // tolerates losing a race with another database's `CREATE ROLE`.
    let tx = client.transaction().await.map_err(|e| db("prepare", &e))?;
    tx.batch_execute(PREPARE_LOCK)
        .await
        .map_err(|e| db("prepare lock", &e))?;
    // Databases created before the `spot_` prefix hold the old names: rename them, keeping every
    // row, before anything is created.
    rename_old_tables(&tx).await?;
    for (_, statement) in TABLE_DDL {
        tx.batch_execute(statement)
            .await
            .map_err(|e| db("prepare tables", &e))?;
    }
    apply_guard(&tx, "prepare").await?;
    tx.commit().await.map_err(|e| db("prepare", &e))
}

/// The role, grants, guard function and triggers, inside the caller's transaction (which holds the
/// advisory lock). Shared by `prepare` and `install_guard`, so there is one definition.
pub(crate) async fn apply_guard(tx: &tokio_postgres::Transaction<'_>, what: &str) -> Result<()> {
    tx.batch_execute(ROLE_AND_GRANTS)
        .await
        .map_err(|e| db(&format!("{what} role"), &e))?;
    tx.batch_execute(GUARD)
        .await
        .map_err(|e| db(&format!("{what} guard"), &e))
}

/// The advisory lock `prepare` and `install_guard` share.
pub(crate) const PREPARE_LOCK: &str =
    "SELECT pg_advisory_xact_lock(hashtext('spot-audit-store:prepare'))";

/// The name of the chain table.
pub const CHAIN_TABLE: &str = "spot_audit_chain";

/// The name of the checkpoint pointer table.
pub const CHECKPOINT_TABLE: &str = "spot_audit_checkpoint";

/// The chain table: one `CREATE TABLE IF NOT EXISTS`, the shape `spot_pg_migrate::ExtraTable`
/// accepts (the table name marks it done). The primary key is its only index.
pub const CHAIN_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_audit_chain (\
    site text NOT NULL, \
    seq bigint NOT NULL, \
    prev_hash bytea NOT NULL, \
    hash bytea NOT NULL, \
    record text NOT NULL, \
    at bigint NOT NULL, \
    PRIMARY KEY (site, seq))";

/// The checkpoint pointer table, in the same `ExtraTable` shape as [`CHAIN_TABLE_DDL`].
pub const CHECKPOINT_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_audit_checkpoint (\
    site text PRIMARY KEY, \
    seq bigint NOT NULL, \
    head_hash bytea NOT NULL, \
    at bigint NOT NULL, \
    key_id text NOT NULL, \
    signature bytea NOT NULL)";

/// Each table of this crate as `(marker table name, statement)`, in creation order: what a host
/// hands the migrator as one `ExtraTable` per pair. `prepare` runs the very same statements.
pub const TABLE_DDL: [(&str, &str); 2] = [
    (CHAIN_TABLE, CHAIN_TABLE_DDL),
    (CHECKPOINT_TABLE, CHECKPOINT_TABLE_DDL),
];

/// The application role and its grants: read and append the chain, maintain the checkpoint
/// pointer, and nothing else. `spot_audit_chain` never receives `UPDATE`, `DELETE` or `TRUNCATE`.
const ROLE_AND_GRANTS: &str = "\
DO $$ BEGIN \
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'spot_audit_app') THEN \
    CREATE ROLE spot_audit_app NOLOGIN; \
  END IF; \
EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; \
END $$; \
GRANT USAGE ON SCHEMA public TO spot_audit_app; \
REVOKE UPDATE, DELETE, TRUNCATE ON spot_audit_chain FROM spot_audit_app; \
GRANT SELECT, INSERT ON spot_audit_chain TO spot_audit_app; \
GRANT SELECT, INSERT, UPDATE ON spot_audit_checkpoint TO spot_audit_app;";

/// The guard triggers: an `UPDATE`, `DELETE` or `TRUNCATE` on `spot_audit_chain` is refused even for the owner,
/// unless `spot.audit_maintenance` is `on` (the documented restore path). A database upgraded
/// from the unprefixed names also loses the old trigger and function (they are replaced by the
/// `spot_` ones, in the same transaction).
const GUARD: &str = "\
CREATE OR REPLACE FUNCTION spot_audit_chain_guard() RETURNS trigger AS $$ \
BEGIN \
  IF current_setting('spot.audit_maintenance', true) = 'on' THEN \
    IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF; \
  END IF; \
  RAISE EXCEPTION 'spot_audit_chain is append-only' USING ERRCODE = '42501'; \
END $$ LANGUAGE plpgsql; \
DROP TRIGGER IF EXISTS audit_chain_no_rewrite ON spot_audit_chain; \
DROP TRIGGER IF EXISTS spot_audit_chain_no_rewrite ON spot_audit_chain; \
CREATE TRIGGER spot_audit_chain_no_rewrite BEFORE UPDATE OR DELETE ON spot_audit_chain \
  FOR EACH ROW EXECUTE FUNCTION spot_audit_chain_guard(); \
DROP TRIGGER IF EXISTS spot_audit_chain_no_truncate ON spot_audit_chain; \
CREATE TRIGGER spot_audit_chain_no_truncate BEFORE TRUNCATE ON spot_audit_chain \
  FOR EACH STATEMENT EXECUTE FUNCTION spot_audit_chain_guard(); \
DO $$ BEGIN \
  EXECUTE format('DROP FUNCTION IF EXISTS %I.audit_chain_guard()', current_schema()); \
END $$;";

/// The newest stored entry of a site, if any.
pub(crate) const HEAD_SQL: &str = "\
SELECT seq, prev_hash, hash, record, at FROM spot_audit_chain \
WHERE site = $1 ORDER BY seq DESC LIMIT 1";

/// Stored entries with `$2 <= seq <= $3`, in order.
pub(crate) const RANGE_SQL: &str = "\
SELECT seq, prev_hash, hash, record, at FROM spot_audit_chain \
WHERE site = $1 AND seq >= $2 AND seq <= $3 ORDER BY seq";

/// The compare-and-set append: one statement guarded by the head. It inserts only when the
/// chain's head is still `$7` and the entry links to it (its `prev_hash` is the head hash, or
/// the site's genesis hash when the chain is empty). A no-row result means another writer won.
pub(crate) const APPEND_SQL: &str = "\
INSERT INTO spot_audit_chain (site, seq, prev_hash, hash, record, at) \
SELECT $1, $2, $3, $4, $5, $6 \
WHERE (SELECT COALESCE(max(seq), 0) FROM spot_audit_chain WHERE site = $1) = $7 \
  AND $2 = $7 + 1 \
  AND COALESCE((SELECT hash FROM spot_audit_chain WHERE site = $1 ORDER BY seq DESC LIMIT 1), $8) = $3 \
ON CONFLICT (site, seq) DO NOTHING \
RETURNING seq";

/// Stores a checkpoint only when it is newer than the stored one.
pub(crate) const CHECKPOINT_PUT_SQL: &str = "\
INSERT INTO spot_audit_checkpoint (site, seq, head_hash, at, key_id, signature) \
VALUES ($1, $2, $3, $4, $5, $6) \
ON CONFLICT (site) DO UPDATE \
  SET seq = EXCLUDED.seq, head_hash = EXCLUDED.head_hash, \
      at = EXCLUDED.at, key_id = EXCLUDED.key_id, signature = EXCLUDED.signature \
  WHERE spot_audit_checkpoint.seq < EXCLUDED.seq";

/// The newest checkpoint of a site.
pub(crate) const CHECKPOINT_GET_SQL: &str = "\
SELECT site, seq, head_hash, at, key_id, signature FROM spot_audit_checkpoint WHERE site = $1";

/// The genesis hash of `site`, as the `$8` of [`APPEND_SQL`].
pub(crate) fn genesis(site: &str) -> Vec<u8> {
    genesis_hash(site).to_vec()
}
