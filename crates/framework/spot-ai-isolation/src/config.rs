//! Per-call, per-session and pool limits, and the settings-declared pool budget.
//!
//! The admission limits are plain values with sensible defaults; the dedicated pool budget
//! (pool size, statement timeout, `idle_in_transaction_session_timeout`, work-mem cap) is
//! declared as four `spot-settings` specs compiled in from `settings/*.toml`, so the console
//! and the settings registry own them like every other platform setting.

use std::time::Duration;

use spot_coordinator::RateLimit;
use spot_settings::{ParamValue, RegistryBuilder, SettingSpec, SettingsRegistry, parse_spec};

use crate::error::{AiError, Result};

/// The admission limits for one AI call path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AiLimits {
    /// The most concurrent AI calls one agent may hold on this node.
    pub per_agent_concurrency: u32,
    /// The most concurrent AI calls one tenant may hold on this node.
    pub per_tenant_concurrency: u32,
    /// The request rate one agent may make (in-process fallback).
    pub per_agent_rate: RateLimit,
    /// The request rate one tenant may make (in-process fallback).
    pub per_tenant_rate: RateLimit,
    /// The per-call deadline: no single call may run longer.
    pub call_timeout: Duration,
    /// The default per-session deadline, when a request does not carry one.
    pub session_timeout: Duration,
    /// The most rows one call may ask for or return.
    pub max_rows: u32,
    /// The most bytes one call's input or result may carry.
    pub max_bytes: usize,
}

impl Default for AiLimits {
    fn default() -> Self {
        Self {
            per_agent_concurrency: 4,
            per_tenant_concurrency: 16,
            per_agent_rate: RateLimit {
                capacity: 60,
                per: Duration::from_secs(60),
            },
            per_tenant_rate: RateLimit {
                capacity: 600,
                per: Duration::from_secs(60),
            },
            call_timeout: Duration::from_secs(30),
            session_timeout: Duration::from_secs(3600),
            max_rows: 500,
            max_bytes: 1024 * 1024,
        }
    }
}

impl AiLimits {
    /// The defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// The dedicated AI connection-pool budget, declared as `spot-settings` keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AiPoolSettings {
    /// `ai.pool.size`: the pool's connection budget, kept apart from human traffic.
    pub pool_size: u32,
    /// `ai.pool.statement_timeout_ms`: the server-side statement timeout for AI sessions.
    pub statement_timeout: Duration,
    /// `ai.pool.idle_in_transaction_timeout_ms`: how long an idle AI transaction may hold a
    /// connection before the server ends it.
    pub idle_in_transaction_timeout: Duration,
    /// `ai.pool.work_mem_kb`: the per-session work-mem cap, in kibibytes.
    pub work_mem_kb: u32,
}

impl Default for AiPoolSettings {
    fn default() -> Self {
        Self {
            pool_size: 4,
            statement_timeout: Duration::from_secs(30),
            idle_in_transaction_timeout: Duration::from_secs(60),
            work_mem_kb: 64 * 1024,
        }
    }
}

impl AiPoolSettings {
    /// The defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The `statement_timeout` in whole milliseconds (the value a `SET` statement takes).
    #[must_use]
    pub fn statement_timeout_ms(&self) -> u64 {
        millis(self.statement_timeout)
    }

    /// The `idle_in_transaction_session_timeout` in whole milliseconds.
    #[must_use]
    pub fn idle_in_transaction_timeout_ms(&self) -> u64 {
        millis(self.idle_in_transaction_timeout)
    }

    /// Reads the four settings from parsed specs, keeping the defaults for any that are absent
    /// and refusing a value that does not fit.
    ///
    /// # Errors
    ///
    /// [`crate::error::INVALID_ARGUMENT`] when a present value is zero, negative or too large.
    pub fn from_specs(specs: &[SettingSpec]) -> Result<Self> {
        let mut pool = Self::default();
        if let Some(size) = int_setting(specs, "size")? {
            pool.pool_size = u32::try_from(size).map_err(|_| {
                AiError::invalid_argument("the AI pool size must be a positive 32-bit value")
            })?;
            if pool.pool_size == 0 {
                return Err(AiError::invalid_argument(
                    "the AI pool size must be at least 1",
                ));
            }
        }
        if let Some(ms) = int_setting(specs, "statement_timeout_ms")? {
            pool.statement_timeout = positive_millis(ms, "statement timeout")?;
        }
        if let Some(ms) = int_setting(specs, "idle_in_transaction_timeout_ms")? {
            pool.idle_in_transaction_timeout = positive_millis(ms, "idle-in-transaction timeout")?;
        }
        if let Some(kb) = int_setting(specs, "work_mem_kb")? {
            pool.work_mem_kb = u32::try_from(kb).map_err(|_| {
                AiError::invalid_argument("the AI work-mem cap must be a positive 32-bit value")
            })?;
            if pool.work_mem_kb == 0 {
                return Err(AiError::invalid_argument(
                    "the AI work-mem cap must be at least 1 KiB",
                ));
            }
        }
        Ok(pool)
    }
}

