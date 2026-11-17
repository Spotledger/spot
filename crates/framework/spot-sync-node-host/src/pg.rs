//! The PostgreSQL pieces of the edge sync service, on the shared blocking bridge (ADR-082
//! addendum 1): the cursor table, the node's outbox reader and the two cursor stores.
//!
//! Everything here is synchronous and refuses to be built or called inside an async runtime
//! (`pg_bridge.wrong_thread`): the loop reaches it only through a
//! [`Worker`](crate::worker::Worker) thread, and the host builds it on that thread.
//!
//! # The table
//!
//! `spot_sync_node_cursor(tenant, stream, seq, position, updated_at)`: two rows per tenant,
//! `serve` (the acknowledged serve-sequence prefix of the hub's queue; `position` unused) and
//! `push` (the acknowledged push sequence and the outbox position of that record). The tenant
//! predicate is in every statement ([`STATEMENTS`]); the host creates nothing at start-up: it
//! refuses to run the service with the table missing and says to run `spot migrate`
//! ([`PgCursors::require_table`]).
//!
//! # The outbox reader
//!
//! `spot_outbox` and its published positions `spot_outbox_pub` live in the tenant's own database
//! and carry no tenant column. The reader returns **whole source transactions only**, exactly as
//! the hub's stream does, so a bundle of one transaction is never split across two pushes.

use std::sync::Arc;
use std::time::Duration;

use spot_pg_bridge::Bridge;

use crate::applier::CursorStore;
use crate::error::{Error, Result};
use crate::push::{OutboxEntry, OutboxSource, PushCursor, PushCursorStore};

/// The cursor table's idempotent DDL, one statement per object, for the migrator's extra-table
/// hook (`spot-cli`'s `schema::extra_tables`).
pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_sync_node_cursor (\
    tenant text NOT NULL, \
    stream text NOT NULL CHECK (stream IN ('serve', 'push')), \
    seq bigint NOT NULL DEFAULT 0 CHECK (seq >= 0), \
    position bigint NOT NULL DEFAULT 0 CHECK (position >= 0), \
    updated_at timestamptz NOT NULL DEFAULT now(), \
    PRIMARY KEY (tenant, stream))";

/// The table this module owns.
pub const TABLES: [&str; 1] = ["spot_sync_node_cursor"];

/// Every statement the module runs, for the tenant-predicate scan.
pub const STATEMENTS: [&str; 4] = [LOAD, STORE, MISSING, QUEUED];

const LOAD: &str =
    "SELECT seq, position FROM spot_sync_node_cursor WHERE tenant = $1 AND stream = $2";
const STORE: &str = "INSERT INTO spot_sync_node_cursor (tenant, stream, seq, position) \
    VALUES ($1, $2, $3, $4) \
    ON CONFLICT (tenant, stream) DO UPDATE SET seq = EXCLUDED.seq, position = EXCLUDED.position, \
    updated_at = now() \
    WHERE spot_sync_node_cursor.seq < EXCLUDED.seq";
const MISSING: &str = "SELECT to_regclass('public.spot_sync_node_cursor') IS NOT NULL";
const PAGE: &str = "SELECT p.seq, o.tx::text, o.ordinal, o.topic, o.subject_entity, \
    o.subject_id::text, o.subject_version, o.schema_version, o.payload \
    FROM spot_outbox_pub p JOIN spot_outbox o ON o.id = p.event_id \
    WHERE p.generation = $1 AND p.seq > $2 ORDER BY p.seq LIMIT $3";
const TXN_REST: &str = "SELECT p.seq, o.tx::text, o.ordinal, o.topic, o.subject_entity, \
    o.subject_id::text, o.subject_version, o.schema_version, o.payload \
    FROM spot_outbox_pub p JOIN spot_outbox o ON o.id = p.event_id \
    WHERE p.generation = $1 AND p.seq > $2 AND o.tx = $3::text::uuid ORDER BY p.seq LIMIT $4";
const QUEUED: &str = "SELECT count(*) FROM spot_outbox_pub WHERE generation = $1 AND seq > $2";

/// Deadline of one bridged call.
pub const DEADLINE: Duration = Duration::from_secs(15);
/// The most rows one read returns, whole-transaction completion included.
pub const MAX_ROWS: usize = 50_000;

/// What went wrong inside one bridged call; mapped to a fixed message, never shown raw (a driver
/// message can quote a key or a value).
#[derive(Debug)]
enum Op {
    Db,
    Decode,
}

impl From<tokio_postgres::Error> for Op {
    fn from(_error: tokio_postgres::Error) -> Self {
        Self::Db
    }
}

fn map<E>(error: spot_pg_bridge::BridgeError<E>) -> Error {
    match error.code() {
        Some(code) => Error::store_failed(format!("the database is not available ({code})")),
        None => Error::store_failed("a database statement failed"),
    }
}

