//! The fix engine: the state machine that plans, rehearses, applies, verifies and rolls back.
//!
//! # The lifecycle
//!
//! `Proposed -> Planned -> Rehearsed -> Applied -> Verified`, with the failure edges
//! `-> RolledBack` (an apply or a verification failed and the inverse plus the residue check
//! proved the pre-state is back) and `-> Failed` (no usable change, or the rollback itself left a
//! residue). Every transition and every step is written to the [`FixJournal`] **before** it
//! happens, and every transition is recorded on the AI audit chain.
//!
//! # The hard floor, made structural
//!
//! There is no apply path in this engine that does not first journal `ApplyStarted`, take a
//! snapshot for a T3/large plan or capture a pre-image for every affected target, and journal
//! the live pre-state fingerprint. A failed verification runs the inverse, re-runs the
//! invariants and compares the pre-state fingerprint with the post-rollback fingerprint; only an
//! empty diff is [`FixState::RolledBack`]. A non-empty diff is [`FixState::Failed`], never a
//! claim that the world is back.
//!
//! # A restart
//!
//! [`FixEngine::recover`] reads the journal. An apply in flight is rolled back; an apply that
//! finished is verified (and rolled back if verification fails); a run that never applied is
//! resumed from its rehearsal. Time is a caller argument and the engine reads no clock.

use spot_agent_tiers::Approval;
use spot_rehearsal::Fingerprint;

use crate::audit::FixAuditEvent;
use crate::error::{FixError, Result};
use crate::faults::{Faults, KillPoint};
use crate::journal::{FixJournal, JournalRecord, Phase, Progress};
use crate::model::{
    AgentRef, ChangeRecord, DeploymentEpoch, FailureStage, FixId, FixOutcome, FixPlan, FixState,
    Mechanism, ResidueEvidence, RollbackCause, RollbackEvidence, UndoWindow,
};
use crate::policy::{FixPolicy, PlanPath};
use crate::ports::{
    ApprovalPort, FixAudit, FixExecutor, FixVerifier, PreImageStore, RehearsalPort, SnapshotPort,
};

/// The ports one engine drives.
pub struct FixPorts<'a> {
    /// The logical pre-image store.
    pub pre_images: &'a mut dyn PreImageStore,
    /// The snapshot and maintenance-gate port.
    pub snapshots: &'a mut dyn SnapshotPort,
    /// The step executor and its inverse.
    pub executor: &'a mut dyn FixExecutor,
    /// The live verifier and fingerprint source.
    pub verifier: &'a mut dyn FixVerifier,
    /// The rehearsal engine.
    pub rehearsal: &'a mut dyn RehearsalPort,
    /// The T3 approval inbox.
    pub approval: &'a mut dyn ApprovalPort,
}

/// The facts every operation of one run shares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixContext {
    /// The site (tenant) the fix belongs to.
    pub site: String,
    /// The agent the run acts as.
    pub agent: AgentRef,
    /// The deployment epoch.
    pub epoch: DeploymentEpoch,
    /// The instant (unix seconds), supplied by the caller.
    pub now: i64,
}

impl FixContext {
    /// A context for one run.
    #[must_use]
    pub fn new(site: &str, agent: AgentRef, epoch: DeploymentEpoch, now: i64) -> Self {
        Self {
            site: site.to_owned(),
            agent,
            epoch,
            now,
        }
    }
}

/// The fix engine.
pub struct FixEngine<'a> {
    policy: FixPolicy,
    journal: &'a dyn FixJournal,
    audit: &'a mut dyn FixAudit,
    ports: FixPorts<'a>,
    faults: &'a dyn Faults,
}

impl<'a> FixEngine<'a> {
    /// An engine over a policy, a journal, an audit sink, the ports and a fault switch.
    #[must_use]
    pub fn new(
        policy: FixPolicy,
        journal: &'a dyn FixJournal,
        audit: &'a mut dyn FixAudit,
        ports: FixPorts<'a>,
        faults: &'a dyn Faults,
    ) -> Self {
        Self {
            policy,
            journal,
            audit,
            ports,
            faults,
        }
    }

