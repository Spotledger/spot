//! The write-ahead change journal and the fold that reads a restart's intent.
//!
//! The engine writes one [`JournalRecord`] **before** it performs each transition or step, so a
//! restart can always tell what was being attempted and whether the world may have changed.
//! [`FixJournal`] is the seam; the in-memory [`InMemoryJournal`] is the reference and a
//! database-backed journal implements the same trait. [`Progress`] is the fold of a fix's
//! records: it answers what completed, what was in flight, what the pre-state was and whether the
//! rollback left a residue.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use spot_agent_tiers::Approval;
use spot_rehearsal::Fingerprint;

use crate::audit::AuditAction;
use crate::error::{FixError, Result};
use crate::model::{
    AgentRef, ChangeRecord, DeploymentEpoch, FailureEvidence, FailureStage, FixId, FixOutcome,
    FixPlan, FixState, Mechanism, PreImage, RehearsalEvidence, ResidueEvidence, RollbackCause,
    RollbackEvidence, SnapshotRef, UndoWindow, VerifyReport,
};
use crate::policy::PlanPath;

/// The journal's sequence number; strictly increasing.
pub type Seq = u64;

/// One journal entry, tagged with its fix and the time it was written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// The sequence number, assigned by the journal.
    pub seq: Seq,
    /// The fix the record belongs to.
    pub fix: FixId,
    /// Unix seconds; the caller supplies time.
    pub at: i64,
    /// What was being attempted.
    pub record: JournalRecord,
}

/// One write-ahead record.
///
/// The large variants are boxed so a record stays cheap to hold in a journal and a sink.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "record")]
pub enum JournalRecord {
    /// The run is registered; nothing has been planned.
    Proposed {
        /// The site (tenant) the fix belongs to.
        site: String,
        /// The agent the run acts as.
        agent: AgentRef,
        /// The deployment epoch.
        epoch: DeploymentEpoch,
    },
    /// A sealed plan exists and the policy chose its path.
    Planned {
        /// The plan.
        plan: Box<FixPlan>,
        /// Direct apply or rehearse first.
        path: PlanPath,
    },
    /// An approval was requested from the BPM task inbox.
    ApprovalRequested {
        /// Why approval is needed.
        reason: String,
    },
    /// A human approved the plan.
    Approved {
        /// The approval.
        approval: Approval,
    },
    /// A registered deep agent self-approved its T3 plan.
    SelfApproved {
        /// The agent that approved it.
        by: String,
    },
    /// The approval was refused.
    ApprovalRefused {
        /// Why, in plain language.
        reason: String,
    },
    /// A rehearsal started.
    RehearsalStarted {
        /// The plan hash being rehearsed.
        plan_hash: String,
    },
    /// The rehearsal passed.
    Rehearsed {
        /// What the clone showed.
        evidence: Box<RehearsalEvidence>,
    },
    /// The rehearsal failed; the fix ends with no change.
    RehearsalFailed {
        /// Why.
        reason: String,
        /// The evidence, when the port produced any.
        evidence: Option<Box<RehearsalEvidence>>,
    },
    /// The apply is about to run.
    ApplyStarted {
        /// Pre-images or a snapshot.
        mechanism: Mechanism,
        /// The plan hash being applied.
        plan_hash: String,
    },
    /// A snapshot was taken.
    SnapshotTaken {
        /// The snapshot.
        snapshot: SnapshotRef,
    },
    /// The logical pre-images were captured.
    PreImagesCaptured {
        /// One image per affected target.
        images: Vec<PreImage>,
    },
    /// The live pre-state fingerprint was captured.
    PreFingerprintCaptured {
        /// The pre-state.
        fingerprint: Box<Fingerprint>,
    },
    /// A step is about to run.
    StepStarted {
        /// The step's index.
        index: usize,
        /// The step's id.
        id: String,
        /// The idempotency key it runs under.
        idempotency_key: String,
    },
    /// A step completed and its change is journaled.
    StepApplied {
        /// The change.
        change: ChangeRecord,
    },
    /// A step failed.
    StepFailed {
        /// The step's index.
        index: usize,
        /// Why.
        reason: String,
    },
    /// Every step ran.
    Applied {
        /// The complete change journal.
        changes: Vec<ChangeRecord>,
    },
    /// Verification is about to run.
    VerifyStarted,
    /// Verification passed.
    Verified {
        /// What the checks found.
        report: Box<VerifyReport>,
    },
    /// Verification failed; the rollback follows.
    VerifyFailed {
        /// The report, when verification ran to completion.
        report: Option<Box<VerifyReport>>,
        /// The failed checks.
        reasons: Vec<String>,
    },
    /// A rollback is about to run.
    RollbackStarted {
        /// Why it runs.
        cause: RollbackCause,
        /// How it will be achieved.
        mechanism: Mechanism,
    },
    /// The rollback finished and the residue check passed.
    RolledBack {
        /// What the rollback measured.
        evidence: Box<RollbackEvidence>,
    },
    /// The rollback failed or left a residue; the fix stays failed.
    RollbackFailed {
        /// Why.
        reason: String,
        /// The evidence, when the rollback got far enough to measure.
        evidence: Option<Box<RollbackEvidence>>,
    },
    /// The residue between the pre-state and the post-rollback state was measured.
    ResidueChecked {
        /// The measurement.
        residue: ResidueEvidence,
    },
    /// The fix failed before it could apply, or after a failed rollback.
    Failed {
        /// The stage.
        stage: FailureStage,
        /// Why.
        reason: String,
    },
    /// The undo window opened.
    UndoWindowOpened {
        /// The window.
        window: UndoWindow,
    },
    /// The undo window closed.
    UndoWindowClosed {
        /// When.
        at: i64,
        /// `expired` or `closed`.
        reason: String,
    },
    /// An undo is about to run.
    UndoStarted {
        /// How it will be achieved.
        mechanism: Mechanism,
        /// Why the undo was requested.
        reason: String,
    },
    /// The undo finished and the residue check passed.
    Undone {
        /// What the undo measured.
        evidence: Box<RollbackEvidence>,
    },
    /// An undo was refused (expired, conflicting or unavailable).
    UndoRefused {
        /// Why.
        reason: String,
        /// The stable code.
        code: String,
    },
}

