//! The tables, created idempotently in one transaction under an advisory lock.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_sync_hub_queue` | one row per queued bundle: tenant, destination, the serve sequence the queue assigned, position, bundle index, redelivery flag, lane, size and the item as JSON; unique on the push identity |
//! | `spot_sync_hub_dest_state` | one row per destination: the last serve sequence ever assigned (it never falls) and the destination's persisted relay snapshot (holds, extras, backlog) as JSON |
//! | `spot_sync_hub_relay` | one row per tenant: the relay's cursor, the last transaction and the lane of each entity |
//! | `spot_sync_node` | the node registry: role, lifecycle state, subscription JSON, last contact, last acknowledged serve sequence and prefix, version range |
//! | `spot_sync_hub_node_record` | the acknowledgement ledger's record per `(tenant, node)` as JSON |
//! | `spot_sync_hub_mandatory` | the mandatory events the obligation ledger tracks, per tenant and position |
//!
//! Every table's key starts with the tenant and every statement this crate runs on them filters
//! on it (see [`crate::STATEMENTS`]). Instants and counters are `bigint`; sizes and queue
//! numbers are checked on the way in. No extension is required.

use spot_pg_bridge::PooledConn;

use crate::error::OpError;

/// The tables, in creation order.
pub const TABLES: [&str; 6] = [
    "spot_sync_hub_queue",
    "spot_sync_hub_dest_state",
    "spot_sync_hub_relay",
    "spot_sync_node",
    "spot_sync_hub_node_record",
    "spot_sync_hub_mandatory",
];

/// Advisory lock key held (transaction-scoped) while the DDL runs.
const PREPARE_LOCK: i64 = 0x0053_594e_4348_5501;

/// The idempotent DDL.
pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_sync_hub_dest_state (\
    tenant text NOT NULL, \
    destination text NOT NULL, \
    last_serve_seq bigint NOT NULL DEFAULT 0 CHECK (last_serve_seq >= 0), \
    snapshot text, \
    updated_at timestamptz NOT NULL DEFAULT now(), \
    PRIMARY KEY (tenant, destination), \
    CHECK (octet_length(tenant) BETWEEN 1 AND 128 AND octet_length(destination) BETWEEN 1 AND 128), \
    CHECK (snapshot IS NULL OR octet_length(snapshot) <= 16777216)); \
CREATE TABLE IF NOT EXISTS spot_sync_hub_queue (\
    tenant text NOT NULL, \
    destination text NOT NULL, \
    serve_seq bigint NOT NULL CHECK (serve_seq >= 1), \
    position bigint NOT NULL CHECK (position >= 0), \
    bundle_index integer NOT NULL CHECK (bundle_index >= 0), \
    redelivery boolean NOT NULL, \
    lane smallint NOT NULL CHECK (lane IN (0, 1)), \
    bytes bigint NOT NULL CHECK (bytes >= 0), \
    entry text NOT NULL, \
    queued_at timestamptz NOT NULL DEFAULT now(), \
    PRIMARY KEY (tenant, destination, serve_seq), \
    UNIQUE (tenant, destination, position, bundle_index, redelivery)); \
CREATE INDEX IF NOT EXISTS spot_sync_hub_queue_order \
    ON spot_sync_hub_queue (tenant, destination, lane, position, bundle_index); \
CREATE TABLE IF NOT EXISTS spot_sync_hub_relay (\
    tenant text PRIMARY KEY, \
    cursor bigint NOT NULL CHECK (cursor >= 0), \
    last_txn text, \
    lane_of_entity text NOT NULL, \
    updated_at timestamptz NOT NULL DEFAULT now()); \
CREATE TABLE IF NOT EXISTS spot_sync_node (\
    tenant text NOT NULL, \
    node_id text NOT NULL, \
    role text NOT NULL CHECK (octet_length(role) BETWEEN 1 AND 64), \
    state text NOT NULL CHECK (state IN ('enrolled', 'active', 'suspended', 'revoked')), \
    subscription_json text NOT NULL, \
    last_seen bigint, \
    last_ack_serve_seq bigint NOT NULL DEFAULT 0 CHECK (last_ack_serve_seq >= 0), \
    applied_prefix bigint NOT NULL DEFAULT 0 CHECK (applied_prefix >= 0), \
    version_range text NOT NULL DEFAULT '', \
    created_at bigint NOT NULL, \
    updated_at bigint NOT NULL, \
    PRIMARY KEY (tenant, node_id), \
    CHECK (octet_length(tenant) BETWEEN 1 AND 128 AND octet_length(node_id) BETWEEN 1 AND 128)); \
CREATE INDEX IF NOT EXISTS spot_sync_node_tenant_state ON spot_sync_node (tenant, state); \
CREATE TABLE IF NOT EXISTS spot_sync_hub_node_record (\
    tenant text NOT NULL, \
    node_id text NOT NULL, \
    record text NOT NULL, \
    PRIMARY KEY (tenant, node_id)); \
CREATE TABLE IF NOT EXISTS spot_sync_hub_mandatory (\
    tenant text NOT NULL, \
    position bigint NOT NULL CHECK (position >= 0), \
    event text NOT NULL, \
    PRIMARY KEY (tenant, position));";

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
