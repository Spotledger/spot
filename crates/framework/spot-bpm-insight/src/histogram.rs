//! A fixed-bucket log-linear integer histogram of millisecond values (WP-BPM-11.T7).
//!
//! The accumulator used to keep one `i64` per completed attempt, so it grew with history. This
//! histogram replaces that list with a bounded structure: values `0..=31` have a bucket each
//! (exact), and every power of two from `2^5` up to `2^42` is split into 32 equal sub-buckets, so
//! a bucket is never wider than 1/32 of its lower bound. Values at or above `2^42` ms (about 140
//! years) saturate into the last bucket and are counted in [`Histogram::saturated`]. The buckets
//! are stored sparsely, so a histogram holds at most [`BUCKETS`] entries (1,216) whatever the
//! history.
//!
//! Beside the buckets it tracks the exact `min`, `max`, `count` and `sum`. A percentile is the
//! integer nearest-rank one: the bucket holding rank `ceil(p * count / 100)`, reported as that
//! bucket's upper bound clamped into `min..=max`. It is therefore deterministic, exact for values
//! up to 31 ms and for any single-valued sample, and otherwise within 1/32 (3.2%) above the true
//! value. No float is used anywhere (ADR-12). A negative value (events out of order) is recorded
//! in bucket 0 and tracked exactly in `min`, `sum` and `max`.
//!
//! Merging is bucket-wise addition, so folding from zero and resuming from a stored state agree
//! exactly.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::InsightError;

/// Values below this are exact: one bucket each.
const EXACT: u64 = 32;
/// Sub-buckets per power of two (`2^SUB_BITS`).
const SUB_BITS: u32 = 5;
/// The highest power of two with its own sub-buckets is `2^(MAX_EXP - 1)`; values from
/// `2^MAX_EXP` up saturate. `2^42` ms is about 140 years.
const MAX_EXP: u32 = 42;
/// The most buckets a histogram can ever hold.
pub const BUCKETS: u16 = 1216;
/// The first value that saturates into the last bucket.
const SATURATION: u64 = 1 << MAX_EXP;

/// The bucket index of a non-negative value.
fn bucket_of(value: u64) -> u16 {
    if value < EXACT {
        return value as u16;
    }
    let value = value.min(SATURATION - 1);
    let exp = 63 - value.leading_zeros();
    let sub = (value >> (exp - SUB_BITS)) & (EXACT - 1);
    (EXACT + u64::from(exp - SUB_BITS) * EXACT + sub) as u16
}

/// The largest value a bucket holds.
fn upper_bound(index: u16) -> i64 {
    let index = u64::from(index);
    if index < EXACT {
        return index as i64;
    }
    let step = (index - EXACT) / EXACT;
    let sub = (index - EXACT) % EXACT;
    let shift = step as u32;
    let lower = (EXACT + sub) << shift;
    (lower + (1u64 << shift) - 1) as i64
}

/// A bounded histogram of millisecond values with exact `min`, `max`, `count` and `sum`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Histogram {
    buckets: BTreeMap<u16, u64>,
    count: u64,
    sum: i64,
    min: i64,
    max: i64,
    saturated: u64,
}

impl Histogram {
    /// An empty histogram.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one value.
    pub fn record(&mut self, value: i64) {
        let non_negative = u64::try_from(value).unwrap_or(0);
        let index = bucket_of(non_negative);
        let slot = self.buckets.entry(index).or_insert(0);
        *slot = slot.saturating_add(1);
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.count = self.count.saturating_add(1);
        self.sum = self.sum.saturating_add(value);
        if non_negative >= SATURATION {
            self.saturated = self.saturated.saturating_add(1);
        }
    }

    /// Adds every observation of `other` (bucket-wise addition).
    pub fn merge(&mut self, other: &Self) {
        if other.count == 0 {
            return;
        }
        for (index, n) in &other.buckets {
            let slot = self.buckets.entry(*index).or_insert(0);
            *slot = slot.saturating_add(*n);
        }
        if self.count == 0 {
            self.min = other.min;
            self.max = other.max;
        } else {
            self.min = self.min.min(other.min);
            self.max = self.max.max(other.max);
        }
        self.count = self.count.saturating_add(other.count);
        self.sum = self.sum.saturating_add(other.sum);
        self.saturated = self.saturated.saturating_add(other.saturated);
    }

    /// How many values were recorded.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The exact sum of the recorded values (saturating).
    #[must_use]
    pub fn sum(&self) -> i64 {
        self.sum
    }

