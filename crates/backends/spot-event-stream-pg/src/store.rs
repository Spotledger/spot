//! [`PgConsumerStore`]: the PostgreSQL [`ConsumerStore`].

use deadpool_postgres::{Object, Pool};
use spot_app_api::AppResult;
use spot_core::SiteId;
use spot_event_stream::position::position_to_feed;
use spot_event_stream::store::StoreFuture;
use spot_event_stream::{
    ConsumerRecord, ConsumerSpec, ConsumerStore, MAX_PARTITIONS, RegisterOutcome, StreamError,
};
use spot_events_contract::envelope::Position;
use time::OffsetDateTime;
use tokio_postgres::Row;

use crate::error::{self, corrupt};
use crate::{schema, sql};

/// The event-stream consumer registry and checkpoints of one site, in PostgreSQL.
///
/// See the crate documentation for the rules it keeps. Call [`PgConsumerStore::prepare`] once
/// at start-up; clones share the pool.
#[derive(Clone)]
pub struct PgConsumerStore {
    pool: Pool,
    site: SiteId,
}

impl std::fmt::Debug for PgConsumerStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgConsumerStore")
            .field("site", &self.site)
            .finish_non_exhaustive()
    }
}

impl PgConsumerStore {
    /// A store for `site` on `pool`. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool, site: SiteId) -> Self {
        Self { pool, site }
    }

    /// Creates the tables if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `event_stream_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        schema::prepare(&self.pool).await
    }

    async fn conn(&self) -> Result<Object, StreamError> {
        self.pool.get().await.map_err(error::pool)
    }

    /// The error for a statement that matched no partition row: the consumer does not exist
    /// (`eventstream.consumer_not_found`) or the partition is out of range
    /// (`eventstream.spec_invalid`), exactly as the in-memory store answers.
    async fn missing(
        &self,
        conn: &Object,
        name: &str,
        partition: u32,
    ) -> Result<StreamError, StreamError> {
        let row = conn
            .query_opt(sql::SELECT_PARTITIONS, &[&self.site.as_str(), &name])
            .await
            .map_err(|e| error::pg(&e))?;
        Ok(match row {
            None => StreamError::ConsumerNotFound {
                name: name.to_owned(),
            },
            Some(row) => {
                let partitions: i32 = row.get(0);
                StreamError::spec(format!("partition {partition} is outside 0..{partitions}"))
            }
        })
    }

    /// The bound columns of a record, validated: the record's checkpoints must match its
    /// partition count.
    fn bind(record: &ConsumerRecord) -> Result<Bound, StreamError> {
        let n = record.spec.partitions as usize;
        if record.spec.partitions == 0
            || record.spec.partitions > MAX_PARTITIONS
            || record.checkpoints.len() != n
            || record.parked.len() != n
            || record.parked_since.len() != n
        {
            return Err(StreamError::spec(
                "the record's checkpoints do not match its partition count",
            ));
        }
        let spec = serde_json::to_value(&record.spec)
            .map_err(|_| StreamError::spec("the consumer definition cannot be stored"))?;
        let partitions = i32::try_from(n).map_err(|_| StreamError::spec("too many partitions"))?;
        let mut parts = Vec::with_capacity(n);
        let mut generations = Vec::with_capacity(n);
        let mut seqs = Vec::with_capacity(n);
        for (index, checkpoint) in record.checkpoints.iter().enumerate() {
            let (generation, seq) = columns(*checkpoint)?;
            parts.push(i32::try_from(index).map_err(|_| StreamError::spec("too many partitions"))?);
            generations.push(generation);
            seqs.push(seq);
        }
        Ok(Bound {
            spec,
            partitions,
            parts,
            generations,
            seqs,
        })
    }

    async fn upsert_partitions(
        &self,
        tx: &deadpool_postgres::Transaction<'_>,
        record: &ConsumerRecord,
        bound: &Bound,
    ) -> Result<(), StreamError> {
        tx.execute(
            sql::UPSERT_PARTITIONS,
            &[
                &self.site.as_str(),
                &record.spec.name.as_str(),
                &bound.parts,
                &bound.generations,
                &bound.seqs,
                &record.parked,
                &record.parked_since,
            ],
        )
        .await
        .map_err(|e| error::pg(&e))?;
        Ok(())
    }

    async fn register_record(&self, record: &ConsumerRecord) -> Result<(), StreamError> {
        let bound = Self::bind(record)?;
        let site = self.site.as_str();
        let name = record.spec.name.as_str();
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await.map_err(|e| error::pg(&e))?;
        tx.execute(
            sql::UPSERT_CONSUMER,
            &[
                &site,
                &name,
                &bound.spec,
                &bound.partitions,
                &record.registered_at,
            ],
        )
        .await
        .map_err(|e| error::pg(&e))?;
        tx.execute(
            sql::DELETE_EXTRA_PARTITIONS,
            &[&site, &name, &bound.partitions],
        )
        .await
        .map_err(|e| error::pg(&e))?;
        self.upsert_partitions(&tx, record, &bound).await?;
        tx.commit().await.map_err(|e| error::pg(&e))
    }

    /// Creates the consumer and its partition rows in one transaction, or returns the stored
    /// consumer untouched. The conflicting insert waits for a concurrent creator to commit, so
    /// the follow-up read sees its row.
    async fn register_absent(
        &self,
        record: &ConsumerRecord,
    ) -> Result<RegisterOutcome, StreamError> {
        let bound = Self::bind(record)?;
        let site = self.site.as_str();
        let name = record.spec.name.as_str();
        // A consumer deleted between the lost insert and the read is simply tried again.
        for _ in 0..REGISTER_ATTEMPTS {
            {
                let mut conn = self.conn().await?;
                let tx = conn.transaction().await.map_err(|e| error::pg(&e))?;
                let inserted = tx
                    .execute(
                        sql::INSERT_CONSUMER_IF_ABSENT,
                        &[
                            &site,
                            &name,
                            &bound.spec,
                            &bound.partitions,
                            &record.registered_at,
                        ],
                    )
                    .await
                    .map_err(|e| error::pg(&e))?;
                if inserted == 1 {
                    self.upsert_partitions(&tx, record, &bound).await?;
                    tx.commit().await.map_err(|e| error::pg(&e))?;
                    return Ok(RegisterOutcome::Created);
                }
                tx.rollback().await.map_err(|e| error::pg(&e))?;
            }
            if let Some(existing) = self.load(Some(name)).await?.into_iter().next() {
                return Ok(RegisterOutcome::Existing(Box::new(existing)));
            }
        }
        Err(StreamError::ConsumerBusy {
            name: name.to_owned(),
            reason: "the consumer was removed while it was being registered".to_owned(),
        })
    }

    async fn load(&self, name: Option<&str>) -> Result<Vec<ConsumerRecord>, StreamError> {
        let conn = self.conn().await?;
        let site = self.site.as_str();
        let rows = match name {
            Some(name) => conn.query(sql::SELECT_ONE, &[&site, &name]).await,
            None => conn.query(sql::SELECT_ALL, &[&site]).await,
        }
        .map_err(|e| error::pg(&e))?;
        records_from_rows(&rows)
    }

    async fn checkpoint_of(&self, name: &str, partition: u32) -> Result<Position, StreamError> {
        let conn = self.conn().await?;
        let part = partition_column(partition);
        let row = match part {
            Some(part) => conn
                .query_opt(sql::SELECT_CHECKPOINT, &[&self.site.as_str(), &name, &part])
                .await
                .map_err(|e| error::pg(&e))?,
            None => None,
        };
        match row {
            Some(row) => position_of(row.get(0), row.get(1)),
            None => Err(self.missing(&conn, name, partition).await?),
        }
    }

    async fn advance_checkpoint(
        &self,
        name: &str,
        partition: u32,
        to: Position,
        expected: Position,
    ) -> Result<(), StreamError> {
        let (to_generation, to_seq) = columns(to)?;
        let (expected_generation, expected_seq) = columns(expected)?;
        let conn = self.conn().await?;
        let site = self.site.as_str();
        let Some(part) = partition_column(partition) else {
            return Err(self.missing(&conn, name, partition).await?);
        };
        let behind = position_to_feed(to) < position_to_feed(expected);
        let moved = if behind {
            0
        } else if to == expected {
            // Idempotent: nothing to write, but a stale writer is still refused.
            conn.query_opt(sql::SELECT_CHECKPOINT, &[&site, &name, &part])
                .await
                .map_err(|e| error::pg(&e))?
                .map_or(0, |row| {
                    let stored: (i64, i64) = (row.get(0), row.get(1));
                    u64::from(stored == (expected_generation, expected_seq))
                })
        } else {
            conn.execute(
                sql::ADVANCE,
                &[
                    &site,
                    &name,
                    &part,
                    &to_generation,
                    &to_seq,
                    &expected_generation,
                    &expected_seq,
                ],
            )
            .await
            .map_err(|e| error::pg(&e))?
        };
        if moved == 1 {
            return Ok(());
        }
        // Not advanced: a missing consumer or partition is reported as such, anything else is a
        // refused acknowledgement (stale writer, or a move backwards).
        let exists = conn
            .query_opt(sql::SELECT_CHECKPOINT, &[&site, &name, &part])
            .await
            .map_err(|e| error::pg(&e))?
            .is_some();
        if !exists {
            return Err(self.missing(&conn, name, partition).await?);
        }
        Err(if behind {
            StreamError::ack("a checkpoint never moves backwards")
        } else {
            StreamError::ack("the checkpoint moved since it was read")
        })
    }

    /// Runs one `UPDATE` on a partition row; zero rows is a missing consumer or partition.
    async fn update_partition(
        &self,
        statement: &str,
        name: &str,
        partition: u32,
        extra: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<(), StreamError> {
        let conn = self.conn().await?;
        let site = self.site.as_str();
        let Some(part) = partition_column(partition) else {
            return Err(self.missing(&conn, name, partition).await?);
        };
        let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&site, &name, &part];
        params.extend_from_slice(extra);
        let rows = conn
            .execute(statement, &params)
            .await
            .map_err(|e| error::pg(&e))?;
        if rows == 0 {
            return Err(self.missing(&conn, name, partition).await?);
        }
        Ok(())
    }
}

