//! `[services.script_kinds]` (WP-EXT-06.T7).

use super::*;

/// `[services.script_kinds]`. Unknown keys are refused. The per-run ceilings (timeouts, memory,
/// instructions, host calls, output size, failure threshold) are the operator's
/// `[services.scripts]` ceilings, shared with document-event scripts.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Serve API scripts and run scheduled scripts. Default true (when the `scripting` app is
    /// installed).
    pub enabled: bool,
    /// The most scripts of each kind that are loaded; the rest are skipped and listed in the
    /// status. 1 to 2,000. Default 200.
    pub max_scripts: u32,
    /// The default rate limit of a script function, calls per minute per caller. 1 to 100,000.
    /// Default 60.
    pub default_rate_limit_per_minute: u32,
    /// How often another node's change is noticed, seconds. 1 to 3,600. Default 30.
    pub refresh_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_scripts: 200,
            default_rate_limit_per_minute: 60,
            refresh_secs: 30,
        }
    }
}

impl Settings {
    /// The first key out of range, or `None`.
    #[must_use]
    pub fn invalid_key(&self) -> Option<&'static str> {
        if !(1..=2_000).contains(&self.max_scripts) {
            return Some("max_scripts");
        }
        if !(1..=100_000).contains(&self.default_rate_limit_per_minute) {
            return Some("default_rate_limit_per_minute");
        }
        if !(1..=3_600).contains(&self.refresh_secs) {
            return Some("refresh_secs");
        }
        None
    }
}
