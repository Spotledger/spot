//! The promotion plan: a customisation bundle apply, wrapped as a `ReleasePlan`-shaped plan.
//!
//! The migration lifecycle already has a plan document ([`ReleasePlan`]), a digest a human
//! approves ([`PlanDigest`]) and a rehearsal that runs that exact plan. A bundle promotion does
//! not need a second lifecycle: [`PromotionPlan::assemble`] turns the bundle's typed
//! [`ApplyPlan`] changes into plan changes, folds any blocking or warning conflicts into the same
//! document, and binds the whole thing with a digest.
//!
//! Every change carries the deterministic hash of the item it writes, so the digest covers the
//! exact desired bytes and not just the item names. Apply re-plans the bundle against the target,
//! compares the apply plan's hash with the approved one, and refuses a plan that changed after
//! approval.
//!
//! [`ReleasePlan`]: spot_migration_plan::ReleasePlan
//! [`PlanDigest`]: spot_migration_approval::PlanDigest
//! [`ApplyPlan`]: spot_custom_bundle::ApplyPlan

use spot_bundle_merge::MergeResult;
use spot_custom_bundle::{
    ApplyPlan, Bundle, ChangeAction as BundleAction, Digest, Risk as BundleRisk, digest::sha256,
};
use spot_migration_approval::PlanDigest;
use spot_migration_conflicts::ConflictReport;
use spot_migration_plan::{
    BlockingClass, Change, ChangeKind, Estimate, ReleasePlan, Risk, SystemDescription, TableStats,
};

use crate::error::PromotionError;

/// Where a plan item came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IssueSource {
    /// An unresolved three-way merge conflict (`spot-bundle-merge`).
    Merge,
    /// A customisation-versus-release conflict (`spot-migration-conflicts`).
    Upgrade,
}

impl IssueSource {
    /// The source's stable lower-case name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Upgrade => "upgrade",
        }
    }
}

/// One conflict or refusal folded into the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanIssue {
    /// The crate the item came from.
    pub source: IssueSource,
    /// The stable conflict code (`bundle_merge.*` or `migration_conflicts.*`).
    pub code: String,
    /// The item the conflict is filed under.
    pub item: String,
    /// One plain-language sentence for a reviewer.
    pub message: String,
    /// How risky the item is.
    pub risk: Risk,
    /// Whether the item stops apply until it is resolved.
    pub blocking: bool,
}

impl PlanIssue {
    /// Every unresolved merge conflict as a **blocking** item: an unresolved conflict means the
    /// merged content is not the bundle anyone reviewed, so nothing may be promoted.
    #[must_use]
    pub fn from_merge(result: &MergeResult) -> Vec<PlanIssue> {
        result
            .conflicts
            .iter()
            .map(|conflict| PlanIssue {
                source: IssueSource::Merge,
                code: conflict.kind.code().to_owned(),
                item: format!("{}/{}", conflict.section(), conflict.key()),
                message: conflict.explanation.clone(),
                risk: Risk::Destructive,
                blocking: true,
            })
            .collect()
    }

    /// Every customisation-versus-release conflict, blocking when it is destructive and a warning
    /// otherwise, so a review-only item does not stop a promotion that a person can accept.
    #[must_use]
    pub fn from_upgrade(report: &ConflictReport) -> Vec<PlanIssue> {
        report
            .items
            .iter()
            .map(|item| {
                let target = item
                    .artefacts
                    .first()
                    .map_or_else(|| item.cause.release_item.clone(), |a| a.pointer());
                PlanIssue {
                    source: IssueSource::Upgrade,
                    code: item.code.to_owned(),
                    item: target,
                    message: format!("{} {}", item.cause.effect, item.resolution),
                    risk: item.risk,
                    blocking: item.risk == Risk::Destructive,
                }
            })
            .collect()
    }
}

/// A bundle promotion, planned as a release plan.
#[derive(Clone, Debug)]
pub struct PromotionPlan {
    release: ReleasePlan,
    digest: PlanDigest,
    apply: ApplyPlan,
    bundle_digest: Digest,
    issues: Vec<PlanIssue>,
    warnings: Vec<String>,
}

