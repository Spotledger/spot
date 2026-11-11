//! The review configuration: bounds, limits and defaults, loadable from TOML.
//!
//! Every field has a sensible default; the whole structure is loadable from TOML with
//! [`ReviewConfig::from_toml`]. An unknown key is an error, so a typo in a review profile is
//! refused rather than silently ignored.

use serde::{Deserialize, Serialize};

/// Everything the review needs that is not the bundle itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewConfig {
    /// The export the resource profile calls when the author ships no `profile.toml`: the
    /// manifest-declared health/init entry. Default `on-event`.
    pub default_entry: String,
    /// The event string passed to [`ReviewConfig::default_entry`]. Default `{}`.
    pub default_event: String,
    /// The optional author profile, relative to the app crate (it must also be an
    /// `[[assets]]` entry). Default `assets/profile.toml`.
    pub profile_asset: String,
    /// The most DocTypes an app may declare for reading before the scan calls it broad.
    pub max_doctypes_read: u32,
    /// The most DocTypes an app may declare for writing before the scan calls it broad.
    pub max_doctypes_write: u32,
    /// The share of the output limit, in percent, above which a result is a Medium finding.
    pub output_share_percent: u32,
    /// The bundle size and count limits passed to `Bundle::open`.
    pub bundle: BundleLimits,
    /// The WASM host's engine configuration.
    pub host: HostLimits,
    /// The per-call limits the resource profile runs under.
    pub limits: CallLimits,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            default_entry: "on-event".to_owned(),
            default_event: "{}".to_owned(),
            profile_asset: "assets/profile.toml".to_owned(),
            max_doctypes_read: 16,
            max_doctypes_write: 16,
            output_share_percent: 50,
            bundle: BundleLimits::default(),
            host: HostLimits::default(),
            limits: CallLimits::default(),
        }
    }
}

impl ReviewConfig {
    /// Loads a configuration from TOML; an unknown key or a wrong type is an error.
    ///
    /// # Errors
    /// [`ReviewConfigError::Parse`] when the TOML does not match this shape.
    pub fn from_toml(text: &str) -> Result<Self, ReviewConfigError> {
        toml::from_str(text).map_err(|err| ReviewConfigError::Parse(err.to_string()))
    }

    /// The host every bundle is opened for: the workspace's platform version.
    pub fn host_info(&self) -> spot_bundle::HostInfo {
        spot_pack::PackOptions::default().host
    }
}

/// Why a review configuration was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReviewConfigError {
    /// The TOML does not parse, or does not match the configuration's shape.
    #[error("the review configuration does not parse: {0}")]
    Parse(String),
}

/// The `Bundle::open` limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BundleLimits {
    /// The largest input, in bytes.
    pub max_compressed_bytes: u64,
    /// The largest total inflated size, in bytes.
    pub max_total_bytes: u64,
    /// The largest number of entries.
    pub max_entries: u32,
    /// The largest inflated size of one entry, in bytes.
    pub max_file_bytes: u64,
}

impl Default for BundleLimits {
    fn default() -> Self {
        let limits = spot_bundle::Limits::new();
        Self {
            max_compressed_bytes: limits.max_compressed_bytes,
            max_total_bytes: limits.max_total_bytes,
            max_entries: limits.max_entries,
            max_file_bytes: limits.max_file_bytes,
        }
    }
}

impl BundleLimits {
    /// The limits as `spot-bundle` carries them.
    pub fn to_limits(self) -> spot_bundle::Limits {
        spot_bundle::Limits {
            max_compressed_bytes: self.max_compressed_bytes,
            max_total_bytes: self.max_total_bytes,
            max_entries: self.max_entries,
            max_file_bytes: self.max_file_bytes,
        }
    }
}

/// The WASM host's engine configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostLimits {
    /// How many calls may run at once; also the pool size.
    pub max_concurrent_calls: u32,
    /// The largest linear memory any call can have, in bytes.
    pub max_memory_bytes: usize,
    /// The wall-clock granularity, in milliseconds.
    pub tick_ms: u64,
    /// The wasm call stack, in bytes.
    pub stack_bytes: usize,
}

impl Default for HostLimits {
    fn default() -> Self {
        let config = spot_wasm::HostConfig::default();
        Self {
            max_concurrent_calls: config.max_concurrent_calls,
            max_memory_bytes: config.max_memory_bytes,
            tick_ms: u64::try_from(config.tick.as_millis()).unwrap_or(5),
            stack_bytes: config.stack_bytes,
        }
    }
}

impl HostLimits {
    /// The host configuration as `spot-wasm` carries it.
    pub fn to_host_config(self) -> spot_wasm::HostConfig {
        spot_wasm::HostConfig {
            max_concurrent_calls: self.max_concurrent_calls,
            max_memory_bytes: self.max_memory_bytes,
            tick: std::time::Duration::from_millis(self.tick_ms),
            stack_bytes: self.stack_bytes,
        }
    }
}

/// The per-call limits the resource profile runs every call under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CallLimits {
    /// Units of fuel, roughly one per instruction.
    pub fuel: u64,
    /// The wall-clock limit, in milliseconds.
    pub wall_clock_ms: u64,
    /// The most bytes of linear memory the call may hold.
    pub memory_bytes: usize,
    /// The most elements a table may hold.
    pub table_elements: usize,
    /// The most calls to host imports.
    pub host_calls: u32,
    /// The largest result, in bytes.
    pub output_bytes: usize,
}

impl Default for CallLimits {
    fn default() -> Self {
        Self {
            fuel: 200_000_000,
            wall_clock_ms: 2_000,
            memory_bytes: 16 * 1024 * 1024,
            table_elements: 10_000,
            host_calls: 1_000,
            output_bytes: 1024 * 1024,
        }
    }
}

impl CallLimits {
    /// The limits as `spot-wasm` carries them.
    pub fn to_limits(self) -> spot_wasm::Limits {
        spot_wasm::Limits {
            fuel: self.fuel,
            wall_clock: std::time::Duration::from_millis(self.wall_clock_ms),
            memory_bytes: self.memory_bytes,
            table_elements: self.table_elements,
            host_calls: self.host_calls,
            output_bytes: self.output_bytes,
        }
    }
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let config = ReviewConfig::default();
        assert_eq!(config.default_entry, "on-event");
        assert_eq!(config.max_doctypes_read, 16);
        assert_eq!(config.output_share_percent, 50);
        assert_eq!(config.limits.host_calls, 1_000);
    }

    #[test]
    fn toml_loads_and_overrides() {
        let config = ReviewConfig::from_toml(
            r#"
            default_entry = "on-init"
            output_share_percent = 10
            [limits]
            fuel = 5
            "#,
        )
        .unwrap();
        assert_eq!(config.default_entry, "on-init");
        assert_eq!(config.output_share_percent, 10);
        assert_eq!(config.limits.fuel, 5);
        // An unmentioned field keeps its default.
        assert_eq!(config.limits.output_bytes, 1024 * 1024);
    }

    #[test]
    fn an_unknown_key_is_an_error() {
        let err = ReviewConfig::from_toml("surprise = 1").unwrap_err();
        assert!(matches!(err, ReviewConfigError::Parse(_)));
    }

    #[test]
    fn a_wrong_type_is_an_error() {
        assert!(ReviewConfig::from_toml("output_share_percent = \"a lot\"").is_err());
    }

    #[test]
    fn host_info_is_the_workspace_version() {
        assert_eq!(
            ReviewConfig::default().host_info().version.to_string(),
            "0.1.0"
        );
    }
}
