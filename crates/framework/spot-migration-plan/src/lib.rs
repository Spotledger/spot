//! `spot-migration-plan` — the release plan engine of the Spotledger Platform
//! (blueprint WS12, WP-MIG-01, ADR-22).
//!
//! Before an upgrade touches a site, someone must be able to read **what will happen, how risky
//! it is and how long it will take**, and the apply engine (WP-MIG-07) must run exactly that
//! and nothing else. This crate computes that plan. It changes nothing: it takes two
//! descriptions of the system and returns the difference.
//!
//! - [`SystemDescription`] is a plain, serde value: the installed apps and the manifest parts
//!   that matter, the **effective** metadata of every DocType (app metadata merged with site
//!   customisations), the controller method declarations, the registered lifecycle steps, the
//!   patches already applied, the explicit rename hints and the table statistics.
//! - [`plan()`] runs four diffs — schema (mapped from `spot-pg-migrate`'s planner through
//!   [`SchemaPlanSource`]; the diff is never duplicated here), metadata, patches and behaviour
//!   — assigns each change a risk from the documented [`RiskTable`], and estimates the duration
//!   from the statistics and a [`CostModel`].
//! - [`ReleasePlan`] is the result: a deterministic JSON document with a `plan_digest` over its
//!   canonical form, [`ReleasePlan::render_text`] and [`ReleasePlan::render_markdown`] for a
//!   human, and [`ReleasePlan::steps`] — the ordered, checkpointable steps the apply engine
//!   runs. [`verify_applied`] re-diffs the plan's target against the result, so "the plan
//!   matches what happened" is checkable.
//!
//! # What this crate does not do
//!
//! It never connects to a database, reads a clock or the environment, or applies anything. It
//! contains no float arithmetic (ADR-12), no `unsafe`, and no hand-built SQL (ADR-45): the
//! backend planner owns the SQL and this crate only reads the shape of its steps.
//!
//! # Example
//!
//! ```
//! use spot_migration_plan::{plan, SchemaPlan, SystemDescription};
//!
//! let from = SystemDescription::new();
//! let to = SystemDescription::new();
//! let release = plan(&from, &to, &SchemaPlan::default());
//! assert!(release.is_empty());
//! ```

pub mod change;
pub mod error;
pub mod estimate;
pub mod model;
pub mod plan;
pub mod render;
pub mod risk;
pub mod schema;
pub mod traversal;
pub mod traversal_model;

mod behaviour;
mod metadata;
mod patches;

pub use change::{Area, BlockingClass, Change, ChangeKind, Mismatches};
pub use error::{PlanError, codes};
pub use estimate::{ChangeEstimate, CostModel, Estimate, estimate};
pub use model::{
    AppVersion, ArgSpec, CapabilitiesSummary, DocTypeDescription, EventDecl, ManifestSummary,
    MethodDescription, PatchDecl, PlannedStep, RenameHint, ScheduleDecl, SystemDescription,
    TableStat, TableStats,
};
pub use plan::{
    ApplyStep, ApplyStepKind, ReleasePlan, plan, plan_with, plan_with_traversal, verify_applied,
};
pub use risk::{Risk, RiskPolicy, RiskTable};
pub use schema::{
    SchemaPlan, SchemaPlanSource, SchemaRefusal, SchemaRunKind, SchemaStep, SchemaStepKind,
};
pub use traversal::{TraversalIndex, TraversalLink, TraversalState, index_name};
pub use traversal_model::state_from_model;
