//! The update engine: what an update does to the disk, and how it undoes itself.
//!
//! [`UpdateEngine::apply`] runs the steps in order over the [`FileSystem`], [`ServiceControl`]
//! and [`DataHooks`] seams:
//!
//! `Receive -> Verify -> Unpack to staging -> Verify on disk -> Quiesce -> Backup -> Migrate ->
//! Switch pointer -> Start new -> Health check -> Commit`.
//!
//! Every step is written to the [journal](crate::journal) before and after it runs, so a power
//! cut at any instant is resumed by [`UpdateEngine::resume`] into exactly one of two states: the
//! old version running and intact, or the new version running and committed. Nothing is ever
//! half-active: the `current` pointer is written atomically, and it only ever names a version
//! directory that exists and was verified on disk.
//!
//! Any failure after the service is stopped restores the previous pointer, restarts the previous
//! version, and restores the data if the migration had started, reporting
//! [`Outcome::RolledBack`]. A healthy new version is watched for a post-commit probation window
//! ([`UpdatePolicy::probation_secs`]): a crash inside it rolls back on the next
//! [`UpdateEngine::tick`]; one outside it is kept. If a rollback cannot complete, the engine
//! reports [`Outcome::NeedsAttention`] and leaves the journal saying exactly what is where —
//! never a pointer naming a version that does not exist or is incomplete.

use semver::Version;

use crate::archive::{ArchiveLimits, StagedFile, verify_archive};
use crate::decide::{Decision, Installed, UpdatePolicy, decide};
use crate::digest::Digest;
use crate::error::{UpdaterError, UpdaterErrorCode};
use crate::fs::FileSystem;
use crate::hooks::DataHooks;
use crate::journal::{Journal, Stage, Step};
use crate::layout::Layout;
use crate::manifest::UpdateManifest;
use crate::service::ServiceControl;
use crate::trust::{UpdateRevocation, UpdateSignature, UpdateTrust};
use crate::verify::{verify_file, verify_payload, verify_update};

/// What an update run ended as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The new version is applied, committed and serving.
    Applied {
        /// The version now serving.
        version: Version,
    },
    /// The committed version was kept past its probation window; the update is complete.
    Kept {
        /// The version kept.
        version: Version,
    },
    /// There was nothing to do (no journal, or a committed update still in probation).
    NothingToDo,
    /// The offered update was skipped, for the given reason.
    Skipped(String),
    /// The installed version must step through the named version first.
    StepVia(Version),
    /// The update was refused with the given code, before anything on disk changed.
    Refused(UpdaterErrorCode),
    /// The update rolled back; the previous version serves and `current`/`previous` are as before.
    RolledBack {
        /// The step whose failure triggered the rollback.
        failed_step: Step,
        /// Why it failed, in plain words.
        reason: String,
    },
    /// Neither the update nor its rollback could complete; the journal says what is where.
    NeedsAttention {
        /// What is wrong and what is where, in plain words.
        detail: String,
    },
    /// A power cut interrupted the run at the named step; call [`UpdateEngine::resume`].
    Interrupted {
        /// The step that was interrupted.
        step: Step,
    },
}

/// Everything [`UpdateEngine::apply`] needs for one run: the verified inputs and the policy.
pub struct ApplyRequest<'a> {
    /// The version currently installed, as the decision and the hooks see it.
    pub installed: Installed,
    /// The offered manifest.
    pub manifest: UpdateManifest,
    /// The detached signature, as read from `update.sig`.
    pub signature: Option<UpdateSignature>,
    /// The update trust anchor.
    pub trust: &'a UpdateTrust,
    /// The signed revocation list that came with the update.
    pub revocations: &'a UpdateRevocation,
    /// The revocation generation the caller already holds.
    pub held_revocation_generation: u64,
    /// The payload archive's bytes (the caller received them; the engine never fetches).
    pub payload: &'a [u8],
    /// The policy: channels, probation, retention, health timeout and free-space factor.
    pub policy: UpdatePolicy,
    /// The archive guards.
    pub limits: ArchiveLimits,
}

