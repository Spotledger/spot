//! Counters for `/metrics` (Prometheus text). No label carries a key, a token, an address or a
//! publisher; the numbers are totals only.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide counters.
#[derive(Debug, Default)]
pub(crate) struct Metrics {
    pub requests_2xx: AtomicU64,
    pub requests_3xx: AtomicU64,
    pub requests_4xx: AtomicU64,
    pub requests_5xx: AtomicU64,
    pub publishes_accepted: AtomicU64,
    pub publishes_refused: AtomicU64,
    pub yanks: AtomicU64,
    pub rate_limited: AtomicU64,
    pub auth_failures: AtomicU64,
    pub queue_refusals: AtomicU64,
    pub audit_failures: AtomicU64,
    pub reviews_finished: AtomicU64,
    pub reviews_failed: AtomicU64,
}

impl Metrics {
    pub fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn count_status(&self, status: u16) {
        let counter = match status {
            200..=299 => &self.requests_2xx,
            300..=399 => &self.requests_3xx,
            400..=499 => &self.requests_4xx,
            _ => &self.requests_5xx,
        };
        Self::bump(counter);
    }

    /// The Prometheus text, with the gauges the caller reads from live state.
    pub fn render(&self, gauges: &[(&str, u64)]) -> String {
        let counters: [(&str, &str, &AtomicU64); 13] = [
            (
                "spot_store_requests_2xx_total",
                "Requests answered 2xx.",
                &self.requests_2xx,
            ),
            (
                "spot_store_requests_3xx_total",
                "Requests answered 3xx.",
                &self.requests_3xx,
            ),
            (
                "spot_store_requests_4xx_total",
                "Requests answered 4xx.",
                &self.requests_4xx,
            ),
            (
                "spot_store_requests_5xx_total",
                "Requests answered 5xx.",
                &self.requests_5xx,
            ),
            (
                "spot_store_publishes_accepted_total",
                "Publishes accepted for review.",
                &self.publishes_accepted,
            ),
            (
                "spot_store_publishes_refused_total",
                "Publishes refused.",
                &self.publishes_refused,
            ),
            ("spot_store_yanks_total", "Versions yanked.", &self.yanks),
            (
                "spot_store_rate_limited_total",
                "Requests refused by a rate limit.",
                &self.rate_limited,
            ),
            (
                "spot_store_auth_failures_total",
                "Failed credential checks.",
                &self.auth_failures,
            ),
            (
                "spot_store_queue_refusals_total",
                "Calls refused because the worker queue was full.",
                &self.queue_refusals,
            ),
            (
                "spot_store_audit_failures_total",
                "Audit entries that could not be written.",
                &self.audit_failures,
            ),
            (
                "spot_store_reviews_finished_total",
                "Reviews that produced a report.",
                &self.reviews_finished,
            ),
            (
                "spot_store_reviews_failed_total",
                "Reviews that ended without a report.",
                &self.reviews_failed,
            ),
        ];
        let mut out = String::new();
        for (name, help, counter) in counters {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            let _ = writeln!(out, "{name} {}", counter.load(Ordering::Relaxed));
        }
        for (name, value) in gauges {
            let _ = writeln!(out, "# TYPE {name} gauge");
            let _ = writeln!(out, "{name} {value}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Metrics;

    #[test]
    fn the_text_lists_counters_and_gauges() {
        let metrics = Metrics::default();
        metrics.count_status(204);
        metrics.count_status(404);
        metrics.count_status(503);
        let text = metrics.render(&[("spot_store_revocation_generation", 7)]);
        assert!(text.contains("spot_store_requests_2xx_total 1"));
        assert!(text.contains("spot_store_requests_4xx_total 1"));
        assert!(text.contains("spot_store_requests_5xx_total 1"));
        assert!(text.contains("spot_store_revocation_generation 7"));
    }
}
