//! The licence state of one site (WP-CTL-04.T3): the verified licence, the clock guard over it,
//! and the answers the gate gives. Everything here is synchronous and clock-injected; the files
//! are touched only by [`LicenceHost::load`], [`LicenceHost::flush`] and the install/renew paths,
//! never by [`LicenceHost::decide`], so asking is cheap and can sit on a hot path.
//!
//! # What "unlicensed" means
//!
//! A site with no licence file is **unlicensed**. That is an explicit standing, not an error and
//! not a panic:
//!
//! | `required` | no file, or a licence that failed verification | `decide` |
//! | --- | --- | --- |
//! | `false` (default) | open-source and developer installs | allow everything |
//! | `true` | commercial installs | allow `Renew` only; deny the rest with `licence_host.unlicensed` |
//!
//! With a verified licence `decide` is exactly `spot_license::Status::allows`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_core::Clock;
use spot_license::{
    Action, ClockGuard, DEFAULT_CLOCK_TOLERANCE, Decision, Entitlements, Licence, SignedLicence,
    Status, Store, TrustedKeys, apply_renewal, verify,
};

use super::code;
use super::files::{Attempt, FileStore, atomic_write, read_attempt, read_optional, write_attempt};

/// Where a site stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// No licence file.
    Unlicensed,
    /// A file is there but failed verification (tampered, foreign tenant, unknown key).
    Invalid,
    /// A verified licence in this status.
    Licensed(Status),
}

/// The gate other modules call before they create a document, export, install an app or renew.
/// Provided with `SiteCtx::provide::<dyn LicenceGate>`. No method does I/O.
pub trait LicenceGate: Send + Sync {
    /// Whether `action` may go ahead now.
    fn decide(&self, action: Action) -> Decision;
    /// Where the site stands now.
    fn standing(&self) -> Standing;
    /// The entitlements of the verified licence; `None` when there is none.
    fn entitlements(&self) -> Option<Entitlements>;
}

/// The `licence_state` signal source of the heartbeat emitter (WP-CTL-03.T3): 0 active, 1 grace,
/// 2 expired, matching `spot_fleet_health::LicenceState`.
///
/// | standing | code |
/// | --- | --- |
/// | `Active`, `RenewalDue` | 0 |
/// | `Grace`, `ClockSuspect` | 1 |
/// | `Restricted`, `Expired`, `NotYetValid`, `Invalid` | 2 |
/// | `Unlicensed` | none: the signal is left out, no value is invented |
pub trait LicenceStateSource: Send + Sync {
    /// The code, or `None` when the site has no licence.
    fn state_code(&self) -> Option<i64>;
}

/// The code of `standing` (see [`LicenceStateSource`]).
#[must_use]
pub fn state_code_of(standing: Standing) -> Option<i64> {
    match standing {
        Standing::Unlicensed => None,
        Standing::Invalid => Some(2),
        Standing::Licensed(status) => Some(match status {
            Status::Active | Status::RenewalDue => 0,
            Status::Grace { .. } | Status::ClockSuspect { .. } => 1,
            Status::Restricted | Status::Expired | Status::NotYetValid => 2,
        }),
    }
}

/// A stable name of a status for `spot licence status` and the docs.
#[must_use]
pub fn status_name(standing: Standing) -> &'static str {
    match standing {
        Standing::Unlicensed => "unlicensed",
        Standing::Invalid => "invalid",
        Standing::Licensed(Status::NotYetValid) => "not_yet_valid",
        Standing::Licensed(Status::Active) => "active",
        Standing::Licensed(Status::RenewalDue) => "renewal_due",
        Standing::Licensed(Status::Grace { .. }) => "grace",
        Standing::Licensed(Status::Restricted) => "restricted",
        Standing::Licensed(Status::Expired) => "expired",
        Standing::Licensed(Status::ClockSuspect { .. }) => "clock_suspect",
    }
}

/// An error of the licence client: a stable code and a sentence that quotes no licence content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostError {
    /// The stable code (`licence_host.*`, or a `licence.*` code of `spot-license`).
    pub code: String,
    /// The sentence.
    pub message: String,
}

impl HostError {
    /// An error with `code` and `message`.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HostError {}

/// What the host needs to know about its files and tenant.
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// Whether an install without a valid licence is refused.
    pub required: bool,
    /// The tenant id this site's licence must name (when the licence names a tenant).
    pub tenant: String,
    /// The licence file.
    pub licence_file: PathBuf,
    /// The clock-guard observation file.
    pub state_file: PathBuf,
    /// The last-attempt file.
    pub attempt_file: PathBuf,
    /// The rewind tolerance of the clock guard.
    pub tolerance: std::time::Duration,
}

