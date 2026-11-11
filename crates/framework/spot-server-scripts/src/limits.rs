//! Per-script limits (ADR-054, ADR-072): a script is bounded, never retried by itself.

use std::time::Duration;

use spot_ts::Limits;

use crate::record::ServerScript;

/// The limits every script runs under, and the policy for disabling one that keeps failing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScriptLimits {
    /// The timeout a script gets when it names none. Default 200 ms.
    pub default_timeout_ms: u64,
    /// The most any script may ask for; a larger request is clamped, never refused. Default 2 s.
    pub max_timeout_ms: u64,
    /// The runtime's memory limit. Default 16 MiB.
    pub memory_bytes: usize,
    /// The instruction budget. Default 100 million.
    pub instructions: u64,
    /// The most host calls one run may make. Default 100.
    pub host_calls: u32,
    /// The largest serialised result. Default 256 KiB.
    pub output_bytes: usize,
    /// How many consecutive failures disable a script. Default 5.
    pub failure_threshold: u32,
    /// How many runs the in-memory log ring keeps. Default 256.
    pub log_capacity: usize,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            default_timeout_ms: 200,
            max_timeout_ms: 2_000,
            memory_bytes: 16 * 1024 * 1024,
            instructions: 100_000_000,
            host_calls: 100,
            output_bytes: 256 * 1024,
            failure_threshold: 5,
            log_capacity: 256,
        }
    }
}

impl ScriptLimits {
    /// The wall-clock deadline for `script`: its own timeout, or the default, clamped to the
    /// site maximum and to at least one millisecond.
    pub fn timeout_for(&self, script: &ServerScript) -> Duration {
        let asked = if script.timeout_ms == 0 {
            self.default_timeout_ms
        } else {
            script.timeout_ms
        };
        Duration::from_millis(asked.clamp(1, self.max_timeout_ms.max(1)))
    }

    /// The engine limits for `script`.
    pub fn engine_limits(&self, script: &ServerScript) -> Limits {
        Limits {
            memory_bytes: self.memory_bytes,
            wall_clock: self.timeout_for(script),
            instructions: self.instructions,
            host_calls: self.host_calls,
            output_bytes: self.output_bytes,
            ..Limits::default()
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::record::{ScriptEvent, ServerScript};

    fn script(timeout_ms: u64) -> ServerScript {
        ServerScript {
            record_id: None,
            name: "s".into(),
            doctype: "note".into(),
            event: ScriptEvent::Validate,
            source: "export function on_validate() {}".into(),
            compiled_hash: None,
            timeout_ms,
            priority: 0,
            enabled: true,
            disabled_reason: None,
        }
    }

    #[test]
    fn timeouts_clamp_to_the_site_maximum() {
        let limits = ScriptLimits::default();
        assert_eq!(limits.timeout_for(&script(0)), Duration::from_millis(200));
        assert_eq!(limits.timeout_for(&script(50)), Duration::from_millis(50));
        assert_eq!(
            limits.timeout_for(&script(9_999)),
            Duration::from_millis(2_000)
        );
        assert_eq!(
            limits.engine_limits(&script(50)).wall_clock,
            Duration::from_millis(50)
        );
    }
}
