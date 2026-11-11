//! The fix vocabulary: ids, states, plans, changes and the evidence a run produces.
//!
//! Everything here is plain data with no clock and no randomness. A [`FixPlan`] is the
//! normalised promise of exactly what a run will change; its [`FixHash`] binds the steps, the
//! risk, reversibility and the affected targets. A [`FixState`] is the coarse lifecycle the
//! blueprint names; a run's in-flight phase is carried by the journal, not by a state the plan can
//! lie about.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use spot_agent_tiers::Tier;
use spot_remedies::{Plan as RemedyPlan, RemedyId, RemedyMeta};

use crate::error::{FixError, Result};

/// A fix's stable address within a site.
///
/// It starts with a letter or digit and holds only ASCII letters, digits, `-`, `_` and `.`.
/// [`FixId::derive`] builds the deterministic id a run uses from its source hash and sequence.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FixId(String);

impl FixId {
    /// Validates and wraps an id.
    ///
    /// # Errors
    ///
    /// [`FixError::InvalidId`] when the id is empty, too long or holds a character an id cannot.
    pub fn new(id: &str) -> Result<Self> {
        if id.is_empty() {
            return Err(FixError::InvalidId {
                id: id.to_owned(),
                detail: "it is empty".to_owned(),
            });
        }
        if id.len() > 128 {
            return Err(FixError::InvalidId {
                id: id.to_owned(),
                detail: "it is longer than 128 bytes".to_owned(),
            });
        }
        let mut chars = id.chars();
        let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
        if !first_ok || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            return Err(FixError::InvalidId {
                id: id.to_owned(),
                detail: "it must start with a letter or digit and hold only ASCII letters, \
                         digits, `-`, `_` and `.`"
                    .to_owned(),
            });
        }
        Ok(Self(id.to_owned()))
    }

    /// A deterministic id from a source hash and a sequence number.
    #[must_use]
    pub fn derive(hash: &str, seq: u64) -> Self {
        let digest = fnv1a(hash.as_bytes());
        Self(format!("fix-{digest:016x}-{seq:016x}"))
    }

    /// The id's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FixId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which structural action a T3 plan came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuralKind {
    /// A customisation bundle apply plan (`spot-custom-bundle::plan_apply`).
    CustomisationBundle,
    /// A release/migration plan (`spot-migration-plan`).
    Migration,
    /// An app install plan (`spot-installer`).
    AppInstall,
}

impl StructuralKind {
    /// The stable snake_case text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CustomisationBundle => "customisation_bundle",
            Self::Migration => "migration",
            Self::AppInstall => "app_install",
        }
    }
}

impl fmt::Display for StructuralKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a fix plan came from: a catalogued remedy's dry-run, or a T3 structural plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FixSource {
    /// A T1/T2/T3 remedy's sealed dry-run plan.
    Remedy {
        /// The remedy's id.
        remedy: RemedyId,
        /// The remedy plan's hash, so the dry-run that produced the fix plan stays auditable.
        plan_hash: String,
    },
    /// A T3 structural plan.
    Structural {
        /// Which structural action.
        structural: StructuralKind,
        /// The source plan's deterministic hash or digest.
        plan_hash: String,
    },
}

impl FixSource {
    /// A stable label for logs and the audit payload.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Remedy { remedy, .. } => format!("remedy:{remedy}"),
            Self::Structural { structural, .. } => format!("structural:{structural}"),
        }
    }
}

/// How much a fix can go wrong, ordered `Low < Medium < High`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Derived or additive state only; nothing existing can be lost.
    Low,
    /// Business state that the pre-image or snapshot restores.
    Medium,
    /// State whose reversal is only as good as the snapshot; explicit approval required.
    High,
}