    /// How many values were at or above `2^42` ms and saturated into the last bucket.
    #[must_use]
    pub fn saturated(&self) -> u64 {
        self.saturated
    }

    /// The exact smallest value, or `None` when empty.
    #[must_use]
    pub fn min(&self) -> Option<i64> {
        (self.count > 0).then_some(self.min)
    }

    /// The exact largest value, or `None` when empty.
    #[must_use]
    pub fn max(&self) -> Option<i64> {
        (self.count > 0).then_some(self.max)
    }

    /// How many buckets are in use (at most [`BUCKETS`]).
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// The integer nearest-rank percentile `p` (clamped to `0..=100`), or `None` when empty.
    ///
    /// The upper bound of the bucket holding rank `ceil(p * count / 100)`, clamped into
    /// `min..=max`: exact up to 31 ms and for a single-valued sample, otherwise within 1/32
    /// above the true value.
    #[must_use]
    pub fn percentile(&self, p: u8) -> Option<i64> {
        if self.count == 0 {
            return None;
        }
        let p = u128::from(p.min(100));
        let count = u128::from(self.count);
        let rank = (p * count).div_ceil(100).clamp(1, count);
        let mut seen: u128 = 0;
        for (index, n) in &self.buckets {
            seen += u128::from(*n);
            if seen >= rank {
                // The last bucket is open-ended (saturated values), so it reports `max`.
                let upper = if *index == BUCKETS - 1 {
                    i64::MAX
                } else {
                    upper_bound(*index)
                };
                return Some(upper.min(self.max).max(self.min));
            }
        }
        Some(self.max)
    }

    /// The serialisable form.
    #[must_use]
    pub fn to_state(&self) -> HistogramState {
        HistogramState {
            buckets: self.buckets.iter().map(|(i, n)| (*i, *n)).collect(),
            count: self.count,
            sum: self.sum,
            min: self.min,
            max: self.max,
            saturated: self.saturated,
        }
    }

    /// Rebuilds a histogram from its stored form, refusing anything inconsistent.
    ///
    /// # Errors
    ///
    /// [`InsightError::StateCorrupt`] when the buckets are unsorted, out of range or empty, the
    /// counts do not add up, or `min`, `max` and `saturated` disagree with the buckets.
    pub fn from_state(state: HistogramState) -> Result<Self, InsightError> {
        let corrupt = |why: &str| InsightError::StateCorrupt {
            detail: format!("histogram: {why}"),
        };
        let HistogramState {
            buckets,
            count,
            sum,
            min,
            max,
            saturated,
        } = state;
        if buckets.len() > usize::from(BUCKETS) {
            return Err(corrupt("too many buckets"));
        }
        let mut map = BTreeMap::new();
        let mut total: u64 = 0;
        let mut previous: Option<u16> = None;
        for (index, n) in buckets {
            if index >= BUCKETS {
                return Err(corrupt("bucket index out of range"));
            }
            if previous.is_some_and(|p| p >= index) {
                return Err(corrupt("bucket indices not strictly ascending"));
            }
            if n == 0 {
                return Err(corrupt("empty bucket"));
            }
            total = total
                .checked_add(n)
                .ok_or_else(|| corrupt("count overflow"))?;
            previous = Some(index);
            map.insert(index, n);
        }
        if total != count {
            return Err(corrupt("count does not equal the bucket total"));
        }
        if count == 0 {
            if sum != 0 || min != 0 || max != 0 || saturated != 0 {
                return Err(corrupt("empty histogram with non-zero fields"));
            }
        } else {
            if min > max {
                return Err(corrupt("min above max"));
            }
            let first = map.keys().next().copied();
            let last = map.keys().next_back().copied();
            let bucket_for = |v: i64| bucket_of(u64::try_from(v).unwrap_or(0));
            if first != Some(bucket_for(min)) || last != Some(bucket_for(max)) {
                return Err(corrupt("min or max disagrees with the buckets"));
            }
            if saturated > map.get(&(BUCKETS - 1)).copied().unwrap_or(0) {
                return Err(corrupt("saturated count exceeds the last bucket"));
            }
        }
        Ok(Self {
            buckets: map,
            count,
            sum,
            min,
            max,
            saturated,
        })
    }
}

