//! The evaluation cadence and window: the two settings that decide how quickly a failure is
//! seen and how much history a rule may look at.
//!
//! Freshness (how old telemetry may be before the subject turns amber) is **not** here: it is
//! a rule data entry in `rules/freshness.toml`, because it is a health threshold like any
//! other and an administrator tunes it as data.

use serde::{Deserialize, Serialize};

use crate::error::FleetHealthError;

/// How often the control plane evaluates the fleet, and how far back rules may look.
///
/// The defaults are the ones the acceptance describes: evaluate every 15 seconds over a
/// 10-minute window. With the shipped rules (a failing probe is red after 30 seconds), a
/// seeded failure becomes visible within 45 seconds, inside the one-minute acceptance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Seconds between evaluation runs. Must be between 1 and 3600.
    pub evaluation_interval_seconds: u64,
    /// How far back an evaluation looks. Must be at least one interval and at most one day.
    pub window_seconds: u64,
}

impl Settings {
    /// The shortest accepted evaluation interval, in seconds.
    pub const MIN_INTERVAL_SECONDS: u64 = 1;
    /// The longest accepted evaluation interval, in seconds.
    pub const MAX_INTERVAL_SECONDS: u64 = 3_600;
    /// The longest accepted evaluation window, in seconds (one day).
    pub const MAX_WINDOW_SECONDS: u64 = 86_400;

    /// The default cadence: every 15 seconds over a 10-minute window.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            evaluation_interval_seconds: 15,
            window_seconds: 600,
        }
    }

    /// Checks the settings against their bounds.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidSettings`] when the interval is zero or too long, the window
    /// is shorter than one interval, or the window exceeds one day.
    pub fn validate(&self) -> Result<(), FleetHealthError> {
        let reason = if !(Self::MIN_INTERVAL_SECONDS..=Self::MAX_INTERVAL_SECONDS)
            .contains(&self.evaluation_interval_seconds)
        {
            Some(format!(
                "the evaluation interval must be between {} and {} seconds",
                Self::MIN_INTERVAL_SECONDS,
                Self::MAX_INTERVAL_SECONDS
            ))
        } else if self.window_seconds < self.evaluation_interval_seconds {
            Some("the evaluation window must be at least one interval long".to_owned())
        } else if self.window_seconds > Self::MAX_WINDOW_SECONDS {
            Some(format!(
                "the evaluation window must be at most {} seconds",
                Self::MAX_WINDOW_SECONDS
            ))
        } else {
            None
        };
        match reason {
            Some(reason) => Err(FleetHealthError::InvalidSettings { reason }),
            None => Ok(()),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_cadence() {
        let settings = Settings::default();
        assert_eq!(settings.evaluation_interval_seconds, 15);
        assert_eq!(settings.window_seconds, 600);
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn impossible_cadences_are_refused() {
        let zero = Settings {
            evaluation_interval_seconds: 0,
            window_seconds: 60,
        };
        assert_eq!(
            zero.validate().map_err(|e| e.code()),
            Err("fleet_health.invalid_settings")
        );
        let short = Settings {
            evaluation_interval_seconds: 30,
            window_seconds: 15,
        };
        assert!(short.validate().is_err());
        let huge = Settings {
            evaluation_interval_seconds: 15,
            window_seconds: 86_401,
        };
        assert!(huge.validate().is_err());
    }
}
