//! The update run: fetch the offer, check it, fetch the payload, hand everything to
//! `spot_updater::UpdateEngine`, and turn what happened into a report the channel can send.
//!
//! One call, [`SelfUpdater::check`], is both the periodic poll for an update and the body of
//! a signed `agent.update` command. [`SelfUpdater::boot`] runs at start-up and lets the
//! engine finish or undo an interrupted update; [`SelfUpdater::tick`] closes the probation
//! window after a commit.

use std::path::PathBuf;

use semver::Version;
use spot_updater::{
    ApplyRequest, ArchiveLimits, Channel, Decision, FileSystem, Installed, Layout, NoDataHooks,
    Outcome, ServiceControl, UpdateEngine, UpdatePolicy, UpdateRevocation, UpdateTrust,
    UpdaterErrorCode, decide, verify_update,
};

use super::error::UpdateError;
use super::source::UpdateSource;
use super::trust::{RevocationStore, list_is_trusted};
use super::wire::{Offer, parse_offer};
use super::{CODE_FAILED, CODE_NEEDS_ATTENTION, CODE_REFUSED, CODE_ROLLED_BACK};

/// What the updater needs to know about this install.
#[derive(Clone, Debug)]
pub struct UpdateConfig {
    /// The product name the manifest must carry (`spot-agent`).
    pub product: String,
    /// The version of the running process.
    pub running: Version,
    /// The install root: `versions/`, `current`, `previous`, `staging/`, the update journal.
    /// Never the state directory: keys and state are outside every version directory and
    /// survive any update untouched.
    pub root: PathBuf,
    /// `linux` or `windows`.
    pub platform: String,
    /// The channel asked of the gateway, when not the default.
    pub channel: Option<Channel>,
    /// The update policy. The default allows `stable` only.
    pub policy: UpdatePolicy,
    /// The archive guards.
    pub limits: ArchiveLimits,
    /// The largest payload accepted, in bytes (checked against the signed manifest before
    /// anything is downloaded).
    pub max_payload: u64,
}

impl UpdateConfig {
    /// A configuration with the agent's defaults for `running` installed at `root`.
    #[must_use]
    pub fn new(running: Version, root: impl Into<PathBuf>) -> Self {
        Self {
            product: "spot-agent".to_owned(),
            running,
            root: root.into(),
            platform: std::env::consts::OS.to_owned(),
            channel: None,
            policy: UpdatePolicy {
                allowed_channels: vec![Channel::Stable],
                ..UpdatePolicy::default()
            },
            limits: ArchiveLimits::default(),
            max_payload: 128 * 1024 * 1024,
        }
    }
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    /// No update is published.
    NoUpdate,
    /// Offered version equals the installed one, or nothing was in flight.
    UpToDate,
    /// The new version is committed; the process should restart into it.
    Applied(Version),
    /// A committed update was kept after its probation window.
    Kept(Version),
    /// The update failed and the previous version serves.
    RolledBack {
        /// The step that failed.
        step: String,
        /// Why, in plain words.
        reason: String,
    },
    /// The offer was refused before anything changed.
    Refused(String),
    /// The installed version must step through another version first.
    StepVia(Version),
    /// The pointer already names a newer version than the running process: restart pending.
    RestartPending,
    /// The run could not happen (source, trust files or layout).
    Failed(String),
    /// Neither the update nor its rollback completed.
    NeedsAttention(String),
}

/// A finished run, ready to be reported as the result of a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateReport {
    /// What happened.
    pub status: UpdateStatus,
    /// The result code, when the run is not a plain success.
    pub code: Option<&'static str>,
    /// A short secret-free sentence.
    pub detail: String,
    /// Whether the caller should report and then call [`SelfUpdater::restart`].
    pub needs_restart: bool,
}

impl UpdateReport {
    fn new(status: UpdateStatus, code: Option<&'static str>, detail: impl Into<String>) -> Self {
        Self {
            status,
            code,
            detail: detail.into(),
            needs_restart: false,
        }
    }

    /// The channel outcome word for this report: `succeeded`, `refused` or `failed`.
    #[must_use]
    pub fn outcome(&self) -> &'static str {
        match self.status {
            UpdateStatus::NoUpdate
            | UpdateStatus::UpToDate
            | UpdateStatus::Applied(_)
            | UpdateStatus::Kept(_)
            | UpdateStatus::RestartPending => "succeeded",
            UpdateStatus::Refused(_) | UpdateStatus::StepVia(_) => "refused",
            UpdateStatus::RolledBack { .. }
            | UpdateStatus::Failed(_)
            | UpdateStatus::NeedsAttention(_) => "failed",
        }
    }

    fn from_error(error: &UpdateError) -> Self {
        let status = match error {
            UpdateError::Unavailable => UpdateStatus::NoUpdate,
            UpdateError::Wire(_) | UpdateError::Updater(_) => UpdateStatus::Refused(error.detail()),
            _ => UpdateStatus::Failed(error.detail()),
        };
        Self::new(status, Some(error.code()), error.detail())
    }
}