    /// The journaled progress of `fix`.
    ///
    /// # Errors
    ///
    /// [`FixError::JournalFailed`] when the journal cannot be read, or
    /// [`FixError::WrongState`] when the fix has no records.
    pub fn progress(&self, fix: &FixId) -> Result<Progress> {
        let entries = self
            .journal
            .entries(fix)
            .map_err(|error| FixError::JournalFailed {
                detail: error.to_string(),
            })?;
        if entries.is_empty() {
            return Err(FixError::WrongState {
                state: "absent".to_owned(),
                action: "read".to_owned(),
            });
        }
        Ok(Progress::fold(fix, entries))
    }

    /// Runs a new fix end to end: propose, plan, rehearse when policy requires, approve a T3
    /// plan, apply, verify, and roll back automatically when any check fails.
    ///
    /// A lifecycle refusal (approval, rehearsal, verification) is journaled and returned as an
    /// outcome in [`FixState::Failed`] or [`FixState::RolledBack`]; an infrastructure failure or
    /// a kill is an `Err`.
    ///
    /// # Errors
    ///
    /// [`FixError::WrongState`] when the fix already has journal entries;
    /// [`FixError::InvalidPlan`] when the plan's hash does not verify;
    /// [`FixError::Refused`] when the agent may not run it; and journal, audit, pre-image,
    /// snapshot or executor failures.
    pub fn run(&mut self, fix: &FixId, plan: &FixPlan, ctx: &FixContext) -> Result<FixOutcome> {
        let existing = self
            .journal
            .entries(fix)
            .map_err(|error| FixError::JournalFailed {
                detail: error.to_string(),
            })?;
        if !existing.is_empty() {
            return Err(FixError::WrongState {
                state: Progress::fold(fix, existing).state().as_str().to_owned(),
                action: "run a new fix".to_owned(),
            });
        }
        if !plan.verify() {
            return Err(FixError::InvalidPlan {
                detail: "the plan's hash does not bind its contents".to_owned(),
            });
        }
        self.policy.admit(plan, &ctx.agent)?;

        self.faults.at(&KillPoint::BeforeProposed)?;
        self.write(
            fix,
            ctx,
            JournalRecord::Proposed {
                site: ctx.site.clone(),
                agent: ctx.agent.clone(),
                epoch: ctx.epoch,
            },
        )?;
        self.faults.at(&KillPoint::AfterProposed)?;

        let path = self.policy.path(plan);
        if path == PlanPath::Direct && plan.tier == spot_agent_tiers::Tier::T3 {
            return Err(FixError::RehearsalRequired);
        }
        self.faults.at(&KillPoint::BeforePlanned)?;
        self.write(
            fix,
            ctx,
            JournalRecord::Planned {
                plan: Box::new(plan.clone()),
                path,
            },
        )?;
        self.faults.at(&KillPoint::AfterPlanned)?;

        self.advance(fix, plan, ctx)
    }

