//! One read-only snapshot of the site's database, and the two readers over it.
//!
//! The change feed and the journal are compared, so they must be read at **one instant**: a
//! [`Snapshot`] begins one `REPEATABLE READ READ ONLY` transaction on one pooled connection and
//! both readers read inside it. A connection that does not report `transaction_read_only = on`
//! is refused ([`CapturePgError::NotReadOnly`]), so a mistake here can never write to the feed
//! or the outbox; the database enforces it, not this code's good behaviour.
//!
//! A snapshot must be ended with [`Snapshot::finish`]. A snapshot dropped without it discards its
//! connection instead of handing a connection with an open transaction back to the pool.

use deadpool_postgres::{Object, Pool};
use spot_capture_invariant::{FeedEntry, Op, Position, StateEvent};

use crate::error::CapturePgError;
use crate::sql;

/// The newest positions of the two streams in the snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heads {
    /// The stream generation.
    pub generation: i64,
    /// The newest published change-feed position.
    pub feed: i64,
    /// The newest published `doc_state` event position.
    pub events: i64,
    /// The newest change-feed position published at least the lag ago.
    pub feed_stable: i64,
    /// The newest event position published at least the lag ago.
    pub events_stable: i64,
}

/// One read-only snapshot. See the module documentation.
pub struct Snapshot {
    conn: Option<Object>,
}

impl Snapshot {
    /// Takes a connection from `pool` and begins the snapshot.
    ///
    /// # Errors
    ///
    /// [`CapturePgError::ReadFailed`] when no connection or transaction can be had, and
    /// [`CapturePgError::NotReadOnly`] when the transaction is not read-only.
    pub async fn open(pool: &Pool) -> Result<Self, CapturePgError> {
        let conn = pool.get().await.map_err(|_| CapturePgError::ReadFailed)?;
        let mut snapshot = Self { conn: Some(conn) };
        let begun = snapshot.client()?.batch_execute(sql::BEGIN).await;
        if begun.is_err() {
            snapshot.discard();
            return Err(CapturePgError::ReadFailed);
        }
        let mode = snapshot.client()?.query_one(sql::READ_ONLY, &[]).await;
        let on = mode
            .ok()
            .and_then(|row| row.try_get::<usize, String>(0).ok())
            .is_some_and(|mode| mode == "on");
        if !on {
            snapshot.finish().await;
            return Err(CapturePgError::NotReadOnly);
        }
        Ok(snapshot)
    }

    fn client(&self) -> Result<&Object, CapturePgError> {
        self.conn.as_ref().ok_or(CapturePgError::ReadFailed)
    }

    fn discard(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Dropping the detached client closes the connection: the pool never sees it again.
            drop(Object::take(conn));
        }
    }

    /// Ends the snapshot and returns the connection to the pool. A failure to roll back
    /// discards the connection instead.
    pub async fn finish(mut self) {
        let ended = match self.conn.as_ref() {
            Some(conn) => conn.batch_execute(sql::END).await.is_ok(),
            None => true,
        };
        if !ended {
            self.discard();
        }
    }

    /// The generation and the heads of both streams. `lag_secs` is how long a position must have
    /// been published to count as stable.
    ///
    /// # Errors
    ///
    /// [`CapturePgError::ReadFailed`] on a failed statement.
    pub async fn heads(&self, lag_secs: u64) -> Result<Heads, CapturePgError> {
        let conn = self.client()?;
        let lag = i64::try_from(lag_secs).map_err(|_| CapturePgError::OptionsInvalid)?;
        let generation = one_i64(conn, sql::GENERATION, &[]).await?;
        let g: &(dyn tokio_postgres::types::ToSql + Sync) = &generation;
        let l: &(dyn tokio_postgres::types::ToSql + Sync) = &lag;
        Ok(Heads {
            generation,
            feed: one_i64(conn, sql::FEED_HEAD, &[g]).await?,
            events: one_i64(conn, sql::EVENT_HEAD, &[g]).await?,
            feed_stable: one_i64(conn, sql::FEED_STABLE, &[g, l]).await?,
            events_stable: one_i64(conn, sql::EVENT_STABLE, &[g, l]).await?,
        })
    }

    /// Where a resumed scan starts in each stream (the position to read *after*): see
    /// `sql::FEED_START`. Both are 0 when either watermark is 0 or gone.
    ///
    /// # Errors
    ///
    /// [`CapturePgError::ReadFailed`] on a failed statement.
    pub async fn starts(
        &self,
        generation: i64,
        feed_mark: i64,
        event_mark: i64,
        lag_secs: u64,
    ) -> Result<(i64, i64), CapturePgError> {
        let conn = self.client()?;
        let lag = i64::try_from(lag_secs).map_err(|_| CapturePgError::OptionsInvalid)?;
        let (g, f, e, l): (
            &(dyn tokio_postgres::types::ToSql + Sync),
            &(dyn tokio_postgres::types::ToSql + Sync),
            &(dyn tokio_postgres::types::ToSql + Sync),
            &(dyn tokio_postgres::types::ToSql + Sync),
        ) = (&generation, &feed_mark, &event_mark, &lag);
        let feed = one_i64(conn, sql::FEED_START, &[g, f, e, l]).await?;
        let events = one_i64(conn, sql::EVENT_START, &[g, f, e, l]).await?;
        Ok((feed, events))
    }

    /// The change-feed reader over this snapshot.
    #[must_use]
    pub fn feed(&self) -> PgFeedReader<'_> {
        PgFeedReader { snapshot: self }
    }

    /// The `doc_state` reader over this snapshot.
    #[must_use]
    pub fn events(&self) -> PgStateReader<'_> {
        PgStateReader { snapshot: self }
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        // Reached only when `finish` was not awaited (an early return, a cancelled task).
        self.discard();
    }
}

