//! LAN discovery for one site (WP-PRD-06.T2), over `spot-discovery` (WP-PRD-06.T1).
//!
//! Optional, off by default (`[services.discovery] enabled = false`): desktop packages turn it
//! on with an installer property; a server a company runs for itself simply leaves it off, and
//! it then registers no service at all, so readiness does not show a default site as degraded.
//!
//! Enabled, it advertises the site under `_spotledger._tcp.local.` with the **minimal record**
//! `spot_discovery::record::Advertisement` encodes: the administrator's chosen instance name,
//! the port, the discovery protocol version and, when TLS is configured, the key fingerprint
//! prefix. There is no field for a company name, a site list or a user count, and
//! `Advertisement::to_txt` cannot be made to carry one (see `tests/discovery.rs`). A failure to
//! bind multicast degrades the service (ADR-068 A1): it never crashes the site and never retries
//! into a loop. The record is withdrawn (`AdvertiserHandle::stop`) when the service's `shutdown`
//! runs, which `Services::shutdown` calls before the process exits.
//!
//! TLS on the advertised endpoint is a separate concern this module does not implement: it only
//! says, in the record, what the administrator configured (`tls` and `fingerprint`). Off is the
//! default and is exactly what "TLS optional on LAN" means; the trust (pairing, TOFU, refusing a
//! changed fingerprint) lives client-side in `spot discover` (`src/commands/discover.rs`).

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use spot_discovery::record::{
    AdvertisedSiteNames, Advertisement, AdvertiserConfig, DEFAULT_APP_PORT, Fingerprint,
    PROTOCOL_VERSION, ServerId, TlsMode,
};
use spot_discovery::{Advertiser, AdvertiserHandle, DiscoveryError};
use tokio::sync::watch;

use crate::services::{CheckFuture, Idle, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the framework's own services.
pub const ORDER: u32 = 1040;

/// `[services.discovery]`. Unknown keys are refused.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether this server advertises itself on the LAN. Off by default: a desktop package
    /// turns it on through an installer property; a server with no reason to be found on a
    /// LAN leaves it off.
    pub enabled: bool,
    /// The service instance name an administrator chose for this server (RFC 6763 label: 1-63
    /// ASCII letters, digits or hyphens). Required when `enabled`. This, never the site name
    /// or any other identifying text, is what a client remembers.
    pub instance_name: Option<String>,
    /// The application port to advertise. Defaults to the discovery crate's documented
    /// default application port when absent.
    pub port: Option<u16>,
    /// `"off"` (the default), `"self-signed"` or `"provided"`.
    #[serde(default)]
    pub tls: Option<String>,
    /// The server's TLS key fingerprint (32 lower-case hex characters), required when `tls` is
    /// not `"off"`. Never a key, a certificate or any other secret: only its fingerprint.
    pub fingerprint: Option<String>,
    /// Explicit interface addresses to advertise on, instead of the private-range default
    /// filter `spot_discovery::record::is_advertisable_address` applies.
    #[serde(default)]
    pub interfaces: Vec<String>,
}

/// Builds the record and config `register` advertises, validating every setting named by the
/// brief. Pure: no I/O, no network, so it is unit-tested directly.
///
/// # Errors
///
/// A missing or invalid `instance_name`, an invalid `tls` value, a missing or malformed
/// `fingerprint` when `tls` is not off, or an interface address that does not parse.
pub fn build_config(settings: &Settings) -> Result<AdvertiserConfig> {
    let name = settings.instance_name.as_deref().unwrap_or_default();
    let service_instance = ServerId::new(name).map_err(|e| {
        Error(format!(
            "services.discovery.instance_name: {}",
            discovery_message(&e)
        ))
    })?;
    let tls = match settings.tls.as_deref() {
        None | Some("off") => TlsMode::Off,
        Some("self-signed") => TlsMode::SelfSigned,
        Some("provided") => TlsMode::Provided,
        Some(other) => {
            return Err(Error(format!(
                "services.discovery.tls: `{other}` is not off, self-signed or provided"
            )));
        }
    };
    let fingerprint = match (tls, settings.fingerprint.as_deref()) {
        (TlsMode::Off, None) => None,
        (TlsMode::Off, Some(_)) => {
            return Err(Error(
                "services.discovery.fingerprint is set while services.discovery.tls is off"
                    .to_owned(),
            ));
        }
        (_, None) => {
            return Err(Error(
                "services.discovery.fingerprint is required when services.discovery.tls is not off"
                    .to_owned(),
            ));
        }
        (_, Some(hex)) => Some(Fingerprint(hex.to_ascii_lowercase())),
    };
    let mut interfaces = Vec::with_capacity(settings.interfaces.len());
    for text in &settings.interfaces {
        let addr: IpAddr = text.parse().map_err(|_| {
            Error(format!(
                "services.discovery.interfaces: `{text}` is not an IP address"
            ))
        })?;
        interfaces.push(addr);
    }
    let advertisement = Advertisement {
        service_instance,
        port: settings.port.unwrap_or(DEFAULT_APP_PORT),
        protocol_version: PROTOCOL_VERSION,
        tls,
        server_key_fingerprint: fingerprint,
        // The product version is deliberately not advertised (it helps an attacker pick an
        // exploit and no client needs it). `spot-discovery` always emits the `ver` key, so it is
        // advertised empty; dropping the key is a follow-up in that crate.
        product_version: String::new(),
        // Only `AdvertiserConfig::reveal_site_names` can turn these on, and this module never
        // sets it: the brief forbids a company name, a site list or a user count by default,
        // and nothing here asks the administrator for one.
        site_names: AdvertisedSiteNames::default(),
    };
    // `to_txt` is the authority on what the wire record contains; validating it here turns a
    // bad setting into a start-up error naming the table, not a silently degraded service.
    advertisement
        .to_txt()
        .map_err(|e| Error(format!("services.discovery: {}", discovery_message(&e))))?;
    Ok(AdvertiserConfig {
        advertisement,
        reveal_site_names: false,
        interfaces,
    })
}

