//! The record of one migration run: what was planned, who approved, what the rehearsal found,
//! what apply did and how it ended.
//!
//! A [`MigrationRun`] is folded from the lifecycle events this crate appends to the
//! tamper-evident audit chain (see [`crate::events`]). The plan, the rehearsal report and the
//! apply journal are **large artefacts**: the record keeps a reference to each, by location and
//! SHA-256 digest, never their bytes. Nothing here holds a plan secret or a connection string.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spot_migration_plan::{Area, ReleasePlan};

use crate::error::MigrationAuditError;

/// The longest run id accepted, in bytes.
pub const MAX_ID_BYTES: usize = 256;

/// A run's identity: a caller-supplied text such as a UUIDv7 or `mig-2026-10-17-0001`.
///
/// The crate reads no clock and no randomness, so the caller always supplies the id. It must be
/// non-empty, at most [`MAX_ID_BYTES`] bytes and made only of `A-Z a-z 0-9 . _ : -`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// A checked run id.
    ///
    /// # Errors
    /// [`MigrationAuditError::BadId`] when the text is empty, too long or holds a character
    /// outside the allowed set.
    pub fn new(text: impl Into<String>) -> Result<Self, MigrationAuditError> {
        let text = text.into();
        let ok = !text.is_empty()
            && text.len() <= MAX_ID_BYTES
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
        if ok {
            Ok(Self(text))
        } else {
            Err(MigrationAuditError::BadId { id: text })
        }
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a run ended, or where it is in the lifecycle.
///
/// `Planned`, `Rehearsed` and `Applied` are stages a live run passes through; `Verified`,
/// `Failed`, `RolledBack` and `Refused` are the terminal outcomes
/// [`finish`](crate::RunRecorder::finish) accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The plan and its approvals were recorded; nothing else yet.
    Planned,
    /// The rehearsal report was recorded.
    Rehearsed,
    /// Apply ran; verify has not finished.
    Applied,
    /// Verify passed and users were admitted.
    Verified,
    /// Apply or verify failed.
    Failed,
    /// The run was rolled back to the old version.
    RolledBack,
    /// The run was refused before apply (no permission, no ticket, a failed rehearsal).
    Refused,
}

impl Outcome {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Rehearsed => "rehearsed",
            Self::Applied => "applied",
            Self::Verified => "verified",
            Self::Failed => "failed",
            Self::RolledBack => "rolled_back",
            Self::Refused => "refused",
        }
    }

    /// The outcome its stable text names, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Outcome> {
        match text {
            "planned" => Some(Self::Planned),
            "rehearsed" => Some(Self::Rehearsed),
            "applied" => Some(Self::Applied),
            "verified" => Some(Self::Verified),
            "failed" => Some(Self::Failed),
            "rolled_back" => Some(Self::RolledBack),
            "refused" => Some(Self::Refused),
            _ => None,
        }
    }

    /// Whether a run may finish on this outcome.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Verified | Self::Failed | Self::RolledBack | Self::Refused
        )
    }
}

/// Which large artefact a reference names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// The release plan document.
    Plan,
    /// The rehearsal report.
    RehearsalReport,
    /// The apply journal.
    ApplyJournal,
    /// Any other evidence a caller attaches.
    Evidence,
}

impl ArtifactKind {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::RehearsalReport => "rehearsal_report",
            Self::ApplyJournal => "apply_journal",
            Self::Evidence => "evidence",
        }
    }

    /// The kind its stable text names, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<ArtifactKind> {
        match text {
            "plan" => Some(Self::Plan),
            "rehearsal_report" => Some(Self::RehearsalReport),
            "apply_journal" => Some(Self::ApplyJournal),
            "evidence" => Some(Self::Evidence),
            _ => None,
        }
    }
}

/// A reference to a large artefact stored elsewhere: where it is and the SHA-256 of its bytes.
///
/// `location` is an opaque store key or path chosen by the caller. It is **never** a connection
/// string, a password or any other secret: the record is stored in the tamper-evident chain and
/// kept forever.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Which artefact this is.
    pub kind: ArtifactKind,
    /// An opaque store key or path (never a secret).
    pub location: String,
    /// The SHA-256 of the artefact's bytes, as 64 lower-case hex characters.
    pub digest: String,
    /// The artefact's size in bytes.
    pub bytes: u64,
}

