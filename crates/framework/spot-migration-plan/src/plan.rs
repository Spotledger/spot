//! The plan document and the two things it drives: the review screen and the apply engine.
//!
//! [`plan`] takes two [`SystemDescription`]s and a schema diff (through
//! [`SchemaPlanSource`]), runs the four diffs, assigns risks from the rule table and estimates
//! the duration. [`ReleasePlan`] is the result: a deterministic JSON document with a
//! `plan_digest` over its canonical form, a plain-text and a Markdown rendering for a human,
//! and [`ReleasePlan::steps`] — the ordered, checkpointable steps the apply engine runs.
//! [`verify_applied`] re-diffs the plan's target against the result, so "the plan matches what
//! happened" is checkable.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::change::{Area, BlockingClass, Change, ChangeKind, Mismatches};
use crate::estimate::{CostModel, Estimate, estimate_changes};
use crate::model::SystemDescription;
use crate::risk::{Risk, RiskTable, rules};
use crate::schema::{SchemaPlan, SchemaPlanSource, SchemaRunKind, SchemaStep, SchemaStepKind};
use crate::traversal::{self, TraversalState};
use crate::{behaviour, metadata, patches};

/// The plan for one release: what changes, how risky it is, how long it takes, and a digest a
/// human approves and the apply engine checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasePlan {
    /// The metadata version of the source system.
    pub from_version: u64,
    /// The metadata version of the target system.
    pub to_version: u64,
    /// The changes, in the order they are applied: schema steps in the planner's order, then
    /// metadata (by id), then patches in dependency order, then behaviour (by id).
    pub changes: Vec<Change>,
    /// The plan's risk: the maximum over its changes.
    pub risk: Risk,
    /// The duration estimate.
    pub estimate: Estimate,
    /// SHA-256 over the canonical form of everything that determines what apply does.
    pub plan_digest: String,
    /// The target system, kept so [`verify_applied`] can re-diff it against the result.
    pub to: SystemDescription,
}

/// Builds a plan with the built-in rule table and cost model.
pub fn plan<S: SchemaPlanSource + ?Sized>(
    from: &SystemDescription,
    to: &SystemDescription,
    schema_plan: &S,
) -> ReleasePlan {
    plan_with(
        from,
        to,
        schema_plan,
        &RiskTable::builtin(),
        &CostModel::default(),
    )
}

/// Builds a plan with a given rule table (defaults plus a site policy) and cost model.
pub fn plan_with<S: SchemaPlanSource + ?Sized>(
    from: &SystemDescription,
    to: &SystemDescription,
    schema_plan: &S,
    table: &RiskTable,
    model: &CostModel,
) -> ReleasePlan {
    build(from, to, schema_plan, table, model, Vec::new())
}

/// Builds a plan that also carries the reverse index of every traversable Link field
/// (WP-STO-07.T3, see [`crate::traversal`]).
///
/// # Errors
/// [`crate::PlanError::BadInput`] when a traversable field is not a Link field of the target
/// system, or when two traversal index names clash (with each other or with a catalogue index).
pub fn plan_with_traversal<S: SchemaPlanSource + ?Sized>(
    from: &SystemDescription,
    to: &SystemDescription,
    schema_plan: &S,
    table: &RiskTable,
    model: &CostModel,
    from_traversal: &TraversalState,
    to_traversal: &TraversalState,
) -> Result<ReleasePlan, crate::PlanError> {
    let schema = schema_plan.schema_plan();
    let extra = traversal::changes(from_traversal, to, to_traversal, &schema, table)?;
    Ok(build(from, to, schema_plan, table, model, extra))
}

/// The plan, with `extra` schema changes placed before the schema's `finalise` step (or last in
/// the schema phase when there is none).
fn build<S: SchemaPlanSource + ?Sized>(
    from: &SystemDescription,
    to: &SystemDescription,
    schema_plan: &S,
    table: &RiskTable,
    model: &CostModel,
    extra: Vec<Change>,
) -> ReleasePlan {
    let schema = schema_plan.schema_plan();
    let mut changes = schema_changes(&schema, table);
    let at = changes
        .iter()
        .position(|c| c.kind == ChangeKind::SchemaFinalise)
        .unwrap_or(changes.len());
    changes.splice(at..at, extra);
    let mut meta = metadata::diff(from, to, table);
    meta.sort_by(|a, b| a.id.cmp(&b.id));
    changes.extend(meta);
    changes.extend(patches::diff(from, to, table));
    let mut behaviour_changes = behaviour::diff(from, to, table);
    behaviour_changes.sort_by(|a, b| a.id.cmp(&b.id));
    changes.extend(behaviour_changes);

    let risk = changes
        .iter()
        .map(|c| c.risk)
        .max()
        .unwrap_or(Risk::Additive);
    let estimate = estimate_changes(&changes, &to.stats, model);
    let mut release = ReleasePlan {
        from_version: from.meta_version,
        to_version: to.meta_version,
        changes,
        risk,
        estimate,
        plan_digest: String::new(),
        to: to.canonical(),
    };
    release.plan_digest = digest_of(&release);
    release
}

