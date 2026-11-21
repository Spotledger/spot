//! Retention: removing finished jobs. A scheduled job calls [`PgJobStore::purge_finished`].

use std::time::Duration;

use spot_jobs::Result;

use crate::BATCH;
use crate::convert::{millis, pg_err};
use crate::sql;
use crate::store::PgJobStore;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// How long finished jobs are kept, by the database clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// `Succeeded` and `Cancelled` jobs: 14 days by default.
    pub succeeded: Duration,
    /// `Dead` jobs, kept longer for diagnosis: 90 days by default.
    pub dead: Duration,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            succeeded: 14 * DAY,
            dead: 90 * DAY,
        }
    }
}

/// What one purge removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// `Succeeded` and `Cancelled` jobs removed.
    pub succeeded: u64,
    /// `Dead` jobs removed.
    pub dead: u64,
    /// Outbox dedupe records removed (those older than the longest retention).
    pub outbox_seen: u64,
}

impl PgJobStore {
    /// Deletes finished jobs older than `retention`, [`BATCH`] rows per statement, and the
    /// outbox dedupe records older than the longer of the two periods (a replay of an event
    /// that old cannot happen: the feed's own retention is shorter). Live jobs are never touched.
    pub async fn purge_finished(&self, retention: &Retention) -> Result<PurgeReport> {
        let conn = self.conn().await?;
        let mut report = PurgeReport::default();
        for (states, age, total) in [
            (
                vec!["succeeded", "cancelled"],
                retention.succeeded,
                &mut report.succeeded,
            ),
            (vec!["dead"], retention.dead, &mut report.dead),
        ] {
            let age_ms = millis(age);
            loop {
                let n = conn
                    .execute(sql::PURGE, &[&states, &age_ms, &i64::from(BATCH)])
                    .await
                    .map_err(pg_err)?;
                *total += n;
                if n < u64::from(BATCH) {
                    break;
                }
            }
        }
        let seen_ms = millis(retention.succeeded.max(retention.dead));
        loop {
            let n = conn
                .execute(
                    "DELETE FROM spot_job_outbox_seen WHERE event_id IN (
                         SELECT event_id FROM spot_job_outbox_seen
                         WHERE seen_at < now() - $1::bigint * interval '1 millisecond'
                         LIMIT $2)",
                    &[&seen_ms, &i64::from(BATCH)],
                )
                .await
                .map_err(pg_err)?;
            report.outbox_seen += n;
            if n < u64::from(BATCH) {
                break;
            }
        }
        Ok(report)
    }
}