impl ArtifactRef {
    /// A reference to `bytes` stored at `location`.
    #[must_use]
    pub fn of(kind: ArtifactKind, location: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            kind,
            location: location.into(),
            digest: digest_of(bytes),
            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        }
    }

    /// Whether the recorded digest is a 64-character lower-case hex string.
    #[must_use]
    pub fn digest_well_formed(&self) -> bool {
        self.digest.len() == 64
            && self
                .digest
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    }
}

/// The SHA-256 of `bytes`, as 64 lower-case hex characters.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

/// Lower-case hex of a byte slice.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// A summary of a release plan: enough for `explain` to say what changed, without the plan
/// document itself (which is stored by reference).
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlanSummary {
    /// The plan's risk class (`additive`, `needs_review` or `destructive`).
    pub risk: String,
    /// The number of changes in total.
    pub changes: u64,
    /// Changes in the schema area.
    pub schema: u64,
    /// Changes in the metadata area.
    pub metadata: u64,
    /// Changes in the patches area.
    pub patches: u64,
    /// Changes in the behaviour area.
    pub behaviour: u64,
}

impl PlanSummary {
    /// The summary of a plan, counted from its changes.
    #[must_use]
    pub fn of(plan: &ReleasePlan) -> Self {
        Self {
            risk: risk_name(plan.risk),
            changes: u64::try_from(plan.len()).unwrap_or(u64::MAX),
            schema: count(plan, Area::Schema),
            metadata: count(plan, Area::Metadata),
            patches: count(plan, Area::Patch),
            behaviour: count(plan, Area::Behaviour),
        }
    }
}

fn count(plan: &ReleasePlan, area: Area) -> u64 {
    u64::try_from(plan.changes_in(area).len()).unwrap_or(u64::MAX)
}

/// The text of a risk class.
#[must_use]
pub fn risk_name(risk: spot_migration_plan::Risk) -> String {
    match risk {
        spot_migration_plan::Risk::Additive => "additive",
        spot_migration_plan::Risk::NeedsReview => "needs_review",
        spot_migration_plan::Risk::Destructive => "destructive",
    }
    .to_owned()
}

/// The plan as the record keeps it: a reference to the stored document and a summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRef {
    /// The stored plan document.
    pub artifact: ArtifactRef,
    /// What the plan changes.
    pub summary: PlanSummary,
}

/// Whether an approval approved or rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// The plan was approved.
    Approved,
    /// The plan was rejected.
    Rejected,
}

impl ApprovalDecision {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }

    /// The decision its stable text names, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<ApprovalDecision> {
        match text {
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

/// One approval (or rejection) of the plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRef {
    /// The approver's identity.
    pub approver: String,
    /// The role the approver held.
    pub role: String,
    /// What they decided.
    pub decision: ApprovalDecision,
    /// Unix microseconds.
    pub at: i64,
}

/// What a rehearsal found, mirroring the rehearsal engine's verdict without depending on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalVerdict {
    /// No difference and every invariant held.
    Clean,
    /// Differences, all declared by the plan, and every invariant held.
    ExpectedChanges,
    /// At least one difference the plan does not explain.
    UnexplainedDifferences,
    /// At least one invariant set failed.
    InvariantsFailed,
}

impl RehearsalVerdict {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::ExpectedChanges => "expected_changes",
            Self::UnexplainedDifferences => "unexplained_differences",
            Self::InvariantsFailed => "invariants_failed",
        }
    }

    /// The verdict its stable text names, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<RehearsalVerdict> {
        match text {
            "clean" => Some(Self::Clean),
            "expected_changes" => Some(Self::ExpectedChanges),
            "unexplained_differences" => Some(Self::UnexplainedDifferences),
            "invariants_failed" => Some(Self::InvariantsFailed),
            _ => None,
        }
    }

    /// Whether the rehearsal admits apply: a clean rehearsal or one whose only differences the
    /// plan declared.
    #[must_use]
    pub fn admits_apply(self) -> bool {
        matches!(self, Self::Clean | Self::ExpectedChanges)
    }
}