/// One published outbox event, as a [`PushMapper`] sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxRow {
    /// The published position within the stream's generation.
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
    /// The record version after the event, when the event carries one.
    pub version: Option<u64>,
    /// The event's schema version.
    pub schema_version: i32,
    /// The payload as stored. A mapper must not copy it into a log or an error.
    pub payload: String,
}

/// Turns an outbox row into a push entry, or drops it (an event the hub does not forward).
///
/// The class decision (State, Domain and Audit events of the partitions the node owns are
/// forwarded; the node's own `sys_*` bookkeeping topics are not) lives here, in the caller's
/// events registry, never in this crate.
pub trait PushMapper: Send + Sync {
    /// The entry for `row`, or `None` when it is not forwarded.
    fn map(&self, row: &OutboxRow) -> Option<OutboxEntry>;
}

/// A [`PushMapper`] that forwards every event with a record version, named `<entity>/<id>`,
/// except topics that start with `sys_` (the node's own bookkeeping), marking each owned: the
/// node's outbox holds only its own writes, because a replicated record enqueues no event and
/// the write gate refuses a write to a record the node does not own.
#[derive(Clone, Copy, Debug, Default)]
pub struct OwnWritesMapper;

impl PushMapper for OwnWritesMapper {
    fn map(&self, row: &OutboxRow) -> Option<OutboxEntry> {
        if row.topic.starts_with("sys_") {
            return None;
        }
        let version = row.version?;
        Some(OutboxEntry {
            position: row.position,
            name: format!("{}/{}", row.entity, row.id),
            version,
            payload: row.payload.clone(),
            owned: true,
        })
    }
}

/// The cursor table and the checks around it, for one tenant on one bridge.
#[derive(Clone)]
pub struct PgCursors {
    bridge: Arc<Bridge>,
    tenant: String,
}

impl std::fmt::Debug for PgCursors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgCursors")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl PgCursors {
    /// Cursors of `tenant`. No I/O happens here.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed` when called inside an async runtime or with an unusable
    /// tenant.
    pub fn new(bridge: Arc<Bridge>, tenant: impl Into<String>) -> Result<Self> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(Error::store_failed(
                "the cursor store is built inside an async runtime; build it on a plain thread",
            ));
        }
        let tenant = tenant.into();
        if tenant.is_empty() || tenant.len() > 128 || tenant.chars().any(char::is_control) {
            return Err(Error::store_failed(
                "the tenant is empty, too long or not printable",
            ));
        }
        Ok(Self { bridge, tenant })
    }

    /// As [`PgCursors::new`] without the runtime check, for a value that is only handed to a
    /// factory which builds its stores on the worker's plain thread.
    pub fn new_unchecked(bridge: Arc<Bridge>, tenant: impl Into<String>) -> Self {
        Self {
            bridge,
            tenant: tenant.into(),
        }
    }

    /// Fails when the cursor table does not exist: the host refuses to start the service and says
    /// to run `spot migrate`; it never creates the table itself.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    pub fn require_table(&self) -> Result<()> {
        let present = self
            .bridge
            .run(DEADLINE, |conn| {
                Box::pin(async move {
                    let row = conn.query_one(MISSING, &[]).await?;
                    Ok::<bool, Op>(row.get::<_, bool>(0))
                })
            })
            .map_err(map)?;
        if present {
            Ok(())
        } else {
            Err(Error::store_failed(
                "spot_sync_node_cursor is missing: run `spot migrate`",
            ))
        }
    }

    fn load(&self, stream: &'static str) -> Result<(u64, u64)> {
        let tenant = self.tenant.clone();
        self.bridge
            .run(DEADLINE, move |conn| {
                Box::pin(async move {
                    let rows = conn.query(LOAD, &[&tenant, &stream]).await?;
                    let Some(row) = rows.first() else {
                        return Ok::<(u64, u64), Op>((0, 0));
                    };
                    let seq = u64::try_from(row.get::<_, i64>(0)).map_err(|_| Op::Decode)?;
                    let position = u64::try_from(row.get::<_, i64>(1)).map_err(|_| Op::Decode)?;
                    Ok((seq, position))
                })
            })
            .map_err(map)
    }

    fn store(&self, stream: &'static str, seq: u64, position: u64) -> Result<()> {
        let tenant = self.tenant.clone();
        let seq = i64::try_from(seq).map_err(|_| Error::store_failed("the cursor is too large"))?;
        let position =
            i64::try_from(position).map_err(|_| Error::store_failed("the cursor is too large"))?;
        self.bridge
            .run(DEADLINE, move |conn| {
                Box::pin(async move {
                    conn.execute(STORE, &[&tenant, &stream, &seq, &position])
                        .await?;
                    Ok::<(), Op>(())
                })
            })
            .map_err(map)
    }

    /// The serve-prefix cursor store.
    pub fn serve(&self) -> PgServeCursor {
        PgServeCursor(self.clone())
    }

    /// The push cursor store.
    pub fn push(&self) -> PgPushCursor {
        PgPushCursor(self.clone())
    }
}

