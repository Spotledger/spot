//! The licence client of a tenant host (WP-CTL-04.T3, ADR-061): load and verify the signed licence
//! **offline**, keep the clock guard, renew from the control plane when it is reachable, and
//! answer "may this happen?" through the [`LicenceGate`] port.
//!
//! `spot-license` is the licence, its state machine and its clock guard; WP-CTL-04.T2 issues and
//! serves licences from the control plane. This module is the tenant-host side: files, time,
//! ports and the renewal task.
//!
//! # Level and failure
//!
//! The service is **optional** unless `[services.licence] required = true`. Optional (the default:
//! developer sites, open-source installs): no licence file is the explicit `Unlicensed` standing and
//! everything is allowed; a licence file that fails verification (tampered, unknown key, another
//! tenant) is remembered, everything stays allowed, and readiness reports the service degraded.
//! Required: a licence that fails verification refuses the site's start, naming the stable code; a
//! missing file starts the site but [`LicenceGate::decide`] allows only `Renew` until one is
//! installed (`spot licence install`). A `Restricted` or `Expired` licence never stops the site:
//! it is the gate that says "read-only" or "nothing", the site keeps answering.
//!
//! # Files
//!
//! Defaults live in `<root>/licence/<site>/`: `licence.txt` (the signed text form),
//! `clock.state` (the clock guard's observation) and `clock.state.attempt` (the last renewal
//! attempt and its result code). All three are written atomically and checked for mode (not
//! writable by group or others); see [`files`].
//!
//! # Time
//!
//! Every instant comes from the site's injected clock; this module never reads the system clock.
//! A gate query evaluates in memory (no I/O); the service's task writes the clock-guard observation
//! once a minute when it moved.
//!
//! # Ports provided
//!
//! - [`LicenceGate`] (`dyn`): `decide(Action)`, `standing()`, `entitlements()`.
//! - [`LicenceStateSource`] (`dyn`): the code of the heartbeat's `licence_state` signal
//!   (WP-CTL-03.T3).
//!
//! # Ports taken
//!
//! With `control_plane` set the host must provide [`LicencePorts`] (the mutual-TLS fetcher and the
//! egress allow-list); without them the site refuses to start. Without `control_plane` the site is
//! fully offline and makes no network call.

#[path = "licence/files.rs"]
pub mod files;
#[path = "licence/host.rs"]
pub mod host;
#[path = "licence/renew.rs"]
pub mod renew;
#[path = "licence/tls.rs"]
pub mod tls;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use spot_core::Clock;
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

pub use self::host::{
    HostConfig, HostError, Installed, LicenceGate, LicenceHost, LicenceStateSource, Standing,
    state_code_of, status_name,
};
pub use self::renew::{LicenceFetcher, RenewReport, renew_once};

/// Start order: after storage and the platform's own services, before the modules that take the
/// gate (1000 and above register in name order inside a tie).
pub const ORDER: u32 = 1003;

/// Stable codes of this module (catalogue `licence_host.toml`). The codes of `spot-license`
/// (`licence.*`) pass through unchanged where a licence itself is refused.
pub mod code {
    /// No licence is installed and the site requires one.
    pub const UNLICENSED: &str = "licence_host.unlicensed";
    /// The licence names another tenant.
    pub const TENANT_MISMATCH: &str = "licence_host.tenant_mismatch";
    /// The control plane reports the licence revoked.
    pub const REVOKED: &str = "licence_host.revoked";
    /// A licence, key or state file is unreadable, malformed or writable by others.
    pub const FILE_UNSAFE: &str = "licence_host.file_unsafe";
    /// A file could not be read.
    pub const READ_FAILED: &str = "licence_host.read_failed";
    /// A file could not be written.
    pub const WRITE_FAILED: &str = "licence_host.write_failed";
    /// The trusted issuer keys are missing or malformed.
    pub const KEYS_INVALID: &str = "licence_host.keys_invalid";
    /// A setting is out of range.
    pub const INVALID_SETTING: &str = "licence_host.invalid_setting";
    /// The control plane is not on the egress allow-list.
    pub const EGRESS_NOT_ALLOWED: &str = "licence_host.egress_not_allowed";
    /// `control_plane` is set but the host provided no licence ports.
    pub const NO_PORTS: &str = "licence_host.no_ports";
    /// The control plane could not be reached.
    pub const FETCH_UNREACHABLE: &str = "licence_host.fetch_unreachable";
    /// The control plane has no licence for this tenant.
    pub const FETCH_NONE: &str = "licence_host.fetch_none";
    /// The control plane's answer was not the documented shape.
    pub const FETCH_MALFORMED: &str = "licence_host.fetch_malformed";
}