/// The update engine over one install root.
pub struct UpdateEngine<'a> {
    fs: &'a dyn FileSystem,
    service: &'a dyn ServiceControl,
    hooks: &'a dyn DataHooks,
    layout: Layout,
}

/// A step that failed, with the step it was.
struct StepFailure {
    step: Step,
    error: UpdaterError,
}

impl<'a> UpdateEngine<'a> {
    /// An engine over the given seams and install root.
    #[must_use]
    pub fn new(
        fs: &'a dyn FileSystem,
        service: &'a dyn ServiceControl,
        hooks: &'a dyn DataHooks,
        layout: Layout,
    ) -> Self {
        Self {
            fs,
            service,
            hooks,
            layout,
        }
    }

    /// The layout this engine writes to.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The version `current` names, if any.
    ///
    /// # Errors
    /// [`UpdaterError::Pointer`] when the pointer names something that is not a version,
    /// [`UpdaterError::Io`] on a read failure.
    pub fn installed_version(&self) -> Result<Option<Version>, UpdaterError> {
        self.read_pointer(&self.layout.current_path())
    }

    /// Runs one update. See the module docs for the steps and the rollback rules.
    ///
    /// A power cut during the run is reported as [`Outcome::Interrupted`]; call
    /// [`resume`](UpdateEngine::resume) after the restart.
    #[must_use]
    pub fn apply(&self, request: &ApplyRequest<'_>, now: i64) -> Outcome {
        match Journal::load(self.fs, &self.layout.journal_path()) {
            Ok(None) => {}
            Ok(Some(journal)) => match journal.stage {
                Stage::InProgress => return self.resume(now),
                Stage::Committed => return Outcome::NothingToDo,
                Stage::RolledBack => {
                    let _ = Journal::remove(self.fs, &self.layout.journal_path());
                }
                Stage::NeedsAttention => {
                    return Outcome::NeedsAttention {
                        detail: journal
                            .note
                            .unwrap_or_else(|| "a previous update needs attention".to_owned()),
                    };
                }
            },
            Err(error) => return Outcome::Refused(error.code()),
        }

        match decide(&request.installed, &request.manifest, &request.policy, now) {
            Decision::Refuse(code) => return Outcome::Refused(code),
            Decision::Skip(reason) => return Outcome::Skipped(reason),
            Decision::StepVia(version) => return Outcome::StepVia(version),
            Decision::Apply => {}
        }

        if let Err(error) = verify_update(
            &request.manifest,
            request.signature.as_ref(),
            request.trust,
            request.revocations,
            now,
            request.held_revocation_generation,
        ) {
            return Outcome::Refused(error.code());
        }
        let payload_len = request.payload.len() as u64;
        if let Err(error) =
            verify_payload(&request.manifest, payload_len, Digest::of(request.payload))
        {
            return Outcome::Refused(error.code());
        }
        let staged = match verify_archive(&request.manifest, request.payload, &request.limits) {
            Ok(staged) => staged,
            Err(error) => return Outcome::Refused(error.code()),
        };
        // Reserve the inflated files as well as the documented payload factor. Keep 64 KiB
        // for pointer/journal rewrites so an unpack cannot consume the recovery reserve.
        let inflated = staged.iter().fold(0_u64, |sum, file| {
            sum.saturating_add(file.bytes.len() as u64)
        });
        let required =
            required_free_space(payload_len, &request.policy).max(inflated.saturating_add(65_536));
        if let Err(error) = self.fs.create_dir_all(self.layout.root()) {
            return Outcome::Refused(error.code());
        }
        match self.fs.free_space(self.layout.root()) {
            Ok(free) if free >= required => {}
            Ok(_) => return Outcome::Refused(UpdaterErrorCode::DiskSpace),
            Err(error) => return Outcome::Refused(error.code()),
        }

        let from = match self.read_pointer(&self.layout.current_path()) {
            Ok(from) => from,
            Err(error) => return Outcome::Refused(error.code()),
        };
        // Bind the pure decision snapshot to the actual installation, preventing stale downgrades.
        if from.as_ref() != Some(&request.installed.version)
            || self
                .fs
                .exists(&self.layout.version_dir(&request.manifest.version))
                .unwrap_or(true)
        {
            return Outcome::Refused(UpdaterErrorCode::Pointer);
        }
        let previous_before = match self.read_pointer(&self.layout.previous_path()) {
            Ok(previous) => previous,
            Err(error) => return Outcome::Refused(error.code()),
        };
        let mut journal = Journal::new(request.manifest.version.clone(), from, previous_before);
        journal.recovery_policy = (&request.policy).into();
        if let Err(error) = journal.store(self.fs, &self.layout.journal_path()) {
            let _ = Journal::remove(self.fs, &self.layout.journal_path());
            return Outcome::Refused(error.code());
        }

        match self.run_update(&mut journal, request, &staged, now) {
            Ok(outcome) => outcome,
            Err(failure) => {
                if matches!(failure.error, UpdaterError::PowerCut) {
                    return Outcome::Interrupted { step: failure.step };
                }
                self.rollback(
                    &mut journal,
                    failure.step,
                    failure.error.to_string(),
                    &request.policy,
                    now,
                )
            }
        }
    }