    /// Finishes a run the journal shows was interrupted, or explains that it already finished.
    ///
    /// An apply in flight is rolled back; an apply that finished is verified and rolled back on
    /// failure; a rehearsal is re-run when it was in flight; a run that never applied is resumed
    /// from its plan. A kill at any journal boundary therefore ends either verified or
    /// byte-identical to the pre-state.
    ///
    /// # Errors
    ///
    /// The infrastructure errors of [`FixEngine::run`], and [`FixError::WrongState`] when the fix
    /// has no records.
    pub fn recover(&mut self, fix: &FixId, ctx: &FixContext) -> Result<FixOutcome> {
        let entries = self
            .journal
            .entries(fix)
            .map_err(|error| FixError::JournalFailed {
                detail: error.to_string(),
            })?;
        if entries.is_empty() {
            // The kill arrived before the proposal was journaled: no change was attempted and
            // the fix is simply still proposed.
            return Ok(FixOutcome::new(fix.clone(), FixState::Proposed));
        }
        let progress = Progress::fold(fix, entries);
        let plan = match &progress.plan {
            Some(plan) => plan.clone(),
            None => return Ok(progress.outcome()),
        };
        Self::check_site(&progress, ctx)?;
        self.policy.admit(&plan, &ctx.agent)?;

        match progress.phase() {
            Phase::Undoing => self.finish_undo(fix, &plan, ctx, true),
            Phase::RollingBack => {
                let cause = progress.rollback_cause.unwrap_or(RollbackCause::Recovery);
                self.rollback(fix, &plan, ctx, cause)
            }
            Phase::Applying => self.rollback(fix, &plan, ctx, RollbackCause::Recovery),
            Phase::Verifying => self.verify(fix, &plan, ctx),
            Phase::Settled => match progress.state() {
                FixState::Verified | FixState::RolledBack | FixState::Failed => {
                    Ok(progress.outcome())
                }
                FixState::Applied => self.verify(fix, &plan, ctx),
                FixState::Rehearsed => {
                    if self.approve(fix, &plan, ctx, &progress)? {
                        self.apply_and_verify(fix, &plan, ctx)
                    } else {
                        Ok(self.progress(fix)?.outcome())
                    }
                }
                FixState::Planned | FixState::Proposed => {
                    // Nothing was applied: resuming is safe. A rehearsal that was required but
                    // not finished runs again; an approval already given is not asked twice.
                    let path = progress.path.unwrap_or_else(|| self.policy.path(&plan));
                    if path == PlanPath::Rehearse
                        && progress.rehearsal.is_none()
                        && !self.rehearse(fix, &plan, ctx)?
                    {
                        return Ok(self.progress(fix)?.outcome());
                    }
                    if self.approve(fix, &plan, ctx, &progress)? {
                        self.apply_and_verify(fix, &plan, ctx)
                    } else {
                        Ok(self.progress(fix)?.outcome())
                    }
                }
            },
        }
    }

    /// Undoes a verified fix inside its window, with conflict detection.
    ///
    /// # Errors
    ///
    /// [`FixError::UndoExpired`] when the window has closed,
    /// [`FixError::UndoConflict`] when a target changed after the fix,
    /// [`FixError::UndoUnavailable`] when the fix is not verified, and the same infrastructure
    /// errors as [`FixEngine::run`].
    pub fn undo(&mut self, fix: &FixId, ctx: &FixContext, reason: &str) -> Result<FixOutcome> {
        let progress = self.progress(fix)?;
        Self::check_site(&progress, ctx)?;
        let plan = progress
            .plan
            .clone()
            .ok_or_else(|| FixError::UndoUnavailable {
                state: progress.state().as_str().to_owned(),
            })?;
        let window = progress.undo.ok_or_else(|| FixError::UndoUnavailable {
            state: progress.state().as_str().to_owned(),
        })?;
        if progress.state() != FixState::Verified {
            return Err(FixError::UndoUnavailable {
                state: progress.state().as_str().to_owned(),
            });
        }
        if window.closed || window.is_expired(ctx.now) {
            self.write(
                fix,
                ctx,
                JournalRecord::UndoWindowClosed {
                    at: ctx.now,
                    reason: "expired".to_owned(),
                },
            )?;
            self.write(
                fix,
                ctx,
                JournalRecord::UndoRefused {
                    reason: "the undo window closed".to_owned(),
                    code: crate::error::codes::UNDO_EXPIRED.to_owned(),
                },
            )?;
            return Err(FixError::UndoExpired {
                expires_at: window.expires_at,
            });
        }
        if !plan.reversible && progress.snapshot.is_none() {
            return Err(FixError::Irreversible {
                detail: "the plan declares no inverse and no snapshot was taken".to_owned(),
            });
        }
        self.detect_conflict(&progress, &plan, ctx)?;

        self.faults.at(&KillPoint::BeforeUndo)?;
        self.write(
            fix,
            ctx,
            JournalRecord::UndoStarted {
                mechanism: progress.mechanism.unwrap_or(Mechanism::PreImage),
                reason: reason.to_owned(),
            },
        )?;
        self.finish_undo(fix, &plan, ctx, false)
    }