/// A `DiscoveryError`'s message, which never carries a secret (fingerprints and instance names
/// are not secrets; the type never wraps key material).
fn discovery_message(e: &DiscoveryError) -> String {
    e.to_string()
}

/// The hosted service: holds the live advertisement (or `None` when multicast could not be
/// confirmed) and withdraws it on shutdown.
pub struct Discovery {
    handle: Mutex<Option<AdvertiserHandle>>,
}

impl Discovery {
    /// Starts advertising `config`. Per ADR-068 A1, a multicast failure never fails this call:
    /// the handle is kept either way (its own `status()` reports `Degraded`), so `shutdown`
    /// always has something to withdraw if the daemon recovers the record later is moot — the
    /// handle's drop/`stop` is always safe to call.
    ///
    /// # Errors
    ///
    /// Only [`DiscoveryError::RecordInvalid`] (a record that cannot be encoded at all, which
    /// [`build_config`] already validates) or a thread start failure.
    pub fn start(config: AdvertiserConfig) -> std::result::Result<Arc<Self>, DiscoveryError> {
        let handle = Advertiser::start(config)?;
        Ok(Arc::new(Self {
            handle: Mutex::new(Some(handle)),
        }))
    }

    /// The current advertiser status, for diagnostics.
    #[must_use]
    pub fn status(&self) -> Option<spot_discovery::AdvertiserStatus> {
        self.handle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(spot_discovery::AdvertiserHandle::status)
    }
}

impl Service for Discovery {
    fn name(&self) -> &'static str {
        "discovery"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            match self.status() {
                // Degraded is reported, not refused: ADR-068 A1 says a multicast failure never
                // stops the site. The readiness page shows the reason through the degradation
                // text `register` attaches below (see `Idle` fallback) is not reached here, so
                // state it plainly in the check's own message.
                Some(status) if status.last_error.is_some() => Err(format!(
                    "advertising is degraded: {}",
                    status
                        .last_error
                        .map(|e| e.to_string())
                        .unwrap_or_default()
                )),
                Some(_) => Ok(()),
                None => Err("the advertiser was withdrawn".to_owned()),
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            // Stop advertising on shutdown: the goodbye leaves before the process exits.
            if let Some(handle) = self.handle.lock().unwrap_or_else(|p| p.into_inner()).take() {
                handle.stop();
            }
            Ok(())
        })
    }
}