impl PromotionPlan {
    /// Assembles the plan from a bundle's apply plan and the conflicts a reviewer supplied.
    ///
    /// # Errors
    /// [`PromotionError::NothingToPromote`] when the bundle matches the target exactly, and
    /// [`PromotionError::PlanFailed`] if a change cannot be represented.
    pub(crate) fn assemble(
        bundle: &Bundle,
        apply: &ApplyPlan,
        issues: Vec<PlanIssue>,
        binding: &str,
    ) -> Result<PromotionPlan, PromotionError> {
        if apply.is_empty() {
            return Err(PromotionError::NothingToPromote);
        }
        let mut changes: Vec<Change> = apply.changes().iter().map(plan_change).collect();
        // A warning conflict is part of the reviewed document, so the person approving sees it
        // and the digest covers it. A blocking one never reaches here: planning refuses first.
        changes.extend(
            issues
                .iter()
                .enumerate()
                .map(|(index, issue)| issue_change(issue, index)),
        );
        let risk = changes
            .iter()
            .map(|change| change.risk)
            .max()
            .unwrap_or(Risk::Additive);
        let mut release = ReleasePlan {
            // A customisation bundle carries no metadata version of its own: the pair is a
            // placeholder, and the hashed change list is the plan's identity.
            from_version: 0,
            to_version: 1,
            changes,
            risk,
            estimate: Estimate::empty(),
            plan_digest: String::new(),
            to: SystemDescription::new(),
        };
        release.estimate = spot_migration_plan::estimate(
            &release,
            &TableStats::default(),
            &spot_migration_plan::CostModel::default(),
        );
        release.plan_digest = canonical_digest(&release, binding);
        let digest = PlanDigest::of(&release);
        let warnings = apply
            .warnings()
            .iter()
            .map(|problem| format!("{}: {}", problem.code, problem.message))
            .collect();
        Ok(PromotionPlan {
            bundle_digest: bundle.digest(),
            apply: apply.clone(),
            release,
            digest,
            issues,
            warnings,
        })
    }

    /// The release plan a reviewer reads and a rehearsal runs.
    #[must_use]
    pub fn release(&self) -> &ReleasePlan {
        &self.release
    }

    /// The digest of the whole plan document: what approval covers and apply checks.
    #[must_use]
    pub fn digest(&self) -> &PlanDigest {
        &self.digest
    }

    /// The bundle apply plan itself: the exact changes apply writes.
    #[must_use]
    pub fn apply_plan(&self) -> &ApplyPlan {
        &self.apply
    }

    /// The bundle apply plan's deterministic hash. Apply re-plans and refuses a different one.
    #[must_use]
    pub fn apply_hash(&self) -> &str {
        self.apply.hash()
    }

    /// The digest of the bundle this plan was built from.
    #[must_use]
    pub fn bundle_digest(&self) -> Digest {
        self.bundle_digest
    }

    /// How many items the apply plan writes.
    #[must_use]
    pub fn apply_changes(&self) -> usize {
        self.apply.changes().len()
    }

    /// Every blocking or warning conflict folded into the plan.
    #[must_use]
    pub fn issues(&self) -> &[PlanIssue] {
        &self.issues
    }

    /// The blocking conflicts, in plan order.
    #[must_use]
    pub fn blocking_issues(&self) -> Vec<&PlanIssue> {
        self.issues.iter().filter(|issue| issue.blocking).collect()
    }

    /// The bundle planner's compatibility warnings, in plain text.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Checks that the plan still hashes to the digest approval covered.
    ///
    /// # Errors
    /// [`PromotionError::PlanChanged`] when a change was added, removed or edited in place.
    pub fn check_unchanged(&self) -> Result<(), PromotionError> {
        let found = PlanDigest::of(&self.release);
        if found != self.digest {
            return Err(PromotionError::PlanChanged {
                expected: self.digest.as_hex().to_owned(),
                found: found.as_hex().to_owned(),
            });
        }
        Ok(())
    }
}

/// One bundle change as a plan change.
///
/// `spot-migration-plan` owns a closed `ChangeKind` vocabulary with no customisation-specific
/// variants, so this reuses the nearest existing kind (the same approach
/// `spot-migration-conflicts` documents for its `to_change`). Nothing is lost: the change's `id`
/// carries the exact section and item key, its `after` evidence is the item's deterministic hash,
/// and its `rule` names the promotion section.
fn plan_change(change: &spot_custom_bundle::Change) -> Change {
    let blocking = match change.action {
        BundleAction::Add => BlockingClass::Online,
        BundleAction::Update => BlockingClass::BriefLock,
        BundleAction::Remove => BlockingClass::MaintenanceWindow,
    };
    let before = match change.action {
        BundleAction::Add => "absent",
        BundleAction::Update | BundleAction::Remove => "present",
    };
    Change::new(
        format!("{}/{}", change.section, change.key),
        plan_kind(&change.section, change.action),
        format!("promotion/{}", change.section),
        change.key.clone(),
        format!(
            "{} {} in the {} section",
            action_name(change.action),
            change.key,
            change.section
        ),
        plan_risk(change.risk),
        blocking,
    )
    .evidence(before, change.hash.clone())
}