    /// Closes the undo window when `now` is past its expiry.
    ///
    /// Returns whether the window was open and is now closed. The fix itself stays
    /// [`FixState::Verified`].
    ///
    /// # Errors
    ///
    /// [`FixError::JournalFailed`] or [`FixError::AuditFailed`] when the observation cannot be
    /// recorded, and [`FixError::WrongState`] when the fix has no records.
    pub fn close_expired(&mut self, fix: &FixId, now: i64) -> Result<bool> {
        let progress = self.progress(fix)?;
        let Some(window) = progress.undo else {
            return Ok(false);
        };
        if progress.state() != FixState::Verified || window.closed || !window.is_expired(now) {
            return Ok(false);
        }
        let ctx = FixContext::new(
            progress.site.as_deref().unwrap_or("unknown"),
            progress
                .agent
                .clone()
                .unwrap_or_else(|| AgentRef::new("system", spot_agent_tiers::Tier::T0, false)),
            progress.epoch.unwrap_or(DeploymentEpoch::new(0)),
            now,
        );
        self.write(
            fix,
            &ctx,
            JournalRecord::UndoWindowClosed {
                at: now,
                reason: "expired".to_owned(),
            },
        )?;
        Ok(true)
    }

    /// Refuses a context for a different site than the one the fix was proposed on, so one
    /// tenant's engine can never roll back or undo another tenant's fix.
    fn check_site(progress: &Progress, ctx: &FixContext) -> Result<()> {
        match progress.site.as_deref() {
            Some(site) if site != ctx.site => Err(FixError::Refused {
                detail: "the fix belongs to another site".to_owned(),
            }),
            _ => Ok(()),
        }
    }

    // -- the lifecycle ----------------------------------------------------------------

    /// The plan's path and approval, then apply and verify.
    fn advance(&mut self, fix: &FixId, plan: &FixPlan, ctx: &FixContext) -> Result<FixOutcome> {
        let path = self.policy.path(plan);
        if path == PlanPath::Rehearse && !self.rehearse(fix, plan, ctx)? {
            return Ok(self.progress(fix)?.outcome());
        }
        let progress = self.progress(fix)?;
        if !self.approve(fix, plan, ctx, &progress)? {
            return Ok(self.progress(fix)?.outcome());
        }
        self.apply_and_verify(fix, plan, ctx)
    }

    fn apply_and_verify(
        &mut self,
        fix: &FixId,
        plan: &FixPlan,
        ctx: &FixContext,
    ) -> Result<FixOutcome> {
        if !self.apply(fix, plan, ctx)? {
            return Ok(self.progress(fix)?.outcome());
        }
        self.verify(fix, plan, ctx)
    }

    /// Rehearses the plan on a clone. A rehearsal failure ends the fix with evidence and no
    /// change. Returns whether the rehearsal passed.
    fn rehearse(&mut self, fix: &FixId, plan: &FixPlan, ctx: &FixContext) -> Result<bool> {
        self.faults.at(&KillPoint::BeforeRehearsal)?;
        self.write(
            fix,
            ctx,
            JournalRecord::RehearsalStarted {
                plan_hash: plan.hash.as_str().to_owned(),
            },
        )?;
        let evidence = match self.ports.rehearsal.rehearse(plan) {
            Ok(evidence) => evidence,
            Err(error) => {
                let reason = error.to_string();
                self.write(
                    fix,
                    ctx,
                    JournalRecord::RehearsalFailed {
                        reason: reason.clone(),
                        evidence: None,
                    },
                )?;
                self.fail(fix, ctx, FailureStage::Rehearsal, &reason)?;
                return Ok(false);
            }
        };
        if !evidence.is_clean() {
            let reason = "the rehearsal's invariants did not hold".to_owned();
            self.write(
                fix,
                ctx,
                JournalRecord::RehearsalFailed {
                    reason: reason.clone(),
                    evidence: Some(Box::new(evidence)),
                },
            )?;
            self.fail(fix, ctx, FailureStage::Rehearsal, &reason)?;
            return Ok(false);
        }
        self.write(
            fix,
            ctx,
            JournalRecord::Rehearsed {
                evidence: Box::new(evidence),
            },
        )?;
        self.faults.at(&KillPoint::AfterRehearsal)?;
        Ok(true)
    }

