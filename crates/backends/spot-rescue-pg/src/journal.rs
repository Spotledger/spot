//! [`PgRescueJournal`]: the rescue run's write-ahead journal over PostgreSQL.

use deadpool_postgres::Pool;
use serde_json::Value;
use spot_core::SiteId;
use spot_rescue::{BoxFuture, JournalEntry, JournalRecord, RescueJournal, Seq};

use crate::error::{Error, Result};
use crate::schema::{client, table};

/// One site's rescue journal.
pub struct PgRescueJournal {
    pool: Pool,
    site: SiteId,
}

impl PgRescueJournal {
    /// The journal of `site` over `pool`. The control schema must exist ([`crate::PgRescueSchema`]).
    #[must_use]
    pub fn new(pool: Pool, site: SiteId) -> Self {
        Self { pool, site }
    }

    /// The site.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// Every record of this site, in sequence order.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] or [`Error::JournalRecord`].
    pub async fn read(&self) -> Result<Vec<JournalEntry>> {
        let client = client(&self.pool, "reading the rescue journal").await?;
        let rows = client
            .query(
                &format!(
                    "SELECT seq, at, record FROM {} WHERE site = $1 ORDER BY seq",
                    table("spot_rescue_journal")
                ),
                &[&self.site.as_str()],
            )
            .await
            .map_err(|error| Error::database("reading the rescue journal", &error))?;
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
                serde_json::from_value(record).map_err(|_| Error::JournalRecord {
                    seq,
                    message: "the stored record does not parse".to_owned(),
                })?;
            entries.push(JournalEntry { seq, at, record });
        }
        Ok(entries)
    }

    /// Appends `record` under `seq` (see the crate docs for the rules).
    ///
    /// # Errors
    /// [`Error::JournalSequence`] for a gap or a fork (nothing is written);
    /// [`Error::ControlUnavailable`] when the write fails.
    pub async fn append_record(&self, seq: Seq, record: JournalRecord, at: i64) -> Result<()> {
        let value = serde_json::to_value(&record).map_err(|_| Error::JournalRecord {
            seq,
            message: "the record cannot be stored".to_owned(),
        })?;
        let seq_i64 = i64::try_from(seq).map_err(|_| Error::JournalRecord {
            seq,
            message: "the sequence number does not fit bigint".to_owned(),
        })?;
        let mut client = client(&self.pool, "appending to the rescue journal").await?;
        let tx = client
            .transaction()
            .await
            .map_err(|error| Error::database("appending to the rescue journal", &error))?;
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext($1))",
            &[&format!("spot-rescue-pg:journal:{}", self.site)],
        )
        .await
        .map_err(|error| Error::database("locking the rescue journal", &error))?;
        let last: Option<i64> = tx
            .query_one(
                &format!(
                    "SELECT MAX(seq) FROM {} WHERE site = $1",
                    table("spot_rescue_journal")
                ),
                &[&self.site.as_str()],
            )
            .await
            .map_err(|error| Error::database("reading the rescue journal head", &error))?
            .get(0);
        let expected = last.map_or(1, |last| u64::try_from(last).unwrap_or(0) + 1);
        if seq != expected {
            // The same record again at the last stored seq is a retry after a lost acknowledgement.
            if last == Some(seq_i64) {
                let stored: Value = tx
                    .query_one(
                        &format!(
                            "SELECT record FROM {} WHERE site = $1 AND seq = $2",
                            table("spot_rescue_journal")
                        ),
                        &[&self.site.as_str(), &seq_i64],
                    )
                    .await
                    .map_err(|error| Error::database("reading the rescue journal head", &error))?
                    .get(0);
                if stored == value {
                    return Ok(());
                }
            }
            return Err(Error::JournalSequence {
                site: self.site.to_string(),
                seq,
                expected,
            });
        }
        tx.execute(
            &format!(
                "INSERT INTO {} (site, seq, at, record) VALUES ($1, $2, $3, $4)",
                table("spot_rescue_journal")
            ),
            &[&self.site.as_str(), &seq_i64, &at, &value],
        )
        .await
        .map_err(|error| Error::database("appending to the rescue journal", &error))?;
        tx.commit()
            .await
            .map_err(|error| Error::database("committing the rescue journal record", &error))
    }
}

fn journal_failed(error: &Error) -> spot_rescue::Error {
    spot_rescue::Error::JournalFailed {
        message: error.to_string(),
    }
}

impl RescueJournal for PgRescueJournal {
    fn append(
        &self,
        seq: Seq,
        record: JournalRecord,
        at: i64,
    ) -> BoxFuture<'_, spot_rescue::Result<()>> {
        Box::pin(async move {
            self.append_record(seq, record, at)
                .await
                .map_err(|error| journal_failed(&error))
        })
    }

    fn entries(&self) -> BoxFuture<'_, spot_rescue::Result<Vec<JournalEntry>>> {
        Box::pin(async move { self.read().await.map_err(|error| journal_failed(&error)) })
    }
}

impl std::fmt::Debug for PgRescueJournal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgRescueJournal")
            .field("site", &self.site)
            .finish_non_exhaustive()
    }
}
