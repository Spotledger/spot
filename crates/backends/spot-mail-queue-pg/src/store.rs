//! [`PgSentLog`]: the durable, shared [`SentLog`].

use deadpool_postgres::{Object, Pool};
use spot_app_api::AppResult;
use spot_core::SiteId;
use spot_mail::SendReceipt;
use spot_mail_queue::{SentLog, Unavailable};
use time::OffsetDateTime;

use crate::convert::{StoredReceipt, micros, receipt_from_row};
use crate::error;
use crate::{schema, sql};

/// The sent log in PostgreSQL.
///
/// See the crate documentation for the rules it keeps. Call [`PgSentLog::prepare`] once at
/// start-up, before the first send; clones share the pool, so one instance per process is enough
/// and a second node needs nothing but the same database.
#[derive(Clone)]
pub struct PgSentLog {
    pool: Pool,
}

impl std::fmt::Debug for PgSentLog {
    /// Shows no connection detail: the pool's own `Debug` names the host and the database.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSentLog").finish_non_exhaustive()
    }
}

impl PgSentLog {
    /// A log on `pool`. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Creates the table, its constraint and its indexes if they are missing. Idempotent and safe
    /// to run from several nodes at once.
    ///
    /// # Errors
    /// `mail_queue_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        schema::prepare(&self.pool).await
    }

    /// Removes this site's receipts recorded before `before`, in bounded batches, and returns how
    /// many rows were removed. A second call with the same argument removes nothing.
    ///
    /// # Choosing `before`
    ///
    /// The instant is the **caller's argument**; this store reads no clock. It decides whether a
    /// retry can still find its receipt, so it must lie further back than the deployment's
    /// maximum retry horizon: every attempt the job engine may still make for a message, which is
    /// the queue policy's longest backoff summed over `max_attempts`, plus the time a job may wait
    /// in the queue between attempts, plus a margin for clock skew between the node that computes
    /// this argument and the database that stamped the rows (the comparison is between the two
    /// clocks), plus room for an operator re-queueing an old job by hand.
    ///
    /// **The horizon must exceed the job engine's longest retry delay.** A receipt removed while a
    /// retry is still possible is a message the queue can send a second time, which is the one
    /// thing this table exists to prevent — an over-long horizon costs disk, an over-short one
    /// costs a duplicate delivery. When in doubt, keep receipts longer than the queue keeps its
    /// dead jobs.
    ///
    /// Each statement removes at most 2,000 rows, oldest first, served by
    /// `spot_mail_sent_log_recorded_idx`, so a purge never holds a long lock on the table.
    ///
    /// # Errors
    /// `Unavailable` when the database cannot be reached or answers with an error.
    pub async fn purge_older_than(
        &self,
        site: &SiteId,
        before: OffsetDateTime,
    ) -> Result<u64, Unavailable> {
        let cutoff = micros(before)?;
        let batch = i64::try_from(sql::PURGE_BATCH_ROWS).map_err(|_| error::unreadable())?;
        let conn = self.conn().await?;
        let mut removed = 0_u64;
        loop {
            let changed = conn
                .execute(sql::PURGE_BATCH, &[&site.as_str(), &cutoff, &batch])
                .await
                .map_err(|e| error::unavailable(&e))?;
            removed += changed;
            if changed < sql::PURGE_BATCH_ROWS {
                return Ok(removed);
            }
        }
    }

    async fn conn(&self) -> Result<Object, Unavailable> {
        self.pool
            .get()
            .await
            .map_err(|e| error::unavailable_pool(&e))
    }
}

#[async_trait::async_trait]
impl SentLog for PgSentLog {
    /// One point read of the primary key: the row's existence is the answer. An error is an
    /// outage, never "not sent" — the queue's caller reads the two apart.
    async fn is_sent(&self, site: &SiteId, message_id: &str) -> Result<bool, Unavailable> {
        let conn = self.conn().await?;
        let row = conn
            .query_opt(sql::IS_SENT, &[&site.as_str(), &message_id])
            .await
            .map_err(|e| error::unavailable(&e))?;
        Ok(row.is_some())
    }

    async fn record_sent(
        &self,
        site: &SiteId,
        message_id: &str,
        receipt: &SendReceipt,
    ) -> Result<bool, Unavailable> {
        let stored = StoredReceipt::of(receipt);
        let conn = self.conn().await?;
        let changed = conn
            .execute(
                sql::RECORD_SENT,
                &[
                    &site.as_str(),
                    &message_id,
                    &stored.accepted,
                    &stored.rejected,
                    &stored.rejected_code,
                ],
            )
            .await
            .map_err(|e| error::unavailable(&e))?;
        // One row modified means this call wrote the receipt; none means another writer was
        // already there (or, in the same statement, this one lost the conflict).
        Ok(changed == 1)
    }

    async fn recorded(
        &self,
        site: &SiteId,
        message_id: &str,
    ) -> Result<Option<SendReceipt>, Unavailable> {
        let conn = self.conn().await?;
        let row = conn
            .query_opt(sql::SELECT_RECEIPT, &[&site.as_str(), &message_id])
            .await
            .map_err(|e| error::unavailable(&e))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let accepted: Vec<String> = row.get(0);
        let rejected: Vec<String> = row.get(1);
        let rejected_code: Vec<i32> = row.get(2);
        receipt_from_row(message_id, accepted, rejected, rejected_code).map(Some)
    }
}