impl JournalRecord {
    /// The audit action this record maps onto.
    #[must_use]
    pub fn audit_action(&self) -> AuditAction {
        match self {
            Self::Proposed { .. } => AuditAction::Proposed,
            Self::Planned { .. } => AuditAction::Planned,
            Self::ApprovalRequested { .. } => AuditAction::ApprovalRequested,
            Self::Approved { .. } => AuditAction::Approved,
            Self::SelfApproved { .. } => AuditAction::SelfApproved,
            Self::ApprovalRefused { .. } => AuditAction::ApprovalRefused,
            Self::RehearsalStarted { .. } => AuditAction::RehearsalStarted,
            Self::Rehearsed { .. } => AuditAction::Rehearsed,
            Self::RehearsalFailed { .. } => AuditAction::RehearsalFailed,
            Self::ApplyStarted { .. } => AuditAction::ApplyStarted,
            Self::SnapshotTaken { .. } => AuditAction::SnapshotTaken,
            Self::PreImagesCaptured { .. } => AuditAction::PreImagesCaptured,
            Self::PreFingerprintCaptured { .. } => AuditAction::PreFingerprintCaptured,
            Self::StepStarted { .. } => AuditAction::StepStarted,
            Self::StepApplied { .. } => AuditAction::StepApplied,
            Self::StepFailed { .. } => AuditAction::StepFailed,
            Self::Applied { .. } => AuditAction::Applied,
            Self::VerifyStarted => AuditAction::VerifyStarted,
            Self::Verified { .. } => AuditAction::Verified,
            Self::VerifyFailed { .. } => AuditAction::VerifyFailed,
            Self::RollbackStarted { .. } => AuditAction::RollbackStarted,
            Self::RolledBack { .. } => AuditAction::RolledBack,
            Self::RollbackFailed { .. } => AuditAction::RollbackFailed,
            Self::ResidueChecked { .. } => AuditAction::ResidueChecked,
            Self::Failed { .. } => AuditAction::Failed,
            Self::UndoWindowOpened { .. } => AuditAction::UndoWindowOpened,
            Self::UndoWindowClosed { .. } => AuditAction::UndoWindowClosed,
            Self::UndoStarted { .. } => AuditAction::UndoStarted,
            Self::Undone { .. } => AuditAction::Undone,
            Self::UndoRefused { .. } => AuditAction::UndoRefused,
        }
    }