impl Risk {
    /// The stable snake_case text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// The stronger of two risks.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        if other > self { other } else { self }
    }

    /// The pipeline's risk for a remedy's declared risk.
    #[must_use]
    pub fn from_remedy(risk: spot_remedies::Risk) -> Self {
        match risk {
            spot_remedies::Risk::Low => Self::Low,
            spot_remedies::Risk::Medium => Self::Medium,
            spot_remedies::Risk::High => Self::High,
        }
    }

    /// The pipeline's risk for a customisation bundle change's risk.
    #[must_use]
    pub fn from_bundle(risk: spot_custom_bundle::Risk) -> Self {
        match risk {
            spot_custom_bundle::Risk::Low => Self::Low,
            spot_custom_bundle::Risk::Medium => Self::Medium,
            spot_custom_bundle::Risk::High => Self::High,
        }
    }

    /// The pipeline's risk for a release plan change's risk.
    #[must_use]
    pub fn from_release(risk: spot_migration_plan::Risk) -> Self {
        match risk {
            spot_migration_plan::Risk::Additive => Self::Low,
            spot_migration_plan::Risk::NeedsReview => Self::Medium,
            spot_migration_plan::Risk::Destructive => Self::High,
        }
    }
}

impl fmt::Display for Risk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The hash that binds a plan to exactly its steps, risk, reversibility and affected targets.
///
/// It is a 64-bit FNV-1a over a length-prefixed canonical encoding, so two plans that would do
/// the same thing hash the same and a plan that lies about its steps is refused.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FixHash(String);

impl FixHash {
    /// Computes the hash for a plan's parts.
    #[must_use]
    pub fn compute(
        source: &FixSource,
        tier: Tier,
        steps: &[FixStep],
        risk: Risk,
        reversible: bool,
        affected: &[String],
    ) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        feed_str(&mut hash, source.label().as_str());
        match source {
            FixSource::Remedy { plan_hash, .. } | FixSource::Structural { plan_hash, .. } => {
                feed_str(&mut hash, plan_hash);
            }
        }
        feed_str(&mut hash, tier.as_str());
        feed_str(&mut hash, risk.as_str());
        feed(&mut hash, &[u8::from(reversible)]);
        for step in steps {
            feed_u64(&mut hash, step.index as u64);
            feed_str(&mut hash, &step.id);
            feed_str(&mut hash, &step.target);
            feed_str(&mut hash, &step.operation);
            feed_str(&mut hash, &step.detail);
        }
        for target in affected {
            feed_str(&mut hash, target);
        }
        Self(format!("{hash:016x}"))
    }

    /// The hash as lower-case hex.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FixHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One normalised step of a fix plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixStep {
    /// The step's zero-based position in the plan.
    pub index: usize,
    /// A stable id, unique within the plan.
    pub id: String,
    /// The typed target reference (`job:17`, `meta:party`).
    pub target: String,
    /// The typed operation's name (`job.retry`, `customise.apply`).
    pub operation: String,
    /// A sentence describing the step; never a secret or a value.
    pub detail: String,
}

impl FixStep {
    /// A step.
    #[must_use]
    pub fn new(index: usize, id: &str, target: &str, operation: &str, detail: &str) -> Self {
        Self {
            index,
            id: id.to_owned(),
            target: target.to_owned(),
            operation: operation.to_owned(),
            detail: detail.to_owned(),
        }
    }
}

/// The normalised promise of a fix: what it will change, how risky it is and whether it can be
/// taken back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixPlan {
    /// Where the plan came from.
    pub source: FixSource,
    /// The tier the plan needs; T3 is always structural and always rehearsed and approved.
    pub tier: Tier,
    /// The ordered steps.
    pub steps: Vec<FixStep>,
    /// The risk, the maximum over the source changes.
    pub risk: Risk,
    /// Whether the source declares a clear inverse. T3 is reversible through its snapshot even
    /// when a particular change is not.
    pub reversible: bool,
    /// The distinct targets the plan may change, sorted.
    pub affected: Vec<String>,
    /// The binding hash.
    pub hash: FixHash,
}