impl HostConfig {
    /// A configuration over the given files with the crate's default tolerance.
    #[must_use]
    pub fn new(
        required: bool,
        tenant: impl Into<String>,
        licence_file: PathBuf,
        state_file: PathBuf,
    ) -> Self {
        let attempt_file = {
            let mut name = state_file
                .file_name()
                .map(std::ffi::OsString::from)
                .unwrap_or_default();
            name.push(".attempt");
            state_file.with_file_name(name)
        };
        Self {
            required,
            tenant: tenant.into(),
            licence_file,
            state_file,
            attempt_file,
            tolerance: DEFAULT_CLOCK_TOLERANCE,
        }
    }
}

struct Inner {
    licence: Option<Licence>,
    invalid: Option<HostError>,
    guard: ClockGuard,
    dirty: bool,
}

/// What an install or a renewal did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    /// The serial now in force.
    pub serial: u64,
    /// The licence id.
    pub licence_id: String,
    /// The standing right after.
    pub standing: Standing,
}

/// The licence state of one site.
pub struct LicenceHost {
    config: HostConfig,
    trusted: TrustedKeys,
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
}

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|p| p.into_inner())
}

/// Checks that `licence` is for this tenant: a licence that names a tenant must name `tenant`.
fn check_tenant(licence: &Licence, tenant: &str) -> Result<(), HostError> {
    match &licence.tenant {
        Some(named) if named != tenant => Err(HostError::new(
            code::TENANT_MISMATCH,
            "the licence is for another tenant",
        )),
        _ => Ok(()),
    }
}

fn verify_text(
    text: &str,
    trusted: &TrustedKeys,
    tenant: &str,
) -> Result<(SignedLicence, Licence), HostError> {
    let signed = SignedLicence::from_text(text.trim())
        .map_err(|e| HostError::new(e.code(), "the licence text is not a licence"))?;
    let licence = verify(&signed, trusted)
        .map_err(|e| HostError::new(e.code(), "the licence did not verify"))?;
    check_tenant(&licence, tenant)?;
    Ok((signed, licence))
}

