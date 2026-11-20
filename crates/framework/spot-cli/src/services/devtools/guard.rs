//! The refusals of the devtools module that need no running host: the settings, the development
//! token, the loopback rule and the build and site checks of `spot-devtools`.

use std::net::SocketAddr;

use serde::Deserialize;
use spot_core::SiteId;
use spot_devtools::{DebugPolicy, DevToken};

/// The most traces the ring may hold, and the fewest.
pub const CAPACITY_RANGE: std::ops::RangeInclusive<usize> = 10..=10_000;

/// The environment variable the development token is read from. It is never an argument and is
/// never written to disk or to a log.
pub const TOKEN_ENV: &str = "SPOT_DEV_TOKEN";

/// `[services.devtools]`. Unknown keys are refused. Off unless `enabled = true`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Turns the debugging and tracing tools on. Development machines only.
    pub enabled: bool,
    /// How many traces the per-site ring keeps (10 to 10,000).
    pub trace_capacity: usize,
    /// Where the trace viewer (`/dev/traces`) listens. Always loopback.
    pub trace_address: String,
    /// Where the debug adapter listens. Port 0 lets the system choose.
    pub debug_address: String,
    /// Allows a non-loopback `debug_address`. Needs the token and warns on every start.
    pub allow_remote: bool,
    /// Marks the site as production: the module then refuses to start.
    pub production: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            trace_capacity: 500,
            trace_address: "127.0.0.1:7312".to_owned(),
            debug_address: "127.0.0.1:0".to_owned(),
            allow_remote: false,
            production: false,
        }
    }
}

/// The settings once checked.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The policy: token, loopback rule, pause timeout.
    pub policy: DebugPolicy,
    /// The trace viewer's address (loopback).
    pub trace_addr: SocketAddr,
    /// The debug adapter's address.
    pub debug_addr: SocketAddr,
    /// The ring capacity.
    pub capacity: usize,
}

/// Checks everything that can be checked without a host. `token` is the value of
/// [`TOKEN_ENV`]. Every refusal is a sentence an operator can act on and never names the token.
///
/// # Errors
///
/// A sentence naming the setting or the rule.
pub fn resolve(
    settings: &Settings,
    site: &SiteId,
    binds: &[SocketAddr],
    token: Option<&str>,
) -> Result<Resolved, String> {
    if let Some(bad) = binds.iter().find(|a| !a.ip().is_loopback()) {
        return Err(format!(
            "services.devtools is for development only and needs every server.bind address to be loopback; {bad} is not"
        ));
    }
    if !CAPACITY_RANGE.contains(&settings.trace_capacity) {
        return Err("services.devtools.trace_capacity must be between 10 and 10000".to_owned());
    }
    let Some(token) = token else {
        return Err(format!(
            "services.devtools needs the development token in the {TOKEN_ENV} environment variable (at least 16 characters)"
        ));
    };
    let token = DevToken::new(token).map_err(|e| e.to_string())?;
    let policy = DebugPolicy::new(token).with_remote(settings.allow_remote);
    policy
        .check_site(site, settings.production)
        .map_err(|e| e.to_string())?;
    policy.check_build().map_err(|e| e.to_string())?;
    let trace_addr: SocketAddr = settings
        .trace_address
        .parse()
        .map_err(|_| "services.devtools.trace_address is not a host:port address".to_owned())?;
    if !trace_addr.ip().is_loopback() {
        return Err("services.devtools.trace_address must be a loopback address".to_owned());
    }
    let debug_addr: SocketAddr = settings
        .debug_address
        .parse()
        .map_err(|_| "services.devtools.debug_address is not a host:port address".to_owned())?;
    policy
        .check_address(debug_addr)
        .map_err(|e| e.to_string())?;
    Ok(Resolved {
        policy,
        trace_addr,
        debug_addr,
        capacity: settings.trace_capacity,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::field_reassign_with_default
)]
mod tests {
    use super::*;

    const TOKEN: &str = "dev-token-0123456789";

    fn site() -> SiteId {
        SiteId::new("acme").unwrap()
    }

    fn local() -> Vec<SocketAddr> {
        vec!["127.0.0.1:8080".parse().unwrap()]
    }

    #[test]
    fn a_clean_development_start_resolves() {
        let resolved = resolve(&Settings::default(), &site(), &local(), Some(TOKEN)).unwrap();
        assert_eq!(resolved.capacity, 500);
        assert!(resolved.trace_addr.ip().is_loopback());
    }

    #[test]
    fn refusals_name_the_rule_and_never_the_token() {
        let public: Vec<SocketAddr> = vec!["0.0.0.0:8080".parse().unwrap()];
        assert!(resolve(&Settings::default(), &site(), &public, Some(TOKEN)).is_err());
        let missing = resolve(&Settings::default(), &site(), &local(), None).unwrap_err();
        assert!(missing.contains(TOKEN_ENV));
        let short = resolve(&Settings::default(), &site(), &local(), Some("short")).unwrap_err();
        assert!(!short.contains("short\""), "{short}");
        let mut production = Settings::default();
        production.production = true;
        assert!(resolve(&production, &site(), &local(), Some(TOKEN)).is_err());
        let mut small = Settings::default();
        small.trace_capacity = 5;
        assert!(resolve(&small, &site(), &local(), Some(TOKEN)).is_err());
    }

    #[test]
    fn a_remote_debug_address_needs_allow_remote() {
        let mut settings = Settings::default();
        settings.debug_address = "0.0.0.0:9229".to_owned();
        assert!(resolve(&settings, &site(), &local(), Some(TOKEN)).is_err());
        settings.allow_remote = true;
        assert!(resolve(&settings, &site(), &local(), Some(TOKEN)).is_ok());
        settings.trace_address = "0.0.0.0:7312".to_owned();
        assert!(resolve(&settings, &site(), &local(), Some(TOKEN)).is_err());
    }
}