/// The canonical bytes the digest is computed over: everything that determines what apply does
/// (the versions, the changes, the risk and the estimate), never the target's raw content and
/// never a timestamp.
fn canonical_bytes(plan: &ReleasePlan) -> Vec<u8> {
    #[derive(Serialize)]
    struct Canonical<'a> {
        from_version: u64,
        to_version: u64,
        changes: &'a [Change],
        risk: Risk,
        estimate: &'a Estimate,
    }
    let canonical = Canonical {
        from_version: plan.from_version,
        to_version: plan.to_version,
        changes: &plan.changes,
        risk: plan.risk,
        estimate: &plan.estimate,
    };
    serde_json::to_vec(&canonical).unwrap_or_default()
}

/// The digest of a plan's canonical form.
fn digest_of(plan: &ReleasePlan) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_bytes(plan));
    hex(&hasher.finalize())
}

/// Lowercase hex of a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Maps the backend's schema plan into changes.
fn schema_changes(schema: &SchemaPlan, table: &RiskTable) -> Vec<Change> {
    let mut out = Vec::new();
    for step in &schema.steps {
        out.push(schema_change(step, table));
    }
    for refusal in &schema.refusals {
        let rule = rules::SCHEMA_BLOCKED;
        let target = match &refusal.field {
            Some(field) => format!("{}.{}", refusal.entity, field),
            None => refusal.entity.clone(),
        };
        out.push(
            Change::new(
                format!("schema/refused/{target}"),
                ChangeKind::SchemaBlocked,
                rule,
                target,
                format!("The backend refuses a schema change: {}", refusal.reason),
                table.risk_of(rule),
                BlockingClass::MaintenanceWindow,
            )
            .evidence("present", "refused")
            .why(refusal.reason.clone()),
        );
    }
    out
}

/// One schema step as a change.
fn schema_change(step: &SchemaStep, table: &RiskTable) -> Change {
    let (kind, rule, blocking) = schema_mapping(step);
    let target = match &step.object {
        Some(object) => format!("{}.{}", step.entity, object),
        None => step.entity.clone(),
    };
    let change = Change::new(
        format!("schema/{}", step.key),
        kind,
        rule,
        target,
        step.sentence.clone(),
        table.risk_of(rule),
        blocking,
    )
    .evidence("live schema", "catalogue");
    match kind {
        ChangeKind::SchemaBackfill => change.why("a backfill writes every row of the table"),
        ChangeKind::SchemaSetNotNull => change
            .why("making a column required can fail on existing records")
            .alternative("add it optional, backfill it, then make it required in a later release"),
        _ => change,
    }
}

/// The change kind, rule and blocking class of a schema step.
fn schema_mapping(step: &SchemaStep) -> (ChangeKind, &'static str, BlockingClass) {
    if step.needs_backfill {
        return (
            ChangeKind::SchemaBackfill,
            rules::SCHEMA_BACKFILL,
            BlockingClass::Online,
        );
    }
    if step.run == SchemaRunKind::Validation {
        // Validating every row blocks writes for as long as it takes.
        return (
            ChangeKind::SchemaValidate,
            rules::SCHEMA_VALIDATE,
            BlockingClass::MaintenanceWindow,
        );
    }
    match step.kind {
        SchemaStepKind::AddEntity => (
            ChangeKind::SchemaAddEntity,
            rules::SCHEMA_ADD_ENTITY,
            BlockingClass::Online,
        ),
        SchemaStepKind::AddColumn => (
            ChangeKind::SchemaAddColumn,
            rules::SCHEMA_ADD_COLUMN,
            BlockingClass::Online,
        ),
        SchemaStepKind::AddIndex => (
            ChangeKind::SchemaAddIndex,
            rules::SCHEMA_ADD_INDEX,
            BlockingClass::Online,
        ),
        SchemaStepKind::WidenType => (
            ChangeKind::SchemaWidenType,
            rules::SCHEMA_WIDEN_TYPE,
            BlockingClass::Online,
        ),
        SchemaStepKind::RelaxConstraint => (
            ChangeKind::SchemaRelaxConstraint,
            rules::SCHEMA_RELAX_CONSTRAINT,
            BlockingClass::Online,
        ),
        SchemaStepKind::AddForeignKey => (
            ChangeKind::SchemaAddForeignKey,
            rules::SCHEMA_ADD_FOREIGN_KEY,
            BlockingClass::Online,
        ),
        SchemaStepKind::ValidateConstraint => (
            ChangeKind::SchemaValidate,
            rules::SCHEMA_VALIDATE,
            BlockingClass::Online,
        ),
        SchemaStepKind::Backfill => (
            ChangeKind::SchemaBackfill,
            rules::SCHEMA_BACKFILL,
            BlockingClass::Online,
        ),
        SchemaStepKind::SetNotNull => (
            ChangeKind::SchemaSetNotNull,
            rules::SCHEMA_SET_NOT_NULL,
            BlockingClass::BriefLock,
        ),
        SchemaStepKind::CleanupIndex => (
            ChangeKind::SchemaCleanupIndex,
            rules::SCHEMA_CLEANUP_INDEX,
            BlockingClass::Online,
        ),
        SchemaStepKind::Finalise => (
            ChangeKind::SchemaFinalise,
            rules::SCHEMA_FINALISE,
            BlockingClass::Online,
        ),
    }
}