async fn one_i64(
    conn: &Object,
    statement: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<i64, CapturePgError> {
    let row = conn
        .query_one(statement, params)
        .await
        .map_err(|_| CapturePgError::ReadFailed)?;
    row.try_get::<usize, i64>(0)
        .map_err(|_| CapturePgError::Corrupt)
}

fn position(generation: i64, seq: i64) -> Result<Position, CapturePgError> {
    Ok(Position {
        generation: u64::try_from(generation).map_err(|_| CapturePgError::Corrupt)?,
        seq: u64::try_from(seq).map_err(|_| CapturePgError::Corrupt)?,
    })
}

/// Reads the change feed: the published entries after a position, in order, one bounded page at
/// a time. Read-only by construction (see [`Snapshot`]).
pub struct PgFeedReader<'a> {
    snapshot: &'a Snapshot,
}

impl PgFeedReader<'_> {
    /// At most `limit` entries of `generation` after `after`, in position order.
    ///
    /// # Errors
    ///
    /// [`CapturePgError::OptionsInvalid`] for a zero `limit`, [`CapturePgError::ReadFailed`] on a
    /// failed statement and [`CapturePgError::Corrupt`] for a row that does not decode.
    pub async fn page(
        &self,
        generation: i64,
        after: i64,
        limit: usize,
    ) -> Result<Vec<FeedEntry>, CapturePgError> {
        let limit = page_limit(limit)?;
        let rows = self
            .snapshot
            .client()?
            .query(sql::FEED_PAGE, &[&generation, &after, &limit])
            .await
            .map_err(|_| CapturePgError::ReadFailed)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let seq: i64 = row.try_get(0).map_err(|_| CapturePgError::Corrupt)?;
            let version: i64 = row.try_get(3).map_err(|_| CapturePgError::Corrupt)?;
            let op: String = row.try_get(4).map_err(|_| CapturePgError::Corrupt)?;
            out.push(FeedEntry {
                position: position(generation, seq)?,
                entity: row.try_get(1).map_err(|_| CapturePgError::Corrupt)?,
                id: row.try_get(2).map_err(|_| CapturePgError::Corrupt)?,
                version: u64::try_from(version).map_err(|_| CapturePgError::Corrupt)?,
                op: match op.as_str() {
                    "I" => Op::Insert,
                    "U" => Op::Update,
                    "D" => Op::Delete,
                    _ => return Err(CapturePgError::Corrupt),
                },
            });
        }
        Ok(out)
    }
}

/// Reads the journal's `doc_state` events (the outbox, topic `doc_state`) the same way.
pub struct PgStateReader<'a> {
    snapshot: &'a Snapshot,
}

impl PgStateReader<'_> {
    /// At most `limit` events of `generation` after `after`, in position order.
    ///
    /// # Errors
    ///
    /// As [`PgFeedReader::page`]; an event without a version or with an unknown operation is
    /// [`CapturePgError::Corrupt`].
    pub async fn page(
        &self,
        generation: i64,
        after: i64,
        limit: usize,
    ) -> Result<Vec<StateEvent>, CapturePgError> {
        let limit = page_limit(limit)?;
        let rows = self
            .snapshot
            .client()?
            .query(sql::EVENT_PAGE, &[&generation, &after, &limit])
            .await
            .map_err(|_| CapturePgError::ReadFailed)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let seq: i64 = row.try_get(0).map_err(|_| CapturePgError::Corrupt)?;
            let version: Option<i64> = row.try_get(3).map_err(|_| CapturePgError::Corrupt)?;
            let op: Option<String> = row.try_get(4).map_err(|_| CapturePgError::Corrupt)?;
            let version = version
                .and_then(|v| u64::try_from(v).ok())
                .ok_or(CapturePgError::Corrupt)?;
            out.push(StateEvent {
                position: position(generation, seq)?,
                entity: row.try_get(1).map_err(|_| CapturePgError::Corrupt)?,
                id: row.try_get(2).map_err(|_| CapturePgError::Corrupt)?,
                version,
                op: match op.as_deref() {
                    Some("insert") => Op::Insert,
                    Some("update") => Op::Update,
                    Some("delete") => Op::Delete,
                    _ => return Err(CapturePgError::Corrupt),
                },
            });
        }
        Ok(out)
    }
}

fn page_limit(limit: usize) -> Result<i64, CapturePgError> {
    if limit == 0 {
        return Err(CapturePgError::OptionsInvalid);
    }
    i64::try_from(limit).map_err(|_| CapturePgError::OptionsInvalid)
}
