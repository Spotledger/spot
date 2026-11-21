//! The tables, created idempotently in one transaction under an advisory lock.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_sync_node_applied` | the applied-record ledger: one row per applied `(tenant, entity, id, version, kind)` with the source, name, position and time; `kind` is `put` or `delete` (a remembered tombstone) |
//! | `spot_sync_node_parked` | bundles waiting for records they reference: the bundle as JSON, its missing keys, arrival number, size, first position and serve stamp, in arrival order |
//! | `spot_sync_node_repair` | bundles that left parking without being applied, for the closure repair loop |
//! | `spot_sync_node_checkpoint` | the applied journal prefix per `(tenant, source)`, forward only |
//! | `spot_sync_node_state` | the hold set and the confirmed freshness horizons per tenant, as JSON |
//!
//! Every table's key starts with the tenant and every statement this crate runs on them filters
//! on it (see [`crate::STATEMENTS`]). Instants and counters are `bigint`. No extension is needed.
//!
//! The ledger lives in the database that holds the documents (one tenant, one database), so that
//! the documents and their ledger rows commit in one transaction.

use spot_pg_bridge::PooledConn;

use crate::error::OpError;

/// The tables, in creation order.
pub const TABLES: [&str; 5] = [
    "spot_sync_node_applied",
    "spot_sync_node_parked",
    "spot_sync_node_repair",
    "spot_sync_node_checkpoint",
    "spot_sync_node_state",
];

/// Advisory lock key held (transaction-scoped) while the DDL runs.
const PREPARE_LOCK: i64 = 0x0053_594e_434e_4402;

/// The idempotent DDL.
pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_sync_node_applied (\
    tenant text NOT NULL, \
    source text NOT NULL, \
    entity text NOT NULL, \
    id text NOT NULL, \
    name text NOT NULL, \
    version bigint NOT NULL CHECK (version >= 0), \
    position bigint NOT NULL CHECK (position >= 0), \
    kind text NOT NULL CHECK (kind IN ('put', 'delete')), \
    applied_at bigint NOT NULL, \
    PRIMARY KEY (tenant, entity, id, version, kind), \
    CHECK (octet_length(tenant) BETWEEN 1 AND 128 AND octet_length(source) BETWEEN 1 AND 256)); \
CREATE INDEX IF NOT EXISTS spot_sync_node_applied_position \
    ON spot_sync_node_applied (tenant, source, position); \
CREATE TABLE IF NOT EXISTS spot_sync_node_parked (\
    tenant text NOT NULL, \
    bundle_id bigint GENERATED ALWAYS AS IDENTITY, \
    arrived_seq bigint NOT NULL CHECK (arrived_seq >= 0), \
    bytes bigint NOT NULL CHECK (bytes >= 0), \
    first_position bigint NOT NULL CHECK (first_position >= 0), \
    serve_seq bigint, \
    serve_after bigint, \
    parked_at_ms bigint NOT NULL CHECK (parked_at_ms >= 0), \
    parked_at timestamptz NOT NULL DEFAULT now(), \
    bundle text NOT NULL, \
    missing_keys text NOT NULL, \
    PRIMARY KEY (tenant, bundle_id), \
    CHECK (octet_length(tenant) BETWEEN 1 AND 128)); \
CREATE TABLE IF NOT EXISTS spot_sync_node_repair (\
    tenant text NOT NULL, \
    repair_id bigint GENERATED ALWAYS AS IDENTITY, \
    first_position bigint NOT NULL CHECK (first_position >= 0), \
    cause text NOT NULL CHECK (cause IN ('bound', 'expired', 'oversize', 'refused')), \
    missing_keys text NOT NULL, \
    queued_at timestamptz NOT NULL DEFAULT now(), \
    PRIMARY KEY (tenant, repair_id), \
    CHECK (octet_length(tenant) BETWEEN 1 AND 128)); \
CREATE TABLE IF NOT EXISTS spot_sync_node_checkpoint (\
    tenant text NOT NULL, \
    source text NOT NULL, \
    last_applied_seq bigint NOT NULL CHECK (last_applied_seq >= 0), \
    updated_at timestamptz NOT NULL DEFAULT now(), \
    PRIMARY KEY (tenant, source), \
    CHECK (octet_length(tenant) BETWEEN 1 AND 128)); \
CREATE TABLE IF NOT EXISTS spot_sync_node_state (\
    tenant text NOT NULL, \
    kind text NOT NULL CHECK (kind IN ('holds', 'horizons')), \
    value text NOT NULL CHECK (octet_length(value) <= 16777216), \
    updated_at timestamptz NOT NULL DEFAULT now(), \
    PRIMARY KEY (tenant, kind), \
    CHECK (octet_length(tenant) BETWEEN 1 AND 128));";

/// Creates the tables under the advisory lock, in one transaction.
pub(crate) async fn prepare(conn: &mut PooledConn) -> Result<(), OpError> {
    let tx = conn.transaction().await?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await?;
    tx.batch_execute(DDL).await?;
    tx.commit().await?;
    Ok(())
}

/// The tables of [`TABLES`] that do not exist in the connection's schema.
pub(crate) async fn missing(conn: &PooledConn) -> Result<Vec<&'static str>, OpError> {
    let mut out = Vec::new();
    for table in TABLES {
        let row = conn
            .query_one(
                "SELECT to_regclass(quote_ident(current_schema()) || '.' || quote_ident($1)) IS NOT NULL",
                &[&table],
            )
            .await?;
        if !row.get::<_, bool>(0) {
            out.push(table);
        }
    }
    Ok(out)
}
