//! Timing capture for the snapshot, apply and restore measurements WP-MIG-05 asks for.
//!
//! A [`TimingRecord`] is one integer measurement: the phase, the elapsed microseconds, the
//! snapshot it belongs to and the caller's instant. Durations come from the injected
//! [`spot_core::Clock`]'s monotonic reading, never from the wall clock, and they are integers:
//! no float is used for a duration anywhere in this crate.
//!
//! [`PgTimingLog`] writes records to `spot_migration_control.timing`; [`RecordingTimings`]
//! keeps them in memory for a test; [`NoTimings`] drops them. A host that wants the numbers in
//! its logs prints [`RecordingTimings::records`] or queries the table.

use std::sync::Mutex;

use deadpool_postgres::Pool;
use spot_core::SiteId;

use crate::error::{Error, Result};
use crate::schema::{client, table};

/// Which measurement a record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingPhase {
    /// Taking the snapshot.
    Snapshot,
    /// Applying the plan (one record per step, plus one for the whole run when the host
    /// records it).
    Apply,
    /// Restoring the snapshot.
    Restore,
}

impl TimingPhase {
    /// The phase's stable name, as the table stores it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Apply => "apply",
            Self::Restore => "restore",
        }
    }
}

/// One integer measurement.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TimingRecord {
    /// Which measurement.
    pub phase: TimingPhase,
    /// How long it took, in microseconds.
    pub duration_micros: i64,
    /// The snapshot the measurement belongs to (empty when none).
    pub snapshot: String,
    /// Unix microseconds the measurement finished at, as the caller supplied it.
    pub at: i64,
}

impl TimingRecord {
    /// A record for `phase` that took `duration_micros`.
    #[must_use]
    pub fn new(
        phase: TimingPhase,
        duration_micros: i64,
        snapshot: impl Into<String>,
        at: i64,
    ) -> Self {
        Self {
            phase,
            duration_micros,
            snapshot: snapshot.into(),
            at,
        }
    }
}

/// Where timings go.
pub trait TimingSink: Send + Sync {
    /// Records one measurement.
    ///
    /// # Errors
    /// A backend error when the measurement cannot be stored.
    fn record(&self, record: TimingRecord) -> Result<()>;
}

/// A sink that drops every measurement.
#[derive(Debug, Default)]
pub struct NoTimings;

impl TimingSink for NoTimings {
    fn record(&self, _record: TimingRecord) -> Result<()> {
        Ok(())
    }
}

/// A sink that keeps the measurements, for tests and for a host that prints them.
#[derive(Debug, Default)]
pub struct RecordingTimings {
    records: Mutex<Vec<TimingRecord>>,
}

impl RecordingTimings {
    /// An empty transcript.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The measurements recorded so far, in order.
    #[must_use]
    pub fn records(&self) -> Vec<TimingRecord> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The measurements of one phase.
    #[must_use]
    pub fn of(&self, phase: TimingPhase) -> Vec<TimingRecord> {
        self.records()
            .into_iter()
            .filter(|record| record.phase == phase)
            .collect()
    }
}

impl TimingSink for RecordingTimings {
    fn record(&self, record: TimingRecord) -> Result<()> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(record);
        Ok(())
    }
}

/// A sink that writes each measurement to the control schema.
#[derive(Clone)]
pub struct PgTimingLog {
    pool: Pool,
    site: SiteId,
}

impl PgTimingLog {
    /// A log for `site` over `pool`.
    #[must_use]
    pub fn new(pool: Pool, site: SiteId) -> Self {
        Self { pool, site }
    }

    /// Every measurement recorded for this site, oldest first.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the table cannot be read.
    pub async fn read(&self) -> Result<Vec<TimingRecord>> {
        let client = client(&self.pool, "reading timings").await?;
        let rows = client
            .query(
                &format!(
                    "SELECT phase, duration_micros, snapshot, at FROM {} \
                     WHERE site = $1 ORDER BY id",
                    table("timing")
                ),
                &[&self.site.as_str()],
            )
            .await
            .map_err(|error| Error::database("reading timings", &error))?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let phase: String = row.get(0);
                TimingRecord {
                    phase: match phase.as_str() {
                        "snapshot" => TimingPhase::Snapshot,
                        "restore" => TimingPhase::Restore,
                        _ => TimingPhase::Apply,
                    },
                    duration_micros: row.get(1),
                    snapshot: row.get(2),
                    at: row.get(3),
                }
            })
            .collect())
    }
}

impl TimingSink for PgTimingLog {
    fn record(&self, record: TimingRecord) -> Result<()> {
        // `record` is synchronous by the trait's shape; the write is queued onto the current
        // runtime so a progress callback never blocks on a database round trip.
        let pool = self.pool.clone();
        let site = self.site.clone();
        let handle = tokio::runtime::Handle::try_current().map_err(|_| Error::TimingFailed {
            message: "no runtime is running to write the timing".to_owned(),
        })?;
        handle.spawn(async move {
            if let Ok(client) = pool.get().await {
                let _ = client
                    .execute(
                        &format!(
                            "INSERT INTO {} (site, phase, duration_micros, snapshot, at) \
                             VALUES ($1, $2, $3, $4, $5)",
                            table("timing")
                        ),
                        &[
                            &site.as_str(),
                            &record.phase.as_str(),
                            &record.duration_micros,
                            &record.snapshot,
                            &record.at,
                        ],
                    )
                    .await;
            }
        });
        Ok(())
    }
}

impl std::fmt::Debug for PgTimingLog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgTimingLog")
            .field("site", &self.site)
            .finish_non_exhaustive()
    }
}
