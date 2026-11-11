//! The control schema: its tables, created idempotently.
//!
//! [`PgControlStore::prepare`](crate::PgControlStore::prepare) runs the statements in [`DDL`]; a
//! second call is a no-op, and two control nodes may call it at once. The tables live in the
//! [`CONTROL_SCHEMA`] namespace of the control database, never in `public` and never in a
//! tenant's schema.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `tenant` | one row per tenant: its name, region, edition, state, placement, created instant, flags, optimistic `version`, retention and name reservation |
//! | `tenant_journal` | the resumable provisioning journal, one row per `(step, kind, at)` |
//! | `tenant_history` | one row per state transition, with the version it produced |
//!
//! Two **partial** unique indexes enforce the rules the in-memory store checks in code: a live
//! tenant's name is unique, and a live tenant's placement is unique. A deleted tenant keeps its
//! row for audit and may share the name with a new tenant once its reservation has expired, so
//! the indexes exclude `deleted` rows.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;

use crate::error::{migration, migration_pool};

/// The namespace this crate's tables live in.
pub const CONTROL_SCHEMA: &str = "spot_control";

/// Every table this crate owns, in creation order.
pub const TABLES: [&str; 3] = ["tenant", "tenant_journal", "tenant_history"];

/// The lock key that serialises concurrent [`prepare`] calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x5445_4e50_4701;

/// The idempotent DDL. Every statement is `IF NOT EXISTS`.
pub const DDL: &str = "\
CREATE SCHEMA IF NOT EXISTS spot_control; \
CREATE TABLE IF NOT EXISTS spot_control.tenant (\
    id uuid PRIMARY KEY, \
    name text NOT NULL, \
    region text NOT NULL, \
    edition text NOT NULL, \
    state text NOT NULL \
        CHECK (state IN ('provisioning', 'active', 'suspended', 'moving', 'deprovisioning', 'deleted')), \
    cluster text NOT NULL, \
    database text NOT NULL, \
    schema text NOT NULL, \
    created bigint NOT NULL CHECK (created >= 0), \
    flags integer NOT NULL CHECK (flags >= 0), \
    version bigint NOT NULL CHECK (version >= 1), \
    backup_ref text, \
    backup_verified boolean, \
    delete_after bigint, \
    name_reserved_until bigint); \
CREATE UNIQUE INDEX IF NOT EXISTS tenant_live_name \
    ON spot_control.tenant (name) WHERE state <> 'deleted'; \
CREATE UNIQUE INDEX IF NOT EXISTS tenant_live_placement \
    ON spot_control.tenant (cluster, database, schema) WHERE state <> 'deleted'; \
CREATE TABLE IF NOT EXISTS spot_control.tenant_journal (\
    tenant uuid NOT NULL REFERENCES spot_control.tenant (id), \
    seq bigint GENERATED ALWAYS AS IDENTITY, \
    step text NOT NULL, \
    kind text NOT NULL CHECK (kind IN ('started', 'done', 'compensated')), \
    at bigint NOT NULL CHECK (at >= 0), \
    PRIMARY KEY (tenant, seq)); \
CREATE TABLE IF NOT EXISTS spot_control.tenant_history (\
    tenant uuid NOT NULL REFERENCES spot_control.tenant (id), \
    seq bigint GENERATED ALWAYS AS IDENTITY, \
    version bigint NOT NULL CHECK (version >= 1), \
    from_state text NOT NULL, \
    to_state text NOT NULL, \
    PRIMARY KEY (tenant, seq));";

/// Creates the control schema and its tables if they are missing. Idempotent and safe to run
/// from several control nodes at once (a transaction-scoped advisory lock serialises them).
///
/// # Errors
/// `tenancy_pg.migration_failed` when the database cannot apply the DDL.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(migration_pool)?;
    let tx = conn.transaction().await.map_err(migration)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(migration)?;
    tx.batch_execute(DDL).await.map_err(migration)?;
    tx.commit().await.map_err(migration)
}
