//! The outbox barrier (WP-RTB-01.T3, ADR-061 section 4): when a journal shipper is configured
//! for the site, retention pruning never removes an outbox entry the shipper has not made
//! durable in the journal, whatever its age.
//!
//! # Why this is a correctness rule
//!
//! The outbox is what the journal shipper reads. Without the barrier, a long journal outage plus
//! the retention pruning of [`super::cleanup`] would remove entries the journal never received,
//! and the recoverable record would hold a silent gap. With it, those entries stay (the outbox
//! grows) and [`super::Cleanup::held_back_events`] says how many and for how long.
//!
//! # How the shipper is detected
//!
//! The shipper keeps its persisted watermark in `spot_journal_ship_state` (one row per site and
//! source; DDL published by `spot-journal-ship`). A database without that table, or with no row
//! in it, has no shipper and prunes as before. The barrier is the lowest watermark sequence of
//! the **site** sources (a source with a `#` is a partition's own log, not this outbox) in the
//! feed's current generation; a site row of an older generation counts as 0, because nothing of
//! the new generation has been shipped yet.

use spot_storage_contract::StorageResult;
use tokio_postgres::Transaction;

use crate::feed::db;

/// Whether the shipper's state table exists in this database.
const TABLE_EXISTS: &str = "SELECT to_regclass('spot_journal_ship_state') IS NOT NULL";

/// `(rows, barrier)`: how many site rows the state table holds and the lowest shipped sequence
/// of the current generation among them (0 when a row is of an older generation).
const BARRIER: &str = "\
WITH g AS (SELECT value::bigint AS generation FROM spot_meta WHERE key = 'feed_generation') \
SELECT count(*), \
       coalesce(min(CASE WHEN s.generation = g.generation THEN s.seq ELSE 0 END), 0) \
FROM spot_journal_ship_state s, g \
WHERE position('#' in s.source) = 0";

/// As the storage layer's outbox pruning, with one more condition on the age branch: the entry
/// is at or below the shipped watermark (`$2`). `$1` is the retention in seconds.
pub(super) const PRUNE_OUTBOX_BARRIERED: &str = "\
WITH g AS (SELECT value::bigint AS generation FROM spot_meta WHERE key = 'feed_generation'), \
gone AS ( \
    DELETE FROM spot_outbox_pub p USING g \
    WHERE p.generation <> g.generation \
       OR (p.published_at < clock_timestamp() - make_interval(secs => $1::double precision) \
           AND p.seq < (SELECT last_seq FROM spot_feed_state WHERE log = 'outbox') \
           AND p.seq <= $2::bigint) \
    RETURNING p.generation, p.seq, p.event_id), \
del AS (DELETE FROM spot_outbox o USING gone WHERE o.id = gone.event_id RETURNING 1), \
fl AS ( \
    UPDATE spot_feed_state s \
    SET floor_seq = greatest(s.floor_seq, \
        coalesce((SELECT max(gone.seq) FROM gone, g WHERE gone.generation = g.generation), 0)) \
    WHERE s.log = 'outbox' RETURNING 1) \
SELECT (SELECT count(*) FROM del)";

/// What the barrier kept this pass: published entries older than the retention that the journal
/// has not received (`$1` is the shipped sequence, `$2` the retention in seconds).
const HELD_BACK: &str = "\
WITH g AS (SELECT value::bigint AS generation FROM spot_meta WHERE key = 'feed_generation') \
SELECT count(*), \
       coalesce(floor(extract(epoch FROM clock_timestamp() - min(p.published_at))), 0)::bigint \
FROM spot_outbox_pub p, g \
WHERE p.generation = g.generation \
  AND p.seq > $1::bigint \
  AND p.published_at < clock_timestamp() - make_interval(secs => $2::double precision)";

/// The shipped sequence entries must not be pruned above, or `None` when no shipper is
/// configured for this database. Runs inside the pruning transaction, after it took the
/// publisher's lock, so the answer cannot go stale before the delete.
pub(super) async fn barrier(tx: &Transaction<'_>) -> StorageResult<Option<i64>> {
    let exists: bool = tx.query_one(TABLE_EXISTS, &[]).await.map_err(db)?.get(0);
    if !exists {
        return Ok(None);
    }
    let row = tx.query_one(BARRIER, &[]).await.map_err(db)?;
    let rows: i64 = row.get(0);
    if rows == 0 {
        return Ok(None);
    }
    Ok(Some(row.get(1)))
}

/// How many entries older than the retention the barrier kept, and the age in seconds of the
/// oldest of them (the `feed.pruning_blocked_by_journal` figures).
pub(super) async fn held_back(
    tx: &Transaction<'_>,
    shipped: i64,
    retention_secs: f64,
) -> StorageResult<(u64, u64)> {
    let row = tx
        .query_one(HELD_BACK, &[&shipped, &retention_secs])
        .await
        .map_err(db)?;
    let count: i64 = row.get(0);
    let age: i64 = row.get(1);
    Ok((
        u64::try_from(count).unwrap_or(0),
        u64::try_from(age).unwrap_or(0),
    ))
}
