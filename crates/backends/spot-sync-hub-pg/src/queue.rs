//! [`PgSendQueue`]: the durable per-destination send queue (the `SendQueue` contract of
//! `spot-sync-hub`, see that trait's documentation for the semantics this store keeps).
//!
//! # How the contract is kept
//!
//! - **Push is one transaction** that first locks the destination's `spot_sync_hub_dest_state`
//!   row (`FOR UPDATE`, creating it when absent), so concurrent pushes to one destination are
//!   serialised; then it looks for the push identity `(position, bundle index, redelivery)`: a hit
//!   is [`PushOutcome::Duplicate`] and changes nothing; otherwise it takes the next serve sequence
//!   (`last_serve_seq + 1`, which **never falls**, so a number is never reused after a trim) and
//!   inserts the row. The item is durable when the transaction commits, before `push` returns.
//! - **Bounds.** A destination holds at most [`QueueLimits::max_rows`] rows and
//!   [`QueueLimits::max_bytes`] bytes. A push that would exceed either returns
//!   `QueueFailed` carrying `sync_hub_pg.queue_full` and stores nothing: back-pressure the relay
//!   surfaces, never a silent drop. (The relay parks a destination before this bound at its own
//!   `max_queue_bytes`; this is the hard guard behind it.)
//! - **Trim** is by serve sequence (`ack_served`), by journal position for non-redeliveries
//!   (`ack`), or by redelivered position (`remove_redelivered`); each is one `DELETE`.
//! - **`peek`** returns items in serving order (lane, position, bundle index) and computes each
//!   item's `serve_after` over the whole queue.

use spot_sync_hub::{HubError, PushOutcome, QueuedBundle, SendQueue};
use spot_sync_obligations::{Lane, Pending};

use crate::db::{Db, MAX_NAME};
use crate::error::{OpError, PgHubError, QUEUE_FULL, map, queue_error};

/// Deadline of a push (one lock, three statements, one commit).
pub const PUSH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
/// Deadline of a peek, a trim or a count.
pub const READ_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) const PUSH_LOCK: &str = "INSERT INTO spot_sync_hub_dest_state (tenant, destination) \
    VALUES ($1, $2) ON CONFLICT (tenant, destination) DO NOTHING";
pub(crate) const PUSH_ROW: &str = "SELECT last_serve_seq FROM spot_sync_hub_dest_state \
    WHERE tenant = $1 AND destination = $2 FOR UPDATE";
pub(crate) const PUSH_EXISTS: &str = "SELECT 1 FROM spot_sync_hub_queue \
    WHERE tenant = $1 AND destination = $2 AND position = $3 AND bundle_index = $4 AND redelivery = $5";
pub(crate) const PUSH_BUMP: &str = "UPDATE spot_sync_hub_dest_state SET last_serve_seq = last_serve_seq + 1, \
    updated_at = now() WHERE tenant = $1 AND destination = $2 RETURNING last_serve_seq";
pub(crate) const PUSH_COUNT: &str = "SELECT count(*), COALESCE(sum(bytes), 0)::bigint FROM spot_sync_hub_queue \
    WHERE tenant = $1 AND destination = $2";
pub(crate) const PUSH_INSERT: &str = "INSERT INTO spot_sync_hub_queue \
    (tenant, destination, serve_seq, position, bundle_index, redelivery, lane, bytes, entry) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)";
pub(crate) const PEEK: &str = "SELECT q.serve_seq, q.entry, COALESCE((SELECT max(q2.serve_seq) \
    FROM spot_sync_hub_queue q2 WHERE q2.tenant = q.tenant AND q2.destination = q.destination \
    AND q2.serve_seq < q.serve_seq), 0) \
    FROM spot_sync_hub_queue q WHERE q.tenant = $1 AND q.destination = $2 \
    ORDER BY q.lane, q.position, q.bundle_index, q.redelivery, q.serve_seq LIMIT $3";
pub(crate) const PEEK_HEAD: &str = "SELECT q.serve_seq, q.entry, COALESCE((SELECT max(q2.serve_seq) \
    FROM spot_sync_hub_queue q2 WHERE q2.tenant = q.tenant AND q2.destination = q.destination \
    AND q2.serve_seq < q.serve_seq), 0) \
    FROM spot_sync_hub_queue q WHERE q.tenant = $1 AND q.destination = $2 \
    ORDER BY q.serve_seq LIMIT 1";
pub(crate) const ACK_POSITION: &str = "DELETE FROM spot_sync_hub_queue \
    WHERE tenant = $1 AND destination = $2 AND NOT redelivery AND position <= $3";
pub(crate) const LAST_SEQ: &str = "SELECT last_serve_seq FROM spot_sync_hub_dest_state \
    WHERE tenant = $1 AND destination = $2";
pub(crate) const LOWEST_AFTER: &str = "SELECT min(position) FROM spot_sync_hub_queue \
    WHERE tenant = $1 AND destination = $2 AND NOT redelivery AND serve_seq > $3";
pub(crate) const ACK_SERVED: &str = "DELETE FROM spot_sync_hub_queue \
    WHERE tenant = $1 AND destination = $2 AND serve_seq <= $3";