    /// Finishes or undoes an interrupted update, after a restart.
    ///
    /// Ends in exactly one of: the old version running and intact (a rollback), or the new
    /// version running and committed (a finished commit). Idempotent: a second call, with no
    /// journal left, is [`Outcome::NothingToDo`].
    ///
    /// Uses the policy captured durably before the update began.
    #[must_use]
    pub fn resume(&self, now: i64) -> Outcome {
        self.recover(now, None, true)
    }

    /// [`resume`](UpdateEngine::resume) with an explicit policy.
    #[must_use]
    pub fn resume_with_policy(&self, now: i64, policy: &UpdatePolicy) -> Outcome {
        self.recover(now, Some(policy), true)
    }

    fn recover(&self, now: i64, override_policy: Option<&UpdatePolicy>, restart: bool) -> Outcome {
        let mut journal = match Journal::load(self.fs, &self.layout.journal_path()) {
            Ok(Some(journal)) => journal,
            Ok(None) => {
                return if restart {
                    self.boot_current(now)
                } else {
                    Outcome::NothingToDo
                };
            }
            Err(error) => return Outcome::Refused(error.code()),
        };
        let stored = journal.recovery_policy.policy();
        let policy = override_policy.unwrap_or(&stored);
        if journal.rollback_started && journal.stage != Stage::NeedsAttention {
            let step = journal.step;
            let reason = journal
                .note
                .clone()
                .unwrap_or_else(|| "resuming rollback".to_owned());
            return self.rollback(&mut journal, step, reason, policy, now);
        }
        match journal.stage {
            Stage::InProgress => {
                let step = journal.step;
                self.rollback(
                    &mut journal,
                    step,
                    "the update was interrupted".to_owned(),
                    policy,
                    now,
                )
            }
            Stage::Committed => self.handle_committed(&mut journal, now, policy, restart),
            Stage::RolledBack => {
                let _ = Journal::remove(self.fs, &self.layout.journal_path());
                if restart {
                    self.boot_current(now)
                } else {
                    Outcome::NothingToDo
                }
            }
            Stage::NeedsAttention => Outcome::NeedsAttention {
                detail: journal
                    .note
                    .unwrap_or_else(|| "the previous update needs attention".to_owned()),
            },
        }
    }

