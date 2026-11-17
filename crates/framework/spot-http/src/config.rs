//! Server configuration: addresses, bounds and limits.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use serde::Deserialize;
use spot_core::SiteId;

use crate::api::router::{
    DEFAULT_AUTH_LIMIT, DEFAULT_RATE_WINDOW, MAX_AUTH_LIMIT, MAX_RATE_WINDOW_SECS, RateLimits,
};
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
    /// Listeners with an explicit audience (ADR-083 Barrier 2), in addition to `bind` (which is
    /// exactly "staff listeners"). Empty by default: a configuration without it behaves as before.
    /// A consumer listener serves only the router given to
    /// [`ServerBuilder::consumer_routes`](crate::ServerBuilder::consumer_routes).
    #[serde(default)]
    pub listeners: Vec<ListenerConfig>,
    /// Requests per window one client address may make to a public group (the `auth` rate
    /// class: the single sign-on routes, the cookie renewal). Defaults to 20; 1 to 10,000.
    /// The limiter keys on the client address the router resolves, which is the socket peer
    /// unless that peer is in `trusted_proxies`: behind a proxy that is not listed every client
    /// shares the proxy's address and therefore this one budget.
    #[serde(default = "default_auth_rate_limit")]
    pub auth_rate_limit: u32,
    /// The window of that budget, in whole seconds (config key `auth_rate_window_seconds`).
    /// Defaults to 60; 1 to 3,600.
    #[serde(
        default = "default_auth_rate_window",
        rename = "auth_rate_window_seconds",
        deserialize_with = "seconds"
    )]
    pub auth_rate_window: Duration,
}

/// The audience group a listener serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListenerAudience {
    /// The ERP listener: routes marked staff or partner, plus everything mounted with
    /// `site_routes`.
    Staff,
    /// The consumer listener: only the consumer router and the minimal health routes.
    Consumer,
}

impl<'de> Deserialize<'de> for ListenerAudience {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Exactly the two lower-case names; the rejected text is not echoed.
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "staff" => Ok(ListenerAudience::Staff),
            "consumer" => Ok(ListenerAudience::Consumer),
            _ => Err(serde::de::Error::custom(
                "audience must be \"staff\" or \"consumer\"",
            )),
        }
    }
}

/// One listener: an address and the audience it serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfig {
    /// The address to bind.
    pub bind: SocketAddr,
    /// The audience group the listener serves (required: there is no default).
    pub audience: ListenerAudience,
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
            listeners: Vec::new(),
            auth_rate_limit: default_auth_rate_limit(),
            auth_rate_window: default_auth_rate_window(),
        }
    }
}

impl ServerConfig {
    /// Every address to bind with its audience: the `bind` list (staff) first, then `listeners`.
    pub fn all_listeners(&self) -> Vec<ListenerConfig> {
        self.bind
            .iter()
            .map(|addr| ListenerConfig {
                bind: *addr,
                audience: ListenerAudience::Staff,
            })
            .chain(self.listeners.iter().copied())
            .collect()
    }

    /// The limits of the public groups' rate class, to hand to
    /// [`ApiServices::with_rate_limits`](crate::api::router::ApiServices::with_rate_limits).
    pub fn rate_limits(&self) -> RateLimits {
        RateLimits {
            auth_limit: self.auth_rate_limit,
            auth_window: self.auth_rate_window,
        }
    }

    /// Checks the invariants the server loop relies on.
    ///
    /// # Errors
    ///
    /// [`ServerError::EmptyBind`] when there is no address at all;
    /// [`ServerError::AddressClash`] when two listeners would bind overlapping addresses (the
    /// same address, or a wildcard address and any address on the same port), whatever their
    /// audiences. Port 0 (an ephemeral port chosen at bind time) never clashes;
    /// [`ServerError::InvalidSetting`] naming the key when `auth_rate_limit` is 0 or above
    /// 10,000, or `auth_rate_window_seconds` is 0 or above 3,600.
    pub fn validate(&self) -> Result<(), ServerError> {
        if self.bind.is_empty() && self.listeners.is_empty() {
            return Err(ServerError::EmptyBind);
        }
        let all = self.all_listeners();
        for (index, first) in all.iter().enumerate() {
            for second in &all[index + 1..] {
                if addresses_overlap(first.bind, second.bind) {
                    // Only the clashing address (the later one) is named.
                    return Err(ServerError::AddressClash { addr: second.bind });
                }
            }
        }
        if self.auth_rate_limit == 0 || self.auth_rate_limit > MAX_AUTH_LIMIT {
            return Err(ServerError::InvalidSetting {
                key: "auth_rate_limit",
                reason: "must be from 1 to 10000",
            });
        }
        if self.auth_rate_window.is_zero()
            || self.auth_rate_window > Duration::from_secs(MAX_RATE_WINDOW_SECS)
            || self.auth_rate_window.subsec_nanos() != 0
        {
            return Err(ServerError::InvalidSetting {
                key: "auth_rate_window_seconds",
                reason: "must be a whole number of seconds from 1 to 3600",
            });
        }
        Ok(())
    }
}

