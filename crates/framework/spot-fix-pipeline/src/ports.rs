//! The typed ports the pipeline drives: pre-images, snapshots, step execution, rehearsal,
//! verification, approval and audit.
//!
//! The engine never touches a database, a file, a clock or the network. Each capability is a
//! trait the host implements; the `testing` feature ships in-memory doubles for tests and later
//! tasks. This is also where the hard floor is made structural: the engine cannot apply a step
//! without first capturing a [`PreImage`] for every affected target, because the only apply path
//! it has goes through these ports in that order.

use spot_agent_tiers::Approval;
use spot_rehearsal::{Fingerprint, InvariantReport};

use crate::error::Result;
use crate::model::{
    AgentRef, ChangeRecord, FixId, FixPlan, FixStep, PreImage, RehearsalEvidence, SnapshotRef,
    VerifyReport,
};

/// Reads and restores the logical pre-image of affected records.
///
/// `capture` must be taken before the first step runs, and `restore` must be idempotent: the
/// engine calls it again during recovery.
pub trait PreImageStore {
    /// Captures one image per target, in the order given.
    ///
    /// # Errors
    ///
    /// A store error when a target cannot be read.
    fn capture(&mut self, targets: &[String]) -> Result<Vec<PreImage>>;

    /// Restores `image`; safe to call again.
    ///
    /// # Errors
    ///
    /// A store error when the record cannot be written.
    fn restore(&mut self, image: &PreImage) -> Result<()>;

    /// The current digest of `target`, for undo conflict detection.
    ///
    /// # Errors
    ///
    /// A store error when the target cannot be read.
    fn digest(&mut self, target: &str) -> Result<String>;
}

/// Takes and restores a site snapshot, and holds the maintenance gate a snapshot path needs.
///
/// The default gate methods do nothing, so a small-fix snapshot port that has no gate stays
/// usable; a T3 implementation overrides them.
pub trait SnapshotPort {
    /// Takes a snapshot for `fix`.
    ///
    /// # Errors
    ///
    /// A port error when the snapshot cannot be taken.
    fn take(&mut self, fix: &FixId) -> Result<SnapshotRef>;

    /// Restores `snapshot`; safe to call again.
    ///
    /// # Errors
    ///
    /// A port error when the snapshot cannot be restored.
    fn restore(&mut self, snapshot: &SnapshotRef) -> Result<()>;

    /// Closes the maintenance gate for a structural apply.
    ///
    /// # Errors
    ///
    /// A port error when the gate cannot be closed.
    fn close_gate(&mut self, _reason: &str) -> Result<()> {
        Ok(())
    }

    /// Opens the maintenance gate after a verified apply or a clean rollback.
    ///
    /// # Errors
    ///
    /// A port error when the gate cannot be opened.
    fn open_gate(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A step's state before and after it ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepChange {
    /// The state digest before the step.
    pub before: String,
    /// The state digest after the step.
    pub after: String,
}

/// Runs and reverses a plan's steps.
///
/// `run_step` is called with an idempotency key that is stable for the fix, the plan and the
/// step, so a resumed run can safely repeat it. `undo_step` is the inverse for one recorded
/// change and must itself be idempotent.
pub trait FixExecutor {
    /// Runs `step` under `idempotency_key`.
    ///
    /// # Errors
    ///
    /// A typed error when the step cannot run.
    fn run_step(&mut self, step: &FixStep, idempotency_key: &str) -> Result<StepChange>;

    /// Reverses `change` for `step`; safe to call again.
    ///
    /// # Errors
    ///
    /// A typed error when the inverse fails.
    fn undo_step(&mut self, step: &FixStep, change: &ChangeRecord) -> Result<()>;
}

/// Rehearses a plan on a snapshot clone (`spot-rehearsal`'s engine behind the port).
pub trait RehearsalPort {
    /// Rehearses `plan` and returns its evidence.
    ///
    /// # Errors
    ///
    /// A port error when the clone, the apply or the measurement failed.
    fn rehearse(&mut self, plan: &FixPlan) -> Result<RehearsalEvidence>;
}

/// Verifies a live apply and measures the state fingerprint.
///
/// `verify` runs the invariant sets, the remedy-specific check and the symptom re-check. `verify`
/// is never called on a rehearsal clone, and `fingerprint` is the fingerprint engine the residue
/// check uses before, after and after the rollback.
pub trait FixVerifier {
    /// Verifies the applied `plan` and its `changes`.
    ///
    /// # Errors
    ///
    /// A port error when a check cannot run. A check that ran and failed is reported in the
    /// [`VerifyReport`], not as an error.
    fn verify(&mut self, plan: &FixPlan, changes: &[ChangeRecord]) -> Result<VerifyReport>;

    /// Re-runs the invariant sets after a rollback (the symptom intentionally remains).
    ///
    /// # Errors
    ///
    /// A port error when a check cannot run.
    fn invariants(&mut self) -> Result<InvariantReport>;

    /// Measures the live state fingerprint.
    ///
    /// # Errors
    ///
    /// A port error when the state cannot be measured.
    fn fingerprint(&mut self) -> Result<Fingerprint>;
}

/// Asks the BPM task inbox (or a deep agent's own registration) for approval of a T3 plan.
pub trait ApprovalPort {
    /// Requests an approval for `plan`.
    ///
    /// # Errors
    ///
    /// A port error when the inbox cannot be reached.
    fn request(
        &mut self,
        fix: &FixId,
        plan: &FixPlan,
        agent: &AgentRef,
        reason: &str,
    ) -> Result<Option<Approval>>;
}

/// Records every transition on the AI audit chain.
///
/// A failure here fails the run closed: the audit log cannot be disabled.
pub trait FixAudit {
    /// Records one event.
    ///
    /// # Errors
    ///
    /// A sink error when the event cannot be appended.
    fn record(&mut self, event: crate::audit::FixAuditEvent) -> Result<()>;
}
