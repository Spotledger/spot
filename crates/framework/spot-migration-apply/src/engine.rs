//! The apply engine: the state machine that runs a plan under maintenance and admits users only
//! after verification.
//!
//! [`ApplyEngine::apply`] runs `Idle -> Gated -> Applying(step n) -> Verifying -> Open`. Every
//! transition is written to the [`ApplyJournal`] **before** it happens. [`ApplyEngine::recover`]
//! reads the journal after a crash and either resumes at the first incomplete step (when the
//! steps make that safe) or restores per the rollback table of ADR-22.
//!
//! # Failing closed
//!
//! The engine opens the maintenance gate on exactly two paths: a verified new version and a
//! restored old version. Every error path leaves the gate closed; a kill (a simulated crash)
//! leaves everything exactly as it was.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use spot_core::SiteId;
use spot_migration_approval::ApplyTicket;
use spot_migration_plan::ReleasePlan;

use crate::BoxFuture;
use crate::error::{Error, Result};
use crate::faults::{Faults, KillPoint, NoFaults};
use crate::gate::AdmissionGate;
use crate::hold::SideEffectHold;
use crate::journal::{ApplyJournal, JournalRecord, Progress, Seq};
use crate::progress::{ProgressEvent, ProgressSink, SilentProgress};
use crate::snapshot::{SnapshotId, SnapshotPort};
use crate::state::{
    ApplyState, RecoveryAction, RecoveryOutcome, RestoreCause, RestoreMechanism, Version,
};
use crate::step::{
    Idempotency, RestorePoint, StepContext, StepDescriptor, StepOutcome, StepRunner,
};
use crate::verify::VerifyPort;

/// Knobs of one engine.
#[derive(Clone, Debug)]
pub struct ApplyOptions {
    /// The default per-step timeout; a descriptor may override it. Default 15 minutes.
    pub step_timeout: Duration,
    /// How long in-flight transactions may take to drain. Default 30 seconds.
    pub drain_deadline_micros: i64,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            step_timeout: Duration::from_secs(15 * 60),
            drain_deadline_micros: 30_000_000,
        }
    }
}

/// Everything one `apply` needs.
pub struct ApplyRequest {
    /// The approved plan.
    pub plan: ReleasePlan,
    /// The ticket that authorises it; required.
    pub ticket: Option<ApplyTicket>,
    /// The taken snapshot the run restores from; required.
    pub snapshot: Option<SnapshotId>,
    /// Unix microseconds; the engine reads no clock.
    pub now: i64,
}

impl ApplyRequest {
    /// A request for `plan` under `ticket`, restoring from `snapshot`.
    #[must_use]
    pub fn new(
        plan: ReleasePlan,
        ticket: Option<ApplyTicket>,
        snapshot: Option<SnapshotId>,
        now: i64,
    ) -> Self {
        Self {
            plan,
            ticket,
            snapshot,
            now,
        }
    }
}

/// What an `apply` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The site is open on a version.
    Opened {
        /// Which version is live.
        version: Version,
        /// Whether verification passed.
        verified: bool,
    },
    /// The site is open on the old version after a rollback.
    Restored {
        /// Why the run rolled back.
        cause: RestoreCause,
        /// How the rollback was achieved.
        mechanism: RestoreMechanism,
    },
}

/// The apply engine as a seam, so a host (for example the agent runtime) can hold it as a trait
/// object.
pub trait ApplyService: Send + Sync {
    /// Runs a plan.
    ///
    /// # Errors
    /// See [`ApplyEngine::apply`].
    fn apply(&self, request: ApplyRequest) -> BoxFuture<'_, Result<ApplyOutcome>>;

    /// Recovers an unfinished run for `plan`.
    ///
    /// # Errors
    /// See [`ApplyEngine::recover`].
    fn recover<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        now: i64,
    ) -> BoxFuture<'a, Result<RecoveryOutcome>>;

    /// The engine's state.
    fn state(&self) -> ApplyState;
}

/// The apply engine for one site.
pub struct ApplyEngine {
    site: SiteId,
    journal: Arc<dyn ApplyJournal>,
    gate: Arc<dyn AdmissionGate>,
    hold: Arc<dyn SideEffectHold>,
    snapshot: Arc<dyn SnapshotPort>,
    steps: Arc<dyn StepRunner>,
    verify: Arc<dyn VerifyPort>,
    progress: Arc<dyn ProgressSink>,
    faults: Arc<dyn Faults>,
    options: ApplyOptions,
    state: Mutex<ApplyState>,
    next_seq: std::sync::atomic::AtomicU64,
}