    /// A one-line, secret-free detail for the audit event.
    #[must_use]
    pub fn audit_detail(&self) -> String {
        match self {
            Self::Proposed { site, .. } => format!("fix proposed for {site}"),
            Self::Planned { plan, path } => {
                format!("plan {} ({})", plan.hash, path.as_str())
            }
            Self::ApprovalRequested { reason } => format!("approval requested: {reason}"),
            Self::Approved { approval } => format!("approved by {}", approval.approver),
            Self::SelfApproved { by } => format!("self-approved by {by}"),
            Self::ApprovalRefused { reason } => format!("approval refused: {reason}"),
            Self::RehearsalStarted { plan_hash } => format!("rehearsal started for {plan_hash}"),
            Self::Rehearsed { evidence } => {
                format!(
                    "rehearsed: {} invariant set(s)",
                    evidence.invariants.sets.len()
                )
            }
            Self::RehearsalFailed { reason, .. } => format!("rehearsal failed: {reason}"),
            Self::ApplyStarted { mechanism, .. } => {
                format!("apply started with {}", mechanism.as_str())
            }
            Self::SnapshotTaken { snapshot } => format!("snapshot {}", snapshot.id),
            Self::PreImagesCaptured { images } => {
                format!("{} pre-image(s) captured", images.len())
            }
            Self::PreFingerprintCaptured { .. } => "pre-state fingerprinted".to_owned(),
            Self::StepStarted { index, id, .. } => format!("step {index} (`{id}`) started"),
            Self::StepApplied { change } => {
                format!("step {} applied to {}", change.step, change.target)
            }
            Self::StepFailed { index, reason } => format!("step {index} failed: {reason}"),
            Self::Applied { changes } => format!("{} change(s) applied", changes.len()),
            Self::VerifyStarted => "verification started".to_owned(),
            Self::Verified { report } => {
                format!(
                    "verified: {} invariant set(s)",
                    report.invariants.sets.len()
                )
            }
            Self::VerifyFailed { reasons, .. } => {
                format!("verification failed: {}", reasons.join(","))
            }
            Self::RollbackStarted { cause, mechanism } => {
                format!("rollback ({cause:?}) with {}", mechanism.as_str())
            }
            Self::RolledBack { evidence } => {
                format!("rolled back; residue {}", evidence.is_clean())
            }
            Self::RollbackFailed { reason, .. } => format!("rollback failed: {reason}"),
            Self::ResidueChecked { residue } => {
                format!("{} residual difference(s)", residue.differences.len())
            }
            Self::Failed { stage, reason } => {
                format!("failed at {}: {reason}", stage.as_str())
            }
            Self::UndoWindowOpened { window } => {
                format!("undo window until {}", window.expires_at)
            }
            Self::UndoWindowClosed { reason, .. } => format!("undo window closed: {reason}"),
            Self::UndoStarted { mechanism, .. } => {
                format!("undo started with {}", mechanism.as_str())
            }
            Self::Undone { evidence } => format!("undone; residue {}", evidence.is_clean()),
            Self::UndoRefused { reason, code } => format!("undo refused ({code}): {reason}"),
        }
    }
}

/// The write-ahead journal of a site's fixes.
///
/// `append` assigns the next sequence number; a backend must make it atomic. `entries` returns
/// one fix's records in order, and `all` returns every record.
pub trait FixJournal: Send + Sync {
    /// Appends `record` for `fix` and returns its sequence number.
    ///
    /// # Errors
    ///
    /// [`FixError::JournalFailed`] when the backend cannot write.
    fn append(&self, fix: &FixId, at: i64, record: JournalRecord) -> Result<Seq>;

    /// Every record of `fix`, in sequence order.
    ///
    /// # Errors
    ///
    /// [`FixError::JournalFailed`] when the backend cannot read.
    fn entries(&self, fix: &FixId) -> Result<Vec<JournalEntry>>;

    /// Every record, in sequence order.
    ///
    /// # Errors
    ///
    /// [`FixError::JournalFailed`] when the backend cannot read.
    fn all(&self) -> Result<Vec<JournalEntry>>;
}

/// The reference in-memory journal: one `Vec`, one counter, no I/O.
#[derive(Debug, Default)]
pub struct InMemoryJournal {
    entries: Mutex<Vec<JournalEntry>>,
    next: AtomicU64,
}