    /// The periodic check after a commit: within the probation window an unhealthy new version
    /// rolls back; once the window passes the commit is kept and the journal is cleared.
    #[must_use]
    pub fn tick(&self, now: i64) -> Outcome {
        self.recover(now, None, false)
    }

    /// [`tick`](UpdateEngine::tick) with an explicit policy.
    #[must_use]
    pub fn tick_with_policy(&self, now: i64, policy: &UpdatePolicy) -> Outcome {
        self.recover(now, Some(policy), false)
    }

    /// Runs the forward steps, leaving a committed (but probation-open) journal on success.
    fn run_update(
        &self,
        journal: &mut Journal,
        request: &ApplyRequest<'_>,
        staged: &[StagedFile],
        now: i64,
    ) -> Result<Outcome, StepFailure> {
        let target = journal.target.clone();
        self.run_step(journal, Step::Unpack, || self.unpack(staged, &target))?;
        self.run_step(journal, Step::VerifyOnDisk, || {
            self.verify_staged(&request.manifest, &target)
        })?;

        // Mark quiesced before stopping: a crash right after the stop must still restart the
        // previous version on resume, whether or not the stop returned.
        journal.quiesced = true;
        self.note(journal, Step::Quiesce)?;
        self.service.stop().map_err(|error| StepFailure {
            step: Step::Quiesce,
            error,
        })?;
        self.finish(journal, Step::Quiesce)?;

        if requires_schema_change(&request.installed, &request.manifest) {
            self.run_step(journal, Step::Backup, || self.hooks.backup())?;
            journal.migrate_started = true;
            self.note(journal, Step::Migrate)?;
            self.hooks
                .migrate(
                    request.installed.schema_epoch,
                    request.manifest.schema_epoch,
                )
                .map_err(|error| StepFailure {
                    step: Step::Migrate,
                    error,
                })?;
            self.finish(journal, Step::Migrate)?;
        }

        self.run_step(journal, Step::Switch, || {
            self.install_version(&target)?;
            self.write_pointer(&self.layout.current_path(), &target)
        })?;
        self.run_step(journal, Step::StartNew, || self.service.start(&target))?;
        self.run_step(journal, Step::Health, || {
            let deadline = now.saturating_add(request.policy.health_timeout_secs);
            self.service.wait_ready(deadline)?;
            if self.service.is_healthy()? {
                Ok(())
            } else {
                Err(UpdaterError::Health {
                    detail: "the new version reported itself unhealthy".to_owned(),
                })
            }
        })?;

        self.note(journal, Step::Commit)?;
        if let Some(from) = journal.from.clone() {
            self.write_pointer(&self.layout.previous_path(), &from)
                .map_err(|error| StepFailure {
                    step: Step::Commit,
                    error,
                })?;
        }
        journal.stage = Stage::Committed;
        journal.committed_at = Some(now);
        self.store(journal, Step::Commit)?;
        self.prune(
            &target,
            request.policy.retention,
            journal.previous_before.as_ref(),
        );
        Ok(Outcome::Applied { version: target })
    }

    /// Notes a step durably, then runs its action.
    fn run_step<T>(
        &self,
        journal: &mut Journal,
        step: Step,
        action: impl FnOnce() -> Result<T, UpdaterError>,
    ) -> Result<T, StepFailure> {
        self.note(journal, step)?;
        let result = action().map_err(|error| StepFailure { step, error })?;
        self.finish(journal, step)?;
        Ok(result)
    }

    /// Writes the journal with `step` as the current step, in progress.
    fn note(&self, journal: &mut Journal, step: Step) -> Result<(), StepFailure> {
        journal.step = step;
        journal.stage = Stage::InProgress;
        journal.step_finished = false;
        self.store(journal, step)
    }

    fn finish(&self, journal: &mut Journal, step: Step) -> Result<(), StepFailure> {
        journal.step_finished = true;
        self.store(journal, step)
    }