    /// Gets the approval a T3 plan needs. Returns whether the fix may apply.
    fn approve(
        &mut self,
        fix: &FixId,
        plan: &FixPlan,
        ctx: &FixContext,
        progress: &Progress,
    ) -> Result<bool> {
        if !self.policy.requires_approval(plan, &ctx.agent) {
            return Ok(true);
        }
        if progress.approval.is_some() || progress.self_approved_by.is_some() {
            return Ok(true);
        }
        if self.policy.may_self_approve(plan, &ctx.agent) {
            self.write(
                fix,
                ctx,
                JournalRecord::SelfApproved {
                    by: ctx.agent.id.clone(),
                },
            )?;
            return Ok(true);
        }
        self.write(
            fix,
            ctx,
            JournalRecord::ApprovalRequested {
                reason: format!("a T3 plan ({}) needs a human approval", plan.hash),
            },
        )?;
        let approval = self.ports.approval.request(
            fix,
            plan,
            &ctx.agent,
            "a T3 structural fix needs approval",
        )?;
        let Some(approval) = approval else {
            let reason = "no approval was given".to_owned();
            self.write(
                fix,
                ctx,
                JournalRecord::ApprovalRefused {
                    reason: reason.clone(),
                },
            )?;
            self.fail(fix, ctx, FailureStage::Approval, &reason)?;
            return Ok(false);
        };
        if !valid_approval(&approval) || approval.approver == ctx.agent.id {
            let reason = "the approval is not a person's approval with a reference".to_owned();
            self.write(
                fix,
                ctx,
                JournalRecord::ApprovalRefused {
                    reason: reason.clone(),
                },
            )?;
            self.fail(fix, ctx, FailureStage::Approval, &reason)?;
            return Ok(false);
        }
        self.write(fix, ctx, JournalRecord::Approved { approval })?;
        Ok(true)
    }

    /// Applies the plan. Returns whether every step ran.
    fn apply(&mut self, fix: &FixId, plan: &FixPlan, ctx: &FixContext) -> Result<bool> {
        let mechanism = self.policy.mechanism(plan);
        self.faults.at(&KillPoint::BeforeApply)?;
        self.write(
            fix,
            ctx,
            JournalRecord::ApplyStarted {
                mechanism,
                plan_hash: plan.hash.as_str().to_owned(),
            },
        )?;

        if mechanism == Mechanism::Snapshot {
            self.faults.at(&KillPoint::BeforeSnapshot)?;
            if self.policy.requires_maintenance_gate(plan) {
                self.ports
                    .snapshots
                    .close_gate(&format!("fix {fix} applies a structural plan"))?;
            }
            let snapshot = match self.ports.snapshots.take(fix) {
                Ok(snapshot) => snapshot,
                Err(error) => return self.abort_apply(fix, plan, ctx, &error.to_string()),
            };
            self.write(fix, ctx, JournalRecord::SnapshotTaken { snapshot })?;
            self.faults.at(&KillPoint::AfterSnapshot)?;
        }

        self.faults.at(&KillPoint::BeforePreImages)?;
        let images = match self.ports.pre_images.capture(&plan.affected) {
            Ok(images) => images,
            Err(error) => return self.abort_apply(fix, plan, ctx, &error.to_string()),
        };
        self.write(fix, ctx, JournalRecord::PreImagesCaptured { images })?;
        let pre = match self.ports.verifier.fingerprint() {
            Ok(fingerprint) => fingerprint,
            Err(error) => return self.abort_apply(fix, plan, ctx, &error.to_string()),
        };
        self.write(
            fix,
            ctx,
            JournalRecord::PreFingerprintCaptured {
                fingerprint: Box::new(pre),
            },
        )?;
        self.faults.at(&KillPoint::AfterPreImages)?;

        let mut changes = Vec::with_capacity(plan.steps.len());
        for step in &plan.steps {
            self.faults.at(&KillPoint::BeforeStep(step.index))?;
            let key = plan.idempotency_key(fix, step);
            self.write(
                fix,
                ctx,
                JournalRecord::StepStarted {
                    index: step.index,
                    id: step.id.clone(),
                    idempotency_key: key.clone(),
                },
            )?;
            let change = match self.ports.executor.run_step(step, &key) {
                Ok(change) => change,
                Err(error) => {
                    let reason = error.to_string();
                    self.write(
                        fix,
                        ctx,
                        JournalRecord::StepFailed {
                            index: step.index,
                            reason,
                        },
                    )?;
                    return self
                        .rollback(
                            fix,
                            plan,
                            ctx,
                            RollbackCause::StepFailed { index: step.index },
                        )
                        // The rollback is terminal (rolled back or failed): the apply did not
                        // complete, so the caller must not go on to verify.
                        .map(|_| false);
                }
            };
            let record = ChangeRecord::new(step.index, &step.target, &change.before, &change.after);
            self.write(
                fix,
                ctx,
                JournalRecord::StepApplied {
                    change: record.clone(),
                },
            )?;
            changes.push(record);
            self.faults.at(&KillPoint::AfterStep(step.index))?;
        }
        self.faults.at(&KillPoint::BeforeApplied)?;
        self.write(fix, ctx, JournalRecord::Applied { changes })?;
        self.faults.at(&KillPoint::AfterApplied)?;
        Ok(true)
    }