/// `[services.licence]`. Unknown keys are refused.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether a site without a valid licence is refused (see the module docs).
    pub required: bool,
    /// The tenant id the licence must name; empty means the site's name.
    pub tenant: String,
    /// The signed licence file; empty means `<root>/licence/<site>/licence.txt`.
    pub licence_file: String,
    /// The trusted issuer keys (see [`files::parse_trusted_keys`]). Required when `required`.
    pub trusted_keys_file: String,
    /// The control plane's host listener, `https://host[:port]`. Empty: fully offline.
    pub control_plane: String,
    /// This host's client certificate (PEM), used by the host's mutual-TLS sender.
    pub host_cert: String,
    /// The client key (PEM), mode 0600.
    pub host_key: String,
    /// The CA that signed the control plane's certificate (PEM).
    pub ca_file: String,
    /// Hours between renewal attempts. 0 means the default, 6.
    pub renew_check_hours: u64,
    /// The clock-guard observation file; empty means `<root>/licence/<site>/clock.state`.
    pub state_file: String,
}

/// The paths of one site's licence files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The licence.
    pub licence_file: PathBuf,
    /// The clock-guard observation.
    pub state_file: PathBuf,
}

fn invalid(name: &str, rule: &str) -> HostError {
    HostError::new(
        code::INVALID_SETTING,
        format!("services.licence.{name} {rule}"),
    )
}

impl Settings {
    /// The hours between renewal attempts.
    #[must_use]
    pub fn renew_hours(&self) -> u64 {
        if self.renew_check_hours == 0 {
            6
        } else {
            self.renew_check_hours
        }
    }

    /// Checks the ranges.
    ///
    /// # Errors
    /// A [`HostError`] naming the setting, never a value.
    pub fn validate(&self) -> std::result::Result<(), HostError> {
        if self.renew_hours() > 24 * 30 {
            return Err(invalid("renew_check_hours", "must be at most 720"));
        }
        if self.required && self.trusted_keys_file.is_empty() {
            return Err(invalid(
                "trusted_keys_file",
                "is required when required = true",
            ));
        }
        if !self.control_plane.is_empty() {
            self.control_plane_endpoint()?;
            if self.host_cert.is_empty() || self.host_key.is_empty() || self.ca_file.is_empty() {
                return Err(invalid(
                    "host_cert",
                    "host_key and ca_file are required with control_plane",
                ));
            }
        }
        Ok(())
    }