    /// Stores the journal, mapping a failure to a [`StepFailure`] for `step`.
    fn store(&self, journal: &Journal, step: Step) -> Result<(), StepFailure> {
        journal
            .store(self.fs, &self.layout.journal_path())
            .map_err(|error| StepFailure { step, error })
    }

    /// Writes every staged file into the staging directory, atomically and durably.
    fn unpack(&self, staged: &[StagedFile], target: &Version) -> Result<(), UpdaterError> {
        let root = self.layout.staging_path(target);
        self.fs.remove_dir_all(&root)?;
        self.fs.create_dir_all(&root)?;
        for file in staged {
            let path = root.join(&file.path);
            if let Some(parent) = path.parent() {
                self.fs.create_dir_all(parent)?;
            }
            self.fs.durable_write(&path, &file.bytes)?;
        }
        Ok(())
    }

    /// Re-reads and re-hashes every staged file against the manifest.
    fn verify_staged(
        &self,
        manifest: &UpdateManifest,
        target: &Version,
    ) -> Result<(), UpdaterError> {
        let root = self.layout.staging_path(target);
        for file in &manifest.files {
            let path = root.join(crate::archive::normalise(&file.path)?);
            let bytes = self.fs.read(&path)?;
            verify_file(file, bytes.len() as u64, Digest::of(&bytes))?;
        }
        Ok(())
    }

    /// Moves the verified staging directory into place as the version directory.
    fn install_version(&self, target: &Version) -> Result<(), UpdaterError> {
        let staging = self.layout.staging_path(target);
        let dest = self.layout.version_dir(target);
        if self.fs.exists(&staging)? {
            self.fs.create_dir_all(&self.layout.versions_dir())?;
            if self.fs.exists(&dest)? {
                self.fs.remove_dir_all(&dest)?;
            }
            self.fs.rename(&staging, &dest)?;
        } else if !self.fs.exists(&dest)? {
            return Err(UpdaterError::Pointer {
                detail: format!("neither staging nor the version directory holds {target}"),
            });
        }
        Ok(())
    }

    /// Reads a pointer file, `None` when it is absent.
    fn read_pointer(&self, path: &std::path::Path) -> Result<Option<Version>, UpdaterError> {
        if !self.fs.exists(path)? {
            return Ok(None);
        }
        let bytes = self.fs.read(path)?;
        let text = std::str::from_utf8(&bytes).map_err(|error| UpdaterError::Pointer {
            detail: format!("{} is not UTF-8: {error}", path.display()),
        })?;
        let text = text.trim();
        Version::parse(text)
            .map(Some)
            .map_err(|error| UpdaterError::Pointer {
                detail: format!("{} names `{text}`, not a version: {error}", path.display()),
            })
    }

    /// Writes a pointer file, durably and atomically.
    fn write_pointer(&self, path: &std::path::Path, version: &Version) -> Result<(), UpdaterError> {
        self.fs.durable_write(path, version.to_string().as_bytes())
    }

    /// Undoes an interrupted or failed update. See the module docs for the rules.
    fn rollback(
        &self,
        journal: &mut Journal,
        failed_step: Step,
        reason: String,
        policy: &UpdatePolicy,
        now: i64,
    ) -> Outcome {
        // Persist rollback intent before touching a committed probation run.
        journal.rollback_started = true;
        journal.stage = Stage::InProgress;
        journal.note = Some(reason.clone());
        if let Err(error) = journal.store(self.fs, &self.layout.journal_path()) {
            return self.recovery_error(journal, failed_step, error);
        }
        if let Err(error) = self.undo(journal, policy, now) {
            return self.recovery_error(journal, failed_step, error);
        }
        journal.stage = Stage::RolledBack;
        if let Err(error) = journal.store(self.fs, &self.layout.journal_path()) {
            return self.recovery_error(journal, failed_step, error);
        }
        if let Err(error) = Journal::remove(self.fs, &self.layout.journal_path()) {
            return self.recovery_error(journal, failed_step, error);
        }
        Outcome::RolledBack {
            failed_step,
            reason,
        }
    }

