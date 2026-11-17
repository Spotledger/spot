//! The sign-in throttle, hosted once per site (WP-IAM-11.T23, ADR-083 task 7).
//!
//! `spot-throttle-coordinator` counts sign-in attempts in four independent buckets (source,
//! destination, account, tenant) on a coordinator, so the counters are shared by every node and
//! survive a restart. This module builds **one** `CoordinatorThrottle` per site and hands every
//! door that checks a credential the same handle:
//!
//! - the **coordinator** is the PostgreSQL one (`spot-coordinator-pg`) in the site's database
//!   whatever the profile, because an in-process coordinator would forget a lockout at restart;
//! - the **salt ring** comes from the secrets service: `DerivedSalt` over the site's key provider
//!   (`derive_key`, never a raw KEK), at `salt_version` and, during a rotation's overlap,
//!   `previous_salt_version`, so two nodes derive the same salts without storing them;
//! - the **policy** is `[services.signin_throttle]` (defaults below);
//! - the module provides two ports: [`SigninThrottle`] (the handle for SSO, SCIM, API keys, TOTP
//!   and password-change hosts, counted per [`Door`]) and `dyn SignInAdmission` (the password door,
//!   wrapped so denials and outages are audited and counted).
//!
//! # Fail closed
//!
//! A coordinator error, a salt that cannot be had or a malformed attempt never admits: the
//! password door answers with the sign-in service's `login.admission_unavailable` (503 with
//! `Retry-After`), the other doors with `server.unavailable` and `Retry-After`, a counter
//! increments, and the attempt is not counted as admitted. Level: **required**. Without the
//! throttle nobody is admitted, so a site that cannot load its salt or prepare the coordinator
//! tables refuses to start, and one whose salt can no longer be derived is reported stopped.
//!
//! # Doors
//!
//! [`DOORS`] lists every place a credential is checked. A test scans `spot-cli/src` and
//! `spot-login/src` for the calls that check one (`Authenticator::`, `verify_password`) and fails
//! if a file that makes them is not named by a door.
//!
//! # Metrics
//!
//! Counters of admitted, throttled and unavailable per door, nothing else: never an account, an
//! address or a bucket key. [`SigninThrottle::counts`] reads them.

/// The error codes this module raises (catalogue `signin_throttle_host`).
const CODE_COORDINATOR_UNAVAILABLE: &str = "signin_throttle_host.coordinator_unavailable";
const CODE_SALT_UNAVAILABLE: &str = "signin_throttle_host.salt_unavailable";
const CODE_SETTINGS_INVALID: &str = "signin_throttle_host.settings_invalid";

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use spot_coordinator::{Coordinator, RateLimit};
use spot_http::api::registry::Code;
use spot_http::api::service::ApiError;
use spot_login::{
    Admission, AdmissionError, BoxFuture, CoordinatorAdmission, SignInAdmission, SignInAttempt,
};
use spot_throttle_coordinator::{
    Attempt, CoordinatorThrottle, DerivedSalt, Kind, SourceAddr, ThrottlePolicy, Verdict, load_ring,
};

use crate::services::secrets::SecretsHost;
use crate::services::{CheckFuture, Idle, Level, SiteCtx};
use crate::{Error, Result};

/// Start order: after the secrets service (1000) and the sign-in stores (1005).
pub const ORDER: u32 = 1006;

/// One window and capacity, as it is written in `common.toml`.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitSetting {
    /// Attempts a full bucket holds.
    pub capacity: u64,
    /// Seconds over which the bucket refills.
    pub per_seconds: u64,
}

impl LimitSetting {
    fn limit(self) -> RateLimit {
        RateLimit {
            capacity: self.capacity,
            per: Duration::from_secs(self.per_seconds),
        }
    }
}

/// `[services.signin_throttle]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// One account name, per kind of attempt (a rate, not a lock).
    pub per_account: LimitSetting,
    /// One source address, or one connection when there is no address.
    pub per_source: LimitSetting,
    /// One destination (an e-mail address or a phone number a code would go to).
    pub per_destination: LimitSetting,
    /// The whole tenant: a ceiling that protects the database.
    pub per_tenant: LimitSetting,
    /// The salt version in force (versions start at 1).
    pub salt_version: u32,
    /// The previous version, while a rotation's overlap runs (one full longest window).
    pub previous_salt_version: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            per_account: LimitSetting {
                capacity: 10,
                per_seconds: 900,
            },
            per_source: LimitSetting {
                capacity: 30,
                per_seconds: 900,
            },
            per_destination: LimitSetting {
                capacity: 10,
                per_seconds: 3600,
            },
            per_tenant: LimitSetting {
                capacity: 6_000,
                per_seconds: 60,
            },
            salt_version: 1,
            previous_salt_version: None,
        }
    }
}

