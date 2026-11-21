//! The tables this backend owns, created idempotently.
//!
//! [`PgCoordinator::prepare`](crate::PgCoordinator::prepare) runs the statements in [`DDL`]; a
//! second call is a no-op, and two nodes may call it at once. The tables are this crate's own:
//! they never touch, and are never touched by, the storage backend's tables.

use deadpool_postgres::Pool;
use spot_coordinator::Result;

/// Runs [`DDL`] on a pooled client, inside one transaction serialized against every other node by
/// [`PREPARE_LOCK`], with pool and statement failures mapped. `CREATE TABLE IF NOT EXISTS` is not
/// safe against a concurrent creator on its own (the loser can fail on a duplicate `pg_class`
/// row), so the transaction-scoped lock makes "two nodes may call it at once" true.
pub(crate) async fn prepare(pool: &Pool) -> Result<()> {
    let mut client = pool
        .get()
        .await
        .map_err(|e| crate::error::pool("prepare", &e))?;
    let tx = client
        .transaction()
        .await
        .map_err(|e| crate::error::pg("prepare", &e))?;
    tx.batch_execute(PREPARE_LOCK)
        .await
        .map_err(|e| crate::error::pg("prepare", &e))?;
    tx.batch_execute(DDL)
        .await
        .map_err(|e| crate::error::pg("prepare", &e))?;
    tx.commit()
        .await
        .map_err(|e| crate::error::pg("prepare", &e))
}

/// Serializes concurrent [`prepare`] calls: a **transaction-scoped** advisory lock, released by
/// the commit or rollback. It is deliberately not a session lock, which a pooled connection would
/// hold and leak; `hashtext` gives it one stable key without a magic number.
const PREPARE_LOCK: &str = "SELECT pg_advisory_xact_lock(hashtext('spot-coordinator-pg:prepare'))";

/// Every statement the backend needs, in one round trip.
///
/// It opens with a **transaction-scoped** advisory lock (`SPOT`/`COOR`, the `0x53504f54`/`0x434f4f52`
/// pair, as `spot-postgres` uses for its own DDL): `batch_execute` runs the whole string as one
/// implicit transaction, and the lock is released when that transaction ends, so a node that
/// creates the schema while another is doing the same waits instead of racing a bare
/// `CREATE TABLE IF NOT EXISTS` into a duplicate-object error. (A *session* advisory lock would
/// leak on a pooled connection and is not used; this one cannot outlive its transaction.)
///
/// `spot_coord_token` is the high-water mark of tokens per name: the lease row's `token` is the
/// authority while the row exists, and `last_token` carries it forward when a released lease row
/// is deleted, so a token is never reused. `spot_coord_counter.value` is `numeric(20,0)`, not
/// `bigint`: the contract's counters run to `u64::MAX`, which does not fit a signed 64-bit
/// integer. `spot_coord_rate.window_start` holds the bucket's **theoretical arrival time** (the
/// generic cell rate algorithm of the in-process backend), and `used` holds the capacity the
/// bucket was built for, so a caller that changes a key's limit starts that bucket afresh.
pub const DDL: &str = "\
SELECT pg_advisory_xact_lock(1397772116, 1129271122); \
CREATE TABLE IF NOT EXISTS spot_coord_lease (\
    name text PRIMARY KEY, \
    holder text NOT NULL, \
    token bigint NOT NULL, \
    expires_at timestamptz NOT NULL, \
    lease_id bigint NOT NULL); \
CREATE TABLE IF NOT EXISTS spot_coord_token (\
    name text PRIMARY KEY, \
    last_token bigint NOT NULL); \
CREATE TABLE IF NOT EXISTS spot_coord_counter (\
    name text PRIMARY KEY, \
    value numeric(20,0) NOT NULL); \
CREATE TABLE IF NOT EXISTS spot_coord_rate (\
    key text PRIMARY KEY, \
    window_start timestamptz NOT NULL, \
    used bigint NOT NULL); \
CREATE TABLE IF NOT EXISTS spot_coord_node (\
    node_id text PRIMARY KEY, \
    seen_at timestamptz NOT NULL, \
    info text NOT NULL); \
CREATE INDEX IF NOT EXISTS spot_coord_node_seen_at ON spot_coord_node (seen_at); \
CREATE INDEX IF NOT EXISTS spot_coord_rate_window_start ON spot_coord_rate (window_start);";
