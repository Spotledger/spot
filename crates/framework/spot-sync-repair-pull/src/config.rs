//! Repair settings. Every number is a setting (ADR-061 to ADR-067 starting values), never hidden.

use crate::error::RepairError;

/// The bounds and timings of the repair loop. Times are milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairConfig {
    /// Most keys in one pull request.
    pub max_keys: usize,
    /// Minimum gap before a key is asked again (also the first backoff step).
    pub ask_interval_ms: u64,
    /// Ceiling of the exponential backoff.
    pub max_interval_ms: u64,
    /// Asks of one key before the loop escalates instead of asking again.
    pub max_asks_per_key: u32,
    /// More than this many keys missing at once escalates.
    pub escalate_keys: usize,
    /// A key missing this long raises `CLOSURE_UNRESOLVED`.
    pub raise_after_ms: u64,
    /// Most distinct missing keys tracked; the oldest is dropped (counted) beyond it and the
    /// loop escalates, so a dropped key is still repaired by the scoped re-send.
    pub max_tracked: usize,
    /// Seed of the deterministic backoff jitter.
    pub jitter_seed: u64,
}

impl Default for RepairConfig {
    fn default() -> Self {
        Self {
            max_keys: 200,
            ask_interval_ms: 30_000,
            max_interval_ms: 900_000,
            max_asks_per_key: 5,
            escalate_keys: 500,
            raise_after_ms: 600_000,
            max_tracked: 100_000,
            jitter_seed: 0,
        }
    }
}

impl RepairConfig {
    /// Checks the settings.
    ///
    /// # Errors
    /// `InvalidConfig` when a bound is zero or `max_interval_ms` is below `ask_interval_ms`.
    pub fn validate(&self) -> Result<(), RepairError> {
        let bad = |reason| Err(RepairError::InvalidConfig { reason });
        if self.max_keys == 0 {
            return bad("max_keys is zero");
        }
        if self.ask_interval_ms == 0 || self.max_interval_ms == 0 || self.raise_after_ms == 0 {
            return bad("an interval is zero");
        }
        if self.max_interval_ms < self.ask_interval_ms {
            return bad("max_interval_ms is below ask_interval_ms");
        }
        if self.max_asks_per_key == 0 || self.escalate_keys == 0 || self.max_tracked == 0 {
            return bad("a bound is zero");
        }
        Ok(())
    }
}
