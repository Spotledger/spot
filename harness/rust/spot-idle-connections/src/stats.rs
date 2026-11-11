//! Latency statistics in whole microseconds. No floating point: percentiles are nearest-rank on
//! the sorted samples.

/// The p50, p95 and p99 of a set of latency samples, and how many there were.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Percentiles {
    /// Number of samples.
    pub count: u64,
    /// Median, microseconds.
    pub p50_us: u64,
    /// 95th percentile, microseconds.
    pub p95_us: u64,
    /// 99th percentile, microseconds.
    pub p99_us: u64,
    /// Largest sample, microseconds.
    pub max_us: u64,
}

/// Nearest-rank percentile `p` (1..=100) of sorted samples; zero for no samples.
fn nearest_rank(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = p.saturating_mul(sorted.len()).div_ceil(100);
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Sorts `samples` and returns their percentiles.
#[must_use]
pub fn percentiles(samples: &mut [u64]) -> Percentiles {
    samples.sort_unstable();
    Percentiles {
        count: u64::try_from(samples.len()).unwrap_or(u64::MAX),
        p50_us: nearest_rank(samples, 50),
        p95_us: nearest_rank(samples, 95),
        p99_us: nearest_rank(samples, 99),
        max_us: samples.last().copied().unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_on_one_to_one_hundred() {
        let mut samples: Vec<u64> = (1..=100).rev().collect();
        let p = percentiles(&mut samples);
        assert_eq!((p.p50_us, p.p95_us, p.p99_us, p.max_us), (50, 95, 99, 100));
        assert_eq!(p.count, 100);
    }

    #[test]
    fn small_and_empty_sets() {
        assert_eq!(percentiles(&mut []), Percentiles::default());
        let mut one = [7];
        let p = percentiles(&mut one);
        assert_eq!((p.p50_us, p.p99_us), (7, 7));
        let mut three = [30, 10, 20];
        let p = percentiles(&mut three);
        assert_eq!((p.p50_us, p.p95_us, p.p99_us), (20, 30, 30));
    }
}