/// The stored form of a [`Histogram`]: integers only, buckets as ascending `(index, count)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistogramState {
    /// The non-empty buckets, strictly ascending by index.
    pub buckets: Vec<(u16, u64)>,
    /// How many values were recorded.
    pub count: u64,
    /// The exact sum of the values (saturating).
    pub sum: i64,
    /// The exact smallest value (0 when empty).
    pub min: i64,
    /// The exact largest value (0 when empty).
    pub max: i64,
    /// How many values saturated into the last bucket.
    pub saturated: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn small_values_are_exact() {
        let mut h = Histogram::new();
        for v in 0..=31 {
            h.record(v);
        }
        assert_eq!(h.percentile(50), Some(15));
        assert_eq!(h.percentile(100), Some(31));
        assert_eq!(h.min(), Some(0));
        assert_eq!(h.max(), Some(31));
    }

    #[test]
    fn a_single_valued_sample_is_exact_at_every_percentile() {
        let mut h = Histogram::new();
        for _ in 0..7 {
            h.record(1_234_567);
        }
        for p in [0, 50, 90, 99, 100] {
            assert_eq!(h.percentile(p), Some(1_234_567));
        }
    }

    #[test]
    fn the_error_above_31_ms_is_below_one_thirty_second() {
        for v in [
            32_i64,
            33,
            63,
            64,
            100,
            1000,
            65_535,
            1_000_000,
            123_456_789,
        ] {
            let mut h = Histogram::new();
            h.record(v);
            h.record(v + v / 2 + 1);
            let p = h.percentile(50).unwrap();
            assert!(p >= v, "never below the true value: {p} < {v}");
            assert!((p - v) * 32 <= v, "within 3.2%: {p} vs {v}");
        }
    }

    #[test]
    fn bucket_bounds_are_contiguous() {
        let mut next = 0_i64;
        for index in 0..BUCKETS {
            let upper = upper_bound(index);
            assert!(upper >= next, "bucket {index} is empty or overlaps");
            assert_eq!(bucket_of(u64::try_from(next).unwrap()), index);
            assert_eq!(bucket_of(u64::try_from(upper).unwrap()), index);
            next = upper + 1;
        }
        assert_eq!(next, 1_i64 << MAX_EXP);
    }

    #[test]
    fn values_past_two_to_the_42_saturate_and_are_counted() {
        let mut h = Histogram::new();
        h.record(i64::MAX);
        h.record((1_i64 << 42) - 1);
        h.record(1_i64 << 42);
        assert_eq!(h.saturated(), 2);
        assert_eq!(h.bucket_count(), 1);
        assert_eq!(h.max(), Some(i64::MAX));
        assert_eq!(h.percentile(99), Some(i64::MAX));
    }

    #[test]
    fn a_negative_value_goes_to_bucket_zero_and_keeps_min_exact() {
        let mut h = Histogram::new();
        h.record(-5);
        h.record(10);
        assert_eq!(h.min(), Some(-5));
        // Bucket 0 holds the negative value; its upper bound is 0.
        assert_eq!(h.percentile(0), Some(0));
        assert_eq!(h.sum(), 5);
        let mut single = Histogram::new();
        single.record(-5);
        assert_eq!(single.percentile(50), Some(-5));
    }

    #[test]
    fn merge_equals_recording_everything() {
        let (mut a, mut b, mut all) = (Histogram::new(), Histogram::new(), Histogram::new());
        for v in [3, 900, 70_000, 31, 32] {
            a.record(v);
            all.record(v);
        }
        for v in [5, 900, 12_345_678] {
            b.record(v);
            all.record(v);
        }
        a.merge(&b);
        assert_eq!(a, all);
    }

    #[test]
    fn state_round_trips_and_corruption_is_refused() {
        let mut h = Histogram::new();
        for v in [1, 40, 5000, 5000, 9_000_000] {
            h.record(v);
        }
        let state = h.to_state();
        assert_eq!(Histogram::from_state(state.clone()).unwrap(), h);

        let mut bad = state.clone();
        bad.count += 1;
        assert!(Histogram::from_state(bad).is_err());
        let mut bad = state.clone();
        bad.buckets.reverse();
        assert!(Histogram::from_state(bad).is_err());
        let mut bad = state.clone();
        bad.buckets.push((BUCKETS, 1));
        assert!(Histogram::from_state(bad).is_err());
        let mut bad = state.clone();
        bad.min = 4000;
        assert!(Histogram::from_state(bad).is_err());
        let mut bad = state;
        bad.buckets[0].1 = 0;
        assert!(Histogram::from_state(bad).is_err());
    }
}