/// The rehearsal report as the record keeps it: a reference and the verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportRef {
    /// The stored rehearsal report.
    pub artifact: ArtifactRef,
    /// What the rehearsal found.
    pub verdict: RehearsalVerdict,
    /// How many findings (differences and invariant failures) it reported.
    pub findings: u64,
}

/// One migration run: the plan, its approvals, the rehearsal, the apply journal and the result.
///
/// `started` and `ended` are unix microseconds supplied by the caller; the crate reads no clock.
/// `actor` is the identity in the audit chain's form (`system`, `user:<id>` or `agent:<id>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRun {
    /// The run's id.
    pub id: RunId,
    /// The approved plan digest (from `spot-migration-approval`).
    pub plan_digest: String,
    /// The stored plan and its summary.
    pub plan: PlanRef,
    /// Every approval recorded, in order.
    pub approvals: Vec<ApprovalRef>,
    /// The rehearsal, when one was recorded.
    pub rehearsal: Option<ReportRef>,
    /// The apply journal, when apply ran.
    pub apply_journal_ref: Option<ArtifactRef>,
    /// Where the run is, or how it ended.
    pub outcome: Outcome,
    /// Unix microseconds the run began.
    pub started: i64,
    /// Unix microseconds the run ended, when it has.
    pub ended: Option<i64>,
    /// The identity that ran it.
    pub actor: String,
}

impl MigrationRun {
    /// How long the run took, in microseconds, when it has ended.
    #[must_use]
    pub fn duration_micros(&self) -> Option<i64> {
        self.ended.map(|ended| ended.saturating_sub(self.started))
    }

    /// The approvers whose latest decision was an approval, in order, distinct.
    #[must_use]
    pub fn approvers(&self) -> Vec<&str> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for approval in &self.approvals {
            if approval.decision == ApprovalDecision::Approved
                && seen.insert(approval.approver.as_str())
            {
                out.push(approval.approver.as_str());
            }
        }
        out
    }

    /// Whether the plan was rejected by anyone.
    #[must_use]
    pub fn rejected(&self) -> bool {
        self.approvals
            .iter()
            .any(|a| a.decision == ApprovalDecision::Rejected)
    }
}

/// A filter over the runs a console lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunFilter {
    /// Only runs in this outcome.
    pub outcome: Option<Outcome>,
    /// Only runs by this actor.
    pub actor: Option<String>,
    /// Only runs that started at or after this time (unix microseconds).
    pub started_after: Option<i64>,
    /// Only runs that started at or before this time (unix microseconds).
    pub started_before: Option<i64>,
}

impl RunFilter {
    /// A filter that matches every run.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// Whether `run` passes the filter.
    #[must_use]
    pub fn matches(&self, run: &MigrationRun) -> bool {
        self.outcome.is_none_or(|o| run.outcome == o)
            && self.actor.as_deref().is_none_or(|a| run.actor == a)
            && self.started_after.is_none_or(|t| run.started >= t)
            && self.started_before.is_none_or(|t| run.started <= t)
    }
}

/// A run and the chain entries that make it up: its full evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunEvidence {
    /// The folded record.
    pub run: MigrationRun,
    /// The chain entries, in chain order, each with its `seq` and hash.
    pub entries: Vec<spot_audit_chain::ChainEntry>,
}

impl RunEvidence {
    /// The first chain entry's `seq`, when there is one.
    #[must_use]
    pub fn first_seq(&self) -> Option<u64> {
        self.entries.first().map(|e| e.seq)
    }

    /// The last chain entry's `seq`, when there is one.
    #[must_use]
    pub fn last_seq(&self) -> Option<u64> {
        self.entries.last().map(|e| e.seq)
    }
}

/// A per-area count of a run's plan changes, for grouping in a console.
#[must_use]
pub fn area_counts(summary: &PlanSummary) -> BTreeMap<&'static str, u64> {
    let mut out = BTreeMap::new();
    out.insert("schema", summary.schema);
    out.insert("metadata", summary.metadata);
    out.insert("patches", summary.patches);
    out.insert("behaviour", summary.behaviour);
    out
}
