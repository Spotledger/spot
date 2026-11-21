//! The write-ahead journal in the control schema.
//!
//! [`PgApplyJournal`] implements [`ApplyJournal`] over
//! `spot_migration_control.journal`: one row per record, keyed by `(site, seq)`. The engine
//! assigns strictly increasing sequence numbers; a backend rejects a number that is not the
//! next one, so a duplicated or skipped write is caught at the journal and not by the fold.
//!
//! Appends are serialised per site with a transaction-scoped advisory lock (`hashtext(site)`),
//! which makes "read the last sequence, compare, insert" atomic without a row to lock. The
//! lock is released by the commit or rollback and cannot leak on a pooled connection.
//!
//! The record is stored as `jsonb`; reading it back is the same serde value the engine wrote,
//! so a restart folds exactly the journal the previous process left.

use deadpool_postgres::Pool;
use serde_json::Value;
use spot_core::SiteId;
use spot_migration_apply::journal::{ApplyJournal, JournalEntry, JournalRecord, Seq};
use spot_migration_apply::{BoxFuture, Error as ApplyError, Result as ApplyResult};

use crate::error::{Error, Result};
use crate::schema::{client, table};

/// The engine's journal, stored in the control schema.
#[derive(Clone)]
pub struct PgApplyJournal {
    pool: Pool,
    site: SiteId,
}

impl PgApplyJournal {
    /// A journal for `site` over `pool`.
    #[must_use]
    pub fn new(pool: Pool, site: SiteId) -> Self {
        Self { pool, site }
    }

    /// The site this journal belongs to.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// Every record of this site, in sequence order.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] or [`Error::JournalRecord`] when a row cannot be read or
    /// parsed.
    pub async fn read(&self) -> Result<Vec<JournalEntry>> {
        let client = client(&self.pool, "reading the journal").await?;
        let rows = client
            .query(
                &format!(
                    "SELECT seq, at, record FROM {} WHERE site = $1 ORDER BY seq",
                    table("journal")
                ),
                &[&self.site.as_str()],
            )
            .await
            .map_err(|error| Error::database("reading the journal", &error))?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let seq: i64 = row.get(0);
            let at: i64 = row.get(1);
            let record: Value = row.get(2);
            let seq = u64::try_from(seq).map_err(|_| Error::JournalRecord {
                seq: 0,
                message: "the stored sequence is negative".to_owned(),
            })?;
            let record: JournalRecord =
                serde_json::from_value(record).map_err(|error| Error::JournalRecord {
                    seq,
                    message: error.to_string(),
                })?;
            entries.push(JournalEntry { seq, at, record });
        }
        Ok(entries)
    }

    /// The next sequence number the journal expects (`1` for an empty journal).
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the journal cannot be read.
    pub async fn next_seq(&self) -> Result<Seq> {
        let entries = self.read().await?;
        Ok(entries.last().map_or(1, |entry| entry.seq + 1))
    }

    /// Appends `record` under `seq`.
    ///
    /// # Errors
    /// [`Error::JournalSequence`] when `seq` is not the next number, and
    /// [`Error::ControlUnavailable`] when the write fails.
    pub async fn append_record(&self, seq: Seq, record: JournalRecord, at: i64) -> Result<()> {
        let mut client = client(&self.pool, "appending to the journal").await?;
        let tx = client
            .transaction()
            .await
            .map_err(|error| Error::database("appending to the journal", &error))?;
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext($1))",
            &[&format!("spot-migration-apply-pg:journal:{}", self.site)],
        )
        .await
        .map_err(|error| Error::database("locking the journal", &error))?;
        let last: Option<i64> = tx
            .query_one(
                &format!("SELECT MAX(seq) FROM {} WHERE site = $1", table("journal")),
                &[&self.site.as_str()],
            )
            .await
            .map_err(|error| Error::database("reading the journal head", &error))?
            .get(0);
        let expected = last.map_or(1, |last| u64::try_from(last).unwrap_or(0) + 1);
        if seq != expected {
            return Err(Error::JournalSequence {
                site: self.site.to_string(),
                seq,
                expected,
            });
        }
        let value = serde_json::to_value(&record).map_err(|error| Error::JournalRecord {
            seq,
            message: error.to_string(),
        })?;
        let seq_i64 = i64::try_from(seq).map_err(|_| Error::JournalRecord {
            seq,
            message: "the sequence number does not fit bigint".to_owned(),
        })?;
        tx.execute(
            &format!(
                "INSERT INTO {} (site, seq, at, record) VALUES ($1, $2, $3, $4)",
                table("journal")
            ),
            &[&self.site.as_str(), &seq_i64, &at, &value],
        )
        .await
        .map_err(|error| Error::database("appending to the journal", &error))?;
        tx.commit()
            .await
            .map_err(|error| Error::database("committing the journal record", &error))
    }
}

impl ApplyJournal for PgApplyJournal {
    fn append(&self, seq: Seq, record: JournalRecord, at: i64) -> BoxFuture<'_, ApplyResult<()>> {
        Box::pin(async move {
            self.append_record(seq, record, at)
                .await
                .map_err(|error| ApplyError::JournalFailed {
                    message: error.to_string(),
                })
        })
    }

    fn entries(&self) -> BoxFuture<'_, ApplyResult<Vec<JournalEntry>>> {
        Box::pin(async move {
            self.read()
                .await
                .map_err(|error| ApplyError::JournalFailed {
                    message: error.to_string(),
                })
        })
    }
}

impl std::fmt::Debug for PgApplyJournal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgApplyJournal")
            .field("site", &self.site)
            .finish_non_exhaustive()
    }
}
