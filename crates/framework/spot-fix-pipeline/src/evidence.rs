//! The evidence a fix produces: pre-images, changes, rehearsal and verification reports, the
//! residue a rollback measured, the undo window and the final outcome.
//!
//! These are plain data with no clock and no randomness; the pipeline writes them to the
//! journal, the audit chain never sees a value from them that is not already a digest.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use spot_agent_tiers::Tier;
use spot_rehearsal::{Fingerprint, FingerprintDiff, InvariantReport};

use crate::model::{FixId, FixState, Mechanism};

/// The state of one affected record before a fix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreImage {
    /// The typed target reference.
    pub target: String,
    /// The digest of the captured state, which undo and conflict detection compare.
    pub digest: String,
    /// The captured value. Opaque to the pipeline and never a secret.
    pub value: Json,
}

impl PreImage {
    /// A captured pre-image.
    #[must_use]
    pub fn new(target: &str, digest: &str, value: Json) -> Self {
        Self {
            target: target.to_owned(),
            digest: digest.to_owned(),
            value,
        }
    }
}

/// A snapshot a T3 or large fix was taken from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRef {
    /// The snapshot's id in the snapshot store.
    pub id: String,
    /// The snapshot's manifest digest.
    pub digest: String,
}

impl SnapshotRef {
    /// A snapshot reference.
    #[must_use]
    pub fn new(id: &str, digest: &str) -> Self {
        Self {
            id: id.to_owned(),
            digest: digest.to_owned(),
        }
    }
}

/// One change the apply made, with the state before and after.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRecord {
    /// The step that made it.
    pub step: usize,
    /// The typed target.
    pub target: String,
    /// The state digest before the change.
    pub before: String,
    /// The state digest after the change.
    pub after: String,
}

impl ChangeRecord {
    /// A change record.
    #[must_use]
    pub fn new(step: usize, target: &str, before: &str, after: &str) -> Self {
        Self {
            step,
            target: target.to_owned(),
            before: before.to_owned(),
            after: after.to_owned(),
        }
    }
}

/// What a rehearsal found on the clone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalEvidence {
    /// The clone's post-apply fingerprint.
    pub fingerprint: Fingerprint,
    /// The digest of the clone's pre fingerprint.
    pub pre_digest: String,
    /// The digest of the clone's post fingerprint.
    pub post_digest: String,
    /// The invariants the rehearsal ran.
    pub invariants: InvariantReport,
    /// Whether the source plan's own rehearsal checks passed.
    pub passed: bool,
    /// A one-line, secret-free detail.
    pub detail: String,
}

impl RehearsalEvidence {
    /// Whether every rehearsal check passed.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.passed && self.invariants.passed()
    }
}

/// What verification found on the live site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    /// The invariants that ran after the apply.
    pub invariants: InvariantReport,
    /// Whether the remedy-specific verification passed.
    pub remedy_verified: bool,
    /// Whether the original symptom cleared.
    pub symptom_cleared: bool,
    /// Extra violations, in plain language and never a secret.
    pub violations: Vec<String>,
    /// The live fingerprint after the apply.
    pub fingerprint: Fingerprint,
}

impl VerifyReport {
    /// Whether every verification check passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.invariants.passed()
            && self.remedy_verified
            && self.symptom_cleared
            && self.violations.is_empty()
    }

    /// The failed check names, in a stable order.
    #[must_use]
    pub fn failures(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.invariants.passed() {
            out.push(format!("invariants:{}", self.invariants.total()));
        }
        if !self.remedy_verified {
            out.push("remedy_verified".to_owned());
        }
        if !self.symptom_cleared {
            out.push("symptom_cleared".to_owned());
        }
        out.extend(self.violations.iter().cloned());
        out
    }
}

/// Why a rollback ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "cause")]
pub enum RollbackCause {
    /// A step failed.
    StepFailed {
        /// The step's index.
        index: usize,
    },
    /// Verification failed.
    VerifyFailed,
    /// A restart found a run that may have changed the world.
    Recovery,
    /// A verified fix was undone inside its window.
    Undo,
}

/// The fingerprint residue a rollback left: empty means the pre-state is back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResidueEvidence {
    /// The pre-state fingerprint digest.
    pub pre_digest: String,
    /// The post-rollback fingerprint digest.
    pub after_digest: String,
    /// Every remaining difference.
    pub differences: Vec<FingerprintDiff>,
}

impl ResidueEvidence {
    /// A residue from the pre and post-rollback fingerprints.
    #[must_use]
    pub fn measure(pre: &Fingerprint, after: &Fingerprint) -> Self {
        Self {
            pre_digest: pre.digest(),
            after_digest: after.digest(),
            differences: pre.diff(after),
        }
    }

