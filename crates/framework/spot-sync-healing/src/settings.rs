//! The healing limits. Settings, not constants (ADR-067 open questions, accepted 26 Oct 2026).

use spot_conditions::Millis;

use crate::HealingError;

/// The width of the rate window: one minute.
pub const RATE_WINDOW: Millis = Millis::new(60_000);

/// How the driver is bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealingSettings {
    /// Repairs started in any one-minute window, over all nodes. Default 30.
    pub max_repairs_per_minute: u32,
    /// Repairs started in any one-minute window against one node. Default 5.
    pub max_repairs_per_node_per_minute: u32,
    /// Repairs executed and not yet verified at one time. Default 16.
    pub max_in_flight: u32,
    /// Executions of one repair before it stops and the condition is raised. Default 4.
    pub max_attempts: u32,
    /// The wait after the first failure; it doubles per failure. Default 10 seconds.
    pub backoff_base: Millis,
    /// The longest wait between two attempts. Default 15 minutes.
    pub backoff_cap: Millis,
    /// How long after a `Done` the finding may still be reported before the repair counts as
    /// not having worked. Default 5 minutes.
    pub verify_after: Millis,
}

impl Default for HealingSettings {
    fn default() -> Self {
        Self {
            max_repairs_per_minute: 30,
            max_repairs_per_node_per_minute: 5,
            max_in_flight: 16,
            max_attempts: 4,
            backoff_base: Millis::new(10_000),
            backoff_cap: Millis::new(900_000),
            verify_after: Millis::new(300_000),
        }
    }
}

impl HealingSettings {
    /// Checks the settings.
    ///
    /// # Errors
    ///
    /// [`HealingError::InvalidSettings`] for a zero limit or a backoff base above the cap.
    pub fn validate(&self) -> Result<(), HealingError> {
        let zero = |reason: &str| {
            Err(HealingError::InvalidSettings {
                reason: reason.to_owned(),
            })
        };
        if self.max_repairs_per_minute == 0 {
            return zero("max_repairs_per_minute is zero");
        }
        if self.max_repairs_per_node_per_minute == 0 {
            return zero("max_repairs_per_node_per_minute is zero");
        }
        if self.max_in_flight == 0 {
            return zero("max_in_flight is zero");
        }
        if self.max_attempts == 0 {
            return zero("max_attempts is zero");
        }
        if self.backoff_base == Millis::ZERO {
            return zero("backoff_base is zero");
        }
        if self.backoff_base > self.backoff_cap {
            return zero("backoff_base is above backoff_cap");
        }
        Ok(())
    }

    /// The wait after the `failures`-th failed or unverified attempt: the base, doubled for
    /// each earlier failure, never above the cap.
    #[must_use]
    pub fn backoff(&self, failures: u32) -> Millis {
        let doublings = failures.saturating_sub(1).min(32);
        let wait = self
            .backoff_base
            .as_u64()
            .saturating_mul(1_u64 << doublings);
        Millis::new(wait.min(self.backoff_cap.as_u64()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_defaults_are_the_accepted_starting_values() {
        let s = HealingSettings::default();
        assert_eq!(s.max_repairs_per_minute, 30);
        assert_eq!(s.max_repairs_per_node_per_minute, 5);
        assert_eq!(s.max_attempts, 4);
        assert!(s.validate().is_ok());
    }

    #[test]
    fn backoff_doubles_then_stops_at_the_cap() {
        let s = HealingSettings::default();
        let waits: Vec<u64> = (1..=10).map(|n| s.backoff(n).as_u64()).collect();
        assert_eq!(waits[..4], [10_000, 20_000, 40_000, 80_000]);
        assert_eq!(waits[9], 900_000);
        assert_eq!(s.backoff(u32::MAX).as_u64(), 900_000);
    }

    #[test]
    fn a_zero_limit_is_refused() {
        let bad = HealingSettings {
            max_attempts: 0,
            ..HealingSettings::default()
        };
        assert_eq!(
            bad.validate().unwrap_err().code(),
            "sync_healing.invalid_settings"
        );
        let bad = HealingSettings {
            backoff_base: Millis::new(2_000_000),
            ..HealingSettings::default()
        };
        assert!(bad.validate().is_err());
    }
}
