//! `[services.scripts]`: the operator ceilings (WP-EXT-06.T4).

use super::*;


/// `[services.scripts]`: the operator's ceilings. Unknown keys are refused. A script can lower
/// what it uses and never raise a ceiling.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether scripts run at all.
    pub enabled: bool,
    /// How long a loaded script list is trusted before it is read again, seconds. A change on
    /// this node is seen at once (the cache is bumped when the save commits); this bounds how long
    /// another node takes to see it.
    pub refresh_secs: u64,
    /// The timeout a script gets when it names none, milliseconds.
    pub default_timeout_ms: u64,
    /// The most any script may take, milliseconds.
    pub max_timeout_ms: u64,
    /// The memory limit of one run, bytes.
    pub memory_bytes: usize,
    /// The instruction budget of one run.
    pub instructions: u64,
    /// The most host calls one run may make.
    pub host_calls: u32,
    /// The largest serialised result, bytes.
    pub output_bytes: usize,
    /// Consecutive failures that disable a script.
    pub failure_threshold: u32,
    /// The most scripts one event of one DocType may run.
    pub max_scripts_per_event: u32,
    /// How many runs the in-memory run log keeps.
    pub log_capacity: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_secs: 30,
            default_timeout_ms: 200,
            max_timeout_ms: 2_000,
            memory_bytes: 16 * 1024 * 1024,
            instructions: 100_000_000,
            host_calls: 100,
            output_bytes: 256 * 1024,
            failure_threshold: 5,
            max_scripts_per_event: 8,
            log_capacity: 256,
        }
    }
}

impl Settings {
    /// The first key that is out of range, or `None`. The bounds are hard: they keep an
    /// operator typo from turning the ceilings into no ceilings.
    #[must_use]
    pub fn invalid_key(&self) -> Option<&'static str> {
        let checks: [(&'static str, bool); 10] = [
            ("refresh_secs", (1..=3_600).contains(&self.refresh_secs)),
            (
                "max_timeout_ms",
                (1..=10_000).contains(&self.max_timeout_ms),
            ),
            (
                "default_timeout_ms",
                self.default_timeout_ms >= 1 && self.default_timeout_ms <= self.max_timeout_ms,
            ),
            (
                "memory_bytes",
                (1024 * 1024..=256 * 1024 * 1024).contains(&self.memory_bytes),
            ),
            (
                "instructions",
                (1_000..=2_000_000_000).contains(&self.instructions),
            ),
            ("host_calls", (1..=10_000).contains(&self.host_calls)),
            (
                "output_bytes",
                (1024..=4 * 1024 * 1024).contains(&self.output_bytes),
            ),
            (
                "failure_threshold",
                (1..=100).contains(&self.failure_threshold),
            ),
            (
                "max_scripts_per_event",
                (1..=32).contains(&self.max_scripts_per_event),
            ),
            ("log_capacity", (1..=10_000).contains(&self.log_capacity)),
        ];
        checks.iter().find(|(_, ok)| !ok).map(|(key, _)| *key)
    }

    /// The limits Server Scripts run under.
    #[must_use]
    pub fn script_limits(&self) -> ScriptLimits {
        ScriptLimits {
            default_timeout_ms: self.default_timeout_ms,
            max_timeout_ms: self.max_timeout_ms,
            memory_bytes: self.memory_bytes,
            instructions: self.instructions,
            host_calls: self.host_calls,
            output_bytes: self.output_bytes,
            failure_threshold: self.failure_threshold,
            log_capacity: self.log_capacity,
        }
    }

    /// The limits TypeScript controllers run under: the same ceilings.
    #[must_use]
    pub fn ts_limits(&self) -> TsLimits {
        TsLimits {
            default_timeout_ms: self.default_timeout_ms,
            max_timeout_ms: self.max_timeout_ms,
            memory_bytes: self.memory_bytes,
            instructions: self.instructions,
            host_calls: self.host_calls,
            output_bytes: self.output_bytes,
            failure_threshold: self.failure_threshold,
            log_capacity: self.log_capacity,
        }
    }

    /// The time budget of one hub step: every script of an event may take its maximum.
    #[must_use]
    pub fn step_budget(&self) -> Duration {
        Duration::from_millis(
            self.max_timeout_ms
                .saturating_mul(u64::from(self.max_scripts_per_event)),
        ) + Duration::from_millis(100)
    }
}

