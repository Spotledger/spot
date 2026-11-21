//! Leased naming-series ranges: when to ask the hub for another, and what exhaustion says.

use std::fmt;

/// The stable code of an exhausted series.
pub const SERIES_EXHAUSTED: &str = "edge_offline.series_exhausted";

/// One configured series (`services.edge_offline.series`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesPolicy {
    /// The series name.
    pub name: String,
    /// The numbers one lease grants.
    pub block: u64,
}

/// Remaining numbers below which a refill is due: 20 percent of the block or 200, whichever is
/// larger (`refill_below` default).
pub fn refill_threshold(block: u64) -> u64 {
    (block / 5).max(200)
}

/// What the host knows about the held ranges.
pub trait RangeView {
    /// Numbers left in the held range of `series` (0 when none is held).
    fn remaining(&self, series: &str) -> u64;
}

/// The series that need a lease now. `hub_reachable` false means nothing can be asked.
pub fn due(view: &dyn RangeView, series: &[SeriesPolicy], hub_reachable: bool) -> Vec<String> {
    if !hub_reachable {
        return Vec::new();
    }
    series
        .iter()
        .filter(|s| view.remaining(&s.name) < refill_threshold(s.block))
        .map(|s| s.name.clone())
        .collect()
}

/// The series that crossed the threshold, for the `SERIES_LOW` condition (raised whether or not
/// the hub is reachable).
pub fn low(view: &dyn RangeView, series: &[SeriesPolicy]) -> Vec<String> {
    due(view, series, true)
}

/// Document creation in `series` failed because the held range is used up offline. Retryable:
/// once the hub is reachable a new range arrives. A number is never reused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesExhausted {
    /// The series.
    pub series: String,
}

impl SeriesExhausted {
    /// The stable code.
    pub fn code(&self) -> &'static str {
        SERIES_EXHAUSTED
    }

    /// Always true.
    pub fn retryable(&self) -> bool {
        true
    }
}

impl fmt::Display for SeriesExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{SERIES_EXHAUSTED}: the numbers leased for series {} are used up; reconnect to the hub to get more",
            self.series
        )
    }
}

impl std::error::Error for SeriesExhausted {}

/// Checks that a number can be issued from a held range of `remaining` numbers.
///
/// # Errors
///
/// [`SeriesExhausted`] when none is left.
pub fn ensure_available(series: &str, remaining: u64) -> Result<(), SeriesExhausted> {
    if remaining == 0 {
        Err(SeriesExhausted {
            series: series.to_owned(),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::collections::BTreeMap;

    use super::*;

    struct View(BTreeMap<String, u64>);

    impl RangeView for View {
        fn remaining(&self, series: &str) -> u64 {
            self.0.get(series).copied().unwrap_or(0)
        }
    }

    fn policy(name: &str, block: u64) -> SeriesPolicy {
        SeriesPolicy {
            name: name.to_owned(),
            block,
        }
    }

    #[test]
    fn the_threshold_is_twenty_percent_or_two_hundred() {
        assert_eq!(refill_threshold(1_000), 200);
        assert_eq!(refill_threshold(5_000), 1_000);
        assert_eq!(refill_threshold(100), 200);
    }

    #[test]
    fn only_a_reachable_hub_is_asked_and_only_for_low_series() {
        let view = View(BTreeMap::from([
            ("INV".to_owned(), 640),
            ("PO".to_owned(), 50),
        ]));
        let series = [
            policy("INV", 1_000),
            policy("PO", 1_000),
            policy("GRN", 1_000),
        ];
        assert_eq!(
            due(&view, &series, true),
            vec!["PO".to_owned(), "GRN".to_owned()]
        );
        assert!(due(&view, &series, false).is_empty());
        assert_eq!(low(&view, &series).len(), 2);
    }

    #[test]
    fn an_exhausted_series_is_retryable_and_names_the_series() {
        let error = ensure_available("INV", 0).unwrap_err();
        assert_eq!(error.code(), SERIES_EXHAUSTED);
        assert!(error.retryable());
        assert!(error.to_string().contains("INV"));
        assert!(ensure_available("INV", 1).is_ok());
    }
}