impl ReleasePlan {
    /// Whether the plan has no changes.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// How many changes the plan has.
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// The changes of one area, in plan order.
    pub fn changes_in(&self, area: Area) -> Vec<&Change> {
        self.changes.iter().filter(|c| c.area == area).collect()
    }

    /// The canonical JSON of the plan document. Deterministic: the same inputs give the same
    /// bytes.
    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// The ordered, checkpointable steps the apply engine runs: schema steps first (in the
    /// planner's order), then metadata publication, then patches, then behaviour activation.
    pub fn steps(&self) -> Vec<ApplyStep> {
        let mut steps = Vec::new();
        for change in self.changes_in(Area::Schema) {
            steps.push(self.step(
                ApplyStepKind::Schema,
                change.id.clone(),
                Some(change.id.clone()),
            ));
        }
        if !self.changes_in(Area::Metadata).is_empty() {
            steps.push(self.step(
                ApplyStepKind::PublishMetadata,
                "publish_metadata".to_owned(),
                None,
            ));
        }
        for change in self.changes_in(Area::Patch) {
            steps.push(self.step(
                ApplyStepKind::Patch,
                change.id.clone(),
                Some(change.id.clone()),
            ));
        }
        if !self.changes_in(Area::Behaviour).is_empty() {
            steps.push(self.step(
                ApplyStepKind::ActivateBehaviour,
                "activate_behaviour".to_owned(),
                None,
            ));
        }
        steps
    }

    /// One apply step, with its idempotency key bound to the plan digest.
    fn step(&self, kind: ApplyStepKind, id: String, change_id: Option<String>) -> ApplyStep {
        ApplyStep {
            idempotency_key: format!("{}:{}", self.plan_digest, id),
            id,
            kind,
            change_id,
        }
    }
}

/// The phase one apply step belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStepKind {
    /// One schema step.
    Schema,
    /// Publish the new metadata.
    PublishMetadata,
    /// One patch.
    Patch,
    /// Activate the new behaviour.
    ActivateBehaviour,
}

/// One ordered, individually identified step of the apply engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyStep {
    /// The step's id: the change's id, or `publish_metadata` / `activate_behaviour`.
    pub id: String,
    /// The phase.
    pub kind: ApplyStepKind,
    /// The change this step applies, when it applies exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// A key the apply engine can checkpoint on: the plan digest and the step id.
    pub idempotency_key: String,
}

/// Re-diffs the plan's target against the actual result and lists every difference.
///
/// The schema part is not re-diffed here: a live schema is `spot-pg-migrate`'s to compare (its
/// drift check), and this crate never reads a database. Metadata, patches and behaviour are
/// re-diffed with the built-in rule table, so an unapplied change and an unplanned one both
/// appear.
pub fn verify_applied(plan: &ReleasePlan, after: &SystemDescription) -> Mismatches {
    let table = RiskTable::builtin();
    let mut mismatches = metadata::diff(&plan.to, after, &table);
    mismatches.extend(patches::diff(&plan.to, after, &table));
    mismatches.extend(behaviour::diff(&plan.to, after, &table));
    mismatches.sort_by(|a, b| a.id.cmp(&b.id));
    Mismatches { mismatches }
}