    /// A failure before any step ran: restore the snapshot when one was taken, then fail.
    fn abort_apply(
        &mut self,
        fix: &FixId,
        _plan: &FixPlan,
        ctx: &FixContext,
        reason: &str,
    ) -> Result<bool> {
        let progress = self.progress(fix)?;
        if let Some(snapshot) = &progress.snapshot {
            self.ports.snapshots.restore(snapshot)?;
        }
        self.ports.snapshots.open_gate()?;
        self.fail(fix, ctx, FailureStage::Apply, reason)?;
        Ok(false)
    }

    /// Verifies the applied plan, and rolls back automatically when any check fails.
    fn verify(&mut self, fix: &FixId, plan: &FixPlan, ctx: &FixContext) -> Result<FixOutcome> {
        self.faults.at(&KillPoint::BeforeVerify)?;
        self.write(fix, ctx, JournalRecord::VerifyStarted)?;
        let progress = self.progress(fix)?;
        let report = match self.ports.verifier.verify(plan, &progress.changes) {
            Ok(report) => report,
            Err(error) => {
                let reason = error.to_string();
                self.write(
                    fix,
                    ctx,
                    JournalRecord::VerifyFailed {
                        report: None,
                        reasons: vec![reason],
                    },
                )?;
                return self.rollback(fix, plan, ctx, RollbackCause::VerifyFailed);
            }
        };
        if !report.passed() {
            let reasons = report.failures();
            self.write(
                fix,
                ctx,
                JournalRecord::VerifyFailed {
                    report: Some(Box::new(report)),
                    reasons,
                },
            )?;
            return self.rollback(fix, plan, ctx, RollbackCause::VerifyFailed);
        }
        self.faults.at(&KillPoint::AfterVerify)?;
        if progress.mechanism == Some(Mechanism::Snapshot) {
            self.ports.snapshots.open_gate()?;
        }
        self.write(
            fix,
            ctx,
            JournalRecord::Verified {
                report: Box::new(report),
            },
        )?;
        let window = UndoWindow::open(ctx.now, self.policy.undo_window_secs);
        self.write(fix, ctx, JournalRecord::UndoWindowOpened { window })?;
        Ok(self.progress(fix)?.outcome())
    }