/// How often `register_if_absent` retries when the consumer vanishes between its lost insert
/// and the read of the winner.
const REGISTER_ATTEMPTS: usize = 3;

/// A record's validated, bound columns.
struct Bound {
    spec: serde_json::Value,
    partitions: i32,
    parts: Vec<i32>,
    generations: Vec<i64>,
    seqs: Vec<i64>,
}

/// A checkpoint component as a database column, refusing what `bigint` cannot hold.
fn columns(position: Position) -> Result<(i64, i64), StreamError> {
    match (
        i64::try_from(position.generation),
        i64::try_from(position.seq),
    ) {
        (Ok(generation), Ok(seq)) => Ok((generation, seq)),
        _ => Err(StreamError::PositionInvalid {
            reason: "a position component is beyond the storable range".to_owned(),
        }),
    }
}

/// A stored position, refusing a negative column.
fn position_of(generation: i64, seq: i64) -> Result<Position, StreamError> {
    match (u64::try_from(generation), u64::try_from(seq)) {
        (Ok(generation), Ok(seq)) => Ok(Position { generation, seq }),
        _ => Err(corrupt("negative position")),
    }
}

/// A partition number as the `part` column; `None` when it cannot be a stored partition.
fn partition_column(partition: u32) -> Option<i32> {
    i32::try_from(partition).ok()
}

