//! [`PgSeenStore`]: the durable set of handled messages.

use async_trait::async_trait;
use deadpool_postgres::Pool;
use spot_core::SiteId;
use spot_mail_intake::{SeenStore, StoreError};
use time::OffsetDateTime;

use crate::{conn, schema, sql, unavailable};

/// The Message-ID digests of each account of one site, in PostgreSQL.
#[derive(Clone)]
pub struct PgSeenStore {
    pool: Pool,
    site: SiteId,
}

impl std::fmt::Debug for PgSeenStore {
    /// Shows no connection detail: the pool's own `Debug` names the host and the database.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSeenStore").finish_non_exhaustive()
    }
}

/// An instant as Unix microseconds, or `Unavailable` when it does not fit.
fn micros(at: OffsetDateTime) -> Result<i64, StoreError> {
    i64::try_from(at.unix_timestamp_nanos() / 1_000).map_err(|_| StoreError::Unavailable)
}

impl PgSeenStore {
    /// A store for `site` on `pool`. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool, site: SiteId) -> Self {
        Self { pool, site }
    }

    /// Creates the tables if they are missing; idempotent and safe from several nodes.
    ///
    /// # Errors
    /// `Unavailable` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> Result<(), StoreError> {
        schema::prepare(&self.pool).await
    }

    /// Removes this site's digests recorded before `instant`, in bounded batches of 2,000 rows
    /// (oldest first), and returns how many were removed. Same as the port's
    /// `purge_older_than`, under the name the task brief uses.
    ///
    /// # Choosing `instant`
    ///
    /// It is the caller's argument; this store reads no clock. It must lie further back than
    /// any rescan of a mailbox: a mailbox whose `UIDVALIDITY` changed is read again from the
    /// start, and a digest purged before that is a message that can be imported twice.
    ///
    /// # Errors
    /// `Unavailable` when the database cannot be reached or answers with an error.
    pub async fn purge_seen_older_than(&self, instant: OffsetDateTime) -> Result<u64, StoreError> {
        let cutoff = micros(instant)?;
        let batch = i64::try_from(sql::PURGE_BATCH_ROWS).map_err(|_| StoreError::Unavailable)?;
        let connection = conn(&self.pool).await?;
        let mut removed = 0_u64;
        loop {
            let changed = connection
                .execute(
                    sql::SEEN_PURGE_BATCH,
                    &[&self.site.as_str(), &cutoff, &batch],
                )
                .await
                .map_err(|e| unavailable(&e))?;
            removed += changed;
            if changed < sql::PURGE_BATCH_ROWS {
                return Ok(removed);
            }
        }
    }
}

#[async_trait]
impl SeenStore for PgSeenStore {
    async fn contains(&self, account: &str, digest: &str) -> Result<bool, StoreError> {
        let connection = conn(&self.pool).await?;
        let row = connection
            .query_opt(
                sql::SEEN_CONTAINS,
                &[&self.site.as_str(), &account, &digest],
            )
            .await
            .map_err(|e| unavailable(&e))?;
        Ok(row.is_some())
    }

    async fn first_time(
        &self,
        account: &str,
        digest: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let seen_at = micros(now)?;
        let connection = conn(&self.pool).await?;
        let changed = connection
            .execute(
                sql::SEEN_INSERT,
                &[&self.site.as_str(), &account, &digest, &seen_at],
            )
            .await
            .map_err(|e| unavailable(&e))?;
        Ok(changed == 1)
    }

    async fn purge_older_than(&self, cutoff: OffsetDateTime) -> Result<u64, StoreError> {
        self.purge_seen_older_than(cutoff).await
    }
}