    fn recovery_error(&self, journal: &mut Journal, step: Step, error: UpdaterError) -> Outcome {
        if matches!(error, UpdaterError::PowerCut) {
            return Outcome::Interrupted { step };
        }
        self.attention(
            journal,
            format!(
                "rollback of {} to {:?}: {error}; restore_done={}",
                journal.target, journal.from, journal.restore_done
            ),
        )
    }

    fn undo(
        &self,
        journal: &mut Journal,
        policy: &UpdatePolicy,
        now: i64,
    ) -> Result<(), UpdaterError> {
        let from = journal.from.clone().ok_or_else(|| UpdaterError::Pointer {
            detail: "no previous version to recover".to_owned(),
        })?;
        if !self.fs.exists(&self.layout.version_dir(&from))? {
            return Err(UpdaterError::Pointer {
                detail: format!("previous version {from} is missing"),
            });
        }
        // Stop before restoring data. Failed stop/restore forbids starting old code.
        self.service.stop()?;
        self.write_pointer(&self.layout.current_path(), &from)?;
        match &journal.previous_before {
            Some(previous) => {
                if !self.fs.exists(&self.layout.version_dir(previous))? {
                    return Err(UpdaterError::Pointer {
                        detail: format!("last good version {previous} is missing"),
                    });
                }
                self.write_pointer(&self.layout.previous_path(), previous)?;
            }
            None => self.fs.remove_file(&self.layout.previous_path())?,
        }
        if journal.migrate_started && !journal.restore_done {
            self.hooks.restore()?;
            journal.restore_done = true;
            journal.store(self.fs, &self.layout.journal_path())?;
        }
        self.service.start(&from)?;
        self.service
            .wait_ready(now.saturating_add(policy.health_timeout_secs))?;
        self.fs
            .remove_dir_all(&self.layout.staging_path(&journal.target))?;
        if from != journal.target && journal.previous_before.as_ref() != Some(&journal.target) {
            self.fs
                .remove_dir_all(&self.layout.version_dir(&journal.target))?;
        }
        Ok(())
    }

