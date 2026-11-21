//! [`PgHubStream`]: the `HubStream` over the tenant's outbox.
//!
//! The outbox (`spot_outbox`) and its published positions (`spot_outbox_pub`, written by the
//! publisher of `spot-postgres`) live in the **tenant's own database**; they carry no tenant
//! column, and this is the one place the crate reads them. A hub stream position is the event's
//! published `seq` within one stream `generation` (a new generation is a new stream: build a new
//! `PgHubStream`), so positions strictly ascend and start above 0.
//!
//! What a hub forwards is a function of the events registry class and of the sync metadata the
//! outbox does not hold (origin node, root or satellite, delivery lane). That decision is the
//! caller's [`EntryMapper`]: it receives each outbox row and returns the [`StreamEntry`], or
//! `None` for an event the hub does not forward (the filter by registry class lives there, and
//! the ledger's own `sys_*` audit topics are dropped there too). A skipped event leaves a gap in
//! positions, which the relay allows (they strictly ascend; they need not be contiguous).
//!
//! **Whole source transactions only.** A page is `max` rows by position; when the last row's
//! transaction has more published rows beyond the page, they are read in the same call and added
//! in position order, so a transaction is never split across two reads.

use spot_sync_hub::{HubError, HubStream, StreamEntry};

use crate::db::Db;
use crate::error::{OpError, stream_error};

/// Deadline of a page read.
pub const READ_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
/// The most rows one call may return, whatever `max` says (whole-transaction completion
/// included): a transaction larger than this fails the read rather than loading it unbounded.
pub const MAX_ROWS: usize = 50_000;

const PAGE: &str = "SELECT p.seq, o.tx::text, o.ordinal, o.topic, o.subject_entity, \
    o.subject_id::text, o.subject_version, o.schema_version, o.payload \
    FROM spot_outbox_pub p JOIN spot_outbox o ON o.id = p.event_id \
    WHERE p.generation = $1 AND p.seq > $2 ORDER BY p.seq LIMIT $3";
const TXN_REST: &str = "SELECT p.seq, o.tx::text, o.ordinal, o.topic, o.subject_entity, \
    o.subject_id::text, o.subject_version, o.schema_version, o.payload \
    FROM spot_outbox_pub p JOIN spot_outbox o ON o.id = p.event_id \
    WHERE p.generation = $1 AND p.seq > $2 AND o.tx = $3::text::uuid ORDER BY p.seq LIMIT $4";
const HEAD: &str = "SELECT COALESCE(max(seq), 0) FROM spot_outbox_pub WHERE generation = $1";

/// One published outbox event, as the mapper sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxRow {
    /// The published position (`seq`) within the stream's generation.
    pub position: u64,
    /// The source transaction.
    pub txn: String,
    /// The event's ordinal within its transaction.
    pub ordinal: i32,
    /// The event topic.
    pub topic: String,
    /// The subject entity.
    pub entity: String,
    /// The subject record id.
    pub id: String,
    /// The record's version after the event, when the event carries one.
    pub version: Option<u64>,
    /// The event's schema version.
    pub schema_version: i32,
    /// The event payload as stored. A mapper must not copy it into a log or an error.
    pub payload: String,
}

/// Turns an outbox row into the hub's stream entry, or drops it.
pub trait EntryMapper: Send + Sync {
    /// The entry for `row`, or `None` when the hub does not forward the event.
    fn map(&self, row: &OutboxRow) -> Option<StreamEntry>;
}

/// The hub stream of one tenant over its outbox.
pub struct PgHubStream<M: EntryMapper> {
    db: Db,
    generation: u64,
    mapper: M,
}

impl<M: EntryMapper> std::fmt::Debug for PgHubStream<M> {
    /// Names the tenant and the generation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgHubStream")
            .field("tenant", &self.db.tenant)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl<M: EntryMapper> PgHubStream<M> {
    /// A stream over generation `generation` of the tenant's outbox, mapping with `mapper`.
    #[must_use]
    pub fn new(db: Db, generation: u64, mapper: M) -> Self {
        Self {
            db,
            generation,
            mapper,
        }
    }
}

fn decode(row: &tokio_postgres::Row) -> Result<OutboxRow, OpError> {
    let position = u64::try_from(row.get::<_, i64>(0)).map_err(|_| OpError::Decode)?;
    let version = row
        .get::<_, Option<i64>>(6)
        .map(u64::try_from)
        .transpose()
        .map_err(|_| OpError::Decode)?;
    Ok(OutboxRow {
        position,
        txn: row.get(1),
        ordinal: row.get(2),
        topic: row.get(3),
        entity: row.get(4),
        id: row.get(5),
        version,
        schema_version: row.get(7),
        payload: row.get(8),
    })
}

impl<M: EntryMapper> HubStream for PgHubStream<M> {
    fn read_after(&self, after: u64, max: usize) -> Result<Vec<StreamEntry>, HubError> {
        let generation = i64::try_from(self.generation).map_err(|_| HubError::StreamFailed {
            reason: "the generation does not fit".to_owned(),
        })?;
        let cap = max.clamp(1, MAX_ROWS);
        let rows = self
            .db
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let after = i64::try_from(after).map_err(|_| OpError::Decode)?;
                    let limit = i64::try_from(cap).map_err(|_| OpError::Decode)?;
                    let page = conn.query(PAGE, &[&generation, &after, &limit]).await?;
                    let mut rows = page.iter().map(decode).collect::<Result<Vec<_>, _>>()?;
                    if let Some(last) = rows.last().cloned() {
                        // Complete the last transaction: everything of it published beyond the
                        // page, bounded by `MAX_ROWS` in total.
                        let room = i64::try_from(MAX_ROWS.saturating_sub(rows.len()) + 1)
                            .map_err(|_| OpError::Decode)?;
                        let beyond = i64::try_from(last.position).map_err(|_| OpError::Decode)?;
                        let rest = conn
                            .query(TXN_REST, &[&generation, &beyond, &last.txn, &room])
                            .await?;
                        if rest.len() >= usize::try_from(room).unwrap_or(usize::MAX) {
                            // A transaction larger than the bound: refuse, never half-read it.
                            return Err(OpError::Decode);
                        }
                        for row in &rest {
                            rows.push(decode(row)?);
                        }
                    }
                    Ok(rows)
                })
            })
            .map_err(stream_error)?;
        Ok(rows.iter().filter_map(|row| self.mapper.map(row)).collect())
    }

    fn head(&self) -> Result<u64, HubError> {
        let generation = i64::try_from(self.generation).map_err(|_| HubError::StreamFailed {
            reason: "the generation does not fit".to_owned(),
        })?;
        self.db
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_one(HEAD, &[&generation]).await?;
                    u64::try_from(row.get::<_, i64>(0)).map_err(|_| OpError::Decode)
                })
            })
            .map_err(stream_error)
    }
}