/// The action's lower-case name.
fn action_name(action: BundleAction) -> &'static str {
    match action {
        BundleAction::Add => "add",
        BundleAction::Update => "update",
        BundleAction::Remove => "remove",
    }
}

/// The nearest plan kind for one section's action.
fn plan_kind(section: &str, action: BundleAction) -> ChangeKind {
    match (section, action) {
        ("meta", BundleAction::Add) => ChangeKind::FieldAdded,
        ("meta", BundleAction::Update) => ChangeKind::FieldRequiredChanged,
        ("meta", BundleAction::Remove) => ChangeKind::FieldRemoved,
        ("views", _) => ChangeKind::ViewLayerChanged,
        ("scripts", BundleAction::Add) => ChangeKind::StepAdded,
        ("scripts", BundleAction::Update) => ChangeKind::StepReprioritised,
        ("scripts", BundleAction::Remove) => ChangeKind::StepRemoved,
        ("rules", BundleAction::Remove) => ChangeKind::PermissionRuleRemoved,
        ("rules", _) => ChangeKind::PermissionRuleAdded,
        ("fixtures", BundleAction::Add) => ChangeKind::FieldAdded,
        ("fixtures", BundleAction::Update) => ChangeKind::FieldDefaultChanged,
        ("fixtures", BundleAction::Remove) => ChangeKind::FieldRemoved,
        // An unknown section is carried opaquely by the bundle; treat it like metadata.
        (_, BundleAction::Add) => ChangeKind::FieldAdded,
        (_, BundleAction::Update) => ChangeKind::FieldRequiredChanged,
        (_, BundleAction::Remove) => ChangeKind::FieldRemoved,
    }
}

/// One conflict as a plan change, so a reviewer reads it in the plan document.
fn issue_change(issue: &PlanIssue, index: usize) -> Change {
    let blocking = match issue.risk {
        Risk::Destructive => BlockingClass::MaintenanceWindow,
        Risk::NeedsReview => BlockingClass::BriefLock,
        Risk::Additive => BlockingClass::Online,
    };
    Change::new(
        format!("conflict/{}/{}", issue.code, index),
        ChangeKind::FieldRemoved,
        format!("promotion/conflict/{}", issue.source.as_str()),
        issue.item.clone(),
        issue.message.clone(),
        issue.risk,
        blocking,
    )
    .why(issue.message.clone())
}

/// The bundle's three risk classes onto the plan's three.
fn plan_risk(risk: BundleRisk) -> Risk {
    match risk {
        BundleRisk::Low => Risk::Additive,
        BundleRisk::Medium => Risk::NeedsReview,
        BundleRisk::High => Risk::Destructive,
    }
}

/// SHA-256 over the same canonical projection `spot-migration-plan` documents: the two versions,
/// the ordered changes, the risk and the estimate — never the target document and never a
/// timestamp. The promotion assembles its plan here rather than through the platform planner
/// (a customisation bundle changes no platform schema or metadata), so it computes the field the
/// planner would have computed. `PlanDigest::of` hashes the whole document and therefore also
/// covers this value.
///
/// `binding` names the promotion and its route. It is hashed in so an approval is valid for this
/// promotion only: the same bundle promoted again, or to another environment, is a different
/// digest and so cannot reuse an earlier promotion's approvals.
fn canonical_digest(plan: &ReleasePlan, binding: &str) -> String {
    #[derive(serde::Serialize)]
    struct Canonical<'a> {
        from_version: u64,
        to_version: u64,
        changes: &'a [Change],
        risk: Risk,
        estimate: &'a Estimate,
        binding: &'a str,
    }
    let canonical = Canonical {
        from_version: plan.from_version,
        to_version: plan.to_version,
        changes: &plan.changes,
        risk: plan.risk,
        estimate: &plan.estimate,
        binding,
    };
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    hex(&sha256(&bytes))
}

/// Lowercase hex of a SHA-256 result.
fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