impl ApplyEngine {
    /// An engine over the given collaborators, with a silent progress sink and no faults.
    #[must_use]
    pub fn new(
        site: SiteId,
        journal: Arc<dyn ApplyJournal>,
        gate: Arc<dyn AdmissionGate>,
        hold: Arc<dyn SideEffectHold>,
        snapshot: Arc<dyn SnapshotPort>,
        steps: Arc<dyn StepRunner>,
        verify: Arc<dyn VerifyPort>,
    ) -> Self {
        Self {
            site,
            journal,
            gate,
            hold,
            snapshot,
            steps,
            verify,
            progress: Arc::new(SilentProgress),
            faults: Arc::new(NoFaults),
            options: ApplyOptions::default(),
            state: Mutex::new(ApplyState::Idle),
            next_seq: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// The same engine reporting progress through `progress`.
    #[must_use]
    pub fn with_progress(mut self, progress: Arc<dyn ProgressSink>) -> Self {
        self.progress = progress;
        self
    }

    /// The same engine with a fault-injection switch.
    #[must_use]
    pub fn with_faults(mut self, faults: Arc<dyn Faults>) -> Self {
        self.faults = faults;
        self
    }

    /// The same engine with different options.
    #[must_use]
    pub fn with_options(mut self, options: ApplyOptions) -> Self {
        self.options = options;
        self
    }

    /// The engine's state.
    #[must_use]
    pub fn state(&self) -> ApplyState {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Runs `request`.
    ///
    /// Refuses without an [`ApplyTicket`] or a taken snapshot, and refuses when an earlier run of
    /// a different plan is unfinished. On success the site is open on the new version after
    /// verification; a failed verification restores the snapshot and opens the old version.
    ///
    /// # Errors
    /// [`Error::NoTicket`], [`Error::NoSnapshot`], [`Error::TicketMismatch`],
    /// [`Error::InProgress`], [`Error::JournalFailed`], [`Error::GateFailed`],
    /// [`Error::DrainTimeout`], [`Error::HoldFailed`], [`Error::StepTimeout`],
    /// [`Error::RestoreFailed`], [`Error::SnapshotFailed`], [`Error::ReleaseFailed`] or
    /// [`Error::Killed`] (the sentinel a fault-injection kill returns, which performs no cleanup).
    pub async fn apply(&self, request: ApplyRequest) -> Result<ApplyOutcome> {
        self.run(request).await
    }

    async fn run(&self, request: ApplyRequest) -> Result<ApplyOutcome> {
        let plan = &request.plan;
        let ticket = request.ticket.as_ref().ok_or(Error::NoTicket)?;
        if !ticket.matches(plan) {
            return Err(Error::TicketMismatch {
                ticket: ticket.digest().to_string(),
                plan: plan.plan_digest.clone(),
            });
        }
        let snapshot = request.snapshot.as_ref().ok_or(Error::NoSnapshot)?;
        let taken =
            self.snapshot
                .exists(snapshot)
                .await
                .map_err(|error| Error::SnapshotFailed {
                    message: error.to_string(),
                })?;
        if !taken {
            return Err(Error::NoSnapshot);
        }
        let now = request.now;
        let entries = self.journal.entries().await.map_err(journal_err)?;
        let progress = Progress::fold(entries);
        self.sync_seq(progress.last_seq);
        if progress.plan_digest.as_deref() == Some(plan.plan_digest.as_str())
            && progress.gate_open == Some((Version::New, true))
        {
            // One ticket authorises one application: never run a verified plan's steps again.
            return Err(Error::AlreadyApplied {
                plan_digest: plan.plan_digest.clone(),
            });
        }
        if progress.in_progress() || progress.halted.is_some() {
            // A halted run needs a person; a new run must not start over its half-done state.
            return Err(Error::InProgress {
                plan_digest: progress.plan_digest.unwrap_or_default(),
            });
        }

        let plan_digest = plan.plan_digest.clone();
        let migration_id = migration_id_for(plan);
        let reason = format!("migration apply in progress for plan {plan_digest}");
        self.append(
            JournalRecord::Started {
                plan_digest: plan_digest.clone(),
                from_version: plan.from_version,
                to_version: plan.to_version,
                snapshot: snapshot.as_str().to_owned(),
            },
            now,
        )
        .await?;
        self.progress.on(ProgressEvent::Started {
            plan_digest: plan_digest.clone(),
            at: now,
        });
        self.close_gate(&reason, now).await?;
        self.ensure_hold(&migration_id, &reason, now).await?;
        self.run_steps(plan, 0, &migration_id, snapshot, now).await
    }

    /// Recovers an unfinished run for `plan` after a restart.
    ///
    /// Reads the journal, re-closes the gate (a crash never opens the site) and then either
    /// resumes at the first incomplete step or restores per the rollback table of ADR-22. When
    /// the journal's plan digest is not `plan`'s, or the journal opens an unverified new version,
    /// it refuses.
    ///
    /// # Errors
    /// [`Error::InProgress`] when the journal is for another plan, [`Error::OpenUnverified`] when
    /// the journal is corrupt, and the errors [`ApplyEngine::apply`] can raise.
    pub async fn recover(&self, plan: &ReleasePlan, now: i64) -> Result<RecoveryOutcome> {
        let entries = self.journal.entries().await.map_err(journal_err)?;
        let progress = Progress::fold(entries);
        self.sync_seq(progress.last_seq);
        if !progress.started {
            let state = ApplyState::Idle;
            self.state_set(state.clone());
            return Ok(RecoveryOutcome {
                action: RecoveryAction::NotStarted,
                state,
            });
        }
        let plan_digest = plan.plan_digest.clone();
        if progress.plan_digest.as_deref() != Some(plan_digest.as_str()) {
            return Err(Error::InProgress {
                plan_digest: progress.plan_digest.clone().unwrap_or_default(),
            });
        }
        if let Some(reason) = progress.halted.clone() {
            let state = ApplyState::Halted { reason };
            self.state_set(state.clone());
            return Ok(RecoveryOutcome {
                action: RecoveryAction::Halted,
                state,
            });
        }
        if let Some((version, verified)) = progress.gate_open {
            if version == Version::New && !verified {
                return Err(Error::OpenUnverified { plan_digest });
            }
            let state = ApplyState::Open { version, verified };
            self.state_set(state.clone());
            return Ok(RecoveryOutcome {
                action: RecoveryAction::AlreadyOpen { version, verified },
                state,
            });
        }

        // Unfinished: fail closed before anything else.
        let snapshot =
            SnapshotId::new(
                progress
                    .snapshot
                    .clone()
                    .ok_or_else(|| Error::JournalFailed {
                        message: "the journal records no snapshot".to_owned(),
                    })?,
            )?;
        let migration_id = migration_id_for(plan);
        let reason = format!("migration apply recovery for plan {plan_digest}");
        self.close_gate(&reason, now).await?;

        // A rollback that started is finished first, so the site is never left half rolled back.
        if let Some((mechanism, restore_point)) = progress.restore_started.clone() {
            if !progress.restore_done {
                self.compensate(
                    RestoreCause::Recovery,
                    restore_point.as_ref(),
                    mechanism,
                    &snapshot,
                    &migration_id,
                    &plan_digest,
                    now,
                )
                .await?;
            }
            if mechanism == RestoreMechanism::Snapshot {
                if !progress.hold_finished {
                    self.discard_hold(&migration_id, "finished the interrupted rollback", now)
                        .await?;
                }
                self.open_gate(Version::Old, false, now).await?;
                return Ok(RecoveryOutcome {
                    action: RecoveryAction::Restored,
                    state: ApplyState::Open {
                        version: Version::Old,
                        verified: false,
                    },
                });
            }
            // A step-level compensation (a transaction abort or a checkpoint restore) was part of
            // a resume: fall through and resume.
        }
        if progress.verified {
            if !progress.hold_finished {
                self.release_hold(&migration_id, now).await?;
            }
            self.open_gate(Version::New, true, now).await?;
            return Ok(RecoveryOutcome {
                action: RecoveryAction::Resumed {
                    from_step: plan.steps().len(),
                },
                state: ApplyState::Open {
                    version: Version::New,
                    verified: true,
                },
            });
        }
        if !progress.hold_finished {
            self.ensure_hold(&migration_id, &reason, now).await?;
        }

        let steps = plan.steps();
        let total = steps.len();
        let from = first_incomplete(&progress.completed, total);
        if progress.verify_started || from >= total {
            self.progress.on(ProgressEvent::Resumed {
                from_step: total,
                at: now,
            });
            let outcome = self
                .run_steps(plan, total, &migration_id, &snapshot, now)
                .await?;
            return Ok(self.recovery_outcome(outcome, total));
        }

        match self.resume_decision(&progress, from) {
            Resume::Restore => {
                self.restore_to_old(
                    RestoreCause::Recovery,
                    &snapshot,
                    &migration_id,
                    &plan_digest,
                    now,
                )
                .await?;
                Ok(RecoveryOutcome {
                    action: RecoveryAction::Restored,
                    state: ApplyState::Open {
                        version: Version::Old,
                        verified: false,
                    },
                })
            }
            Resume::Plain => {
                self.progress.on(ProgressEvent::Resumed {
                    from_step: from,
                    at: now,
                });
                let outcome = self
                    .run_steps(plan, from, &migration_id, &snapshot, now)
                    .await?;
                Ok(self.recovery_outcome(outcome, from))
            }
            Resume::TransactionAbort => {
                self.compensate(
                    RestoreCause::Recovery,
                    None,
                    RestoreMechanism::TransactionAbort,
                    &snapshot,
                    &migration_id,
                    &plan_digest,
                    now,
                )
                .await?;
                self.progress.on(ProgressEvent::Resumed {
                    from_step: from,
                    at: now,
                });
                let outcome = self
                    .run_steps(plan, from, &migration_id, &snapshot, now)
                    .await?;
                Ok(self.recovery_outcome(outcome, from))
            }
            Resume::Checkpoint(restore_point) => {
                self.compensate(
                    RestoreCause::Recovery,
                    Some(&restore_point),
                    RestoreMechanism::Checkpoint,
                    &snapshot,
                    &migration_id,
                    &plan_digest,
                    now,
                )
                .await?;
                self.progress.on(ProgressEvent::Resumed {
                    from_step: from,
                    at: now,
                });
                let outcome = self
                    .run_steps(plan, from, &migration_id, &snapshot, now)
                    .await?;
                Ok(self.recovery_outcome(outcome, from))
            }
        }
    }

    /// The recovery outcome for a resumed run: `Resumed` when it opened the new version,
    /// `Restored` when the resumed run rolled back.
    fn recovery_outcome(&self, outcome: ApplyOutcome, from: usize) -> RecoveryOutcome {
        let action = match outcome {
            ApplyOutcome::Opened { .. } => RecoveryAction::Resumed { from_step: from },
            ApplyOutcome::Restored { .. } => RecoveryAction::Restored,
        };
        RecoveryOutcome {
            action,
            state: self.state(),
        }
    }

    /// Runs the steps from `from`, then verification, then opens the new version on a pass.
    async fn run_steps(
        &self,
        plan: &ReleasePlan,
        from: usize,
        migration_id: &str,
        snapshot: &SnapshotId,
        now: i64,
    ) -> Result<ApplyOutcome> {
        let steps = plan.steps();
        let total = steps.len();
        let plan_digest = plan.plan_digest.clone();
        for (index, step) in steps.iter().enumerate().skip(from) {
            let descriptor = self.steps.describe(index, step);
            let restore_point = if descriptor.transactional {
                None
            } else {
                self.steps.restore_point(&descriptor)
            };
            self.append(
                JournalRecord::StepStarted {
                    index,
                    id: step.id.clone(),
                    transactional: descriptor.transactional,
                    idempotency: descriptor.idempotency,
                    restore_point: restore_point.clone(),
                },
                now,
            )
            .await?;
            self.state_set(ApplyState::Applying { step: index, total });
            self.progress.on(ProgressEvent::StepStarted {
                index,
                id: step.id.clone(),
                at: now,
            });
            self.faults.at(&KillPoint::BeforeStep(index))?;
            let ctx = StepContext::new(&self.site, plan_digest.clone(), migration_id, now);
            match self.run_step(&descriptor, &ctx).await {
                Ok(outcome) => {
                    self.append(
                        JournalRecord::StepCompleted {
                            index,
                            id: step.id.clone(),
                            checkpoint: outcome.checkpoint.map(|checkpoint| checkpoint.key),
                        },
                        now,
                    )
                    .await?;
                    self.progress.on(ProgressEvent::StepCompleted {
                        index,
                        id: step.id.clone(),
                        at: now,
                    });
                    self.faults.at(&KillPoint::AfterStep(index))?;
                }
                Err(error) if error.is_killed() => return Err(error),
                Err(error) => {
                    self.append(
                        JournalRecord::StepFailed {
                            index,
                            id: step.id.clone(),
                            reason: error.to_string(),
                        },
                        now,
                    )
                    .await?;
                    return self
                        .restore_to_old(
                            RestoreCause::StepFailed { index },
                            snapshot,
                            migration_id,
                            &plan_digest,
                            now,
                        )
                        .await;
                }
            }
        }

        self.append(JournalRecord::VerifyStarted, now).await?;
        self.state_set(ApplyState::Verifying);
        self.progress.on(ProgressEvent::VerifyStarted { at: now });
        self.faults.at(&KillPoint::BeforeVerify)?;
        let outcome = match self.verify.verify(plan).await {
            Ok(outcome) => outcome,
            Err(error) if error.is_killed() => return Err(error),
            Err(_error) => {
                self.append(
                    JournalRecord::VerifyFailed {
                        mismatches: 0,
                        invariants: 0,
                    },
                    now,
                )
                .await?;
                self.progress.on(ProgressEvent::VerifyFailed {
                    mismatches: 0,
                    invariants: 0,
                    at: now,
                });
                return self
                    .restore_to_old(
                        RestoreCause::VerifyFailed,
                        snapshot,
                        migration_id,
                        &plan_digest,
                        now,
                    )
                    .await;
            }
        };
        if outcome.passed {
            self.append(JournalRecord::Verified, now).await?;
            self.progress.on(ProgressEvent::VerifyPassed { at: now });
            self.faults.at(&KillPoint::AfterVerify)?;
            self.release_hold(migration_id, now).await?;
            self.open_gate(Version::New, true, now).await?;
            Ok(ApplyOutcome::Opened {
                version: Version::New,
                verified: true,
            })
        } else {
            let mismatches = u64::try_from(outcome.mismatches.len()).unwrap_or(u64::MAX);
            let invariants = outcome.violations();
            self.append(
                JournalRecord::VerifyFailed {
                    mismatches,
                    invariants,
                },
                now,
            )
            .await?;
            self.progress.on(ProgressEvent::VerifyFailed {
                mismatches,
                invariants,
                at: now,
            });
            self.restore_to_old(
                RestoreCause::VerifyFailed,
                snapshot,
                migration_id,
                &plan_digest,
                now,
            )
            .await
        }
    }

    async fn run_step(
        &self,
        descriptor: &StepDescriptor,
        ctx: &StepContext,
    ) -> Result<StepOutcome> {
        let timeout = descriptor.timeout.unwrap_or(self.options.step_timeout);
        match tokio::time::timeout(timeout, self.steps.run(descriptor, ctx)).await {
            Ok(result) => result,
            Err(_) => Err(Error::StepTimeout {
                index: descriptor.index,
                id: descriptor.step.id.clone(),
                timeout_micros: micros(timeout),
            }),
        }
    }

    /// Compensates a rollback: aborts a transaction, restores a checkpoint, or restores the
    /// snapshot.
    #[allow(clippy::too_many_arguments)]
    async fn compensate(
        &self,
        cause: RestoreCause,
        restore_point: Option<&RestorePoint>,
        mechanism: RestoreMechanism,
        snapshot: &SnapshotId,
        migration_id: &str,
        plan_digest: &str,
        now: i64,
    ) -> Result<RestoreMechanism> {
        self.append(
            JournalRecord::RestoreStarted {
                cause: cause.clone(),
                mechanism,
                restore_point: restore_point.cloned(),
            },
            now,
        )
        .await?;
        self.state_set(ApplyState::Restoring {
            cause: cause.clone(),
        });
        self.progress
            .on(ProgressEvent::RestoreStarted { cause, at: now });
        self.faults.at(&KillPoint::BeforeRestore)?;
        match mechanism {
            RestoreMechanism::TransactionAbort => {}
            RestoreMechanism::Checkpoint => {
                let restore_point = restore_point.ok_or_else(|| Error::RestoreFailed {
                    message: "a checkpoint rollback needs a restore point".to_owned(),
                })?;
                let ctx = StepContext::new(&self.site, plan_digest, migration_id, now);
                self.steps
                    .rollback(restore_point, &ctx)
                    .await
                    .map_err(|error| Error::RestoreFailed {
                        message: error.to_string(),
                    })?;
            }
            RestoreMechanism::Snapshot => {
                self.snapshot
                    .restore(snapshot, now)
                    .await
                    .map_err(|error| Error::SnapshotFailed {
                        message: error.to_string(),
                    })?;
            }
        }
        self.append(JournalRecord::Restored { mechanism }, now)
            .await?;
        self.progress
            .on(ProgressEvent::Restored { mechanism, at: now });
        self.faults.at(&KillPoint::AfterRestore)?;
        Ok(mechanism)
    }

    /// Restores the snapshot, discards the held effects and opens the old version.
    async fn restore_to_old(
        &self,
        cause: RestoreCause,
        snapshot: &SnapshotId,
        migration_id: &str,
        plan_digest: &str,
        now: i64,
    ) -> Result<ApplyOutcome> {
        self.compensate(
            cause.clone(),
            None,
            RestoreMechanism::Snapshot,
            snapshot,
            migration_id,
            plan_digest,
            now,
        )
        .await?;
        let reason = format!("restored the old version after {}", cause_label(&cause));
        self.discard_hold(migration_id, &reason, now).await?;
        self.open_gate(Version::Old, false, now).await?;
        Ok(ApplyOutcome::Restored {
            cause,
            mechanism: RestoreMechanism::Snapshot,
        })
    }

    async fn close_gate(&self, reason: &str, now: i64) -> Result<()> {
        self.append(
            JournalRecord::GateClosing {
                reason: reason.to_owned(),
            },
            now,
        )
        .await?;
        self.progress.on(ProgressEvent::GateClosing {
            reason: reason.to_owned(),
            at: now,
        });
        self.gate
            .close(reason, now)
            .await
            .map_err(|error| Error::GateFailed {
                message: error.to_string(),
            })?;
        self.append(JournalRecord::GateClosed, now).await?;
        self.state_set(ApplyState::Gated {
            reason: reason.to_owned(),
            since: now,
        });
        self.progress.on(ProgressEvent::GateClosed { at: now });
        let report = self
            .gate
            .drain(self.options.drain_deadline_micros, now)
            .await
            .map_err(|error| Error::GateFailed {
                message: error.to_string(),
            })?;
        if report.timed_out {
            self.halt(
                format!(
                    "in-flight transactions did not drain within {} microseconds",
                    report.waited_micros
                ),
                now,
            )
            .await?;
            return Err(Error::DrainTimeout {
                waited_micros: report.waited_micros,
            });
        }
        Ok(())
    }

    async fn ensure_hold(&self, migration_id: &str, reason: &str, now: i64) -> Result<()> {
        self.append(
            JournalRecord::HoldBeginning {
                migration_id: migration_id.to_owned(),
            },
            now,
        )
        .await?;
        if let Err(error) = self.hold.begin(migration_id, reason, now).await {
            self.halt(
                format!("beginning the side-effect hold failed: {error}"),
                now,
            )
            .await?;
            return Err(error);
        }
        self.append(JournalRecord::HoldBegun, now).await?;
        self.progress.on(ProgressEvent::HoldBegun {
            migration_id: migration_id.to_owned(),
            at: now,
        });
        Ok(())
    }

    async fn release_hold(&self, migration_id: &str, now: i64) -> Result<()> {
        if let Err(error) = self.hold.release(migration_id, now).await {
            self.halt(format!("releasing the held effects failed: {error}"), now)
                .await?;
            return Err(error);
        }
        self.append(JournalRecord::HoldReleased, now).await?;
        self.progress.on(ProgressEvent::HoldReleased { at: now });
        Ok(())
    }

    async fn discard_hold(&self, migration_id: &str, reason: &str, now: i64) -> Result<()> {
        if let Err(error) = self.hold.discard(migration_id, reason, now).await {
            self.halt(format!("discarding the held effects failed: {error}"), now)
                .await?;
            return Err(error);
        }
        self.append(
            JournalRecord::HoldDiscarded {
                reason: reason.to_owned(),
            },
            now,
        )
        .await?;
        self.progress.on(ProgressEvent::HoldDiscarded {
            reason: reason.to_owned(),
            at: now,
        });
        Ok(())
    }

    async fn open_gate(&self, version: Version, verified: bool, now: i64) -> Result<()> {
        self.gate
            .open(now)
            .await
            .map_err(|error| Error::GateFailed {
                message: error.to_string(),
            })?;
        self.append(JournalRecord::GateOpened { version, verified }, now)
            .await?;
        self.state_set(ApplyState::Open { version, verified });
        self.progress.on(ProgressEvent::Opened {
            version,
            verified,
            at: now,
        });
        Ok(())
    }

    async fn halt(&self, reason: String, now: i64) -> Result<()> {
        self.append(
            JournalRecord::Halted {
                reason: reason.clone(),
            },
            now,
        )
        .await?;
        self.state_set(ApplyState::Halted {
            reason: reason.clone(),
        });
        self.progress.on(ProgressEvent::Halted { reason, at: now });
        Ok(())
    }

    async fn append(&self, record: JournalRecord, now: i64) -> Result<()> {
        let seq = self.next_seq.load(std::sync::atomic::Ordering::SeqCst);
        self.faults.at(&KillPoint::BeforeJournal(seq))?;
        self.journal
            .append(seq, record, now)
            .await
            .map_err(journal_err)?;
        self.next_seq
            .store(seq + 1, std::sync::atomic::Ordering::SeqCst);
        self.faults.at(&KillPoint::AfterJournal(seq))?;
        Ok(())
    }

    fn sync_seq(&self, last_seq: Seq) {
        self.next_seq
            .store(last_seq + 1, std::sync::atomic::Ordering::SeqCst);
    }

    fn state_set(&self, state: ApplyState) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = state;
    }

    /// Whether a restart may resume the step at `from`, and how it must make the step clean
    /// first.
    fn resume_decision(&self, progress: &Progress, from: usize) -> Resume {
        match &progress.started_step {
            Some(start) if start.index == from => {
                if start.idempotency != Some(Idempotency::Idempotent) {
                    // An undeclared or at-most-once step is never run twice: restore instead.
                    return Resume::Restore;
                }
                if start.transactional {
                    // The step's transaction aborted with the crash: nothing was committed.
                    return Resume::TransactionAbort;
                }
                match &start.restore_point {
                    Some(restore_point) => Resume::Checkpoint(restore_point.clone()),
                    // A non-transactional step with no compensating point cannot be made clean.
                    None => Resume::Restore,
                }
            }
            // A step out of order means the journal is not one we understand: fail closed.
            Some(_) => Resume::Restore,
            None => Resume::Plain,
        }
    }
}

impl ApplyService for ApplyEngine {
    fn apply(&self, request: ApplyRequest) -> BoxFuture<'_, Result<ApplyOutcome>> {
        Box::pin(async move { ApplyEngine::apply(self, request).await })
    }

    fn recover<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        now: i64,
    ) -> BoxFuture<'a, Result<RecoveryOutcome>> {
        Box::pin(async move { ApplyEngine::recover(self, plan, now).await })
    }

    fn state(&self) -> ApplyState {
        ApplyEngine::state(self)
    }
}

/// How a restart must make an incomplete step clean before it resumes.
enum Resume {
    /// The step never started; run it directly.
    Plain,
    /// A transactional step's transaction aborted; run it directly.
    TransactionAbort,
    /// A non-transactional step is rolled back to its restore point, then run.
    Checkpoint(RestorePoint),
    /// The step cannot be made clean; restore the snapshot.
    Restore,
}

/// The first index in `0..total` that is not completed.
fn first_incomplete(completed: &BTreeMap<usize, Option<String>>, total: usize) -> usize {
    (0..total)
        .find(|index| !completed.contains_key(index))
        .unwrap_or(total)
}

/// The migration id an apply run uses for its hold.
fn migration_id_for(plan: &ReleasePlan) -> String {
    format!("migration:{}", plan.plan_digest)
}

/// A short label for a rollback cause.
fn cause_label(cause: &RestoreCause) -> String {
    match cause {
        RestoreCause::StepFailed { index } => format!("step {index} failed"),
        RestoreCause::VerifyFailed => "verification failed".to_owned(),
        RestoreCause::Recovery => "recovery".to_owned(),
    }
}

fn journal_err(error: Error) -> Error {
    Error::JournalFailed {
        message: error.to_string(),
    }
}

fn micros(duration: Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}
