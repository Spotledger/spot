//! Exact integer percentiles: the nearest-rank method, with no float anywhere (ADR-12).

/// The exact `p`-th percentile of a **sorted ascending** slice of whole milliseconds.
///
/// The nearest-rank method: rank `r = ceil(p / 100 * n)` (1-based), clamped to `[1, n]`, and the
/// value is `sorted[r - 1]`. Every step is integer arithmetic, so p50, p90 and p99 are identical
/// on every host and never round through a float. `p` is clamped to `0..=100`. Returns `None`
/// for an empty slice.
///
/// This is the method the brief asks for: an exact integer percentile, not an interpolated
/// (and therefore fractional) one.
#[must_use]
pub fn percentile(sorted: &[i64], p: u8) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len() as u64;
    let p = u64::from(p.min(100));
    // ceil(p * n / 100) with integers only; `div_ceil` rounds up.
    let rank = (p * n).div_ceil(100);
    let rank = rank.clamp(1, n);
    let index = (rank - 1) as usize;
    sorted.get(index).copied()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_slice_has_no_percentile() {
        assert_eq!(percentile(&[], 50), None);
    }

    #[test]
    fn the_single_value_is_every_percentile() {
        assert_eq!(percentile(&[7], 50), Some(7));
        assert_eq!(percentile(&[7], 99), Some(7));
    }

    #[test]
    fn nearest_rank_is_exact() {
        let sorted: Vec<i64> = (1..=10).collect();
        // ceil(0.5 * 10) = 5 -> the fifth value.
        assert_eq!(percentile(&sorted, 50), Some(5));
        // ceil(0.9 * 10) = 9 -> the ninth value.
        assert_eq!(percentile(&sorted, 90), Some(9));
        // ceil(0.99 * 10) = 10 -> the tenth value.
        assert_eq!(percentile(&sorted, 99), Some(10));
    }

    #[test]
    fn ranks_never_run_off_the_end() {
        let sorted = vec![1, 2, 3];
        assert_eq!(percentile(&sorted, 0), Some(1));
        assert_eq!(percentile(&sorted, 100), Some(3));
        assert_eq!(percentile(&sorted, 255), Some(3));
    }
}