    /// The control plane's `(host, port)`; the port defaults to 443.
    ///
    /// # Errors
    /// A [`HostError`] when `control_plane` is not an `https://host[:port]` URL.
    pub fn control_plane_endpoint(&self) -> std::result::Result<(String, u16), HostError> {
        let bad = || invalid("control_plane", "must be an https://host[:port] URL");
        let rest = self
            .control_plane
            .strip_prefix("https://")
            .ok_or_else(bad)?;
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() || rest.contains(['/', '@', '?']) {
            return Err(bad());
        }
        match rest.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && !host.contains(':') => {
                Ok((host.to_owned(), port.parse().map_err(|_| bad())?))
            }
            Some(_) => Err(bad()),
            None => Ok((rest.to_owned(), 443)),
        }
    }

    /// The effective file paths. `root` is the host's configuration root (`None` in a context
    /// without one: then both files must be named).
    ///
    /// # Errors
    /// A [`HostError`] when a path is empty and there is no root to default under.
    pub fn resolve(
        &self,
        root: Option<&Path>,
        site: &str,
    ) -> std::result::Result<Resolved, HostError> {
        let default = |name: &str, file: &str| {
            root.map(|r| r.join("licence").join(site).join(file))
                .ok_or_else(|| {
                    invalid(name, "must be set (there is no host root to default under)")
                })
        };
        Ok(Resolved {
            licence_file: if self.licence_file.is_empty() {
                default("licence_file", "licence.txt")?
            } else {
                PathBuf::from(&self.licence_file)
            },
            state_file: if self.state_file.is_empty() {
                default("state_file", "clock.state")?
            } else {
                PathBuf::from(&self.state_file)
            },
        })
    }

    /// Loads the licence host these settings describe.
    ///
    /// # Errors
    /// A [`HostError`]: a bad setting or key file, unsafe files, or (when `required`) a licence
    /// that fails verification.
    pub fn load_host(
        &self,
        root: Option<&Path>,
        site: &str,
        clock: Arc<dyn Clock>,
    ) -> std::result::Result<LicenceHost, HostError> {
        self.validate()?;
        let paths = self.resolve(root, site)?;
        let trusted = if self.trusted_keys_file.is_empty() {
            spot_license::TrustedKeys::empty()
        } else {
            let text = files::read_optional(Path::new(&self.trusted_keys_file))
                .map_err(|m| HostError::new(code::FILE_UNSAFE, m))?
                .ok_or_else(|| {
                    HostError::new(code::KEYS_INVALID, "the trusted keys file does not exist")
                })?;
            files::parse_trusted_keys(&text).map_err(|m| HostError::new(code::KEYS_INVALID, m))?
        };
        let tenant = if self.tenant.is_empty() {
            site.to_owned()
        } else {
            self.tenant.clone()
        };
        let config = HostConfig::new(self.required, tenant, paths.licence_file, paths.state_file);
        LicenceHost::load(config, trusted, clock)
    }
}

/// Whether the site's egress policy lets the control plane through. The host implements it over
/// its guard's tenant policy.
pub trait LicenceEgress: Send + Sync {
    /// Whether `host:port` is allowed for the licence fetch.
    fn allows(&self, host: &str, port: u16) -> bool;
}

/// What the renewal needs from the rest of the host when `control_plane` is set. Provide one with
/// `SiteCtx::provide::<LicencePorts>`; with `control_plane` set and no provider the site refuses to
/// start.
pub struct LicencePorts {
    /// The mutual-TLS fetcher (usually a [`renew::EgressFetcher`]).
    pub fetcher: Arc<dyn LicenceFetcher>,
    /// The egress allow-list, checked once at start.
    pub egress: Arc<dyn LicenceEgress>,
}

/// The service: the periodic flush and renewal task.
pub struct LicenceService {
    host: Arc<LicenceHost>,
    settings: Settings,
    fetcher: Option<Arc<dyn LicenceFetcher>>,
    clock: Arc<dyn Clock>,
}

/// How often the task wakes to persist the clock-guard observation and look at the renewal timer.
const TICK: Duration = Duration::from_secs(60);

impl LicenceService {
    /// A service over `host`; `fetcher` is `Some` when `control_plane` is set.
    #[must_use]
    pub fn new(
        host: Arc<LicenceHost>,
        settings: Settings,
        fetcher: Option<Arc<dyn LicenceFetcher>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            host,
            settings,
            fetcher,
            clock,
        }
    }

    /// One wake: persist the observation, and renew when the standing wants it and the last
    /// attempt is older than `renew_check_hours`. Never panics; failures are recorded attempts.
    pub async fn tick(&self) -> Option<RenewReport> {
        let _ = self.host.flush();
        let fetcher = self.fetcher.as_ref()?;
        if !renew::wants_renewal(LicenceGate::standing(self.host.as_ref())) {
            return None;
        }
        let now = self.clock.now_utc().unix_timestamp();
        let every =
            i64::try_from(self.settings.renew_hours().saturating_mul(3_600)).unwrap_or(i64::MAX);
        if let Some(last) = self.host.last_attempt()
            && now.saturating_sub(last.at) < every
            && last.at <= now
        {
            return None;
        }
        Some(renew_once(self.host.as_ref(), fetcher.as_ref(), false).await)
    }
}