/// [`CursorStore`] over `spot_sync_node_cursor` (stream `serve`).
#[derive(Clone, Debug)]
pub struct PgServeCursor(PgCursors);

impl CursorStore for PgServeCursor {
    fn load(&mut self) -> Result<u64> {
        Ok(self.0.load("serve")?.0)
    }

    fn store(&mut self, prefix: u64) -> Result<()> {
        self.0.store("serve", prefix, 0)
    }
}

/// [`PushCursorStore`] over `spot_sync_node_cursor` (stream `push`).
#[derive(Clone, Debug)]
pub struct PgPushCursor(PgCursors);

impl PushCursorStore for PgPushCursor {
    fn load(&mut self) -> Result<PushCursor> {
        let (seq, position) = self.0.load("push")?;
        Ok(PushCursor { seq, position })
    }

    fn store(&mut self, cursor: PushCursor) -> Result<()> {
        self.0.store("push", cursor.seq, cursor.position)
    }
}

/// [`OutboxSource`] over the tenant's outbox, one stream generation.
pub struct PgOutboxReader<M: PushMapper> {
    bridge: Arc<Bridge>,
    generation: u64,
    mapper: Arc<M>,
}

impl<M: PushMapper> std::fmt::Debug for PgOutboxReader<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgOutboxReader")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl<M: PushMapper + 'static> PgOutboxReader<M> {
    /// A reader over `generation` of the outbox, mapping with `mapper`.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed` inside an async runtime.
    pub fn new(bridge: Arc<Bridge>, generation: u64, mapper: M) -> Result<Self> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(Error::store_failed(
                "the outbox reader is built inside an async runtime; build it on a plain thread",
            ));
        }
        Ok(Self {
            bridge,
            generation,
            mapper: Arc::new(mapper),
        })
    }
}

fn decode(row: &tokio_postgres::Row) -> std::result::Result<OutboxRow, Op> {
    let position = u64::try_from(row.get::<_, i64>(0)).map_err(|_| Op::Decode)?;
    let version = row
        .get::<_, Option<i64>>(6)
        .map(u64::try_from)
        .transpose()
        .map_err(|_| Op::Decode)?;
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

impl<M: PushMapper + 'static> OutboxSource for PgOutboxReader<M> {
    fn read_after(&mut self, after: u64, max: usize) -> Result<Vec<OutboxEntry>> {
        let generation = i64::try_from(self.generation)
            .map_err(|_| Error::store_failed("the generation does not fit"))?;
        let cap = max.clamp(1, MAX_ROWS);
        let rows = self
            .bridge
            .run(DEADLINE, move |conn| {
                Box::pin(async move {
                    let after = i64::try_from(after).map_err(|_| Op::Decode)?;
                    let limit = i64::try_from(cap).map_err(|_| Op::Decode)?;
                    let page = conn.query(PAGE, &[&generation, &after, &limit]).await?;
                    let mut rows = page
                        .iter()
                        .map(decode)
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    if let Some(last) = rows.last().cloned() {
                        // Complete the last transaction: whatever of it was published beyond the
                        // page, bounded by `MAX_ROWS` in total.
                        let room = i64::try_from(MAX_ROWS.saturating_sub(rows.len()) + 1)
                            .map_err(|_| Op::Decode)?;
                        let beyond = i64::try_from(last.position).map_err(|_| Op::Decode)?;
                        let rest = conn
                            .query(TXN_REST, &[&generation, &beyond, &last.txn, &room])
                            .await?;
                        for row in &rest {
                            rows.push(decode(row)?);
                        }
                        if rows.len() > MAX_ROWS {
                            return Err(Op::Decode);
                        }
                    }
                    Ok::<Vec<OutboxRow>, Op>(rows)
                })
            })
            .map_err(map)?;
        let mapper = Arc::clone(&self.mapper);
        Ok(rows.iter().filter_map(|row| mapper.map(row)).collect())
    }

    fn queued_after(&mut self, after: u64) -> Result<u64> {
        let generation = i64::try_from(self.generation)
            .map_err(|_| Error::store_failed("the generation does not fit"))?;
        self.bridge
            .run(DEADLINE, move |conn| {
                Box::pin(async move {
                    let after = i64::try_from(after).map_err(|_| Op::Decode)?;
                    let row = conn.query_one(QUEUED, &[&generation, &after]).await?;
                    u64::try_from(row.get::<_, i64>(0)).map_err(|_| Op::Decode)
                })
            })
            .map_err(map)
    }
}
