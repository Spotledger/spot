//! The tables, created idempotently in one transaction under an advisory lock.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_partition_assignment` | one row per assignment: scope key, owning node, version and the effective interval; unique on (scope type, scope value, version) |
//! | `spot_partition_history` | the audit trail, one row per change; append-only (a trigger refuses UPDATE, DELETE and TRUNCATE) |
//! | `spot_range_series` | one row per naming series: the next free number, the cursor every grant advances |
//! | `spot_range_grant` | one row per granted range; append-only; unique on (series, first number) |
//!
//! Instants and the cursor are `numeric(39,0)` (exact nanoseconds since the Unix epoch; a `u128`
//! cursor), bound and read as text so no driver feature is needed. No extension is required: the
//! overlap rule is enforced under a per-scope advisory lock, not an exclusion constraint, so the
//! crate works on a plain PostgreSQL (no `btree_gist`, no `pg_trgm`).
//!
//! Statements carry no schema name: they run in the connection's default schema. The bridge resets
//! nothing between calls, so nothing here changes the session (no `SET`, no session lock).

use spot_pg_bridge::PooledConn;

use crate::error::OpError;

/// The tables, in creation order.
pub const TABLES: [&str; 4] = [
    "spot_partition_assignment",
    "spot_partition_history",
    "spot_range_series",
    "spot_range_grant",
];

/// Advisory lock key held (transaction-scoped) while the DDL runs, so two processes preparing at
/// once do not race on `CREATE TABLE IF NOT EXISTS`.
const PREPARE_LOCK: i64 = 0x5350_4152_5401;

/// The idempotent DDL.
pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_partition_assignment (\
    id bigint GENERATED ALWAYS AS IDENTITY, \
    scope_type text NOT NULL, \
    scope_value text NOT NULL, \
    node text NOT NULL, \
    version bigint NOT NULL CHECK (version >= 1), \
    effective_from numeric(39,0) NOT NULL, \
    effective_to numeric(39,0), \
    CHECK (effective_to IS NULL OR effective_to > effective_from), \
    PRIMARY KEY (scope_type, scope_value, version)); \
CREATE INDEX IF NOT EXISTS spot_partition_assignment_id ON spot_partition_assignment (id); \
CREATE TABLE IF NOT EXISTS spot_partition_history (\
    seq bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
    scope_type text NOT NULL, \
    scope_value text NOT NULL, \
    node text NOT NULL, \
    version bigint NOT NULL, \
    effective_from numeric(39,0) NOT NULL, \
    effective_to numeric(39,0), \
    actor text NOT NULL, \
    reason text NOT NULL, \
    recorded_at numeric(39,0) NOT NULL); \
CREATE TABLE IF NOT EXISTS spot_range_series (\
    series_key text PRIMARY KEY, \
    next_free numeric(39,0) NOT NULL CHECK (next_free >= 1)); \
CREATE TABLE IF NOT EXISTS spot_range_grant (\
    lease_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
    series_key text NOT NULL, \
    node text NOT NULL, \
    first_number numeric(39,0) NOT NULL CHECK (first_number >= 1), \
    last_number numeric(39,0) NOT NULL, \
    granted_at numeric(39,0) NOT NULL, \
    CHECK (last_number >= first_number), \
    UNIQUE (series_key, first_number)); \
CREATE OR REPLACE FUNCTION spot_partition_append_only() RETURNS trigger LANGUAGE plpgsql AS $$ \
BEGIN RAISE EXCEPTION '% is append-only', TG_TABLE_NAME; END $$; \
DROP TRIGGER IF EXISTS spot_partition_history_append_only ON spot_partition_history; \
CREATE TRIGGER spot_partition_history_append_only BEFORE UPDATE OR DELETE ON spot_partition_history \
    FOR EACH ROW EXECUTE FUNCTION spot_partition_append_only(); \
DROP TRIGGER IF EXISTS spot_partition_history_no_truncate ON spot_partition_history; \
CREATE TRIGGER spot_partition_history_no_truncate BEFORE TRUNCATE ON spot_partition_history \
    FOR EACH STATEMENT EXECUTE FUNCTION spot_partition_append_only(); \
DROP TRIGGER IF EXISTS spot_range_grant_append_only ON spot_range_grant; \
CREATE TRIGGER spot_range_grant_append_only BEFORE UPDATE OR DELETE ON spot_range_grant \
    FOR EACH ROW EXECUTE FUNCTION spot_partition_append_only(); \
DROP TRIGGER IF EXISTS spot_range_grant_no_truncate ON spot_range_grant; \
CREATE TRIGGER spot_range_grant_no_truncate BEFORE TRUNCATE ON spot_range_grant \
    FOR EACH STATEMENT EXECUTE FUNCTION spot_partition_append_only();";

/// Creates the tables under the advisory lock, in one transaction.
pub(crate) async fn prepare(conn: &mut PooledConn) -> Result<(), OpError> {
    let tx = conn.transaction().await?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await?;
    tx.batch_execute(DDL).await?;
    tx.commit().await?;
    Ok(())
}