fn map_outcome(outcome: Outcome) -> UpdateReport {
    match outcome {
        Outcome::Applied { version } => {
            let mut report = UpdateReport::new(
                UpdateStatus::Applied(version.clone()),
                None,
                format!("updated to {version}"),
            );
            report.needs_restart = true;
            report
        }
        Outcome::Kept { version } => UpdateReport::new(
            UpdateStatus::Kept(version.clone()),
            None,
            format!("kept {version}"),
        ),
        Outcome::NothingToDo => UpdateReport::new(UpdateStatus::UpToDate, None, "nothing to do"),
        Outcome::Skipped(reason) => UpdateReport::new(UpdateStatus::UpToDate, None, reason),
        Outcome::StepVia(version) => UpdateReport::new(
            UpdateStatus::StepVia(version.clone()),
            Some(CODE_REFUSED),
            format!("update through {version} first"),
        ),
        Outcome::Refused(code) => UpdateReport::new(
            UpdateStatus::Refused(code.as_str().to_owned()),
            Some(CODE_REFUSED),
            format!("refused: {}", code.as_str()),
        ),
        Outcome::RolledBack {
            failed_step,
            reason,
        } => UpdateReport::new(
            UpdateStatus::RolledBack {
                step: failed_step.as_str().to_owned(),
                reason: reason.clone(),
            },
            Some(CODE_ROLLED_BACK),
            format!("rolled back after {}: {reason}", failed_step.as_str()),
        ),
        Outcome::NeedsAttention { detail } => UpdateReport::new(
            UpdateStatus::NeedsAttention(detail.clone()),
            Some(CODE_NEEDS_ATTENTION),
            detail,
        ),
        Outcome::Interrupted { step } => UpdateReport::new(
            UpdateStatus::Failed(format!("interrupted at {}", step.as_str())),
            Some(CODE_FAILED),
            format!("interrupted at {}", step.as_str()),
        ),
    }
}

/// Whether the manifest's free-text `requires_os` names the other platform. A manifest that
/// names no operating system, or names this one, passes.
fn platform_ok(requires: Option<&str>, platform: &str) -> bool {
    let Some(text) = requires else {
        return true;
    };
    let text = text.to_ascii_lowercase();
    let other = match platform {
        "windows" => "linux",
        "linux" => "windows",
        _ => return true,
    };
    !text.contains(other) || text.contains(platform)
}

/// The self-updater: configuration, the seams, and the trust files.
pub struct SelfUpdater<'a> {
    config: &'a UpdateConfig,
    fs: &'a dyn FileSystem,
    service: &'a dyn ServiceControl,
    source: &'a dyn UpdateSource,
    restarter: &'a dyn super::service::Restarter,
    trust: &'a UpdateTrust,
    revocations: &'a dyn RevocationStore,
}

impl<'a> SelfUpdater<'a> {
    /// An updater over the given seams. `trust` is the anchor the package installed.
    #[must_use]
    pub fn new(
        config: &'a UpdateConfig,
        fs: &'a dyn FileSystem,
        service: &'a dyn ServiceControl,
        source: &'a dyn UpdateSource,
        restarter: &'a dyn super::service::Restarter,
        trust: &'a UpdateTrust,
        revocations: &'a dyn RevocationStore,
    ) -> Self {
        Self {
            config,
            fs,
            service,
            source,
            restarter,
            trust,
            revocations,
        }
    }

