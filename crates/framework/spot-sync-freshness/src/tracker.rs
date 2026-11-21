//! The per-node freshness tracker (ADR-065 section 4).
//!
//! Staleness is **elapsed monotonic time since the node last confirmed itself current for a
//! class**. No wall-clock value enters here: every instant is a monotonic millisecond count the
//! caller reads from its own monotonic clock (ADR-077), so a wall-clock jump, or skew against the
//! hub, cannot make the node look fresher than it is.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The ADR-065 default for `max_staleness`: 48 hours.
pub const DEFAULT_MAX_STALENESS_MS: u64 = 48 * 60 * 60 * 1000;

/// The tenant's `max_staleness` per freshness class, with a default for classes not listed.
///
/// A tunable setting, never a hidden constant: build it from the tenant's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessSettings {
    /// Applies to a class with no entry in `per_class`.
    pub default_max_staleness_ms: u64,
    /// Per-class override, milliseconds.
    pub per_class: BTreeMap<String, u64>,
}

impl Default for FreshnessSettings {
    fn default() -> Self {
        Self {
            default_max_staleness_ms: DEFAULT_MAX_STALENESS_MS,
            per_class: BTreeMap::new(),
        }
    }
}

impl FreshnessSettings {
    /// The `max_staleness` that applies to `class`, in milliseconds.
    #[must_use]
    pub fn max_staleness(&self, class: &str) -> u64 {
        self.per_class
            .get(class)
            .copied()
            .unwrap_or(self.default_max_staleness_ms)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Confirmation {
    at_monotonic_millis: u64,
    horizon: Option<u64>,
}

/// Tracks, per freshness class, when this node was last confirmed current.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreshnessTracker {
    confirmed: BTreeMap<String, Confirmation>,
}

impl FreshnessTracker {
    /// A tracker that has confirmed nothing yet. Every class is stale until confirmed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the node is confirmed current for `class` at monotonic instant
    /// `at_monotonic_millis`. Call it when the applied prefix covers the class horizon the hub
    /// returned. An earlier instant than one already recorded is ignored.
    pub fn confirm(&mut self, class: &str, at_monotonic_millis: u64) {
        self.record(class, at_monotonic_millis, None);
    }

    /// Like [`confirm`](Self::confirm) and also remembers the horizon position that was covered,
    /// so the stamp can show it in `class_horizons_confirmed`.
    pub fn confirm_with_horizon(&mut self, class: &str, horizon: u64, at_monotonic_millis: u64) {
        self.record(class, at_monotonic_millis, Some(horizon));
    }

    /// Confirm `class` only when `applied_prefix` covers `horizon`. Returns whether it did.
    pub fn confirm_if_covered(
        &mut self,
        class: &str,
        applied_prefix: u64,
        horizon: u64,
        at_monotonic_millis: u64,
    ) -> bool {
        if applied_prefix >= horizon {
            self.confirm_with_horizon(class, horizon, at_monotonic_millis);
            true
        } else {
            false
        }
    }

    fn record(&mut self, class: &str, at: u64, horizon: Option<u64>) {
        match self.confirmed.get(class) {
            Some(prev) if prev.at_monotonic_millis > at => {}
            _ => {
                self.confirmed.insert(
                    class.to_owned(),
                    Confirmation {
                        at_monotonic_millis: at,
                        horizon,
                    },
                );
            }
        }
    }

    /// Milliseconds since `class` was last confirmed, or `None` if never confirmed.
    ///
    /// A `now` earlier than the confirmation (a misuse of a monotonic clock) reads as zero.
    #[must_use]
    pub fn elapsed(&self, class: &str, now_monotonic_millis: u64) -> Option<u64> {
        self.confirmed
            .get(class)
            .map(|c| now_monotonic_millis.saturating_sub(c.at_monotonic_millis))
    }

    /// Whether the node is stale for `class`: elapsed strictly exceeds `max_staleness`, or the
    /// class was never confirmed (the node cannot know what it is missing).
    #[must_use]
    pub fn is_stale(
        &self,
        class: &str,
        now_monotonic_millis: u64,
        settings: &FreshnessSettings,
    ) -> bool {
        match self.elapsed(class, now_monotonic_millis) {
            Some(e) => e > settings.max_staleness(class),
            None => true,
        }
    }

    /// The members of `classes` that are stale now, in the order given, without duplicates.
    #[must_use]
    pub fn stale_classes(
        &self,
        classes: &[String],
        now_monotonic_millis: u64,
        settings: &FreshnessSettings,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in classes {
            if !out.contains(c) && self.is_stale(c, now_monotonic_millis, settings) {
                out.push(c.clone());
            }
        }
        out
    }

    /// The horizon positions recorded by `confirm_with_horizon`, for the stamp.
    #[must_use]
    pub fn confirmed_horizons(&self) -> BTreeMap<String, u64> {
        self.confirmed
            .iter()
            .filter_map(|(k, v)| v.horizon.map(|h| (k.clone(), h)))
            .collect()
    }
}