/// Groups joined consumer/partition rows into records, failing closed on a row set that is
/// not a whole consumer (a missing or reordered partition, an unparsable definition).
fn records_from_rows(rows: &[Row]) -> Result<Vec<ConsumerRecord>, StreamError> {
    let mut records: Vec<ConsumerRecord> = Vec::new();
    for row in rows {
        let name: String = row.get(0);
        let starts_new = records.last().is_none_or(|last| last.spec.name != name);
        if starts_new {
            if let Some(last) = records.last() {
                check_complete(last)?;
            }
            let spec_json: serde_json::Value = row.get(1);
            let spec: ConsumerSpec =
                serde_json::from_value(spec_json).map_err(|_| corrupt("definition"))?;
            if spec.name != name {
                return Err(corrupt("definition names another consumer"));
            }
            let registered_at: OffsetDateTime = row.get(3);
            records.push(ConsumerRecord {
                spec,
                checkpoints: Vec::new(),
                parked: Vec::new(),
                parked_since: Vec::new(),
                registered_at,
            });
        }
        let part: Option<i32> = row.get(4);
        let Some(part) = part else { continue };
        let Some(record) = records.last_mut() else {
            return Err(corrupt("row order"));
        };
        if usize::try_from(part).ok() != Some(record.checkpoints.len()) {
            return Err(corrupt("partition numbering"));
        }
        record
            .checkpoints
            .push(position_of(row.get(5), row.get(6))?);
        record.parked.push(row.get(7));
        record.parked_since.push(row.get(8));
    }
    if let Some(last) = records.last() {
        check_complete(last)?;
    }
    Ok(records)
}

