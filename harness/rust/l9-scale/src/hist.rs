//! A log-linear (HDR-style) histogram of microsecond values.
//!
//! Values below 128 are exact; above that each power of two is split into 128 sub-buckets, so
//! a reported percentile is within one part in 128 (under 0.8 %) of the true value and never
//! below it. Everything is integer arithmetic; the maximum is kept exactly.

const SUB_BITS: u32 = 7;
const SUB: u64 = 1 << SUB_BITS;
/// The largest value recorded (about 12.7 days in microseconds); larger values are clamped.
pub const MAX_VALUE: u64 = (1 << 40) - 1;
const BUCKETS: usize = (SUB as usize) * (40 - SUB_BITS as usize + 2);

/// A histogram of `u64` values (microseconds in this crate).
#[derive(Debug, Clone)]
pub struct Histogram {
    counts: Vec<u64>,
    total: u64,
    max: u64,
    min: u64,
    sum: u128,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

fn index_of(value: u64) -> usize {
    let value = value.min(MAX_VALUE);
    if value < SUB {
        return value as usize;
    }
    let high = 63 - value.leading_zeros();
    let shift = high - SUB_BITS;
    let sub = value >> shift;
    (SUB as usize) + (shift as usize) * (SUB as usize) + (sub - SUB) as usize
}

fn upper_of(index: usize) -> u64 {
    if index < SUB as usize {
        return index as u64;
    }
    let k = index - SUB as usize;
    let shift = (k / SUB as usize) as u32;
    let sub = SUB + (k % SUB as usize) as u64;
    ((sub + 1) << shift) - 1
}

impl Histogram {
    /// An empty histogram.
    pub fn new() -> Self {
        Self {
            counts: vec![0; BUCKETS],
            total: 0,
            max: 0,
            min: u64::MAX,
            sum: 0,
        }
    }

    /// Records one value.
    pub fn record(&mut self, value: u64) {
        let value = value.min(MAX_VALUE);
        let index = index_of(value);
        if let Some(slot) = self.counts.get_mut(index) {
            *slot += 1;
        }
        self.total += 1;
        self.max = self.max.max(value);
        self.min = self.min.min(value);
        self.sum += u128::from(value);
    }

    /// Adds every value of `other`.
    pub fn merge(&mut self, other: &Histogram) {
        for (slot, add) in self.counts.iter_mut().zip(&other.counts) {
            *slot += add;
        }
        self.total += other.total;
        self.max = self.max.max(other.max);
        self.min = self.min.min(other.min);
        self.sum += other.sum;
    }

    /// How many values were recorded.
    pub fn count(&self) -> u64 {
        self.total
    }

    /// The largest recorded value (exact); `0` when empty.
    pub fn max(&self) -> u64 {
        self.max
    }

    /// The smallest recorded value (exact); `0` when empty.
    pub fn min(&self) -> u64 {
        if self.total == 0 { 0 } else { self.min }
    }

    /// The mean, rounded down; `0` when empty.
    pub fn mean(&self) -> u64 {
        if self.total == 0 {
            0
        } else {
            u64::try_from(self.sum / u128::from(self.total)).unwrap_or(u64::MAX)
        }
    }

    /// The value at `permille` per thousand (500 is the median, 950 is p95, 990 is p99): the
    /// upper edge of the bucket holding that rank, never above the exact maximum. `0` when
    /// empty.
    pub fn percentile(&self, permille: u32) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let permille = u64::from(permille.min(1000));
        let rank = ((self.total * permille).div_ceil(1000)).max(1);
        let mut seen = 0u64;
        for (index, count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= rank {
                return upper_of(index).min(self.max);
            }
        }
        self.max
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn small_values_are_exact() {
        let mut h = Histogram::new();
        for v in 0..100 {
            h.record(v);
        }
        assert_eq!(h.count(), 100);
        assert_eq!(h.percentile(500), 49);
        assert_eq!(h.percentile(950), 94);
        assert_eq!(h.percentile(990), 98);
        assert_eq!(h.max(), 99);
        assert_eq!(h.min(), 0);
    }

    #[test]
    fn large_values_are_within_one_part_in_128_and_never_below() {
        for exact in [200u64, 1_000, 12_345, 987_654, 31_000_000, 4_000_000_000] {
            let mut h = Histogram::new();
            h.record(exact);
            h.record(exact * 2);
            let got = h.percentile(500);
            assert!(got >= exact, "{got} < {exact}");
            assert!(got - exact <= exact / 100, "{got} too far above {exact}");
        }
    }

    #[test]
    fn the_index_and_its_upper_edge_agree_across_the_whole_range() {
        let mut v = 1u64;
        while v < MAX_VALUE {
            let index = index_of(v);
            assert!(index < BUCKETS);
            assert!(upper_of(index) >= v, "{v}");
            assert_eq!(index_of(upper_of(index)), index, "{v}");
            v = v + v / 3 + 1;
        }
        assert!(index_of(u64::MAX) < BUCKETS);
    }

    #[test]
    fn percentiles_follow_the_rank_and_clamp_to_the_maximum() {
        let mut h = Histogram::new();
        for _ in 0..99 {
            h.record(1_000);
        }
        h.record(5_000_000);
        assert!(h.percentile(500) <= 1_100);
        assert!(h.percentile(990) <= 1_100);
        assert_eq!(h.percentile(1000), 5_000_000);
        assert_eq!(h.max(), 5_000_000);
        assert!(h.mean() > 1_000);
    }

    #[test]
    fn merging_adds_counts_and_keeps_the_extremes() {
        let mut a = Histogram::new();
        let mut b = Histogram::new();
        a.record(10);
        b.record(20_000);
        b.record(5);
        a.merge(&b);
        assert_eq!(a.count(), 3);
        assert_eq!(a.max(), 20_000);
        assert_eq!(a.min(), 5);
    }

    #[test]
    fn an_empty_histogram_reports_zeros() {
        let h = Histogram::new();
        assert_eq!((h.count(), h.max(), h.min(), h.mean()), (0, 0, 0, 0));
        assert_eq!(h.percentile(950), 0);
    }
}