/// The four pool settings, parsed from their compiled-in TOML specs.
///
/// # Errors
///
/// [`crate::error::INVALID_ARGUMENT`] when a compiled-in spec does not parse (a build-time
/// mistake, caught by the crate tests).
pub fn pool_settings_specs() -> Result<Vec<SettingSpec>> {
    const FILES: [&str; 4] = [
        include_str!("../settings/ai_pool_size.toml"),
        include_str!("../settings/ai_pool_statement_timeout.toml"),
        include_str!("../settings/ai_pool_idle_in_transaction_timeout.toml"),
        include_str!("../settings/ai_pool_work_mem.toml"),
    ];
    let mut specs = Vec::with_capacity(FILES.len());
    for text in FILES {
        let spec = parse_spec(text).map_err(|report| {
            AiError::invalid_argument(format!("an AI pool setting spec is invalid: {report}"))
        })?;
        specs.push(spec);
    }
    Ok(specs)
}

/// The pool settings registered in a validated `spot-settings` registry.
///
/// # Errors
///
/// [`crate::error::INVALID_ARGUMENT`] when the specs do not parse or do not validate.
pub fn pool_settings_registry() -> Result<SettingsRegistry> {
    let specs = pool_settings_specs()?;
    RegistryBuilder::new()
        .register_all(specs)
        .build()
        .map_err(|report| {
            AiError::invalid_argument(format!("AI pool settings are invalid: {report}"))
        })
}

/// The `Duration` for `ms` whole milliseconds, refusing a non-positive value.
fn positive_millis(ms: i64, what: &str) -> Result<Duration> {
    if ms <= 0 {
        return Err(AiError::invalid_argument(format!(
            "the AI {what} must be at least 1 ms"
        )));
    }
    Ok(Duration::from_millis(ms.unsigned_abs()))
}

/// The default of the pool setting whose final key segment is `name`, when present.
fn int_setting(specs: &[SettingSpec], name: &str) -> Result<Option<i64>> {
    for spec in specs {
        if spec.key.rsplit('.').next() != Some(name) {
            continue;
        }
        return match &spec.default {
            Some(ParamValue::Int(value)) => Ok(Some(*value)),
            Some(_) => Err(AiError::invalid_argument(format!(
                "the AI pool setting ending in {name} must be an integer"
            ))),
            None => Ok(None),
        };
    }
    Ok(None)
}

/// A `Duration` in whole milliseconds, saturating rather than panicking.
fn millis(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_pool_settings_parse_and_validate() {
        let specs = pool_settings_specs().expect("the compiled-in specs parse");
        assert_eq!(specs.len(), 4);
        let registry = pool_settings_registry().expect("the specs validate");
        assert_eq!(registry.len(), 4);
        assert!(registry.keys().all(|key| key.starts_with("ai.pool.")));
    }

    #[test]
    fn the_defaults_are_read_back_from_the_specs() {
        let specs = pool_settings_specs().expect("the compiled-in specs parse");
        let pool = AiPoolSettings::from_specs(&specs).expect("the defaults fit");
        assert_eq!(pool.pool_size, 4);
        assert_eq!(pool.statement_timeout, Duration::from_secs(30));
        assert_eq!(pool.idle_in_transaction_timeout, Duration::from_secs(60));
        assert_eq!(pool.work_mem_kb, 64 * 1024);
    }

    #[test]
    fn an_empty_spec_list_keeps_the_defaults() {
        let pool = AiPoolSettings::from_specs(&[]).expect("no specs is fine");
        assert_eq!(pool, AiPoolSettings::default());
    }
}
