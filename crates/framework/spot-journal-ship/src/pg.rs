//! The PostgreSQL side: the watermark table and the outbox reader over the site's backend.

use deadpool_postgres::Pool;
use spot_events_contract::envelope::Position;
use spot_events_contract::{EventEnvelope, SourceId};
use spot_postgres::PgBackend;
use spot_storage_contract::{ChangeFeed, ErrorKind, FeedPosition};

use crate::error::ShipFault;
use crate::ports::{
    ForkMark, Mark, OutboxEntry, OutboxRead, OutboxSource, PortFuture, WatermarkStore,
};

/// The shipper's state table: one row per site and source. The watermark is the last position
/// the relay reported durable; the row's existence is what switches the outbox barrier on
/// (`spot-postgres` `feed::cleanup` never prunes above it). The host's `spot migrate` creates
/// the table through `schema::extra_tables`; the serving host never does. One database is one
/// site (ADR-34): there is no tenant column.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_journal_ship_state (
    source            text PRIMARY KEY,
    generation        bigint NOT NULL CHECK (generation >= 0),
    seq               bigint NOT NULL CHECK (seq >= 0),
    forked_from       bigint CHECK (forked_from >= 0),
    forked_at_millis  bigint,
    updated_at_millis bigint NOT NULL DEFAULT 0
);
"#;

/// The tables [`DDL`] creates, for a presence check.
pub const TABLES: [&str; 1] = ["spot_journal_ship_state"];

const LOAD: &str = "SELECT generation, seq, forked_from, forked_at_millis \
                    FROM spot_journal_ship_state WHERE source = $1";

const SAVE: &str = "INSERT INTO spot_journal_ship_state \
        (source, generation, seq, forked_from, forked_at_millis, updated_at_millis) \
        VALUES ($1, $2, $3, $4, $5, $6) \
        ON CONFLICT (source) DO UPDATE SET generation = EXCLUDED.generation, \
        seq = EXCLUDED.seq, forked_from = EXCLUDED.forked_from, \
        forked_at_millis = EXCLUDED.forked_at_millis, \
        updated_at_millis = EXCLUDED.updated_at_millis";

/// The watermark in `spot_journal_ship_state`.
#[derive(Clone)]
pub struct PgWatermarks {
    pool: Pool,
}

impl PgWatermarks {
    /// Watermarks over the site's own pool.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Whether the table exists: the host starts the service degraded with the reason when not.
    pub async fn present(&self) -> bool {
        let Ok(conn) = self.pool.get().await else {
            return false;
        };
        conn.query_one(
            "SELECT to_regclass('spot_journal_ship_state') IS NOT NULL",
            &[],
        )
        .await
        .is_ok_and(|row| row.try_get::<_, bool>(0).unwrap_or(false))
    }
}

fn to_i64(value: u64) -> Result<i64, ShipFault> {
    i64::try_from(value).map_err(|_| ShipFault::Watermark)
}

fn to_u64(value: i64) -> Result<u64, ShipFault> {
    u64::try_from(value).map_err(|_| ShipFault::Watermark)
}

impl WatermarkStore for PgWatermarks {
    fn load<'a>(&'a self, source: &'a str) -> PortFuture<'a, Result<Option<Mark>, ShipFault>> {
        Box::pin(async move {
            let conn = self.pool.get().await.map_err(|_| ShipFault::Watermark)?;
            let row = conn
                .query_opt(LOAD, &[&source])
                .await
                .map_err(|_| ShipFault::Watermark)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let generation: i64 = row.try_get(0).map_err(|_| ShipFault::Watermark)?;
            let seq: i64 = row.try_get(1).map_err(|_| ShipFault::Watermark)?;
            let forked_from: Option<i64> = row.try_get(2).map_err(|_| ShipFault::Watermark)?;
            let forked_at: Option<i64> = row.try_get(3).map_err(|_| ShipFault::Watermark)?;
            let fork = match (forked_from, forked_at) {
                (Some(from), Some(at)) => Some(ForkMark {
                    forked_from: to_u64(from)?,
                    at_millis: at,
                }),
                _ => None,
            };
            Ok(Some(Mark {
                position: Position {
                    generation: to_u64(generation)?,
                    seq: to_u64(seq)?,
                },
                fork,
            }))
        })
    }

    fn save<'a>(
        &'a self,
        source: &'a str,
        mark: &'a Mark,
        now_millis: i64,
    ) -> PortFuture<'a, Result<(), ShipFault>> {
        Box::pin(async move {
            let generation = to_i64(mark.position.generation)?;
            let seq = to_i64(mark.position.seq)?;
            let forked_from = mark
                .fork
                .as_ref()
                .map(|f| to_i64(f.forked_from))
                .transpose()?;
            let forked_at = mark.fork.as_ref().map(|f| f.at_millis);
            let conn = self.pool.get().await.map_err(|_| ShipFault::Watermark)?;
            conn.execute(
                SAVE,
                &[
                    &source,
                    &generation,
                    &seq,
                    &forked_from,
                    &forked_at,
                    &now_millis,
                ],
            )
            .await
            .map_err(|_| ShipFault::Watermark)?;
            Ok(())
        })
    }
}

/// The site's outbox over its PostgreSQL backend: published entries as envelope lines.
#[derive(Clone)]
pub struct PgOutbox {
    backend: PgBackend,
    source: SourceId,
}

impl PgOutbox {
    /// A reader that stamps envelopes with `source` (the site's event source id).
    #[must_use]
    pub fn new(backend: PgBackend, source: SourceId) -> Self {
        Self { backend, source }
    }
}

impl OutboxSource for PgOutbox {
    fn read(&self, after: Position, limit: usize) -> PortFuture<'_, Result<OutboxRead, ShipFault>> {
        Box::pin(async move {
            let limit = u32::try_from(limit).unwrap_or(u32::MAX);
            let bookmark = FeedPosition::new(after.generation, after.seq);
            let records = match self.backend.read_outbox(bookmark, limit).await {
                Ok(records) => records,
                Err(error) if error.kind == ErrorKind::ResyncRequired => {
                    return Ok(OutboxRead::Resync);
                }
                Err(_) => return Err(ShipFault::OutboxUnavailable),
            };
            let mut entries = Vec::with_capacity(records.len());
            for record in &records {
                let envelope = EventEnvelope::from_record(&self.source, record)
                    .map_err(|_| ShipFault::OutboxInvalid("an outbox entry is not an envelope"))?;
                let line = envelope
                    .to_json()
                    .map_err(|_| ShipFault::OutboxInvalid("an envelope cannot be written"))?;
                entries.push(OutboxEntry {
                    position: Position {
                        generation: record.position.generation(),
                        seq: record.position.seq(),
                    },
                    line: line.into_bytes(),
                });
            }
            Ok(OutboxRead::Entries(entries))
        })
    }
}