/// Whether binding both addresses would clash: same port (not 0) and the same IP, or one of them
/// is a wildcard (`0.0.0.0` or `::`) of either family.
fn addresses_overlap(a: SocketAddr, b: SocketAddr) -> bool {
    if a.port() == 0 || b.port() == 0 || a.port() != b.port() {
        return false;
    }
    a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified()
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

fn default_auth_rate_limit() -> u32 {
    DEFAULT_AUTH_LIMIT
}

fn default_auth_rate_window() -> Duration {
    DEFAULT_RATE_WINDOW
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

    fn addr(text: &str) -> SocketAddr {
        text.parse().must()
    }

    #[test]
    fn overlap_rules() {
        assert!(addresses_overlap(addr("127.0.0.1:9"), addr("127.0.0.1:9")));
        assert!(addresses_overlap(addr("0.0.0.0:9"), addr("127.0.0.1:9")));
        assert!(addresses_overlap(addr("[::]:9"), addr("0.0.0.0:9")));
        assert!(addresses_overlap(addr("[::]:9"), addr("[::1]:9")));
        assert!(!addresses_overlap(addr("127.0.0.1:9"), addr("127.0.0.2:9")));
        assert!(!addresses_overlap(addr("0.0.0.0:9"), addr("127.0.0.1:10")));
        assert!(!addresses_overlap(addr("127.0.0.1:0"), addr("127.0.0.1:0")));
    }

    #[test]
    fn validate_refuses_clashes_across_bind_and_listeners() {
        let cfg = ServerConfig {
            bind: vec![addr("0.0.0.0:7000")],
            listeners: vec![ListenerConfig {
                bind: addr("127.0.0.1:7000"),
                audience: ListenerAudience::Consumer,
            }],
            ..ServerConfig::default()
        };
        assert!(matches!(
            cfg.validate(),
            Err(ServerError::AddressClash { .. })
        ));
    }

    #[test]
    fn listener_audience_parses_exactly() {
        let ok: ListenerConfig =
            serde_json::from_str(r#"{"bind":"127.0.0.1:1","audience":"consumer"}"#).must();
        assert_eq!(ok.audience, ListenerAudience::Consumer);
        for bad in [
            r#"{"bind":"127.0.0.1:1","audience":"Staff"}"#,
            r#"{"bind":"127.0.0.1:1","audience":"admin"}"#,
            r#"{"bind":"127.0.0.1:1"}"#,
            r#"{"bind":"127.0.0.1:1","audience":"staff","extra":1}"#,
        ] {
            assert!(
                serde_json::from_str::<ListenerConfig>(bad).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn defaults_apply_when_fields_are_missing() {
        let cfg: ServerConfig = serde_json::from_str("{}").must();
        assert_eq!(cfg.max_body_bytes, 10 * 1024 * 1024);
        assert_eq!(cfg.request_timeout, Duration::from_secs(60));
        assert_eq!(cfg.shutdown_grace, Duration::from_secs(30));
        assert!(cfg.fallback_site.is_none());
        assert!(cfg.trusted_proxies.is_empty());
        assert!(cfg.listeners.is_empty());
    }

    #[test]
    fn the_rate_limit_defaults_are_unchanged() {
        let cfg: ServerConfig = serde_json::from_str("{}").must();
        assert_eq!(cfg.auth_rate_limit, 20);
        assert_eq!(cfg.auth_rate_window, Duration::from_secs(60));
        assert_eq!(cfg.rate_limits(), RateLimits::default());
        assert!(ServerConfig::default().validate().is_ok());
    }

    #[test]
    fn explicit_rate_limit_values_parse_and_reach_the_limiter() {
        let cfg: ServerConfig =
            serde_json::from_str(r#"{"auth_rate_limit": 3, "auth_rate_window_seconds": 120}"#)
                .must();
        assert_eq!(cfg.auth_rate_limit, 3);
        assert_eq!(cfg.auth_rate_window, Duration::from_secs(120));
        assert!(cfg.validate().is_ok());
        let limits = cfg.rate_limits();
        assert_eq!(limits.auth_limit, 3);
        assert_eq!(limits.auth_window, Duration::from_secs(120));
        // The bounds themselves are allowed.
        for body in [
            r#"{"auth_rate_limit": 1, "auth_rate_window_seconds": 1}"#,
            r#"{"auth_rate_limit": 10000, "auth_rate_window_seconds": 3600}"#,
        ] {
            let cfg: ServerConfig = serde_json::from_str(body).must();
            assert!(cfg.validate().is_ok(), "{body}");
        }
    }

    #[test]
    fn rate_limit_values_out_of_bounds_fail_naming_the_key() {
        let cases = [
            (r#"{"auth_rate_limit": 0}"#, "auth_rate_limit"),
            (r#"{"auth_rate_limit": 10001}"#, "auth_rate_limit"),
            (
                r#"{"auth_rate_window_seconds": 0}"#,
                "auth_rate_window_seconds",
            ),
            (
                r#"{"auth_rate_window_seconds": 3601}"#,
                "auth_rate_window_seconds",
            ),
        ];
        for (body, key) in cases {
            let cfg: ServerConfig = serde_json::from_str(body).must();
            match cfg.validate() {
                Err(ServerError::InvalidSetting { key: named, .. }) => {
                    assert_eq!(named, key, "{body}");
                }
                other => panic!("{body}: expected InvalidSetting, got {other:?}"),
            }
        }
    }

    #[test]
    fn rate_limit_values_that_are_negative_fractional_or_overflow_do_not_deserialize() {
        for body in [
            r#"{"auth_rate_limit": -1}"#,
            r#"{"auth_rate_limit": 2.5}"#,
            r#"{"auth_rate_limit": "20"}"#,
            r#"{"auth_rate_limit": 4294967296}"#,
            r#"{"auth_rate_window_seconds": -1}"#,
            r#"{"auth_rate_window_seconds": 1.5}"#,
            r#"{"auth_rate_window_seconds": 18446744073709551616}"#,
        ] {
            assert!(
                serde_json::from_str::<ServerConfig>(body).is_err(),
                "{body}"
            );
        }
    }
}