    fn boot_current(&self, now: i64) -> Outcome {
        let result = (|| {
            if self.service.is_healthy()? {
                return Ok(());
            }
            if let Some(current) = self.read_pointer(&self.layout.current_path())? {
                if !self.fs.exists(&self.layout.version_dir(&current))? {
                    return Err(UpdaterError::Pointer {
                        detail: format!("current version {current} is missing"),
                    });
                }
                self.service.start(&current)?;
                self.service
                    .wait_ready(now.saturating_add(UpdatePolicy::default().health_timeout_secs))?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => Outcome::NothingToDo,
            Err(error) => Outcome::NeedsAttention {
                detail: error.to_string(),
            },
        }
    }

    /// Handles a committed update: inside probation an unhealthy new version rolls back; once
    /// the window passes the commit is kept and the journal is cleared.
    fn handle_committed(
        &self,
        journal: &mut Journal,
        now: i64,
        policy: &UpdatePolicy,
        restart: bool,
    ) -> Outcome {
        let target = journal.target.clone();
        let committed = journal.committed_at.unwrap_or(now);
        if !self.version_dir_exists(&target) {
            return self.attention(
                journal,
                format!("the committed version {target} is not on disk"),
            );
        }
        let within = now < probation_deadline(committed, policy.probation_secs);
        if !within {
            // Past probation the update is final: bring the version up best effort, prune, clear
            // the journal. A crash now is the service supervisor's concern, not a rollback.
            let _ = self.service.start(&target).and_then(|()| {
                self.service
                    .wait_ready(now.saturating_add(policy.health_timeout_secs))
            });
            self.prune(&target, policy.retention, None);
            let _ = Journal::remove(self.fs, &self.layout.journal_path());
            return Outcome::Kept { version: target };
        }
        // A tick must observe a crashed service BEFORE a restart could hide the crash.
        let initial = if restart {
            self.service.start(&target)
        } else {
            Ok(())
        };
        let started = initial
            .and_then(|()| {
                self.service
                    .wait_ready(now.saturating_add(policy.health_timeout_secs))
            })
            .and_then(|()| match self.service.is_healthy() {
                Ok(true) => Ok(()),
                Ok(false) => Err(UpdaterError::Health {
                    detail: "the committed version reported itself unhealthy".to_owned(),
                }),
                Err(error) => Err(error),
            });
        match started {
            Ok(()) => Outcome::Applied { version: target },
            Err(error) => self.rollback(journal, Step::Health, error.to_string(), policy, now),
        }
    }

    /// Marks the journal as needing attention and returns the outcome.
    fn attention(&self, journal: &mut Journal, detail: String) -> Outcome {
        journal.stage = Stage::NeedsAttention;
        journal.note = Some(detail.clone());
        let _ = journal.store(self.fs, &self.layout.journal_path());
        Outcome::NeedsAttention { detail }
    }

    /// Whether a complete version directory exists for `version`.
    fn version_dir_exists(&self, version: &Version) -> bool {
        self.fs
            .exists(&self.layout.version_dir(version))
            .unwrap_or(false)
    }

    /// Removes version directories beyond the retention count, never the current or previous
    /// version. Best effort: an individual removal that fails leaves that directory in place.
    fn prune(&self, target: &Version, retention: usize, protected: Option<&Version>) {
        // If either pointer cannot be read, deletion is unsafe; leave all versions intact.
        let Ok(current) = self.read_pointer(&self.layout.current_path()) else {
            return;
        };
        let Ok(previous) = self.read_pointer(&self.layout.previous_path()) else {
            return;
        };
        let Ok(names) = self.fs.list(&self.layout.versions_dir()) else {
            return;
        };
        let mut versions: Vec<Version> = names
            .iter()
            .filter_map(|name| Version::parse(name).ok())
            .collect();
        versions.sort();
        versions.reverse();
        let mut keep: Vec<Version> = Vec::new();
        keep.push(target.clone());
        if let Some(protected) = protected {
            keep.push(protected.clone());
        }
        if let Some(current) = current {
            keep.push(current);
        }
        if let Some(previous) = previous {
            keep.push(previous);
        }
        for version in versions.iter().take(retention) {
            keep.push(version.clone());
        }
        for version in versions {
            if !keep.iter().any(|kept| kept == &version) {
                let _ = self.fs.remove_dir_all(&self.layout.version_dir(&version));
            }
        }
    }
}

/// Whether an update changes the database schema, and so needs a backup and a migration.
///
/// The manifest's `schema_epoch` is the epoch the new version runs against; when it differs
/// from the installed database's, the update changes the schema.
#[must_use]
pub fn requires_schema_change(installed: &Installed, manifest: &UpdateManifest) -> bool {
    manifest.schema_epoch != installed.schema_epoch
}

/// The free bytes an unpack needs, from the policy's factor: the payload size times
/// `free_space_num / free_space_den`, rounded up, so the staging copy fits with room to spare.
#[must_use]
pub fn required_free_space(payload: u64, policy: &UpdatePolicy) -> u64 {
    match policy.free_space_den {
        0 => u64::MAX,
        den => u64::try_from(
            u128::from(payload)
                .saturating_mul(u128::from(policy.free_space_num))
                .div_ceil(u128::from(den)),
        )
        .unwrap_or(u64::MAX)
        .max(payload),
    }
}

/// The instant a commit's probation window closes.
fn probation_deadline(committed_at: i64, secs: u64) -> i64 {
    committed_at.saturating_add(i64::try_from(secs).unwrap_or(i64::MAX))
}
