//! Quarantine series.

use std::time::Duration;

use crate::count_value;

/// Gauge: open quarantine items.
pub const OPEN: &str = "spot_quarantine_open";
/// Gauge: age of the oldest open item.
pub const OLDEST_AGE_SECONDS: &str = "spot_quarantine_oldest_age_seconds";

/// Sets the number of open items.
pub fn set_open(count: u64) {
    metrics::gauge!(OPEN).set(count_value(count));
}

/// Sets the age of the oldest open item (zero when none).
pub fn set_oldest_age(age: Duration) {
    metrics::gauge!(OLDEST_AGE_SECONDS).set(age.as_secs_f64());
}