impl Settings {
    /// The policy these settings describe.
    ///
    /// # Errors
    ///
    /// `signin_throttle_host.settings_invalid` for a limit the throttle refuses.
    pub fn policy(&self) -> Result<ThrottlePolicy> {
        ThrottlePolicy::new(
            self.per_account.limit(),
            self.per_source.limit(),
            self.per_destination.limit(),
            self.per_tenant.limit(),
        )
        .map_err(|_| {
            Error(format!("{CODE_SETTINGS_INVALID}: [services.signin_throttle] holds a limit the throttle refuses"))
        })
    }
}

/// A place a credential is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Door {
    /// A password sign-in.
    Password,
    /// A one-time (second factor) code.
    OneTimeCode,
    /// A recovery code.
    RecoveryCode,
    /// An API key presented as a bearer.
    ApiKey,
    /// A SCIM credential.
    ScimKey,
    /// An SSO callback.
    SsoCallback,
    /// The re-authentication of a password change.
    PasswordChange,
}

impl Door {
    /// The metric label: a fixed word, never a value.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::OneTimeCode => "one_time_code",
            Self::RecoveryCode => "recovery_code",
            Self::ApiKey => "api_key",
            Self::ScimKey => "scim_key",
            Self::SsoCallback => "sso_callback",
            Self::PasswordChange => "password_change",
        }
    }

    /// The kind of attempt the door is counted as. Every door that checks a credential for an
    /// account counts as a sign-in: the account bucket is per kind, so a recovery flood cannot
    /// deny a name's sign-in, and the doors that guess a secret share the sign-in bucket.
    fn kind(self) -> Kind {
        match self {
            Self::RecoveryCode => Kind::Recovery,
            _ => Kind::SignIn,
        }
    }
}

/// Every door, and the source files that check a credential for it. A file that calls
/// `Authenticator::` or `verify_password` and is not listed here fails the scan test.
pub const DOORS: &[(Door, &[&str])] = &[
    (
        Door::Password,
        &["spot-login/src/service.rs", "spot-login/src/ports.rs"],
    ),
    (
        Door::ApiKey,
        &["spot-login/src/authenticator.rs", "spot-cli/src/login.rs"],
    ),
    (Door::PasswordChange, &["spot-login/src/service.rs"]),
    (Door::RecoveryCode, &["spot-login/src/service.rs"]),
    (Door::OneTimeCode, &["spot-login/src/service.rs"]),
    (Door::ScimKey, &["spot-cli/src/services/scim.rs"]),
    (Door::SsoCallback, &[]),
];

/// Admitted, throttled and unavailable counts of one door.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Attempts admitted.
    pub admitted: u64,
    /// Attempts refused for being over a limit.
    pub throttled: u64,
    /// Attempts refused because the throttle could not count.
    pub unavailable: u64,
}

#[derive(Default)]
struct Counters {
    admitted: AtomicU64,
    throttled: AtomicU64,
    unavailable: AtomicU64,
}

/// The one throttle handle of a site, shared by every door.
pub struct SigninThrottle {
    throttle: Arc<CoordinatorThrottle>,
    counters: BTreeMap<Door, Counters>,
}

impl SigninThrottle {
    /// A handle over `throttle`.
    #[must_use]
    pub fn new(throttle: Arc<CoordinatorThrottle>) -> Self {
        let counters = DOORS
            .iter()
            .map(|(door, _)| (*door, Counters::default()))
            .collect();
        Self { throttle, counters }
    }

    /// The throttle itself (for `CoordinatorAdmission`).
    #[must_use]
    pub fn throttle(&self) -> Arc<CoordinatorThrottle> {
        Arc::clone(&self.throttle)
    }

    fn count(&self, door: Door) -> &Counters {
        // Every door is in `DOORS`, so the map has it; a missing one would be a programming
        // error and must not panic a request, so it counts under the password door.
        self.counters
            .get(&door)
            .or_else(|| self.counters.get(&Door::Password))
            .unwrap_or_else(|| unreachable_counters())
    }

