//! `spot-migration-ops` — the migration lifecycle as operations (WP-MIG-04.T5).
//!
//! The lifecycle libraries are finished; this crate is where they become operations shared by
//! the CLI (`spot migrate <verb>`), the management API and the console.
//!
//! - [`context`]: [`LifecycleContext`], the ports every verb needs and nothing else. Every port
//!   is a trait object, so a verb is unit-testable without PostgreSQL. It never carries a raw
//!   database handle, and the actor is mandatory.
//! - [`error`]: [`LifecycleError`] with stable `migration.*` codes, an exit code and a
//!   management API error code.
//! - [`catalogue`]: the `site.migrate.*` operation entries (the set of the management API
//!   contract 0.2.0) and [`catalogue::handlers`], which is **fail-closed**: a verb whose module
//!   has not registered a handler refuses with `migration.not_implemented`.
//! - one module per verb ([`plan`], [`approve`], [`rehearse`], [`apply`], [`hold`], [`rescue`],
//!   [`audit`]), each owned by a later task so that none of them edits `lib.rs`.

pub mod apply;
pub mod approve;
pub mod audit;
pub mod catalogue;
pub mod context;
pub mod error;
pub mod hold;
pub mod plan;
pub mod rehearse;
pub mod rescue;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use catalogue::{
    HandlerOutput, HandlerRegistry, MIGRATION_OPS_MODULE, MigrationOpsModule, OperationHandler,
    handlers, specs,
};
pub use context::{
    Actor, Approvals, Channel, Clock, LifecycleContext, LifecyclePorts, PlanHash, PlanState,
    PlanStore, PlanSummary, RehearsalLedger, RehearsalRecord, SharedAuditSink, StoredPlan,
    off_runtime,
};
pub use error::{LifecycleError, codes};
