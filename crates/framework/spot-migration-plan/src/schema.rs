//! The schema part of the plan: the shape this crate reads a live-database schema diff in.
//!
//! The diff itself lives in `spot-pg-migrate` (WP-STO-11): it compares a live schema against
//! the catalogue and refuses destructive changes. This crate never repeats that work. It
//! accepts the planner's output through the small [`SchemaPlanSource`] trait — the production
//! implementation maps a [`spot_pg_migrate::MigrationPlan`], and tests pass a [`SchemaPlan`]
//! directly, so no test needs a database.

use spot_pg_migrate::{MigrationPlan, Risk as MigrateRisk, Run as MigrateRun};

/// How one schema step runs, mapped from the backend planner without carrying its SQL.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SchemaRunKind {
    /// These statements in one short transaction.
    Transaction,
    /// One statement outside a transaction (a concurrent index build).
    Concurrent,
    /// A batched, resumable fill of a column's empty values.
    Backfill,
    /// A last sweep, then `SET NOT NULL`, in one transaction.
    SetNotNull,
    /// Install the change feed capture and record the catalogue version.
    Finalise,
    /// A check that the database's extension is available and recent enough. It never installs
    /// anything; the audited install operation does.
    Extension,
    /// One statement that validates every existing row while it blocks writes (an exclusion
    /// constraint added to a table that holds rows). Planned as a maintenance window.
    Validation,
}

/// The kind of one schema step.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SchemaStepKind {
    /// Create a table.
    AddEntity,
    /// Add a column.
    AddColumn,
    /// Create an index.
    AddIndex,
    /// Widen a numeric column.
    WidenType,
    /// Drop a `NOT NULL`, a length limit or another constraint.
    RelaxConstraint,
    /// Add a foreign key.
    AddForeignKey,
    /// Check existing records against a constraint.
    ValidateConstraint,
    /// Fill a new required column in batches.
    Backfill,
    /// Make a column required.
    SetNotNull,
    /// Drop an unusable leftover of a failed index build.
    CleanupIndex,
    /// Record the catalogue version.
    Finalise,
}

/// One ordered schema step, as this plan reads it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaStep {
    /// The planner's stable step key, for example `add_column:party.note`.
    pub key: String,
    /// The kind of change.
    pub kind: SchemaStepKind,
    /// The entity (table) affected.
    pub entity: String,
    /// The column or index affected, when the step names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// Whether the backend classifies it as additive or needing a backfill.
    pub needs_backfill: bool,
    /// How it runs.
    pub run: SchemaRunKind,
    /// What it does, in one sentence.
    pub sentence: String,
}

/// One refused schema change: the backend will not apply it online.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaRefusal {
    /// The entity (table) concerned.
    pub entity: String,
    /// The field (column) concerned, when the change is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// The backend's stable refusal code.
    pub code: String,
    /// The named reason, one sentence, no values.
    pub reason: String,
}

/// The schema diff between two systems, as this crate reads it.
///
/// The steps are already in the backend planner's execution order; [`crate::ReleasePlan::steps`]
/// keeps that order for the schema phase.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaPlan {
    /// The ordered steps.
    #[serde(default)]
    pub steps: Vec<SchemaStep>,
    /// The refused changes; a non-empty list means the schema part cannot be applied online.
    #[serde(default)]
    pub refusals: Vec<SchemaRefusal>,
}

impl SchemaPlan {
    /// Whether there is nothing to do and nothing refused.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.refusals.is_empty()
    }

    /// Maps the PostgreSQL backend's migration plan into this crate's schema plan.
    ///
    /// The step order, the refusal reasons and the backfill classification are the backend's;
    /// only the shape changes. The raw SQL the backend carries is summarised into a
    /// [`SchemaRunKind`] and never enters the plan document.
    pub fn from_migration(plan: &MigrationPlan) -> Self {
        let steps = plan
            .steps
            .iter()
            .map(|s| {
                let (kind, entity, object) = classify(&s.key);
                SchemaStep {
                    key: s.key.clone(),
                    kind,
                    entity,
                    object,
                    needs_backfill: s.risk == MigrateRisk::NeedsBackfill,
                    run: run_kind(&s.run, s.risk),
                    sentence: s.sentence.clone(),
                }
            })
            .collect();
        let refusals = plan
            .refused
            .iter()
            .map(|r| SchemaRefusal {
                entity: r.entity.clone(),
                field: r.field.clone(),
                code: r.code.to_owned(),
                reason: r.reason.clone(),
            })
            .collect();
        Self { steps, refusals }
    }
}

/// Anything that can describe the schema diff for [`crate::plan()`].
///
/// Implemented for this crate's [`SchemaPlan`] (tests pass one directly, with no database) and
/// for [`spot_pg_migrate::MigrationPlan`] (production maps the live-database planner's output).
pub trait SchemaPlanSource {
    /// The schema plan this source describes.
    fn schema_plan(&self) -> SchemaPlan;
}

impl SchemaPlanSource for SchemaPlan {
    fn schema_plan(&self) -> SchemaPlan {
        self.clone()
    }
}

impl SchemaPlanSource for MigrationPlan {
    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan::from_migration(self)
    }
}

/// The kind, entity and object of a planner step key such as `add_column:party.note`.
fn classify(key: &str) -> (SchemaStepKind, String, Option<String>) {
    let (prefix, rest) = key.split_once(':').unwrap_or((key, ""));
    let (entity, object) = match rest.split_once('.') {
        Some((e, o)) => (e.to_owned(), Some(o.to_owned())),
        None => (rest.to_owned(), None),
    };
    let kind = match prefix {
        "create_table" => SchemaStepKind::AddEntity,
        "add_column" => SchemaStepKind::AddColumn,
        "create_index" => SchemaStepKind::AddIndex,
        "widen_numeric" => SchemaStepKind::WidenType,
        "drop_not_null" | "drop_limit" | "widen_limit" => SchemaStepKind::RelaxConstraint,
        "add_fk" => SchemaStepKind::AddForeignKey,
        "validate" => SchemaStepKind::ValidateConstraint,
        "backfill" => SchemaStepKind::Backfill,
        "set_not_null" => SchemaStepKind::SetNotNull,
        "drop_invalid_index" => SchemaStepKind::CleanupIndex,
        "finalise" => SchemaStepKind::Finalise,
        _ => SchemaStepKind::AddColumn,
    };
    (kind, entity, object)
}

/// The run kind of a planner step, without its SQL.
///
/// A step whose risk is `NeedsValidation` blocks writes for the whole validation, so it is its
/// own kind whatever statement shape it runs as; an unknown risk must never read as online.
fn run_kind(run: &MigrateRun, risk: MigrateRisk) -> SchemaRunKind {
    if risk == MigrateRisk::NeedsValidation {
        return SchemaRunKind::Validation;
    }
    match run {
        MigrateRun::Tx(_) => SchemaRunKind::Transaction,
        MigrateRun::Outside(_) => SchemaRunKind::Concurrent,
        MigrateRun::Backfill(_) => SchemaRunKind::Backfill,
        MigrateRun::SetNotNull(_) => SchemaRunKind::SetNotNull,
        MigrateRun::Finalise => SchemaRunKind::Finalise,
        MigrateRun::Extension { .. } => SchemaRunKind::Extension,
    }
}