    /// Counts one attempt at `door` and says whether it may go on. The peer address is whatever
    /// the edge trusts (`RequestCtx::peer`); with none, `connection` is the source, and with
    /// neither the attempt is refused (never one shared bucket).
    ///
    /// # Errors
    ///
    /// `server.unavailable` with a `Retry-After` hint when the throttle could not count, or when
    /// the attempt is over a limit (the same answer the contract gives a rate-limited caller:
    /// `api.rate_limited`).
    pub async fn admit(
        &self,
        door: Door,
        tenant: &str,
        account: &str,
        peer: Option<&str>,
        connection: Option<&str>,
    ) -> std::result::Result<(), ApiError> {
        let source = match peer.filter(|p| !p.trim().is_empty()) {
            Some(peer) => SourceAddr::new(peer),
            None => SourceAddr {
                address: "",
                connection_id: connection,
            },
        };
        let attempt = Attempt {
            kind: door.kind(),
            account: Some(account),
            source,
            destination: None,
            tenant,
        };
        let counters = self.count(door);
        match self.throttle.admit(&attempt).await {
            Ok(Verdict::Allow) => {
                counters.admitted.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Ok(Verdict::Deny { retry_after }) => {
                counters.throttled.fetch_add(1, Ordering::Relaxed);
                Err(ApiError::from_code(Code::ApiRateLimited).detail(
                    "retry_after",
                    serde_json::Value::from(retry_after.as_secs().max(1)),
                ))
            }
            Err(_) => {
                counters.unavailable.fetch_add(1, Ordering::Relaxed);
                Err(ApiError::from_code(Code::ServerUnavailable)
                    .detail("retry_after", serde_json::Value::from(5)))
            }
        }
    }

    /// The counters of every door.
    #[must_use]
    pub fn counts(&self) -> Vec<(Door, Counts)> {
        self.counters
            .iter()
            .map(|(door, c)| {
                (
                    *door,
                    Counts {
                        admitted: c.admitted.load(Ordering::Relaxed),
                        throttled: c.throttled.load(Ordering::Relaxed),
                        unavailable: c.unavailable.load(Ordering::Relaxed),
                    },
                )
            })
            .collect()
    }
}

fn unreachable_counters() -> &'static Counters {
    static FALLBACK: std::sync::OnceLock<Counters> = std::sync::OnceLock::new();
    FALLBACK.get_or_init(Counters::default)
}

/// The password door: `CoordinatorAdmission` that counts per door.
struct PasswordAdmission {
    inner: CoordinatorAdmission,
    handle: Arc<SigninThrottle>,
}

impl SignInAdmission for PasswordAdmission {
    fn admit<'a>(
        &'a self,
        attempt: SignInAttempt<'a>,
    ) -> BoxFuture<'a, std::result::Result<Admission, AdmissionError>> {
        Box::pin(async move {
            let answer = self.inner.admit(attempt).await;
            let counters = self.handle.count(Door::Password);
            match &answer {
                Ok(Admission::Allowed) => counters.admitted.fetch_add(1, Ordering::Relaxed),
                Ok(Admission::Denied { .. }) => counters.throttled.fetch_add(1, Ordering::Relaxed),
                Err(_) => counters.unavailable.fetch_add(1, Ordering::Relaxed),
            };
            answer
        })
    }
}

