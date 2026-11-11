//! Assignment sources as a pure reconciler (WP-IAM-04.T1; ADR-070 sections 3, 4 and 9).
//!
//! `spot-iam-roles` models an [`Assignment`] with a [`Source`]. This crate is the logic that
//! creates, keeps and ends assignments for the two automatic sources and polices the manual
//! one. It is pure: no storage, no events, no clock. It returns a [`Plan`] of changes and the
//! event payloads, which the persistence layer applies in one transaction.
//!
//! | Need | Where |
//! | --- | --- |
//! | Rules from business facts, in the shared expression language | [`RuleSet::load`], [`plan_rules_guarded`] |
//! | IdP group mapping | [`GroupRoleMap`], [`plan_idp_guarded`] |
//! | Who may change what; IdP-managed roles | [`authorise_change`], [`ManagedRoles`] |
//! | Expiry events, exactly once | [`expire`] |
//! | Where a granted assignment's id comes from | [`IdSource`] |
//!
//! Conditions and scope values are `spot-expr` expressions over the user's [`Facts`]; write a
//! fact as `facts.company` (the language's record root `doc` is the same map).

mod error;
mod facts;
mod idp;
mod ids;
mod ownership;
mod plan;
mod reconcile;
mod rules;
mod sync;

pub use error::{AssignError, AssignErrors};
pub use facts::{FactValue, Facts, UserId};
pub use idp::{Group, GroupRole, GroupRoleMap, StaticRestriction};
pub use ids::IdSource;
pub use ownership::{Actor, ChangeRequest, ManagedRoles, authorise_change};
pub use plan::{Change, EndCause, EventPayload, Plan, UserError};
pub use rules::{RestrictionSpec, RoleDirectory, Rule, RuleSet, RuleSpec};
pub use spot_iam_roles::{Assignment, OutsideGuard, RoleName, Scope, Source};
#[allow(deprecated)]
pub use sync::{expire, plan_idp, plan_idp_guarded, plan_rules, plan_rules_guarded};

/// The stable error codes of this crate, as listed in the error catalogue.
pub mod codes {
    pub use crate::error::{
        BAD_EXPRESSION, CONDITION_NOT_BOOLEAN, DUPLICATE_RULE, EMERGENCY_END, EMERGENCY_ROLE,
        EVAL_FAILED, IDP_MANAGED, INVALID_RULE, NOT_OWNER, SCOPE_NOT_LINK, SOURCE_EDIT,
        UNKNOWN_ROLE, WRONG_SOURCE,
    };
}