impl InMemoryJournal {
    /// An empty journal whose next sequence number is `1`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
        }
    }

    /// The entries written so far, for a test to inspect.
    #[must_use]
    pub fn written(&self) -> Vec<JournalEntry> {
        self.entries
            .lock()
            .map(|entries| entries.clone())
            .unwrap_or_default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Vec<JournalEntry>>> {
        self.entries.lock().map_err(|_| FixError::JournalFailed {
            detail: "the in-memory journal lock is poisoned".to_owned(),
        })
    }
}

impl FixJournal for InMemoryJournal {
    fn append(&self, fix: &FixId, at: i64, record: JournalRecord) -> Result<Seq> {
        let mut entries = self.lock()?;
        let seq = self.next.fetch_add(1, Ordering::SeqCst);
        entries.push(JournalEntry {
            seq,
            fix: fix.clone(),
            at,
            record,
        });
        Ok(seq)
    }

    fn entries(&self, fix: &FixId) -> Result<Vec<JournalEntry>> {
        let entries = self.lock()?;
        Ok(entries
            .iter()
            .filter(|entry| &entry.fix == fix)
            .cloned()
            .collect())
    }

    fn all(&self) -> Result<Vec<JournalEntry>> {
        let entries = self.lock()?;
        Ok(entries.clone())
    }
}

/// The in-flight phase a journal reveals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// No operation is in flight.
    Settled,
    /// An apply started and its changes may be partial.
    Applying,
    /// Every step ran and verification is in flight.
    Verifying,
    /// A rollback started and may be partial.
    RollingBack,
    /// An undo started and may be partial.
    Undoing,
}

impl Phase {
    /// The stable snake_case text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::Applying => "applying",
            Self::Verifying => "verifying",
            Self::RollingBack => "rolling_back",
            Self::Undoing => "undoing",
        }
    }
}

/// The fold of a fix's journal: what the run intended and how far it got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    /// The fix.
    pub fix: FixId,
    /// Whether a run ever started.
    pub started: bool,
    /// The site (tenant).
    pub site: Option<String>,
    /// The agent.
    pub agent: Option<AgentRef>,
    /// The deployment epoch.
    pub epoch: Option<DeploymentEpoch>,
    /// The plan.
    pub plan: Option<FixPlan>,
    /// The policy's path.
    pub path: Option<PlanPath>,
    /// Whether an approval was requested.
    pub approval_requested: bool,
    /// The human approval.
    pub approval: Option<Approval>,
    /// The deep agent that self-approved.
    pub self_approved_by: Option<String>,
    /// Why approval was refused.
    pub approval_refused: Option<String>,
    /// Whether a rehearsal started.
    pub rehearsal_started: bool,
    /// The rehearsal evidence.
    pub rehearsal: Option<RehearsalEvidence>,
    /// Why the rehearsal failed.
    pub rehearsal_failed: Option<String>,
    /// The mechanism the apply used.
    pub mechanism: Option<Mechanism>,
    /// Whether the apply started.
    pub apply_started: bool,
    /// The snapshot, when one was taken.
    pub snapshot: Option<SnapshotRef>,
    /// The captured pre-images.
    pub pre_images: Vec<PreImage>,
    /// The pre-state fingerprint.
    pub pre_fingerprint: Option<Fingerprint>,
    /// The step in flight when the journal ended.
    pub started_step: Option<(usize, String)>,
    /// The steps that completed.
    pub completed_steps: BTreeSet<usize>,
    /// The change journal, in apply order.
    pub changes: Vec<ChangeRecord>,
    /// Whether every step ran.
    pub applied: bool,
    /// Whether verification started.
    pub verify_started: bool,
    /// The verification report.
    pub verify: Option<VerifyReport>,
    /// The failed checks.
    pub verify_failed: Option<Vec<String>>,
    /// Whether a rollback started.
    pub rollback_started: bool,
    /// Why the rollback ran.
    pub rollback_cause: Option<RollbackCause>,
    /// The rolled-back evidence.
    pub rollback: Option<RollbackEvidence>,
    /// Why the rollback failed.
    pub rollback_failed: Option<String>,
    /// The last residue measurement.
    pub residue: Option<ResidueEvidence>,
    /// Why the fix failed.
    pub failure: Option<FailureEvidence>,
    /// The undo window.
    pub undo: Option<UndoWindow>,
    /// When the undo window was observed closed.
    pub undo_closed_at: Option<i64>,
    /// Whether an undo started.
    pub undo_started: bool,
    /// Whether the undo finished.
    pub undone: bool,
    /// Why an undo was refused.
    pub undo_refused: Option<String>,
    /// The last record's instant.
    pub last_at: i64,
    /// The last record's sequence number (`0` when empty).
    pub last_seq: Seq,
}