fn check_complete(record: &ConsumerRecord) -> Result<(), StreamError> {
    if record.checkpoints.len() == record.spec.partitions as usize {
        Ok(())
    } else {
        Err(corrupt("partition count"))
    }
}

impl ConsumerStore for PgConsumerStore {
    fn register<'a>(&'a self, record: &'a ConsumerRecord) -> StoreFuture<'a, ()> {
        Box::pin(self.register_record(record))
    }

    fn register_if_absent<'a>(
        &'a self,
        record: &'a ConsumerRecord,
    ) -> StoreFuture<'a, RegisterOutcome> {
        Box::pin(self.register_absent(record))
    }

    fn get<'a>(&'a self, name: &'a str) -> StoreFuture<'a, Option<ConsumerRecord>> {
        Box::pin(async move { Ok(self.load(Some(name)).await?.into_iter().next()) })
    }

    fn list(&self) -> StoreFuture<'_, Vec<ConsumerRecord>> {
        Box::pin(self.load(None))
    }

    fn checkpoint<'a>(&'a self, name: &'a str, partition: u32) -> StoreFuture<'a, Position> {
        Box::pin(self.checkpoint_of(name, partition))
    }

    fn advance<'a>(
        &'a self,
        name: &'a str,
        partition: u32,
        to: Position,
        expected: Position,
    ) -> StoreFuture<'a, ()> {
        Box::pin(self.advance_checkpoint(name, partition, to, expected))
    }

    fn restart<'a>(&'a self, name: &'a str, partition: u32, from: Position) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let (generation, seq) = columns(from)?;
            self.update_partition(sql::RESTART, name, partition, &[&generation, &seq])
                .await
        })
    }

    fn park<'a>(
        &'a self,
        name: &'a str,
        partition: u32,
        at: OffsetDateTime,
    ) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.update_partition(sql::PARK, name, partition, &[&at])
                .await
        })
    }

    fn unpark<'a>(&'a self, name: &'a str, partition: u32) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.update_partition(sql::UNPARK, name, partition, &[])
                .await
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let conn = self.conn().await?;
            conn.execute(sql::DELETE_CONSUMER, &[&self.site.as_str(), &name])
                .await
                .map_err(|e| error::pg(&e))?;
            Ok(())
        })
    }
}
