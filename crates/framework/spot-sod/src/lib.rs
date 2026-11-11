//! The segregation-of-duties decision core (WP-SEC-02, ADR-070 section 8): rules over duties
//! and over roles for convenience, the conflict analysis over assignments, the grant-time
//! check and the overrides that demand a reason and a mitigating control.
//!
//! # Shape
//!
//! - [`model`]: the data: `Duty` and `Role` newtypes, `SodRule` with its sides and
//!   `ScopeMode` (`same_scope` or `any_scope`), `Assignment` with its own scope, the
//!   `RoleCatalogue`; all serde and TOML loadable, with stable `sod.*` load errors.
//! - [`analysis`]: the decisions: `analyze` (today's conflicts), `check_grant` (what one
//!   proposed grant would create) and `internal_conflicts` (a single role holding both
//!   sides of a rule, the Workday finding).
//! - [`override_sod`]: the override records and the decision to accept one:
//!   `SodOverride` covering exactly one (rule, user, scope), `apply_override` and the
//!   `Rights` the grantor needs.
//! - [`error`]: the one error type, with stable `sod.*` codes.
//!
//! # Rules (the behaviour this crate is, in our own words)
//!
//! - A rule is violated when a user holds **at least one duty from every side**, in the same
//!   scope for a `same_scope` rule or in any scope for an `any_scope` rule.
//! - A side may also name **roles** for convenience: a user assigned such a role satisfies
//!   that side through the role itself, even when the catalogue does not expand the role to
//!   the side's duties.
//! - Duties reach the analysis through the role catalogue (`model::RoleCatalogue`); a role
//!   the catalogue does not know expands to nothing, and still counts for a side that names
//!   it directly.
//! - The reason is part of the result: every conflict (`model::Conflict`) carries
//!   `HeldDuty` entries saying which side is satisfied, by which duty or role, held
//!   directly or through which role. Nothing is reconstructed later.
//! - `analyze` (`analysis::analyze`) is deterministic: sorted by (user, severity descending,
//!   rule id, scope), independent of the input order.
//! - `check_grant` (`analysis::check_grant`) blames a grant only for conflicts it **creates**
//!   (same rule, user and scope); conflicts already present are the access review's problem.
//! - An override covers one (rule, user, scope) only. Accepting one needs the
//!   `override_sod` right, a matching override for **every** conflicting rule, and a
//!   non-blank reason and mitigating control — otherwise a typed refusal listing exactly
//!   what is missing. An overridden conflict is still reported (`overridden: true`): the
//!   access review must show it.
//!
//! # Purity
//!
//! No storage, no clock, no network, no ambient reads, no floats, no `unsafe` (the project
//! coding rules). `granted_at` on an override is a parameter: the caller stamps it.

pub mod analysis;
#[cfg(feature = "testing")]
pub mod contract;
pub mod error;
pub mod memory;
pub mod model;
pub mod override_sod;
pub mod store;

pub use analysis::{
    GrantCheck, InternalConflict, analyze, check_grant, holdings_of, internal_conflicts,
};
pub use error::SodError;
pub use memory::MemorySodStore;
pub use model::{
    Assignment, Conflict, Duty, Grant, HeldDuty, Hold, Role, RoleCatalogue, RuleSet, ScopeMode,
    Severity, Side, SodRule, Via,
};
pub use override_sod::{
    OVERRIDE_SOD_RIGHT, OverrideDecision, OverrideRefusal, Rights, SodOverride, Uncovered,
    apply_override,
};
pub use store::{SodStore, SodStoreError};

/// The crate name, for diagnostics.
pub const CRATE_NAME: &str = "spot-sod";