impl Progress {
    /// Folds `entries` (one fix's records, in order) into a [`Progress`].
    #[must_use]
    pub fn fold(fix: &FixId, entries: Vec<JournalEntry>) -> Self {
        let mut progress = Self {
            fix: fix.clone(),
            started: false,
            site: None,
            agent: None,
            epoch: None,
            plan: None,
            path: None,
            approval_requested: false,
            approval: None,
            self_approved_by: None,
            approval_refused: None,
            rehearsal_started: false,
            rehearsal: None,
            rehearsal_failed: None,
            mechanism: None,
            apply_started: false,
            snapshot: None,
            pre_images: Vec::new(),
            pre_fingerprint: None,
            started_step: None,
            completed_steps: BTreeSet::new(),
            changes: Vec::new(),
            applied: false,
            verify_started: false,
            verify: None,
            verify_failed: None,
            rollback_started: false,
            rollback_cause: None,
            rollback: None,
            rollback_failed: None,
            residue: None,
            failure: None,
            undo: None,
            undo_closed_at: None,
            undo_started: false,
            undone: false,
            undo_refused: None,
            last_at: 0,
            last_seq: 0,
        };
        for entry in entries {
            progress.last_at = entry.at;
            progress.last_seq = entry.seq;
            progress.apply(entry.record);
        }
        progress
    }

    fn apply(&mut self, record: JournalRecord) {
        match record {
            JournalRecord::Proposed { site, agent, epoch } => {
                // A fix id names one run: a proposal resets every artifact so a backend can keep
                // several attempts under one journal without a restart reading a stale one.
                let fix = self.fix.clone();
                *self = Self::fold(&fix, Vec::new());
                self.started = true;
                self.site = Some(site);
                self.agent = Some(agent);
                self.epoch = Some(epoch);
            }
            JournalRecord::Planned { plan, path } => {
                self.plan = Some(*plan);
                self.path = Some(path);
            }
            JournalRecord::ApprovalRequested { .. } => self.approval_requested = true,
            JournalRecord::Approved { approval } => self.approval = Some(approval),
            JournalRecord::SelfApproved { by } => self.self_approved_by = Some(by),
            JournalRecord::ApprovalRefused { reason } => self.approval_refused = Some(reason),
            JournalRecord::RehearsalStarted { .. } => {
                self.rehearsal_started = true;
                self.rehearsal = None;
                self.rehearsal_failed = None;
            }
            JournalRecord::Rehearsed { evidence } => self.rehearsal = Some(*evidence),
            JournalRecord::RehearsalFailed { reason, evidence } => {
                self.rehearsal_failed = Some(reason);
                self.rehearsal = evidence.map(|boxed| *boxed);
            }
            JournalRecord::ApplyStarted {
                mechanism,
                plan_hash: _,
            } => {
                self.apply_started = true;
                self.mechanism = Some(mechanism);
                self.applied = false;
                self.started_step = None;
                self.completed_steps.clear();
                self.changes.clear();
                self.verify_started = false;
                self.verify = None;
                self.verify_failed = None;
                self.rollback_started = false;
                self.rollback = None;
                self.rollback_failed = None;
                self.residue = None;
            }
            JournalRecord::SnapshotTaken { snapshot } => self.snapshot = Some(snapshot),
            JournalRecord::PreImagesCaptured { images } => self.pre_images = images,
            JournalRecord::PreFingerprintCaptured { fingerprint } => {
                self.pre_fingerprint = Some(*fingerprint);
            }
            JournalRecord::StepStarted { index, id, .. } => {
                self.started_step = Some((index, id));
            }
            JournalRecord::StepApplied { change } => {
                self.completed_steps.insert(change.step);
                self.changes.push(change);
                self.started_step = None;
            }
            JournalRecord::StepFailed { .. } => self.started_step = None,
            JournalRecord::Applied { changes } => {
                self.applied = true;
                self.changes = changes;
                self.started_step = None;
            }
            JournalRecord::VerifyStarted => {
                self.verify_started = true;
                self.verify = None;
                self.verify_failed = None;
            }
            JournalRecord::Verified { report } => {
                self.verify = Some(*report);
                self.verify_failed = None;
            }
            JournalRecord::VerifyFailed { report, reasons } => {
                self.verify = report.map(|boxed| *boxed);
                self.verify_failed = Some(reasons);
            }
            JournalRecord::RollbackStarted { cause, mechanism } => {
                self.rollback_started = true;
                self.rollback_cause = Some(cause);
                self.mechanism = Some(mechanism);
                self.rollback = None;
                self.rollback_failed = None;
            }
            JournalRecord::RolledBack { evidence } => {
                self.residue = evidence.residue.clone();
                self.rollback = Some(*evidence);
                self.rollback_failed = None;
            }
            JournalRecord::RollbackFailed { reason, evidence } => {
                self.rollback_failed = Some(reason);
                if let Some(boxed) = evidence {
                    self.residue = boxed.residue.clone();
                    self.rollback = Some(*boxed);
                }
            }
            JournalRecord::ResidueChecked { residue } => self.residue = Some(residue),
            JournalRecord::Failed { stage, reason } => {
                self.failure = Some(FailureEvidence::new(stage, &reason));
            }
            JournalRecord::UndoWindowOpened { window } => self.undo = Some(window),
            JournalRecord::UndoWindowClosed { at, .. } => {
                self.undo_closed_at = Some(at);
                if let Some(window) = &mut self.undo {
                    window.closed = true;
                }
            }
            JournalRecord::UndoStarted { mechanism, .. } => {
                self.undo_started = true;
                self.mechanism = Some(mechanism);
            }
            JournalRecord::Undone { evidence } => {
                self.undone = true;
                self.undo_started = false;
                self.residue = evidence.residue.clone();
                self.rollback = Some(*evidence);
            }
            JournalRecord::UndoRefused { reason, .. } => self.undo_refused = Some(reason),
        }
    }