impl LicenceHost {
    /// Loads the files. A missing licence file is `Unlicensed`. A licence that fails verification
    /// refuses the load when `required`, and otherwise is remembered as `Invalid` (readiness
    /// reports it degraded).
    ///
    /// # Errors
    /// A [`HostError`]: unreadable or unsafe files, a malformed state file, or (when `required`)
    /// an invalid licence.
    pub fn load(
        config: HostConfig,
        trusted: TrustedKeys,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, HostError> {
        let observation = FileStore::new(config.state_file.clone())
            .load()
            .map_err(|m| HostError::new(code::READ_FAILED, m))?;
        let guard = ClockGuard::new(observation, config.tolerance);
        let text = read_optional(&config.licence_file)
            .map_err(|m| HostError::new(code::FILE_UNSAFE, m))?;
        let (licence, invalid) = match text {
            None => (None, None),
            Some(text) => match verify_text(&text, &trusted, &config.tenant) {
                Ok((_, licence)) => (Some(licence), None),
                Err(error) if config.required => return Err(error),
                Err(error) => (None, Some(error)),
            },
        };
        Ok(Self {
            config,
            trusted,
            clock,
            inner: Mutex::new(Inner {
                licence,
                invalid,
                guard,
                dirty: false,
            }),
        })
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// The trusted keys.
    #[must_use]
    pub fn trusted(&self) -> &TrustedKeys {
        &self.trusted
    }

    /// Why the file on disk was not accepted at load, when it was not.
    #[must_use]
    pub fn invalid(&self) -> Option<HostError> {
        lock(&self.inner).invalid.clone()
    }

    /// The verified licence, when there is one.
    #[must_use]
    pub fn licence(&self) -> Option<Licence> {
        lock(&self.inner).licence.clone()
    }

    fn instants(&self) -> (i64, u64) {
        (
            self.clock.now_utc().unix_timestamp(),
            self.clock.monotonic().as_secs(),
        )
    }

    fn standing_in(&self, inner: &mut Inner) -> Standing {
        let Some(licence) = inner.licence.clone() else {
            return if inner.invalid.is_some() {
                Standing::Invalid
            } else {
                Standing::Unlicensed
            };
        };
        let (now, mono) = self.instants();
        // `ClockGuard` only ever advances a mark it already holds: the first evaluation of an
        // install seeds it, or a clock set back later would never be noticed.
        if inner.guard.observation().is_none() {
            let _ = inner.guard.reanchor(now, mono);
            inner.dirty = true;
        }
        let before = inner.guard.observation();
        let last_seen = before.map(|o| o.high_water_mark);
        let guarded = inner.guard.evaluate(&licence, now, mono, last_seen);
        if inner.guard.observation() != before {
            inner.dirty = true;
        }
        Standing::Licensed(guarded.status)
    }

    /// Writes the clock-guard observation when evaluation moved it. Called by the service's task
    /// and by the commands; [`LicenceHost::decide`] only marks it.
    ///
    /// # Errors
    /// A [`HostError`] when the state file cannot be written.
    pub fn flush(&self) -> Result<(), HostError> {
        let mut inner = lock(&self.inner);
        let _ = self.standing_in(&mut inner);
        if !inner.dirty {
            return Ok(());
        }
        let Some(observation) = inner.guard.observation() else {
            return Ok(());
        };
        FileStore::new(self.config.state_file.clone())
            .save(&observation)
            .map_err(|m| HostError::new(code::WRITE_FAILED, m))?;
        inner.dirty = false;
        Ok(())
    }

    /// The last renewal attempt recorded on disk.
    #[must_use]
    pub fn last_attempt(&self) -> Option<Attempt> {
        read_attempt(&self.config.attempt_file)
    }

    /// Records a renewal attempt's result.
    pub fn record_attempt(&self, code: &str) {
        let at = self.clock.now_utc().unix_timestamp();
        let _ = write_attempt(
            &self.config.attempt_file,
            &Attempt {
                at,
                code: code.to_owned(),
            },
        );
    }

    /// Verifies `text` and makes it the installed licence.
    ///
    /// With a licence installed of the same id, the rules of `spot_license::apply_renewal` apply:
    /// a serial that is not strictly higher is refused (`licence.stale`), as is another tenant.
    /// `same_id_only` (a renewal fetched from the control plane) also refuses a different licence
    /// id; an operator's `install` accepts one (a new purchase) but never a licence that fails
    /// verification or names another tenant. `revoked` lists licence ids the control plane has
    /// revoked: such a licence is refused with `licence_host.revoked`.
    ///
    /// On success the file is replaced atomically and the clock-guard anchor is reset.
    ///
    /// # Errors
    /// A [`HostError`] with the stable code of the refusal; nothing changes on an error.
    pub fn install_text(
        &self,
        text: &str,
        same_id_only: bool,
        revoked: &[String],
    ) -> Result<Installed, HostError> {
        let (signed, incoming) = verify_text(text, &self.trusted, &self.config.tenant)?;
        if revoked.contains(&incoming.licence_id) {
            return Err(HostError::new(
                code::REVOKED,
                "the control plane has revoked this licence",
            ));
        }
        let mut inner = lock(&self.inner);
        if let Some(current) = inner.licence.clone()
            && (current.licence_id == incoming.licence_id || same_id_only)
        {
            apply_renewal(&current, &incoming)
                .map_err(|e| HostError::new(e.code(), "the licence is not a valid renewal"))?;
        }
        let text = signed
            .to_text()
            .map_err(|e| HostError::new(e.code(), "the licence cannot be written"))?;
        atomic_write(&self.config.licence_file, format!("{text}\n").as_bytes())
            .map_err(|m| HostError::new(code::WRITE_FAILED, m))?;
        let (now, mono) = self.instants();
        inner.licence = Some(incoming.clone());
        inner.invalid = None;
        // A renewal ends a clock-suspect window: the mark is re-anchored on the current reading.
        let _ = inner.guard.reanchor(now, mono);
        inner.dirty = true;
        let standing = self.standing_in(&mut inner);
        if let Some(observation) = inner.guard.observation() {
            FileStore::new(self.config.state_file.clone())
                .save(&observation)
                .map_err(|m| HostError::new(code::WRITE_FAILED, m))?;
            inner.dirty = false;
        }
        Ok(Installed {
            serial: incoming.serial,
            licence_id: incoming.licence_id,
            standing,
        })
    }
}

impl LicenceGate for LicenceHost {
    fn decide(&self, action: Action) -> Decision {
        let mut inner = lock(&self.inner);
        match self.standing_in(&mut inner) {
            Standing::Licensed(status) => status.allows(action),
            Standing::Unlicensed | Standing::Invalid => {
                if !self.config.required || action == Action::Renew {
                    Decision::Allow
                } else {
                    Decision::Deny {
                        reason: code::UNLICENSED,
                    }
                }
            }
        }
    }

    fn standing(&self) -> Standing {
        let mut inner = lock(&self.inner);
        self.standing_in(&mut inner)
    }

    fn entitlements(&self) -> Option<Entitlements> {
        lock(&self.inner)
            .licence
            .as_ref()
            .map(|l| l.entitlements.clone())
    }
}

impl LicenceStateSource for LicenceHost {
    fn state_code(&self) -> Option<i64> {
        state_code_of(LicenceGate::standing(self))
    }
}