impl FixPlan {
    /// Seals and validates a plan.
    ///
    /// # Errors
    ///
    /// [`FixError::InvalidPlan`] when there are no steps, when a step's index is not its
    /// position, when an id or target is empty, or when two steps share an id.
    pub fn seal(
        source: FixSource,
        tier: Tier,
        steps: Vec<FixStep>,
        risk: Risk,
        reversible: bool,
        affected: Vec<String>,
    ) -> Result<Self> {
        if steps.is_empty() {
            return Err(FixError::InvalidPlan {
                detail: "it has no steps".to_owned(),
            });
        }
        let mut ids = BTreeSet::new();
        for (position, step) in steps.iter().enumerate() {
            if step.index != position {
                return Err(FixError::InvalidPlan {
                    detail: format!(
                        "step {position} carries index {} instead of its position",
                        step.index
                    ),
                });
            }
            if step.id.is_empty() || step.target.is_empty() {
                return Err(FixError::InvalidPlan {
                    detail: format!("step {position} has an empty id or target"),
                });
            }
            if !ids.insert(step.id.as_str()) {
                return Err(FixError::InvalidPlan {
                    detail: format!("step id `{}` is declared twice", step.id),
                });
            }
        }
        let mut affected = affected;
        if affected.is_empty() {
            affected = steps.iter().map(|s| s.target.clone()).collect();
        }
        affected.sort();
        affected.dedup();
        let hash = FixHash::compute(&source, tier, &steps, risk, reversible, &affected);
        Ok(Self {
            source,
            tier,
            steps,
            risk,
            reversible,
            affected,
            hash,
        })
    }

    /// Whether the hash still binds the plan to its contents.
    #[must_use]
    pub fn verify(&self) -> bool {
        self.hash
            == FixHash::compute(
                &self.source,
                self.tier,
                &self.steps,
                self.risk,
                self.reversible,
                &self.affected,
            )
    }

    /// A one-line summary.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} step(s) from {} at risk {} ({} target(s))",
            self.steps.len(),
            self.source.label(),
            self.risk,
            self.affected.len()
        )
    }

    /// The idempotency key a step runs under: stable for a fix, a plan and a step.
    #[must_use]
    pub fn idempotency_key(&self, fix: &FixId, step: &FixStep) -> String {
        format!("{}:{}:{}", fix.as_str(), self.hash.as_str(), step.index)
    }

    /// The remedy id, when the plan came from a remedy.
    #[must_use]
    pub fn remedy(&self) -> Option<&RemedyId> {
        match &self.source {
            FixSource::Remedy { remedy, .. } => Some(remedy),
            FixSource::Structural { .. } => None,
        }
    }

    /// Normalises a remedy's dry-run plan and metadata.
    ///
    /// # Errors
    ///
    /// [`FixError::InvalidPlan`] when the remedy's plan hash does not verify.
    pub fn from_remedy(meta: &RemedyMeta, plan: &RemedyPlan) -> Result<Self> {
        if !plan.verify() {
            return Err(FixError::InvalidPlan {
                detail: format!("the dried plan for `{}` does not verify", plan.remedy),
            });
        }
        let steps = plan
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                FixStep::new(
                    index,
                    &format!("remedy-{index}"),
                    &step.target,
                    &step.operation,
                    &step.detail,
                )
            })
            .collect();
        Self::seal(
            FixSource::Remedy {
                remedy: plan.remedy.clone(),
                plan_hash: plan.hash.as_str().to_owned(),
            },
            meta.tier,
            steps,
            Risk::from_remedy(meta.risk),
            meta.undo.is_reversible(),
            Vec::new(),
        )
    }

    /// Normalises a customisation bundle apply plan (`spot-custom-bundle::plan_apply`).
    ///
    /// # Errors
    ///
    /// [`FixError::InvalidPlan`] when the plan is empty.
    pub fn from_custom_bundle(plan: &spot_custom_bundle::ApplyPlan) -> Result<Self> {
        let mut risk = Risk::Low;
        let mut reversible = true;
        let steps = plan
            .changes()
            .iter()
            .enumerate()
            .map(|(index, change)| {
                risk = risk.max(Risk::from_bundle(change.risk));
                reversible = reversible && change.reversible;
                FixStep::new(
                    index,
                    &format!("customise-{index}"),
                    &format!("{}:{}", change.section, change.key),
                    &format!("customise.{}", action_name(change.action)),
                    &format!("{} `{}`", change.section, change.key),
                )
            })
            .collect();
        Self::seal(
            FixSource::Structural {
                structural: StructuralKind::CustomisationBundle,
                plan_hash: plan.hash().to_owned(),
            },
            Tier::T3,
            steps,
            risk,
            reversible,
            Vec::new(),
        )
    }

    /// Normalises a release (migration) plan.
    ///
    /// A migration is taken back through the snapshot the pipeline always takes for T3, so the
    /// normalised plan is reversible even when one change is destructive.
    ///
    /// # Errors
    ///
    /// [`FixError::InvalidPlan`] when the plan is empty.
    pub fn from_release_plan(plan: &spot_migration_plan::ReleasePlan) -> Result<Self> {
        let mut risk = Risk::Low;
        let steps = plan
            .changes
            .iter()
            .enumerate()
            .map(|(index, change)| {
                risk = risk.max(Risk::from_release(change.risk));
                FixStep::new(
                    index,
                    &change.id,
                    &change.target,
                    &format!("migration.{}", change.rule),
                    &change.description,
                )
            })
            .collect();
        Self::seal(
            FixSource::Structural {
                structural: StructuralKind::Migration,
                plan_hash: plan.plan_digest.clone(),
            },
            Tier::T3,
            steps,
            risk,
            true,
            Vec::new(),
        )
    }

    /// Normalises an app install plan: the ordered actions of a `spot-installer` install plan,
    /// already reduced to steps by the caller (WP-AI-08 names app installs as a T3 structural
    /// action).
    ///
    /// # Errors
    ///
    /// [`FixError::InvalidPlan`] when the plan is empty.
    pub fn from_app_install(
        plan_hash: &str,
        steps: Vec<FixStep>,
        risk: Risk,
        reversible: bool,
    ) -> Result<Self> {
        Self::seal(
            FixSource::Structural {
                structural: StructuralKind::AppInstall,
                plan_hash: plan_hash.to_owned(),
            },
            Tier::T3,
            steps,
            risk,
            reversible,
            Vec::new(),
        )
    }
}

