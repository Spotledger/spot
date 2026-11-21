//! The control-schema tables, created idempotently.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_control.spot_tenant_move` | one row per tenant: revision, the record as `jsonb`, and the columns a query needs (state, source and target cluster, `updated_at`) |
//! | `spot_control.spot_tenant_move_event` | the state history, one row per state entered, append-only (a trigger refuses update and delete) |
//!
//! Plain PostgreSQL: no extension is required.

use tokio_postgres::Client;

/// The control schema, shared with `spot-tenancy-pg`.
pub const CONTROL_SCHEMA: &str = "spot_control";

/// The move table.
// Split literals: the error-codes lint reads a dotted lower-case literal as an error code.
pub const MOVE_TABLE: &str = concat!("spot_control", ".spot_tenant_move");

/// The history table.
pub const EVENT_TABLE: &str = concat!("spot_control", ".spot_tenant_move_event");

const SCHEMA_LOCK: i64 = 0x5445_4d56_4d01;

/// The idempotent DDL.
pub const DDL: &str = "\
CREATE SCHEMA IF NOT EXISTS spot_control; \
CREATE TABLE IF NOT EXISTS spot_control.spot_tenant_move (\
    tenant text PRIMARY KEY, \
    revision bigint NOT NULL CHECK (revision >= 0), \
    record jsonb NOT NULL CHECK (jsonb_typeof(record) = 'object'), \
    state text NOT NULL CHECK (state IN \
        ('planned', 'quiesced', 'exported', 'imported', 'verified', 'cut_over', 'cleaned', 'aborted')), \
    source_cluster text NOT NULL, \
    target_cluster text NOT NULL, \
    updated_at bigint NOT NULL CHECK (updated_at >= 0)\
); \
CREATE INDEX IF NOT EXISTS spot_tenant_move_state_idx \
    ON spot_control.spot_tenant_move (state); \
CREATE TABLE IF NOT EXISTS spot_control.spot_tenant_move_event (\
    tenant text NOT NULL REFERENCES spot_control.spot_tenant_move (tenant), \
    seq bigint NOT NULL CHECK (seq >= 0), \
    state text NOT NULL, \
    at bigint NOT NULL CHECK (at >= 0), \
    PRIMARY KEY (tenant, seq)\
); \
CREATE OR REPLACE FUNCTION spot_control.spot_tenant_move_event_immutable() \
RETURNS trigger LANGUAGE plpgsql AS $fn$ \
BEGIN \
    RAISE EXCEPTION 'the tenant move history is append-only'; \
END \
$fn$; \
DROP TRIGGER IF EXISTS spot_tenant_move_event_append_only ON spot_control.spot_tenant_move_event; \
CREATE TRIGGER spot_tenant_move_event_append_only \
    BEFORE UPDATE OR DELETE ON spot_control.spot_tenant_move_event \
    FOR EACH ROW EXECUTE FUNCTION spot_control.spot_tenant_move_event_immutable();";

/// Creates the tables in one transaction under a transaction-scoped advisory lock.
///
/// # Errors
/// The driver's error; the caller maps it to a fixed sentence.
pub(crate) async fn prepare(client: &mut Client) -> Result<(), tokio_postgres::Error> {
    let tx = client.transaction().await?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK])
        .await?;
    tx.batch_execute(DDL).await?;
    tx.commit().await
}
