//! The loop's settings: every bound the operator can set under `[services.sync_node]`.

use std::time::Duration;

use crate::error::{Error, Result};

/// The loop's timings and sizes. Built by [`LoopSettings::new`] or [`Default`]; both are valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopSettings {
    /// The shortest wait after a failed session (`services.sync_node.backoff_min`).
    pub backoff_min: Duration,
    /// The longest wait after repeated failures (`services.sync_node.backoff_max`).
    pub backoff_max: Duration,
    /// The least time between two connection attempts, whatever the backoff says, so 100 edges
    /// coming back together do not hammer the hub in lockstep (the hub's limits are the second
    /// line).
    pub min_reconnect_interval: Duration,
    /// The wait between rounds when both directions were idle.
    pub poll_idle: Duration,
    /// The budget of one request, across the connection and the body.
    pub request_deadline: Duration,
    /// The most records one pull asks for and one push batch carries.
    pub batch_records: usize,
    /// The most records one push offer holds (the sender's window).
    pub offer_records: usize,
    /// The most pull and push steps one round runs before the loop looks at the stop signal.
    pub steps_per_round: usize,
    /// The most served items kept in the per-session "seen, not yet settled" set; a hostile hub
    /// cannot grow it without bound.
    pub max_unsettled: usize,
}

impl LoopSettings {
    /// Settings; fails on a zero or inverted bound.
    ///
    /// # Errors
    ///
    /// `sync_node_host.invalid_settings`.
    pub fn new(backoff_min: Duration, backoff_max: Duration) -> Result<Self> {
        Self {
            backoff_min,
            backoff_max,
            ..Self::default()
        }
        .validated()
    }

    /// The same settings, checked.
    ///
    /// # Errors
    ///
    /// `sync_node_host.invalid_settings` when a duration or size is zero, the backoff is
    /// inverted, or the backoff maximum is above a day.
    pub fn validated(self) -> Result<Self> {
        let bad = |what: &str| Err(Error::invalid_settings(what.to_owned()));
        if self.backoff_min.is_zero() {
            return bad("backoff_min is at least one millisecond");
        }
        if self.backoff_max < self.backoff_min {
            return bad("backoff_max is at least backoff_min");
        }
        if self.backoff_max > Duration::from_secs(24 * 60 * 60) {
            return bad("backoff_max is at most one day");
        }
        if self.request_deadline.is_zero() {
            return bad("the request deadline is not zero");
        }
        if self.batch_records == 0 || self.offer_records == 0 || self.steps_per_round == 0 {
            return bad("batch_records, offer_records and steps_per_round are at least one");
        }
        if self.offer_records < self.batch_records {
            return bad("an offer holds at least one batch");
        }
        if self.max_unsettled == 0 {
            return bad("max_unsettled is at least one");
        }
        Ok(self)
    }
}

impl Default for LoopSettings {
    fn default() -> Self {
        Self {
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(5 * 60),
            min_reconnect_interval: Duration::from_secs(5),
            poll_idle: Duration::from_secs(1),
            request_deadline: Duration::from_secs(60),
            batch_records: 256,
            offer_records: 1_024,
            steps_per_round: 64,
            max_unsettled: 8_192,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_defaults_are_valid() {
        LoopSettings::default().validated().unwrap();
    }

    #[test]
    fn inverted_and_zero_bounds_are_refused() {
        assert!(LoopSettings::new(Duration::ZERO, Duration::from_secs(1)).is_err());
        assert!(LoopSettings::new(Duration::from_secs(10), Duration::from_secs(1)).is_err());
        assert!(LoopSettings::new(Duration::from_secs(1), Duration::from_secs(90_000)).is_err());
        let settings = LoopSettings {
            batch_records: 0,
            ..LoopSettings::default()
        };
        assert!(settings.validated().is_err());
    }
}
