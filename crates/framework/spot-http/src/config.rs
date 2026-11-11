//! Server configuration: addresses, bounds and limits.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use serde::Deserialize;
use spot_core::SiteId;

use crate::error::ServerError;

/// All settings the HTTP core needs. Defaults are safe for development
/// (`127.0.0.1:8000`); production values come from the service configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Addresses to bind. Defaults to `127.0.0.1:8000`.
    #[serde(default = "default_bind")]
    pub bind: Vec<SocketAddr>,
    /// Largest accepted request body in bytes. Defaults to 10 MiB.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: u64,
    /// Whole-request budget. Defaults to 60 seconds.
    #[serde(default = "default_request_timeout", deserialize_with = "seconds")]
    pub request_timeout: Duration,
    /// In-flight connections finish within this window while shutting down. Defaults to 30
    /// seconds.
    #[serde(default = "default_shutdown_grace", deserialize_with = "seconds")]
    pub shutdown_grace: Duration,
    /// Proxy addresses whose `X-Forwarded-Host` is honored (read left and trusted, unlike raw
    /// headers).
    #[serde(default)]
    pub trusted_proxies: Vec<IpAddr>,
    /// Optional fallback site for unresolvable (or empty) hosts, so legacy or default clients
    /// still reach the shared catalog.
    #[serde(default, deserialize_with = "optional_site")]
    pub fallback_site: Option<SiteId>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            max_body_bytes: default_max_body_bytes(),
            request_timeout: default_request_timeout(),
            shutdown_grace: default_shutdown_grace(),
            trusted_proxies: Vec::new(),
            fallback_site: None,
        }
    }
}

impl ServerConfig {
    /// Checks the invariants the server loop relies on.
    ///
    /// # Errors
    ///
    /// [`ServerError::EmptyBind`] when `bind` is empty.
    pub fn validate(&self) -> Result<(), ServerError> {
        if self.bind.is_empty() {
            return Err(ServerError::EmptyBind);
        }
        Ok(())
    }
}

fn default_bind() -> Vec<SocketAddr> {
    vec![SocketAddr::new(
        IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        8000,
    )]
}

fn default_max_body_bytes() -> u64 {
    10 * 1024 * 1024
}

fn default_request_timeout() -> Duration {
    Duration::from_secs(60)
}

fn default_shutdown_grace() -> Duration {
    Duration::from_secs(30)
}

/// Resolved bounds extracted from the config, shared as middleware state.
#[derive(Debug)]
pub(crate) struct ServerLimits {
    /// Largest accepted request body in bytes.
    pub(crate) max_body_bytes: u64,
    /// Whole-request budget.
    pub(crate) request_timeout: Duration,
}

/// `u64` seconds → `Duration` (TOML has no duration type; the config uses plain integers).
fn seconds<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let secs = u64::deserialize(deserializer)?;
    Ok(Duration::from_secs(secs))
}

/// `string → SiteId` for the optional fallback site.
fn optional_site<'de, D>(deserializer: D) -> Result<Option<SiteId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw {
        None => Ok(None),
        Some(site) => SiteId::new(site)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Must;
    use crate::error::ServerError;

    fn config_with(bind: Vec<SocketAddr>) -> ServerConfig {
        ServerConfig {
            bind,
            ..ServerConfig::default()
        }
    }

    #[test]
    fn defaults_are_localhost() {
        let cfg = ServerConfig::default();
        let addr = cfg.bind[0];
        assert_eq!(addr.to_string(), "127.0.0.1:8000");
    }

    #[test]
    fn validate_rejects_empty_bind() {
        let cfg = config_with(Vec::new());
        match cfg.validate() {
            Err(ServerError::EmptyBind) => {}
            other => panic!("expected EmptyBind, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_nonempty_bind() {
        let cfg = config_with(vec![SocketAddr::new(
            IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            8000,
        )]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn parses_json_with_fallback_site_and_proxy() {
        let body = r#"{
            "bind": ["127.0.0.1:9000"],
            "max_body_bytes": 2048,
            "request_timeout": 5,
            "shutdown_grace": 1,
            "trusted_proxies": ["10.0.0.1"],
            "fallback_site": "demo"
        }"#;
        let cfg: ServerConfig = serde_json::from_str(body).must();
        assert_eq!(cfg.max_body_bytes, 2048);
        assert_eq!(cfg.request_timeout, Duration::from_secs(5));
        assert_eq!(cfg.shutdown_grace, Duration::from_secs(1));
        assert_eq!(cfg.trusted_proxies.len(), 1);
        assert!(cfg.fallback_site.is_some());
    }

    #[test]
    fn defaults_apply_when_fields_are_missing() {
        let cfg: ServerConfig = serde_json::from_str("{}").must();
        assert_eq!(cfg.max_body_bytes, 10 * 1024 * 1024);
        assert_eq!(cfg.request_timeout, Duration::from_secs(60));
        assert_eq!(cfg.shutdown_grace, Duration::from_secs(30));
        assert!(cfg.fallback_site.is_none());
        assert!(cfg.trusted_proxies.is_empty());
    }
}
