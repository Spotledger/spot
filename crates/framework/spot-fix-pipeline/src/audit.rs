//! The audit trail: one valid `spot-audit-chain` record per transition, always, refusals first.
//!
//! [`FixAuditEvent`] carries the fix, the journaled state, the action, the agent identity, the
//! tier and the deployment epoch. [`FixAuditEvent::audit_record`] maps it onto the chain's
//! [`AuditRecord`] (`actor = agent:<id>`, a `fix.*` kind, integer/boolean/text payload only), so
//! the host can append it with `AuditChain::append`. The event never carries a secret or a value.
//!
//! [`MemoryFixAudit`] is the reference sink for tests; a root [`FixAudit`](crate::ports::FixAudit)
//! implementation is the engine's seam.

use spot_audit_chain::{AuditRecord, Payload, PayloadValue};

use crate::error::{FixError, Result};
use crate::model::{AgentRef, DeploymentEpoch, FixId, FixState};

/// What a transition did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditAction {
    /// The fix was proposed.
    Proposed,
    /// A sealed plan exists.
    Planned,
    /// An approval was requested from the BPM task inbox.
    ApprovalRequested,
    /// A human approved the plan.
    Approved,
    /// A registered deep agent self-approved its T3 plan.
    SelfApproved,
    /// The approval was refused.
    ApprovalRefused,
    /// A rehearsal started.
    RehearsalStarted,
    /// The rehearsal passed.
    Rehearsed,
    /// The rehearsal failed.
    RehearsalFailed,
    /// The apply started.
    ApplyStarted,
    /// A snapshot was taken.
    SnapshotTaken,
    /// The pre-images were captured.
    PreImagesCaptured,
    /// The pre-apply fingerprint was captured.
    PreFingerprintCaptured,
    /// A step started.
    StepStarted,
    /// A step was applied.
    StepApplied,
    /// A step failed.
    StepFailed,
    /// Every step ran.
    Applied,
    /// Verification started.
    VerifyStarted,
    /// Verification passed.
    Verified,
    /// Verification failed.
    VerifyFailed,
    /// A rollback started.
    RollbackStarted,
    /// The rollback restored the pre-state with no residue.
    RolledBack,
    /// The rollback failed or left a residue.
    RollbackFailed,
    /// The residue was measured.
    ResidueChecked,
    /// The fix failed.
    Failed,
    /// The undo window opened.
    UndoWindowOpened,
    /// The undo window closed by expiry.
    UndoWindowClosed,
    /// An undo started.
    UndoStarted,
    /// An undo finished.
    Undone,
    /// An undo was refused.
    UndoRefused,
}

impl AuditAction {
    /// The stable snake_case text, used as the audit kind's suffix.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Planned => "planned",
            Self::ApprovalRequested => "approval_requested",
            Self::Approved => "approved",
            Self::SelfApproved => "self_approved",
            Self::ApprovalRefused => "approval_refused",
            Self::RehearsalStarted => "rehearsal_started",
            Self::Rehearsed => "rehearsed",
            Self::RehearsalFailed => "rehearsal_failed",
            Self::ApplyStarted => "apply_started",
            Self::SnapshotTaken => "snapshot_taken",
            Self::PreImagesCaptured => "pre_images_captured",
            Self::PreFingerprintCaptured => "pre_fingerprint_captured",
            Self::StepStarted => "step_started",
            Self::StepApplied => "step_applied",
            Self::StepFailed => "step_failed",
            Self::Applied => "applied",
            Self::VerifyStarted => "verify_started",
            Self::Verified => "verified",
            Self::VerifyFailed => "verify_failed",
            Self::RollbackStarted => "rollback_started",
            Self::RolledBack => "rolled_back",
            Self::RollbackFailed => "rollback_failed",
            Self::ResidueChecked => "residue_checked",
            Self::Failed => "failed",
            Self::UndoWindowOpened => "undo_window_opened",
            Self::UndoWindowClosed => "undo_window_closed",
            Self::UndoStarted => "undo_started",
            Self::Undone => "undone",
            Self::UndoRefused => "undo_refused",
        }
    }
}

/// One transition, as the audit trail sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixAuditEvent {
    /// The fix.
    pub fix: FixId,
    /// The journaled state after the transition.
    pub state: FixState,
    /// What happened.
    pub action: AuditAction,
    /// The agent (never a person's secret).
    pub agent: AgentRef,
    /// The deployment epoch.
    pub epoch: DeploymentEpoch,
    /// The instant (unix seconds), supplied by the caller.
    pub at: i64,
    /// A one-line, secret-free detail.
    pub detail: String,
}

impl FixAuditEvent {
    /// The event as an AI audit chain record.
    ///
    /// # Errors
    ///
    /// [`FixError::AuditFailed`] when the record is invalid or the epoch does not fit the
    /// chain's integer payload.
    pub fn audit_record(&self) -> Result<AuditRecord> {
        let mut payload = Payload::new();
        payload.insert("fix".to_owned(), PayloadValue::Text(self.fix.to_string()));
        payload.insert(
            "state".to_owned(),
            PayloadValue::Text(self.state.as_str().to_owned()),
        );
        payload.insert(
            "tier".to_owned(),
            PayloadValue::Text(self.agent.tier.as_str().to_owned()),
        );
        payload.insert("deep".to_owned(), PayloadValue::Bool(self.agent.deep));
        let epoch = i64::try_from(self.epoch.value()).map_err(|_| FixError::AuditFailed {
            detail: "the deployment epoch does not fit an integer payload".to_owned(),
        })?;
        payload.insert("epoch".to_owned(), PayloadValue::Int(epoch));
        payload.insert("detail".to_owned(), PayloadValue::Text(self.detail.clone()));
        let record = AuditRecord {
            kind: format!("fix.{}", self.action.as_str()),
            actor: format!("agent:{}", self.agent.id),
            on_behalf_of: None,
            subject: None,
            at: self.at.saturating_mul(1_000_000),
            payload,
            masked: true,
        };
        record.validate().map_err(|error| FixError::AuditFailed {
            detail: error.to_string(),
        })?;
        Ok(record)
    }
}

/// An in-memory audit sink: every event, and the chain records they map onto.
#[derive(Clone, Debug, Default)]
pub struct MemoryFixAudit {
    events: Vec<FixAuditEvent>,
}

impl MemoryFixAudit {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every event, in order.
    #[must_use]
    pub fn events(&self) -> &[FixAuditEvent] {
        &self.events
    }

    /// The events for one fix, in order.
    #[must_use]
    pub fn events_for(&self, fix: &FixId) -> Vec<&FixAuditEvent> {
        self.events
            .iter()
            .filter(|event| &event.fix == fix)
            .collect()
    }

    /// The chain records every event maps onto; a test appends them to a real chain.
    ///
    /// # Errors
    ///
    /// The mapping error of the first invalid event.
    pub fn records(&self) -> Result<Vec<AuditRecord>> {
        self.events
            .iter()
            .map(FixAuditEvent::audit_record)
            .collect()
    }
}

impl crate::ports::FixAudit for MemoryFixAudit {
    fn record(&mut self, event: FixAuditEvent) -> Result<()> {
        self.events.push(event);
        Ok(())
    }
}
