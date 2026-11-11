//! Latency samples and their percentiles, in integer microseconds.

use std::time::Duration;

use serde::Serialize;

/// A set of latency samples.
#[derive(Clone, Debug, Default)]
pub struct Samples {
    micros: Vec<u64>,
}

/// The summary of a [`Samples`] set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LatencySummary {
    /// How many samples.
    pub count: u64,
    /// The median, in microseconds.
    pub p50_us: u64,
    /// The 95th percentile, in microseconds.
    pub p95_us: u64,
    /// The 99th percentile, in microseconds.
    pub p99_us: u64,
    /// The largest sample, in microseconds.
    pub max_us: u64,
}

impl Samples {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one sample.
    pub fn push(&mut self, sample: Duration) {
        self.micros
            .push(u64::try_from(sample.as_micros()).unwrap_or(u64::MAX));
    }

    /// Adds every sample of `other`.
    pub fn merge(&mut self, other: &Samples) {
        self.micros.extend_from_slice(&other.micros);
    }

    /// How many samples.
    #[must_use]
    pub fn len(&self) -> usize {
        self.micros.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.micros.is_empty()
    }

    /// The summary; all zeros for an empty set.
    #[must_use]
    pub fn summary(&self) -> LatencySummary {
        let mut sorted = self.micros.clone();
        sorted.sort_unstable();
        LatencySummary {
            count: sorted.len() as u64,
            p50_us: nearest_rank(&sorted, 50),
            p95_us: nearest_rank(&sorted, 95),
            p99_us: nearest_rank(&sorted, 99),
            max_us: sorted.last().copied().unwrap_or(0),
        }
    }
}

/// The nearest-rank percentile of an ascending slice: the smallest value with at least `pct`
/// per cent of the samples at or below it. Zero for an empty slice.
#[must_use]
pub fn nearest_rank(sorted: &[u64], pct: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len() as u64;
    let rank = (n * pct).div_ceil(100).clamp(1, n);
    sorted
        .get(usize::try_from(rank - 1).unwrap_or(0))
        .copied()
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_matches_the_textbook_definition() {
        let sorted: Vec<u64> = (1..=100).collect();
        assert_eq!(nearest_rank(&sorted, 50), 50);
        assert_eq!(nearest_rank(&sorted, 95), 95);
        assert_eq!(nearest_rank(&sorted, 99), 99);
        assert_eq!(nearest_rank(&[7], 99), 7);
        assert_eq!(nearest_rank(&[], 99), 0);
    }

    #[test]
    fn a_summary_merges_and_reports_the_maximum() {
        let mut a = Samples::new();
        let mut b = Samples::new();
        for ms in 1..=50 {
            a.push(Duration::from_millis(ms));
        }
        for ms in 51..=100 {
            b.push(Duration::from_millis(ms));
        }
        a.merge(&b);
        let s = a.summary();
        assert_eq!(s.count, 100);
        assert_eq!(s.p95_us, 95_000);
        assert_eq!(s.max_us, 100_000);
        assert_eq!(Samples::new().summary(), LatencySummary::default());
    }
}