/// The verb of a customisation change.
fn action_name(action: spot_custom_bundle::ChangeAction) -> &'static str {
    match action {
        spot_custom_bundle::ChangeAction::Add => "add",
        spot_custom_bundle::ChangeAction::Update => "update",
        spot_custom_bundle::ChangeAction::Remove => "remove",
    }
}

/// The coarse lifecycle of a fix (blueprint WS13, WP-AI-08).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixState {
    /// The run has been registered and nothing is planned.
    Proposed,
    /// A sealed plan exists.
    Planned,
    /// The plan was rehearsed on a clone and the rehearsal passed.
    Rehearsed,
    /// Every step ran and the change journal holds each change.
    Applied,
    /// Verification passed and the undo window is open.
    Verified,
    /// The inverse ran and the residue check proved the pre-state is back.
    RolledBack,
    /// The fix ended without a usable change; evidence says why.
    Failed,
}

impl FixState {
    /// The stable snake_case text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Planned => "planned",
            Self::Rehearsed => "rehearsed",
            Self::Applied => "applied",
            Self::Verified => "verified",
            Self::RolledBack => "rolled_back",
            Self::Failed => "failed",
        }
    }

    /// Whether no further transition is possible.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Verified | Self::RolledBack | Self::Failed)
    }

    /// Whether the world may have changed in this state.
    #[must_use]
    pub fn may_have_changed(self) -> bool {
        matches!(self, Self::Applied | Self::Verified)
    }
}

impl fmt::Display for FixState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which apply/rollback mechanism a run uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mechanism {
    /// The logical pre-image of every affected record.
    PreImage,
    /// A site snapshot, restored wholesale (T3 and fixes too large for the small-fix path).
    Snapshot,
}

impl Mechanism {
    /// The stable snake_case text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreImage => "pre_image",
            Self::Snapshot => "snapshot",
        }
    }
}

pub use crate::evidence::{
    AgentRef, ChangeRecord, DeploymentEpoch, FailureEvidence, FailureStage, FixOutcome, PreImage,
    RehearsalEvidence, ResidueEvidence, RollbackCause, RollbackEvidence, SnapshotRef, UndoWindow,
    VerifyReport,
};

/// Folds a byte into the FNV-1a hash.
fn feed(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// Folds a length-prefixed string into the hash.
fn feed_str(hash: &mut u64, text: &str) {
    feed_u64(hash, text.len() as u64);
    feed(hash, text.as_bytes());
}

/// Folds a big-endian u64 into the hash.
fn feed_u64(hash: &mut u64, value: u64) {
    feed(hash, &value.to_be_bytes());
}

/// A 64-bit FNV-1a hash of `bytes`.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    feed(&mut hash, bytes);
    hash
}