    /// Runs the inverse, re-checks the invariants and proves the pre-state is back.
    fn rollback(
        &mut self,
        fix: &FixId,
        plan: &FixPlan,
        ctx: &FixContext,
        cause: RollbackCause,
    ) -> Result<FixOutcome> {
        let progress = self.progress(fix)?;
        let mechanism = if progress.snapshot.is_some() {
            Mechanism::Snapshot
        } else {
            Mechanism::PreImage
        };
        self.faults.at(&KillPoint::BeforeRollback)?;
        self.write(
            fix,
            ctx,
            JournalRecord::RollbackStarted { cause, mechanism },
        )?;
        if let Err(error) = self.perform_inverse(&progress, plan, mechanism) {
            return self.rollback_failed(fix, ctx, &error.to_string(), None);
        }
        self.faults.at(&KillPoint::BeforeResidue)?;
        let invariants = match self.ports.verifier.invariants() {
            Ok(invariants) => invariants,
            Err(error) => return self.rollback_failed(fix, ctx, &error.to_string(), None),
        };
        let after = match self.ports.verifier.fingerprint() {
            Ok(fingerprint) => fingerprint,
            Err(error) => return self.rollback_failed(fix, ctx, &error.to_string(), None),
        };
        let (residue, provable) = self.residue(&progress, &after);
        if let Some(residue) = &residue {
            self.write(
                fix,
                ctx,
                JournalRecord::ResidueChecked {
                    residue: residue.clone(),
                },
            )?;
        }
        let clean = invariants.passed()
            && provable
            && residue.as_ref().is_none_or(ResidueEvidence::is_empty);
        let evidence = RollbackEvidence {
            cause,
            mechanism,
            invariants,
            residue,
        };
        if !clean {
            let reason = if !provable {
                "the pre-state fingerprint is missing, so the rollback cannot be proven clean"
                    .to_owned()
            } else if evidence.invariants.passed() {
                "the rollback left a residue".to_owned()
            } else {
                "the invariants did not hold after the rollback".to_owned()
            };
            return self.rollback_failed(fix, ctx, &reason, Some(evidence));
        }
        if mechanism == Mechanism::Snapshot {
            self.ports.snapshots.open_gate()?;
        }
        self.write(
            fix,
            ctx,
            JournalRecord::RolledBack {
                evidence: Box::new(evidence),
            },
        )?;
        self.faults.at(&KillPoint::AfterRollback)?;
        Ok(self.progress(fix)?.outcome())
    }

    fn rollback_failed(
        &mut self,
        fix: &FixId,
        ctx: &FixContext,
        reason: &str,
        evidence: Option<RollbackEvidence>,
    ) -> Result<FixOutcome> {
        self.write(
            fix,
            ctx,
            JournalRecord::RollbackFailed {
                reason: reason.to_owned(),
                evidence: evidence.map(Box::new),
            },
        )?;
        self.fail(fix, ctx, FailureStage::Rollback, reason)?;
        Ok(self.progress(fix)?.outcome())
    }

    /// Finishes an undo, with conflict detection unless the undo was already in flight.
    fn finish_undo(
        &mut self,
        fix: &FixId,
        plan: &FixPlan,
        ctx: &FixContext,
        recovering: bool,
    ) -> Result<FixOutcome> {
        let progress = self.progress(fix)?;
        if !recovering {
            self.detect_conflict(&progress, plan, ctx)?;
        }
        let mechanism = progress.mechanism.unwrap_or(Mechanism::PreImage);
        if let Err(error) = self.perform_inverse(&progress, plan, mechanism) {
            self.write(
                fix,
                ctx,
                JournalRecord::UndoRefused {
                    reason: error.to_string(),
                    code: error.code().to_owned(),
                },
            )?;
            self.fail(fix, ctx, FailureStage::Undo, &error.to_string())?;
            return Ok(self.progress(fix)?.outcome());
        }
        let invariants = self.ports.verifier.invariants()?;
        let after = self.ports.verifier.fingerprint()?;
        let (residue, provable) = self.residue(&progress, &after);
        if let Some(residue) = &residue {
            self.write(
                fix,
                ctx,
                JournalRecord::ResidueChecked {
                    residue: residue.clone(),
                },
            )?;
        }
        let clean = invariants.passed()
            && provable
            && residue.as_ref().is_none_or(ResidueEvidence::is_empty);
        let evidence = RollbackEvidence {
            cause: RollbackCause::Undo,
            mechanism,
            invariants,
            residue,
        };
        if !clean {
            self.fail(fix, ctx, FailureStage::Undo, "the undo left a residue")?;
            return Ok(self.progress(fix)?.outcome());
        }
        if mechanism == Mechanism::Snapshot {
            self.ports.snapshots.open_gate()?;
        }
        self.write(
            fix,
            ctx,
            JournalRecord::Undone {
                evidence: Box::new(evidence),
            },
        )?;
        self.faults.at(&KillPoint::AfterUndo)?;
        Ok(self.progress(fix)?.outcome())
    }