/// Joins the site: prepares the coordinator tables, loads the salt ring, builds the throttle,
/// provides the handle and the password door's admission, and adds the required check.
///
/// # Errors
///
/// A setting that does not decode or a limit the throttle refuses; no secrets service; the
/// coordinator tables or the salt ring cannot be had.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("signin_throttle")?;
    let policy = settings.policy()?;
    let (site, backend) = {
        let env = ctx.env()?;
        (env.site.site.clone(), env.backend.clone())
    };
    let secrets = ctx.port::<SecretsHost>().ok_or_else(|| {
        Error(format!("{CODE_SALT_UNAVAILABLE}: [services.signin_throttle] needs [services.secrets]"))
    })?;
    let source = Arc::new(DerivedSalt::new(Arc::clone(secrets.keyring())));
    let ring = load_ring(
        source.as_ref(),
        &site,
        settings.salt_version,
        settings.previous_salt_version,
    )
    .await
    .map_err(|_| {
        Error(format!("{CODE_SALT_UNAVAILABLE}: the salt ring could not be loaded"))
    })?;

    let coordinator = spot_coordinator_pg::PgCoordinator::new(
        backend.pool(),
        spot_coordinator_pg::PgCoordinatorConfig::default(),
    );
    coordinator.prepare().await.map_err(|e| {
        Error(format!("{CODE_COORDINATOR_UNAVAILABLE}: {}", e.code))
    })?;
    let coordinator: Arc<dyn Coordinator> = Arc::new(coordinator);
    let throttle = Arc::new(CoordinatorThrottle::with_ring(coordinator, ring, policy));
    let handle = Arc::new(SigninThrottle::new(Arc::clone(&throttle)));

    if ctx.assembling() {
        let probe = Arc::clone(&source);
        let tenant = site.clone();
        let version = settings.salt_version;
        ctx.add(Idle::new(
            "signin_throttle",
            Level::Required,
            move || -> CheckFuture<'static> {
                let probe = Arc::clone(&probe);
                let tenant = tenant.clone();
                Box::pin(async move {
                    spot_throttle_coordinator::SaltSource::load(probe.as_ref(), &tenant, version)
                        .await
                        .map(|_| ())
                        .map_err(|_| {
                            "the sign-in throttle cannot derive its salt, so every sign-in is refused (fail closed)".to_owned()
                        })
                })
            },
        ));
    }

    let admission: Arc<dyn SignInAdmission> = Arc::new(PasswordAdmission {
        inner: CoordinatorAdmission::new(throttle),
        handle: Arc::clone(&handle),
    });
    ctx.provide(handle)?;
    ctx.provide(admission)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn the_defaults_are_a_valid_policy() {
        assert!(Settings::default().policy().is_ok());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn a_limit_the_throttle_refuses_is_a_named_error() {
        let mut settings = Settings::default();
        settings.per_account.capacity = 0;
        let error = settings.policy().unwrap_err().to_string();
        assert!(error.contains("signin_throttle_host.settings_invalid"));
        // A tenant ceiling slower than one source's rate is refused too.
        let mut settings = Settings::default();
        settings.per_tenant = LimitSetting {
            capacity: 1,
            per_seconds: 3600,
        };
        assert!(settings.policy().is_err());
    }

    #[test]
    fn the_settings_refuse_unknown_keys() {
        let bad: std::result::Result<Settings, _> =
            toml::from_str("per_acount = { capacity = 1, per_seconds = 1 }");
        assert!(bad.is_err());
        let ok: Settings = toml::from_str("salt_version = 2\nprevious_salt_version = 1").unwrap();
        assert_eq!((ok.salt_version, ok.previous_salt_version), (2, Some(1)));
    }

    #[test]
    fn labels_are_fixed_words_and_unique() {
        let mut labels: Vec<&str> = DOORS.iter().map(|(d, _)| d.label()).collect();
        labels.sort_unstable();
        let before = labels.len();
        labels.dedup();
        assert_eq!(before, labels.len());
        assert!(
            labels
                .iter()
                .all(|l| l.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        );
    }

    /// The door registry lint: every file that checks a credential is named by a door.
    #[test]
    fn a_file_that_checks_a_credential_is_a_listed_door() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let listed: Vec<&str> = DOORS
            .iter()
            .flat_map(|(_, files)| files.iter().copied())
            .collect();
        let unlisted = scan(&crates, &listed, &["spot-cli/src", "spot-login/src"]);
        assert!(
            unlisted.is_empty(),
            "files that check a credential but are in no door: {unlisted:?}"
        );
    }

    #[test]
    fn the_lint_fails_on_a_seeded_unlisted_door() {
        let dir = std::env::temp_dir().join(format!("spot-door-lint-{}", std::process::id()));
        let src = dir.join("spot-cli/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("rogue.rs"), "fn f() { verify_password(a, b); }").unwrap();
        let found = scan(&dir, &[], &["spot-cli/src"]);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, vec!["spot-cli/src/rogue.rs".to_owned()]);
    }

    /// The files under `roots` (relative to `base`) that call a credential check and are not in
    /// `listed` (paths relative to `base`).
    fn scan(base: &Path, listed: &[&str], roots: &[&str]) -> Vec<String> {
        let mut found = Vec::new();
        for root in roots {
            walk(&base.join(root), &mut |path: PathBuf| {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    return;
                };
                let checks = text
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("//"))
                    .any(|l| l.contains("Authenticator::") || l.contains("verify_password("));
                if !checks {
                    return;
                }
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                // This file names the calls it scans for.
                if rel.ends_with("services/signin_throttle.rs") {
                    return;
                }
                if !listed.iter().any(|l| rel.ends_with(l)) {
                    found.push(rel);
                }
            });
        }
        found.sort();
        found
    }

    fn walk(dir: &Path, visit: &mut dyn FnMut(PathBuf)) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, visit);
            } else if path.extension().is_some_and(|e| e == "rs") {
                visit(path);
            }
        }
    }
}