    /// The coarse lifecycle state the journal proves.
    #[must_use]
    pub fn state(&self) -> FixState {
        if self.failure.is_some() || self.rollback_failed.is_some() {
            return FixState::Failed;
        }
        if self.undone || self.rollback.is_some() {
            return FixState::RolledBack;
        }
        if self.verify.is_some() && self.verify_failed.is_none() {
            return FixState::Verified;
        }
        if self.applied {
            return FixState::Applied;
        }
        if self.rehearsal.is_some() {
            return FixState::Rehearsed;
        }
        if self.plan.is_some() {
            return FixState::Planned;
        }
        FixState::Proposed
    }

    /// The operation in flight when the journal ended.
    #[must_use]
    pub fn phase(&self) -> Phase {
        if self.undo_started && !self.undone {
            return Phase::Undoing;
        }
        if self.rollback_started
            && self.rollback.is_none()
            && self.rollback_failed.is_none()
            && !self.undone
        {
            return Phase::RollingBack;
        }
        if self.apply_started && !self.applied && !self.undone {
            return Phase::Applying;
        }
        if self.verify_started && self.verify.is_none() && self.verify_failed.is_none() {
            return Phase::Verifying;
        }
        Phase::Settled
    }

    /// Whether an operation was in flight when the journal ended.
    #[must_use]
    pub fn in_flight(&self) -> bool {
        self.phase() != Phase::Settled
    }

    /// Whether the world may have changed: a step ran, or a rollback/undo did.
    #[must_use]
    pub fn world_may_have_changed(&self) -> bool {
        self.apply_started
            || !self.changes.is_empty()
            || self.applied
            || self.rollback_started
            || self.undo_started
            || self.undone
            || self.rollback.is_some()
    }

    /// The plan's hash, when a plan exists.
    #[must_use]
    pub fn plan_hash(&self) -> Option<&str> {
        self.plan.as_ref().map(|plan| plan.hash.as_str())
    }

    /// The outcome this progress describes.
    #[must_use]
    pub fn outcome(&self) -> FixOutcome {
        FixOutcome {
            fix: self.fix.clone(),
            state: self.state(),
            plan_hash: self.plan_hash().map(ToOwned::to_owned),
            undo: self.undo,
            residue: self.residue.clone(),
            failure: self.failure.clone(),
        }
    }
}
