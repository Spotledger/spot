//! The ports that add replication health to the readiness line: the sync lag (the lag port of
//! `spot-sync-health`'s `NodeHealthFacts`) and the number of open drifts in the last verify
//! report (`spot_edge_pg::DriftReport`). Both are information: they never make the site not
//! ready, and a missing port leaves its part out of the line.
//!
//! `quarantine: 3 items (oldest 2d), lag 0.2s, drift 0`

use std::sync::{Arc, Mutex};

use spot_edge_pg::DriftReport;

/// How far behind the hub this node is, in milliseconds. `None` when unknown (never connected).
pub trait LagSource: Send + Sync {
    /// The lag.
    fn lag_millis(&self) -> Option<u64>;
}

/// How many drifts the last verify run found. `None` when no run has finished.
pub trait DriftSource: Send + Sync {
    /// The number of entries in the last report.
    fn open_drift(&self) -> Option<u64>;
}

/// [`DriftSource`] over the last [`DriftReport`] a verify run produced.
#[derive(Clone, Default)]
pub struct LastReport(Arc<Mutex<Option<DriftReport>>>);

impl LastReport {
    /// Keeps `report` as the latest.
    pub fn set(&self, report: DriftReport) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(report);
    }
}

impl DriftSource for LastReport {
    fn open_drift(&self) -> Option<u64> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|report| u64::try_from(report.entries.len()).unwrap_or(u64::MAX))
    }
}

/// `0.2s`, `12.0s`: milliseconds as seconds and tenths, integers only.
pub fn seconds(millis: u64) -> String {
    format!("{}.{}s", millis / 1_000, (millis % 1_000) / 100)
}

/// `base` with the lag and the drift appended when their ports answer.
pub fn line(base: &str, lag: Option<&dyn LagSource>, drift: Option<&dyn DriftSource>) -> String {
    let mut out = base.to_owned();
    if let Some(ms) = lag.and_then(LagSource::lag_millis) {
        out.push_str(", lag ");
        out.push_str(&seconds(ms));
    }
    if let Some(count) = drift.and_then(DriftSource::open_drift) {
        out.push_str(&format!(", drift {count}"));
    }
    out
}