    /// Runs the inverse for the recorded changes and restores every pre-image.
    fn perform_inverse(
        &mut self,
        progress: &Progress,
        plan: &FixPlan,
        mechanism: Mechanism,
    ) -> Result<()> {
        match mechanism {
            Mechanism::Snapshot => {
                let snapshot =
                    progress
                        .snapshot
                        .as_ref()
                        .ok_or_else(|| FixError::SnapshotFailed {
                            detail: "the journal has no snapshot to restore".to_owned(),
                        })?;
                self.ports.snapshots.restore(snapshot)
            }
            Mechanism::PreImage => {
                for change in progress.changes.iter().rev() {
                    if let Some(step) = plan.steps.get(change.step) {
                        self.ports.executor.undo_step(step, change)?;
                    }
                }
                for image in progress.pre_images.iter().rev() {
                    self.ports.pre_images.restore(image)?;
                }
                Ok(())
            }
        }
    }

    /// The residue between the pre-state and `after`.
    ///
    /// `provable` is false when a change was made but no pre-state fingerprint was captured,
    /// which must fail closed rather than claim the pre-state is back.
    fn residue(&self, progress: &Progress, after: &Fingerprint) -> (Option<ResidueEvidence>, bool) {
        match &progress.pre_fingerprint {
            Some(pre) => (Some(ResidueEvidence::measure(pre, after)), true),
            None if progress.changes.is_empty() && progress.completed_steps.is_empty() => {
                // Nothing ran: the current state *is* the pre-state, and the empty diff proves
                // it. A crash before the pre-fingerprint is journaled can only reach here.
                let digest = after.digest();
                (
                    Some(ResidueEvidence {
                        pre_digest: digest.clone(),
                        after_digest: digest,
                        differences: Vec::new(),
                    }),
                    true,
                )
            }
            None => (None, false),
        }
    }

    /// Refuses an undo when a target changed after the fix.
    fn detect_conflict(
        &mut self,
        progress: &Progress,
        _plan: &FixPlan,
        ctx: &FixContext,
    ) -> Result<()> {
        if progress.mechanism.unwrap_or(Mechanism::PreImage) == Mechanism::Snapshot {
            let expected = progress
                .verify
                .as_ref()
                .map(|report| report.fingerprint.digest())
                .ok_or_else(|| FixError::UndoUnavailable {
                    state: "verified without a fingerprint".to_owned(),
                })?;
            let current = self.ports.verifier.fingerprint()?.digest();
            if current != expected {
                self.write(
                    &progress.fix,
                    ctx,
                    JournalRecord::UndoRefused {
                        reason: "the site changed after the fix".to_owned(),
                        code: crate::error::codes::UNDO_CONFLICT.to_owned(),
                    },
                )?;
                return Err(FixError::UndoConflict {
                    target: "site".to_owned(),
                });
            }
            return Ok(());
        }
        for change in &progress.changes {
            let current = self.ports.pre_images.digest(&change.target)?;
            if current != change.after {
                self.write(
                    &progress.fix,
                    ctx,
                    JournalRecord::UndoRefused {
                        reason: format!("`{}` changed after the fix", change.target),
                        code: crate::error::codes::UNDO_CONFLICT.to_owned(),
                    },
                )?;
                return Err(FixError::UndoConflict {
                    target: change.target.clone(),
                });
            }
        }
        Ok(())
    }

    // -- journal and audit ------------------------------------------------------------

    /// Journals a record, then records the resulting transition on the AI audit chain.
    fn write(&mut self, fix: &FixId, ctx: &FixContext, record: JournalRecord) -> Result<()> {
        let action = record.audit_action();
        let detail = record.audit_detail();
        self.journal.append(fix, ctx.now, record)?;
        let progress = self.progress(fix)?;
        self.audit.record(FixAuditEvent {
            fix: fix.clone(),
            state: progress.state(),
            action,
            agent: ctx.agent.clone(),
            epoch: ctx.epoch,
            at: ctx.now,
            detail,
        })
    }

    /// Journals and audits a terminal failure.
    fn fail(
        &mut self,
        fix: &FixId,
        ctx: &FixContext,
        stage: FailureStage,
        reason: &str,
    ) -> Result<()> {
        self.write(
            fix,
            ctx,
            JournalRecord::Failed {
                stage,
                reason: reason.to_owned(),
            },
        )
    }
}

/// Whether an approval is a person's, carries a reference, and does not name an agent.
fn valid_approval(approval: &Approval) -> bool {
    !approval.approver.is_empty()
        && !approval.approver.starts_with("agent:")
        && !approval.reference.is_empty()
}
