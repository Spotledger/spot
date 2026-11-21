//! Per-call limits: the site defaults, per-app overrides, and the site maxima that cap both.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use spot_wasm::Limits;

/// An app's own limits. Every field is optional; a value above the site maximum is cut to it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppLimits {
    /// Units of fuel per call.
    pub fuel: Option<u64>,
    /// Wall-clock milliseconds per call.
    pub wall_clock_ms: Option<u64>,
    /// Linear memory, in MiB.
    pub memory_mib: Option<u64>,
    /// Host calls per call.
    pub host_calls: Option<u32>,
    /// Largest result, in KiB.
    pub output_kib: Option<u64>,
}

/// The limit table of `[services.apps]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitTable {
    /// Default fuel per call.
    #[serde(default = "d_fuel")]
    pub fuel: u64,
    /// Default wall clock per call, in milliseconds.
    #[serde(default = "d_wall")]
    pub wall_clock_ms: u64,
    /// Default memory, in MiB.
    #[serde(default = "d_mem")]
    pub memory_mib: u64,
    /// Default host calls per call.
    #[serde(default = "d_calls")]
    pub host_calls: u32,
    /// Default largest result, in KiB.
    #[serde(default = "d_out")]
    pub output_kib: u64,
    /// The site maximum fuel: no app can be given more.
    #[serde(default = "d_max_fuel")]
    pub max_fuel: u64,
    /// The site maximum wall clock, in milliseconds.
    #[serde(default = "d_max_wall")]
    pub max_wall_clock_ms: u64,
    /// The site maximum memory, in MiB. It is also the pool's slab size.
    #[serde(default = "d_max_mem")]
    pub max_memory_mib: u64,
    /// Per-app overrides, by app name.
    #[serde(default)]
    pub apps: BTreeMap<String, AppLimits>,
}

fn d_fuel() -> u64 {
    100_000_000
}
fn d_wall() -> u64 {
    1_000
}
fn d_mem() -> u64 {
    16
}
fn d_calls() -> u32 {
    1_000
}
fn d_out() -> u64 {
    1_024
}
fn d_max_fuel() -> u64 {
    1_000_000_000
}
fn d_max_wall() -> u64 {
    10_000
}
fn d_max_mem() -> u64 {
    64
}

impl Default for LimitTable {
    fn default() -> Self {
        Self {
            fuel: d_fuel(),
            wall_clock_ms: d_wall(),
            memory_mib: d_mem(),
            host_calls: d_calls(),
            output_kib: d_out(),
            max_fuel: d_max_fuel(),
            max_wall_clock_ms: d_max_wall(),
            max_memory_mib: d_max_mem(),
            apps: BTreeMap::new(),
        }
    }
}

impl LimitTable {
    /// The first setting out of range.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.fuel == 0 || self.max_fuel == 0 {
            return Err("fuel");
        }
        if self.wall_clock_ms == 0 || self.max_wall_clock_ms == 0 {
            return Err("wall_clock_ms");
        }
        if self.memory_mib == 0 || !(1..=1_024).contains(&self.max_memory_mib) {
            return Err("memory_mib");
        }
        if self.host_calls == 0 || self.output_kib == 0 {
            return Err("host_calls");
        }
        Ok(())
    }

    /// The site's memory ceiling in bytes: the host's slab size.
    pub fn max_memory_bytes(&self) -> usize {
        usize::try_from(self.max_memory_mib.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX)
    }

    /// The limits one call of `app` runs under: its override, else the default, never above the
    /// site maxima.
    pub fn for_app(&self, app: &str) -> Limits {
        let own = self.apps.get(app).cloned().unwrap_or_default();
        let fuel = own.fuel.unwrap_or(self.fuel).min(self.max_fuel);
        let wall = own
            .wall_clock_ms
            .unwrap_or(self.wall_clock_ms)
            .min(self.max_wall_clock_ms);
        let mem_mib = own
            .memory_mib
            .unwrap_or(self.memory_mib)
            .min(self.max_memory_mib);
        let defaults = Limits::default();
        Limits {
            fuel,
            wall_clock: Duration::from_millis(wall),
            memory_bytes: usize::try_from(mem_mib.saturating_mul(1024 * 1024))
                .unwrap_or(usize::MAX),
            table_elements: defaults.table_elements,
            host_calls: own.host_calls.unwrap_or(self.host_calls),
            output_bytes: usize::try_from(
                own.output_kib
                    .unwrap_or(self.output_kib)
                    .saturating_mul(1024),
            )
            .unwrap_or(usize::MAX),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn an_override_is_capped_by_the_site_maximum() {
        let mut table = LimitTable::default();
        table.apps.insert(
            "greedy".to_owned(),
            AppLimits {
                fuel: Some(u64::MAX),
                wall_clock_ms: Some(10 * 60 * 1000),
                memory_mib: Some(10_000),
                ..AppLimits::default()
            },
        );
        let limits = table.for_app("greedy");
        assert_eq!(limits.fuel, table.max_fuel);
        assert_eq!(
            limits.wall_clock,
            Duration::from_millis(table.max_wall_clock_ms)
        );
        assert_eq!(limits.memory_bytes, table.max_memory_bytes());
    }

    #[test]
    fn an_app_without_an_override_gets_the_defaults() {
        let table = LimitTable::default();
        let limits = table.for_app("plain");
        assert_eq!(limits.fuel, table.fuel);
        assert_eq!(limits.host_calls, table.host_calls);
        assert!(table.validate().is_ok());
    }
}
