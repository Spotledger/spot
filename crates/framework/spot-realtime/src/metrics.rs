//! Realtime delivery metrics (WP-PLT-15.T5): what the hub dropped, by why.
//!
//! The crate defines the name, the closed set of reasons and the sink; a host turns the call into
//! a Prometheus series. The only label is the reason, from a fixed enum: never a room, a site, a
//! user or a subscription id (rule M3 of the metrics lint).

/// Counter: realtime events or subscriptions dropped, labelled `reason`.
pub const REALTIME_DROPPED_TOTAL: &str = "spot_realtime_dropped_total";

/// Why something was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DropReason {
    /// A subscriber's queue was full and it fell behind.
    SlowConsumer,
    /// A resume point reached into events that are no longer replayable.
    ReplayGap,
    /// An event was refused because the sender exceeded its limit.
    RateLimited,
    /// Anything else.
    Other,
}

impl DropReason {
    /// Every reason, in the order they are documented.
    pub const ALL: [DropReason; 4] = [
        DropReason::SlowConsumer,
        DropReason::ReplayGap,
        DropReason::RateLimited,
        DropReason::Other,
    ];

    /// The label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::SlowConsumer => "slow_consumer",
            DropReason::ReplayGap => "replay_gap",
            DropReason::RateLimited => "rate_limited",
            DropReason::Other => "other",
        }
    }
}

/// Where the count goes.
pub trait RealtimeMetricsSink: Send + Sync {
    /// Adds `count` drops for `reason`.
    fn dropped(&self, reason: &'static str, count: u64);
}

/// Records `count` drops for `reason`; zero is not recorded.
pub fn record_dropped(sink: &dyn RealtimeMetricsSink, reason: DropReason, count: u64) {
    if count > 0 {
        sink.dropped(reason.as_str(), count);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recording(Mutex<BTreeMap<&'static str, u64>>);

    impl RealtimeMetricsSink for Recording {
        fn dropped(&self, reason: &'static str, count: u64) {
            *self.0.lock().unwrap().entry(reason).or_insert(0) += count;
        }
    }

    #[test]
    fn the_counter_moves_by_reason() {
        let sink = Recording::default();
        record_dropped(&sink, DropReason::SlowConsumer, 2);
        record_dropped(&sink, DropReason::SlowConsumer, 3);
        record_dropped(&sink, DropReason::ReplayGap, 1);
        record_dropped(&sink, DropReason::Other, 0);
        let seen = sink.0.lock().unwrap();
        assert_eq!(seen["slow_consumer"], 5);
        assert_eq!(seen["replay_gap"], 1);
        assert!(!seen.contains_key("other"), "zero is not recorded");
    }

    #[test]
    fn the_reasons_are_a_closed_snake_case_set() {
        let names: Vec<&str> = DropReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            names,
            ["slow_consumer", "replay_gap", "rate_limited", "other"]
        );
        assert!(
            names
                .iter()
                .all(|n| n.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
        );
    }
}