    fn engine(&self) -> UpdateEngine<'_> {
        UpdateEngine::new(
            self.fs,
            self.service,
            &NoDataHooks,
            Layout::new(self.config.root.clone()),
        )
    }

    /// Start-up: finishes or undoes an update the last run did not complete.
    #[must_use]
    pub fn boot(&self, now: i64) -> UpdateReport {
        map_outcome(self.engine().resume_with_policy(now, &self.config.policy))
    }

    /// The periodic probation check after a commit.
    #[must_use]
    pub fn tick(&self, now: i64) -> UpdateReport {
        map_outcome(self.engine().tick_with_policy(now, &self.config.policy))
    }

    /// Asks the service manager to restart into the committed version. Call after the report
    /// of an applied update has been sent.
    ///
    /// # Errors
    /// [`UpdateError::Layout`] when the request cannot be issued; the new version then starts
    /// at the next restart of the service.
    pub fn restart(&self) -> Result<(), UpdateError> {
        self.restarter.restart()
    }

    /// Looks for an update and installs it when everything checks out. Never panics and
    /// never leaves the install half-changed (the engine journals every step).
    #[must_use]
    pub fn check(&self, now: i64) -> UpdateReport {
        match self.check_inner(now) {
            Ok(report) => report,
            Err(error) => UpdateReport::from_error(&error),
        }
    }

    fn check_inner(&self, now: i64) -> Result<UpdateReport, UpdateError> {
        let engine = self.engine();
        let installed_version = engine
            .installed_version()?
            .ok_or_else(|| UpdateError::Layout("no `current` pointer".to_owned()))?;
        if installed_version != self.config.running {
            // The pointer names another version than the one running: a commit is waiting
            // for its restart (or an operator moved the pointer). Never stack an update.
            return Ok(UpdateReport::new(
                UpdateStatus::RestartPending,
                None,
                format!(
                    "`current` names {installed_version}, the running version is {}",
                    self.config.running
                ),
            ));
        }
        let installed = Installed {
            product: self.config.product.clone(),
            version: installed_version,
            channel: Channel::Stable,
            schema_epoch: 0,
            payload_sha256: None,
        };

        let data = self
            .source
            .offer(self.config.channel.map(Channel::as_str), now)?;
        let offer = parse_offer(&data)?;
        self.pre_checks(&offer)?;

        let (revocations, held) = self.revocations_for(&offer, now)?;

        // Everything checked here is checked again inside `apply`; doing it first means
        // nothing is downloaded for an unsigned, revoked, stale or downgrading offer.
        match decide(&installed, &offer.manifest, &self.config.policy, now) {
            Decision::Apply => {}
            Decision::Skip(reason) => {
                return Ok(UpdateReport::new(UpdateStatus::UpToDate, None, reason));
            }
            Decision::StepVia(version) => return Ok(map_outcome(Outcome::StepVia(version))),
            Decision::Refuse(code) => return Ok(map_outcome(Outcome::Refused(code))),
        }
        verify_update(
            &offer.manifest,
            offer.signature.as_ref(),
            self.trust,
            &revocations,
            now,
            held,
        )?;

        let payload = self.source.payload(
            &offer.manifest.payload.file,
            offer.manifest.payload.size,
            now,
        )?;
        let request = ApplyRequest {
            installed,
            manifest: offer.manifest,
            signature: offer.signature,
            trust: self.trust,
            revocations: &revocations,
            held_revocation_generation: held,
            payload: &payload,
            policy: self.config.policy.clone(),
            limits: self.config.limits,
        };
        Ok(map_outcome(engine.apply(&request, now)))
    }

    /// The checks only an agent makes: size, platform, schema epoch.
    fn pre_checks(&self, offer: &Offer) -> Result<(), UpdateError> {
        let manifest = &offer.manifest;
        if manifest.payload.size > self.config.max_payload {
            return Err(UpdateError::Updater(UpdaterErrorCode::ArchiveBomb));
        }
        if !platform_ok(manifest.requires_os.as_deref(), &self.config.platform) {
            return Err(UpdateError::Updater(UpdaterErrorCode::ProductMismatch));
        }
        // The agent has no database: a manifest that wants a migration is not for it.
        if manifest.schema_epoch != 0 {
            return Err(UpdateError::Updater(UpdaterErrorCode::SchemaEpoch));
        }
        Ok(())
    }

    /// Picks the revocation list to verify with, and the generation held before this run.
    /// A list that came with the offer replaces the stored one only if its issuer is
    /// trusted, its signature verifies and its generation is higher; it is stored before
    /// anything is installed, so the held generation only ever goes up.
    fn revocations_for(
        &self,
        offer: &Offer,
        now: i64,
    ) -> Result<(UpdateRevocation, u64), UpdateError> {
        let stored = self.revocations.load()?;
        let held = stored.as_ref().map_or(0, |list| list.body.generation);
        let mut best = stored;
        if let Some(offered) = &offer.revocations {
            let newer = best
                .as_ref()
                .is_none_or(|list| offered.body.generation > list.body.generation);
            if newer && list_is_trusted(self.trust, offered, now) {
                self.revocations.save(offered)?;
                best = Some(offered.clone());
            }
        }
        let list = best.ok_or(UpdateError::Wire("revocations"))?;
        Ok((list, held))
    }
}
