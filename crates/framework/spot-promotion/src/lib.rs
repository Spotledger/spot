//! `spot-promotion` — the promotion pipeline (blueprint WS19, WP-ALM-04, ADR-22).
//!
//! A customisation bundle (`spot-custom-bundle`) travels from one environment to the next
//! through the **same lifecycle a platform migration uses**: request, plan, approve, rehearse,
//! apply, verify. This crate does not build a second lifecycle; it specialises the migration
//! one:
//!
//! - [`PromotionPlan`] wraps the bundle's typed apply plan as a
//!   [`ReleasePlan`](spot_migration_plan::ReleasePlan), with a
//!   [`PlanDigest`](spot_migration_approval::PlanDigest) over the exact changes, so apply runs
//!   what was approved and refuses a plan that changed after approval.
//! - [`PromotionPolicy`] says what each target environment needs: production requires two
//!   approvers in the `owner` role (and the requester never counts), test auto-approves an
//!   additive bundle, and a rehearsal can be required per environment.
//! - Approvals go through `spot-migration-approval`'s `ApprovalService` (verified identity, role,
//!   recorded author) and `authorise_apply`; the ticket it
//!   issues is the only way to the apply step.
//! - A rehearsal returns `spot-rehearsal`'s report and verdict. **Any failure blocks the
//!   transition to `Applied`**: the promotion is marked failed, the report is recorded, and the
//!   target is never written.
//! - Apply captures a pre-image of the target first, writes through the [`BundleApplier`] port,
//!   verifies that a re-plan is empty, and on a verification failure writes the pre-image back
//!   and records the evidence.
//! - Every transition is recorded in the migration audit store
//!   (`spot-migration-audit`) under the promotion id, through the [`PromotionAudit`] port. A
//!   migration audit record is plan-scoped, so the run opens at planning; the `Requested`
//!   transition lives in the promotion journal, which the store persists with the state.
//!
//! # The state machine
//!
//! `Requested -> Planned -> Approved -> Rehearsed -> Applied -> Verified`, with `Failed` and
//! `Rejected` terminal. Transitions only move forward and every one is journaled on the
//! [`Promotion`], which is what makes a resume after a crash possible: a promotion found in
//! `Applied` is verified, never applied again.
//!
//! # What this crate does not do
//!
//! It reads no clock, no random source and no environment: every time and identity is an
//! argument. It contains no `unsafe`, no float arithmetic and no hand-built SQL. It never writes
//! to a target before approval and a passing rehearsal, and it never logs content or a secret:
//! evidence names codes, item keys and digests.
//!
//! # Example
//!
//! ```
//! use spot_promotion::PromotionState;
//!
//! let planned = PromotionState::Planned;
//! assert!(planned.allows(PromotionState::Approved));
//! assert!(!planned.allows(PromotionState::Verified));
//! ```

#![forbid(unsafe_code)]

/// The stable `promotion.*` error codes.
pub mod codes;
/// Typed errors with the stable codes.
pub mod error;
/// The bundle plan, wrapped as a release plan.
pub mod plan;
/// Approval and rehearsal policy per environment.
pub mod policy;
/// The ports the pipeline drives and the concrete bundle applier.
pub mod ports;
/// The promotion pipeline itself.
pub mod service;
/// The promotion record and its state machine.
pub mod state;
/// Where promotions live between steps.
pub mod store;

#[cfg(test)]
mod tests;

pub use error::PromotionError;
pub use plan::{IssueSource, PlanIssue, PromotionPlan};
pub use policy::{EnvironmentPolicy, PromotionPolicy};
pub use ports::{
    ApplyEvidence, BundleApplier, CustomBundleApplier, FailureEvidence, MigrationAudit, OpenRun,
    PromotionAudit, RehearsalEvidence, RehearsalRequest, RehearsalRunner, RestoreEvidence,
    TargetSite, VerifyEvidence,
};
pub use service::PromotionService;
pub use state::{Promotion, PromotionState, Transition};
pub use store::{MemoryPromotionStore, PromotionStore};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-promotion";