impl Service for LicenceService {
    fn name(&self) -> &'static str {
        "licence"
    }

    fn level(&self) -> Level {
        if self.settings.required {
            Level::Required
        } else {
            Level::Optional
        }
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if let Some(error) = self.host.invalid() {
                return Err(format!(
                    "the licence file was not accepted ({})",
                    error.code
                ));
            }
            if self.fetcher.is_some() {
                check_credentials(&self.settings)?;
            }
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                self.tick().await;
                tokio::select! {
                    () = tokio::time::sleep(TICK) => {}
                    _ = stop.changed() => break,
                }
                if *stop.borrow() {
                    break;
                }
            }
            // The last observation is persisted on the way out.
            let _ = self.host.flush();
            Ok(())
        })
    }
}

/// Checks the credential files of the renewal: they exist, and the key is not readable by group or
/// others.
///
/// # Errors
/// A message naming the file; never its content.
pub fn check_credentials(settings: &Settings) -> std::result::Result<(), String> {
    for (name, path) in [
        ("host_cert", &settings.host_cert),
        ("host_key", &settings.host_key),
        ("ca_file", &settings.ca_file),
    ] {
        if std::fs::File::open(path).is_err() || !Path::new(path).is_file() {
            return Err(format!(
                "{}: services.licence.{name} ({path}) does not name a readable file",
                code::FILE_UNSAFE
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&settings.host_key)
            .map_err(|_| {
                format!(
                    "{}: services.licence.host_key ({}) cannot be read",
                    code::FILE_UNSAFE,
                    settings.host_key
                )
            })?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "{}: services.licence.host_key ({}) must be mode 0600 or stricter",
                code::FILE_UNSAFE,
                settings.host_key
            ));
        }
    }
    Ok(())
}

fn to_error(error: HostError) -> Error {
    Error(error.to_string())
}

/// Registers the licence client: loads and verifies the licence, provides the [`LicenceGate`] and
/// [`LicenceStateSource`] ports, and adds the renewal task.
///
/// # Errors
/// A setting out of range; unsafe or malformed files; (when `required`) a licence that fails
/// verification; with `control_plane` set: unsafe credentials, missing [`LicencePorts`], or a
/// control plane that is not on the egress allow-list (the message names the `host:port` entry).
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("licence")?;
    settings.validate().map_err(to_error)?;
    if !ctx.assembling() {
        return Ok(());
    }
    let root = ctx.env().ok().map(|env| env.root.to_path_buf());
    let clock = Arc::clone(ctx.clock());
    let host = Arc::new(
        settings
            .load_host(root.as_deref(), ctx.label(), Arc::clone(&clock))
            .map_err(to_error)?,
    );
    let fetcher = if settings.control_plane.is_empty() {
        None
    } else {
        check_credentials(&settings).map_err(Error)?;
        let ports = ctx.port::<LicencePorts>().ok_or_else(|| {
            Error(format!(
                "{}: services.licence.control_plane is set but the host provided no licence ports",
                code::NO_PORTS
            ))
        })?;
        let (name, port) = settings.control_plane_endpoint().map_err(to_error)?;
        if !ports.egress.allows(&name, port) {
            return Err(Error(format!(
                "{}: add {name}:{port} to the egress allow-list for the licence fetch (services.licence.control_plane)",
                code::EGRESS_NOT_ALLOWED
            )));
        }
        Some(Arc::clone(&ports.fetcher))
    };
    ctx.provide::<dyn LicenceGate>(host.clone())?;
    ctx.provide::<dyn LicenceStateSource>(host.clone())?;
    ctx.add(Arc::new(LicenceService::new(
        host, settings, fetcher, clock,
    )));
    Ok(())
}