pub(crate) const REMOVE_REDELIVERED: &str = "DELETE FROM spot_sync_hub_queue \
    WHERE tenant = $1 AND destination = $2 AND redelivery AND position = $3";
pub(crate) const BYTES: &str = "SELECT COALESCE(sum(bytes), 0)::bigint FROM spot_sync_hub_queue \
    WHERE tenant = $1 AND destination = $2";
pub(crate) const DEPTH: &str = "SELECT count(*) FROM spot_sync_hub_queue \
    WHERE tenant = $1 AND destination = $2";
pub(crate) const OLDEST_AGE: &str = "SELECT COALESCE(floor(extract(epoch FROM (now() - min(queued_at))))::bigint, 0) \
    FROM spot_sync_hub_queue WHERE tenant = $1 AND destination = $2";

/// The bounds of one destination's queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueLimits {
    /// The most rows one destination may hold.
    pub max_rows: u64,
    /// The most bytes (the bundles' own sizes) one destination may hold.
    pub max_bytes: u64,
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            max_rows: 1_000_000,
            max_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

/// The durable send queue of one tenant.
#[derive(Clone, Debug)]
pub struct PgSendQueue {
    db: Db,
    limits: QueueLimits,
}

fn to_i64(value: u64) -> Result<i64, OpError> {
    i64::try_from(value).map_err(|_| OpError::Decode)
}

fn from_i64(value: i64) -> Result<u64, OpError> {
    u64::try_from(value).map_err(|_| OpError::Decode)
}

fn lane_code(lane: Lane) -> i16 {
    match lane {
        Lane::Mandatory => 0,
        Lane::Bulk => 1,
    }
}

/// Whether a destination name is usable as a key.
fn check_dest(dest: &str) -> Result<(), HubError> {
    if dest.is_empty() || dest.len() > MAX_NAME || dest.chars().any(char::is_control) {
        return Err(HubError::QueueFailed {
            reason: "the destination name is empty, too long or not printable".to_owned(),
        });
    }
    Ok(())
}

impl PgSendQueue {
    pub(crate) fn new(db: Db, limits: QueueLimits) -> Self {
        Self { db, limits }
    }

    /// The bounds this queue keeps.
    #[must_use]
    pub fn limits(&self) -> QueueLimits {
        self.limits
    }

    /// Rows queued for `dest`.
    ///
    /// # Errors
    /// The store's failure.
    pub fn depth(&self, dest: &str) -> Result<u64, PgHubError> {
        let (tenant, dest) = (self.db.tenant.clone(), dest.to_owned());
        self.db
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_one(DEPTH, &[&tenant, &dest]).await?;
                    from_i64(row.get::<_, i64>(0))
                })
            })
            .map_err(map)
    }

    /// Seconds since the oldest queued item of `dest` was queued (0 when empty). The age uses the
    /// database's clock for the row's `queued_at`; this library reads no clock of its own.
    ///
    /// # Errors
    /// The store's failure.
    pub fn oldest_age_seconds(&self, dest: &str) -> Result<u64, PgHubError> {
        let (tenant, dest) = (self.db.tenant.clone(), dest.to_owned());
        self.db
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_one(OLDEST_AGE, &[&tenant, &dest]).await?;
                    from_i64(row.get::<_, i64>(0).max(0))
                })
            })
            .map_err(map)
    }

    fn push_item(
        &self,
        dest: &str,
        mut item: Pending<QueuedBundle>,
    ) -> Result<PushOutcome, HubError> {
        check_dest(dest)?;
        let (tenant, dest) = (self.db.tenant.clone(), dest.to_owned());
        let limits = self.limits;
        self.db
            .run(PUSH_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let position = to_i64(item.position)?;
                    let index = i32::try_from(item.item.index).map_err(|_| OpError::Decode)?;
                    let bytes = to_i64(
                        u64::try_from(item.item.bundle.bytes).map_err(|_| OpError::Decode)?,
                    )?;
                    let tx = conn.transaction().await?;
                    tx.execute(PUSH_LOCK, &[&tenant, &dest]).await?;
                    // Serialise pushes to this destination: the row lock is held to the commit.
                    tx.query_one(PUSH_ROW, &[&tenant, &dest]).await?;
                    let exists = tx
                        .query_opt(
                            PUSH_EXISTS,
                            &[&tenant, &dest, &position, &index, &item.item.redelivery],
                        )
                        .await?;
                    if exists.is_some() {
                        return Ok(PushOutcome::Duplicate);
                    }
                    let totals = tx.query_one(PUSH_COUNT, &[&tenant, &dest]).await?;
                    let rows = from_i64(totals.get::<_, i64>(0))?;
                    let held = from_i64(totals.get::<_, i64>(1))?;
                    if rows.saturating_add(1) > limits.max_rows
                        || held.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX))
                            > limits.max_bytes
                    {
                        return Err(OpError::Refused(PgHubError::QueueFull));
                    }
                    let seq: i64 = tx.query_one(PUSH_BUMP, &[&tenant, &dest]).await?.get(0);
                    item.item.serve_seq = from_i64(seq)?;
                    item.item.serve_after = 0;
                    let entry = serde_json::to_string(&item).map_err(|_| OpError::Decode)?;
                    tx.execute(
                        PUSH_INSERT,
                        &[
                            &tenant,
                            &dest,
                            &seq,
                            &position,
                            &index,
                            &item.item.redelivery,
                            &lane_code(item.lane),
                            &bytes,
                            &entry,
                        ],
                    )
                    .await?;
                    tx.commit().await?;
                    Ok(PushOutcome::Queued)
                })
            })
            .map_err(queue_error)
    }

    fn trim(&self, dest: &str, sql: &'static str, bound: u64) -> Result<usize, HubError> {
        check_dest(dest)?;
        let (tenant, dest) = (self.db.tenant.clone(), dest.to_owned());
        self.db
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let bound = to_i64(bound)?;
                    let removed = conn.execute(sql, &[&tenant, &dest, &bound]).await?;
                    usize::try_from(removed).map_err(|_| OpError::Decode)
                })
            })
            .map_err(queue_error)
    }
}