    /// Whether the pre-state is byte-identical by fingerprint.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.differences.is_empty() && self.pre_digest == self.after_digest
    }
}

/// What a rollback produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackEvidence {
    /// Why the rollback ran.
    pub cause: RollbackCause,
    /// How it was achieved.
    pub mechanism: Mechanism,
    /// The invariants re-run after the rollback.
    pub invariants: InvariantReport,
    /// The residue check; `None` before it ran.
    pub residue: Option<ResidueEvidence>,
}

impl RollbackEvidence {
    /// Whether the rollback restored the pre-state with no residue.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.invariants.passed() && self.residue.as_ref().is_some_and(ResidueEvidence::is_empty)
    }
}

/// Why a fix ended in [`FixState::Failed`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureEvidence {
    /// The stage that failed.
    pub stage: FailureStage,
    /// A one-line, secret-free reason.
    pub reason: String,
}

impl FailureEvidence {
    /// A failure.
    #[must_use]
    pub fn new(stage: FailureStage, reason: &str) -> Self {
        Self {
            stage,
            reason: reason.to_owned(),
        }
    }
}

/// The stage a failed fix ended at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStage {
    /// The proposal or the plan.
    Planning,
    /// The approval for a T3 fix.
    Approval,
    /// The rehearsal.
    Rehearsal,
    /// The apply.
    Apply,
    /// The verification (and the rollback that followed).
    Verify,
    /// The automatic rollback.
    Rollback,
    /// The undo.
    Undo,
}

impl FailureStage {
    /// The stable snake_case text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::Approval => "approval",
            Self::Rehearsal => "rehearsal",
            Self::Apply => "apply",
            Self::Verify => "verify",
            Self::Rollback => "rollback",
            Self::Undo => "undo",
        }
    }
}

/// The window in which a verified fix can be undone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoWindow {
    /// When the window opened (unix seconds).
    pub opened_at: i64,
    /// When it closes (unix seconds).
    pub expires_at: i64,
    /// Whether expiry was observed and the window closed.
    pub closed: bool,
}

impl UndoWindow {
    /// An open window.
    #[must_use]
    pub fn open(opened_at: i64, window_secs: i64) -> Self {
        Self {
            opened_at,
            expires_at: opened_at.saturating_add(window_secs.max(0)),
            closed: false,
        }
    }

    /// Whether the window has closed at `now`.
    #[must_use]
    pub fn is_expired(&self, now: i64) -> bool {
        now > self.expires_at
    }

    /// Whether undo is still possible at `now`.
    #[must_use]
    pub fn is_open(&self, now: i64) -> bool {
        !self.closed && !self.is_expired(now)
    }
}

/// The agent a fix runs as, captured so the audit record never depends on a later lookup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    /// The agent's id.
    pub id: String,
    /// The agent's highest tier.
    pub tier: Tier,
    /// Whether the agent is registered deep (read-only by design in `spot-agent-tiers`).
    pub deep: bool,
}

impl AgentRef {
    /// An agent reference.
    #[must_use]
    pub fn new(id: &str, tier: Tier, deep: bool) -> Self {
        Self {
            id: id.to_owned(),
            tier,
            deep,
        }
    }

    /// The reference from a registered identity.
    #[must_use]
    pub fn from_identity(identity: &spot_agent_tiers::AgentIdentity) -> Self {
        Self {
            id: identity.id.clone(),
            tier: identity.max_tier,
            deep: identity.registered_deep(),
        }
    }
}

/// The deployment epoch a fix ran against (WP-AI-02's versioned deployment model).
///
/// The caller supplies it; the pipeline never reads the environment. `spot-deployment-model`
/// is not in the tree yet (WP-AI-02.T1), so the epoch is carried as a plain value type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeploymentEpoch(u64);

impl DeploymentEpoch {
    /// An epoch.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The epoch's number.
    #[must_use]
    pub fn value(self) -> u64 {
        self.0
    }
}

impl fmt::Display for DeploymentEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The outcome of a pipeline operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixOutcome {
    /// The fix.
    pub fix: FixId,
    /// The journaled state.
    pub state: FixState,
    /// The plan's hash, when a plan exists.
    pub plan_hash: Option<String>,
    /// The undo window, when verification passed.
    pub undo: Option<UndoWindow>,
    /// The residue the last rollback measured, if any.
    pub residue: Option<ResidueEvidence>,
    /// Why the fix failed, if it did.
    pub failure: Option<FailureEvidence>,
}

impl FixOutcome {
    /// An outcome with nothing optional set.
    #[must_use]
    pub fn new(fix: FixId, state: FixState) -> Self {
        Self {
            fix,
            state,
            plan_hash: None,
            undo: None,
            residue: None,
            failure: None,
        }
    }
}
