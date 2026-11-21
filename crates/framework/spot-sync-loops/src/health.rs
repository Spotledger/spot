//! `SyncHealthView`: what the sync loops expose to the health surface (WP-RTB-02.T11): the
//! summary line, the open conditions by step, the last tick's outcome per step and the sampled
//! series. Cheap to clone and read from any thread; it holds names and numbers only.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::runner::{Summary, TickReport};
use crate::series::{Sample, SeriesSink};

/// The most points kept per series.
pub const SERIES_CAPACITY: usize = 720;

#[derive(Default)]
struct Inner {
    summary: Summary,
    last: Option<TickReport>,
    last_at: Option<i64>,
    series: BTreeMap<&'static str, Vec<(i64, f64)>>,
}

/// The shared view.
#[derive(Clone, Default)]
pub struct SyncHealthView(Arc<Mutex<Inner>>);

impl SyncHealthView {
    /// An empty view.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the result of one tick.
    pub fn update(&self, at: i64, report: TickReport, summary: Summary) {
        let mut inner = self.lock();
        inner.last = Some(report);
        inner.last_at = Some(at);
        inner.summary = summary;
    }

    /// The summary line: `sync_loops: calm` or `N amber, M red`.
    #[must_use]
    pub fn line(&self) -> String {
        self.lock().summary.line()
    }

    /// The steps that failed in the last tick, with their reason (a step that is not available says
    /// so here).
    #[must_use]
    pub fn failed(&self) -> Vec<(&'static str, String)> {
        self.lock()
            .last
            .as_ref()
            .map(|report| report.failed.clone())
            .unwrap_or_default()
    }

    /// When the last tick ran.
    #[must_use]
    pub fn last_tick(&self) -> Option<i64> {
        self.lock().last_at
    }

    /// One series, oldest first.
    #[must_use]
    pub fn series(&self, name: &str) -> Vec<(i64, f64)> {
        self.lock()
            .series
            .iter()
            .find(|(series, _)| **series == name)
            .map(|(_, points)| points.clone())
            .unwrap_or_default()
    }
}

impl SeriesSink for SyncHealthView {
    fn record(&mut self, at: i64, sample: &Sample) {
        let mut inner = self.lock();
        let points = inner.series.entry(sample.name).or_default();
        points.push((at, sample.value));
        if points.len() > SERIES_CAPACITY {
            let excess = points.len() - SERIES_CAPACITY;
            points.drain(..excess);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_view_shows_the_line_the_failures_and_a_bounded_series() {
        let mut view = SyncHealthView::new();
        assert_eq!(view.line(), "sync_loops: calm");
        let report = TickReport {
            failed: vec![("reconcile", "not available".to_owned())],
            ..TickReport::default()
        };
        view.update(10, report, Summary { amber: 1, red: 0 });
        assert_eq!(view.line(), "sync_loops: 1 amber, 0 red");
        assert_eq!(view.failed(), [("reconcile", "not available".to_owned())]);
        assert_eq!(view.last_tick(), Some(10));
        for at in 0..(SERIES_CAPACITY as i64 + 5) {
            view.record(
                at,
                &Sample {
                    name: "parked",
                    value: 1.0,
                },
            );
        }
        let points = view.series("parked");
        assert_eq!(points.len(), SERIES_CAPACITY);
        assert_eq!(points[0].0, 5);
        assert!(view.series("nothing").is_empty());
    }
}