impl SendQueue for PgSendQueue {
    fn push(&mut self, dest: &str, item: Pending<QueuedBundle>) -> Result<PushOutcome, HubError> {
        self.push_item(dest, item)
    }

    fn peek(&self, dest: &str, n: usize) -> Result<Vec<Pending<QueuedBundle>>, HubError> {
        check_dest(dest)?;
        let (tenant, dest) = (self.db.tenant.clone(), dest.to_owned());
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        self.db
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let mut rows = conn.query(PEEK, &[&tenant, &dest, &limit]).await?;
                    // The item with the lowest serve sequence is always in the window (see
                    // `MemoryQueue::peek`): an acknowledgement is a prefix of the serve sequence.
                    if limit > 1
                        && i64::try_from(rows.len()).is_ok_and(|len| len >= limit)
                        && let Some(head) = conn.query_opt(PEEK_HEAD, &[&tenant, &dest]).await?
                    {
                        let head_seq: i64 = head.get(0);
                        if !rows.iter().any(|row| row.get::<_, i64>(0) == head_seq) {
                            rows.pop();
                            rows.push(head);
                        }
                    }
                    let mut out = Vec::with_capacity(rows.len());
                    for row in rows {
                        let seq = from_i64(row.get::<_, i64>(0))?;
                        let after = from_i64(row.get::<_, i64>(2))?;
                        let text: String = row.get(1);
                        let mut item: Pending<QueuedBundle> =
                            serde_json::from_str(&text).map_err(|_| OpError::Decode)?;
                        item.item.serve_seq = seq;
                        item.item.serve_after = after;
                        out.push(item);
                    }
                    out.sort_by_key(|p| {
                        (
                            p.lane,
                            p.position,
                            p.item.index,
                            p.item.redelivery,
                            p.item.serve_seq,
                        )
                    });
                    Ok(out)
                })
            })
            .map_err(queue_error)
    }

    fn ack(&mut self, dest: &str, up_to: u64) -> Result<usize, HubError> {
        self.trim(dest, ACK_POSITION, up_to)
    }

    fn last_serve_seq(&self, dest: &str) -> Result<u64, HubError> {
        check_dest(dest)?;
        let (tenant, dest) = (self.db.tenant.clone(), dest.to_owned());
        self.db
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_opt(LAST_SEQ, &[&tenant, &dest]).await?;
                    match row {
                        Some(row) => from_i64(row.get::<_, i64>(0)),
                        None => Ok(0),
                    }
                })
            })
            .map_err(queue_error)
    }

    fn lowest_position_after(&self, dest: &str, through: u64) -> Result<Option<u64>, HubError> {
        check_dest(dest)?;
        let (tenant, dest) = (self.db.tenant.clone(), dest.to_owned());
        self.db
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let through = to_i64(through)?;
                    let row = conn
                        .query_one(LOWEST_AFTER, &[&tenant, &dest, &through])
                        .await?;
                    row.get::<_, Option<i64>>(0).map(from_i64).transpose()
                })
            })
            .map_err(queue_error)
    }

    fn ack_served(&mut self, dest: &str, through: u64) -> Result<usize, HubError> {
        self.trim(dest, ACK_SERVED, through)
    }

    fn remove_redelivered(&mut self, dest: &str, position: u64) -> Result<usize, HubError> {
        self.trim(dest, REMOVE_REDELIVERED, position)
    }

    fn queued_bytes(&self, dest: &str) -> Result<usize, HubError> {
        check_dest(dest)?;
        let (tenant, dest) = (self.db.tenant.clone(), dest.to_owned());
        self.db
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_one(BYTES, &[&tenant, &dest]).await?;
                    usize::try_from(from_i64(row.get::<_, i64>(0))?).map_err(|_| OpError::Decode)
                })
            })
            .map_err(queue_error)
    }
}

/// Whether a queue failure was the `sync_hub_pg.queue_full` back-pressure.
#[must_use]
pub fn is_queue_full(error: &HubError) -> bool {
    matches!(error, HubError::QueueFailed { reason } if reason.contains(QUEUE_FULL))
}