/// Registers the discovery service.
///
/// # Errors
///
/// A setting that does not decode, or (when enabled) one `build_config` refuses.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("discovery")?;
    if !ctx.assembling() {
        // A reload keeps the running advertiser (or the disabled notice): nothing to rebuild.
        return Ok(());
    }
    if !settings.enabled {
        // Off registers nothing, so a default site is not shown degraded on readiness.
        return Ok(());
    }
    let config = build_config(&settings)?;
    match Discovery::start(config) {
        Ok(service) => ctx.add(service),
        Err(e) => {
            // Only an encoding failure reaches here (build_config already validated the
            // record), or a thread start failure: both are environment/config problems, not a
            // multicast failure (which `Advertiser::start` itself absorbs into a degraded
            // handle), so they are reported as a degraded optional service, not a refused site.
            let reason = discovery_message(&e);
            ctx.add(Idle::new("discovery", Level::Optional, move || -> CheckFuture<'static> {
                let reason = reason.clone();
                Box::pin(async move { Err(reason) })
            }));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            enabled: true,
            instance_name: Some("shop-floor-1".to_owned()),
            port: Some(8443),
            tls: None,
            fingerprint: None,
            interfaces: Vec::new(),
        }
    }

    /// Every allowed field, and only them: no company name, no site list, no user count, ever.
    const ALLOWED_FIELDS: [&str; 4] = ["instance", "port", "pv", "tls"];

    /// `spot-discovery` always emits the `ver` key; this host must advertise it empty.
    fn without_empty_ver(txt: Vec<(String, String)>) -> Vec<(String, String)> {
        for (k, v) in &txt {
            if k == "ver" {
                assert!(v.is_empty(), "the product version must not be advertised");
            }
        }
        txt.into_iter().filter(|(k, _)| k != "ver").collect()
    }

    #[test]
    fn the_default_record_carries_only_the_allowed_fields() {
        let config = build_config(&settings()).unwrap_or_else(|e| panic!("{e}"));
        let txt = config
            .effective_advertisement()
            .to_txt()
            .unwrap_or_else(|e| panic!("{e:?}"));
        let txt = without_empty_ver(txt);
        for (key, _) in &txt {
            assert!(
                ALLOWED_FIELDS.contains(&key.as_str()),
                "field `{key}` is not in the allowed list {ALLOWED_FIELDS:?}"
            );
        }
        let keys: Vec<&str> = txt.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ALLOWED_FIELDS, "the default record's exact field list");
        assert_eq!(
            config.effective_advertisement().site_names,
            AdvertisedSiteNames::default(),
            "no revealed name ever leaves this module"
        );
    }

    #[test]
    fn a_tls_instance_also_carries_only_the_allowed_fields_plus_the_fingerprint() {
        let mut s = settings();
        s.tls = Some("self-signed".to_owned());
        s.fingerprint = Some("ab".repeat(16));
        let config = build_config(&s).unwrap_or_else(|e| panic!("{e}"));
        let txt = config
            .effective_advertisement()
            .to_txt()
            .unwrap_or_else(|e| panic!("{e:?}"));
        let txt = without_empty_ver(txt);
        let mut allowed = ALLOWED_FIELDS.to_vec();
        allowed.push("fp");
        for (key, _) in &txt {
            assert!(
                allowed.contains(&key.as_str()),
                "field `{key}` is not in the allowed list {allowed:?}"
            );
            for forbidden in ["tenant", "company", "user", "db", "database", "site"] {
                assert_ne!(key.to_lowercase(), forbidden);
            }
        }
    }

    #[test]
    fn a_missing_instance_name_is_a_start_up_error_naming_the_setting() {
        let mut s = settings();
        s.instance_name = None;
        let err = build_config(&s).unwrap_err();
        assert!(err.0.contains("services.discovery.instance_name"), "{err}");
    }

    #[test]
    fn tls_without_a_fingerprint_and_a_fingerprint_without_tls_are_both_refused() {
        let mut s = settings();
        s.tls = Some("self-signed".to_owned());
        let err = build_config(&s).unwrap_err();
        assert!(err.0.contains("fingerprint"), "{err}");

        let mut s = settings();
        s.fingerprint = Some("ab".repeat(16));
        let err = build_config(&s).unwrap_err();
        assert!(err.0.contains("fingerprint"), "{err}");
    }

    #[test]
    fn an_unknown_tls_word_is_refused() {
        let mut s = settings();
        s.tls = Some("mutual".to_owned());
        let err = build_config(&s).unwrap_err();
        assert!(err.0.contains("services.discovery.tls"), "{err}");
    }

    #[test]
    fn disabled_is_the_default_and_disabling_never_touches_the_network() {
        let s = Settings::default();
        assert!(!s.enabled);
        // `build_config` is never called for a disabled site: `register` returns before it.
    }

    #[test]
    fn the_setting_refuses_unknown_keys() {
        let ok: std::result::Result<Settings, _> = toml::from_str("enabled = false");
        assert!(!ok.unwrap().enabled);
        let bad: std::result::Result<Settings, _> = toml::from_str("enabledd = true");
        assert!(bad.is_err());
    }

    #[test]
    fn an_interface_that_does_not_parse_is_refused() {
        let mut s = settings();
        s.interfaces = vec!["not-an-ip".to_owned()];
        let err = build_config(&s).unwrap_err();
        assert!(err.0.contains("services.discovery.interfaces"), "{err}");
    }
}
