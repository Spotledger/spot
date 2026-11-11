//! The AI isolation health surface (WP-AI-05.T1).
//!
//! [`AiHealthReport`] is the typed snapshot a host publishes: how many calls are in flight,
//! the current AIMD admission limit, the shed and refused counters, and every breaker that
//! has been touched with its state and, when open, when it next admits a probe. An open
//! breaker on a scope a tenant depends on is the "needs you" condition of ADR-067.

use crate::breaker::BreakerHealth;

/// A snapshot of the AI isolation state for the health surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiHealthReport {
    /// How many AI calls are admitted right now.
    pub in_flight: u32,
    /// The current AIMD admission limit (0 when AI traffic is shed).
    pub aimd_limit: u32,
    /// How many calls were shed because interactive load was high.
    pub shed_total: u64,
    /// How many calls were refused because the admission budget was full.
    pub refused_total: u64,
    /// Every breaker that has been touched, ordered by scope.
    pub breakers: Vec<BreakerHealth>,
}

impl AiHealthReport {
    /// Whether every breaker is closed.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.breakers
            .iter()
            .all(|breaker| breaker.state == crate::breaker::BreakerState::Closed)
    }
}
