//! The permission decision core (WP-PLT-03.T1): roles, DocType permissions, user permissions,
//! shares, owner rules and the lifecycle operations, as a pure, deterministic evaluator.
//!
//! # Shape
//!
//! - [`model`]: the data the engine evaluates: [`RoleGrant`](model::RoleGrant) (DocType rights),
//!   [`FieldGrant`](model::FieldGrant) and [`ProtectionGroup`](model::ProtectionGroup) (field
//!   rights, ADR-070 section 12), [`AssignmentContext`](model::AssignmentContext) with its own
//!   scope, [`Share`](model::Share); all serde and TOML-loadable, with stable `perm.*` load
//!   errors reported all at once. There is no `permlevel`.
//! - [`snapshot`]: the immutable view the engine reads, built from `spot_meta::EffectiveMeta`
//!   plus the loaded model, `Arc`-shareable; a change builds a new
//!   [`Snapshot`](snapshot::Snapshot) (the swap is WP-IAM-03's job).
//! - [`engine`]: the decisions: [`can`](engine::PermissionEngine::can),
//!   [`field_access`](engine::PermissionEngine::field_access), the field sets,
//!   [`filter_ok`](engine::PermissionEngine::filter_ok),
//!   [`check_write`](engine::PermissionEngine::check_write),
//!   [`check_links`](engine::PermissionEngine::check_links),
//!   [`allowed_values`](engine::PermissionEngine::allowed_values),
//!   [`explain`](engine::PermissionEngine::explain), and [`phase_requirements`].
//! - [`app_permission`]: named app permissions (WP-PLT-03.T3): the validated
//!   [`AppPermissionName`](app_permission::AppPermissionName), the per-site
//!   [`AppPermissionCatalogue`](app_permission::AppPermissionCatalogue), role grants, and the
//!   decision [`app_permission`](engine::PermissionEngine::app_permission), failing closed.
//! - [`reason`]: one [`Reason`](reason::Reason) per rule that granted or denied, each naming
//!   its assignment and role, plus the support text and the stable machine form.
//! - [`error`]: the one error type, with `perm.*` codes, every load error at once.
//!
//! # Rules (the behaviour this crate is, in our own words)
//!
//! - **Evaluation is per assignment.** "Manager in Company A" and "Viewer in Company B" are two
//!   assignments, each with its own scope. An assignment whose scope does not cover a document
//!   contributes nothing, no grant and no denial; rights are the union over the contributing
//!   assignments' roles; a denial from a state rule, an explicit field `deny`, or a scope that
//!   blocks every assignment is never overridden.
//! - **`if_owner` grants** apply only to documents the acting user created.
//! - **Shares** add rights on one document; they never lift a denial.
//! - **State rules**: a posted document is touched only through `update_posted`, a cancelled
//!   one is read-only.
//! - **Fields** (ADR-070 section 12): closed by default when protected, the strictest group
//!   decides, the ladder (`update` implies `read`, `reveal` needs `read_masked`), never above
//!   the DocType right, explicit deny wins, conditions, masks, and no inference through
//!   queries ([`filter_ok`](engine::PermissionEngine::filter_ok)). `hidden` is presentation,
//!   never security (ADR-068).
//! - **`correct`** (ADR-42) is cancel and amend in one transaction: it needs **both** the
//!   `cancel` and the `create` right.
//!
//! # Purity
//!
//! No storage, no clock, no ambient reads, no floats, no `unsafe`, no `unwrap`/`expect`
//! outside tests (the project coding rules).

pub mod app_permission;
pub mod engine;
pub mod error;
pub mod ids;
pub mod model;
pub mod phase;
pub mod reason;
pub mod snapshot;

pub use app_permission::{
    AppDecision, AppNameError, AppPermissionCatalogue, AppPermissionDecl, AppPermissionError,
    AppPermissionGrant, AppPermissionName, AppReason, load_app_grants,
};
pub use engine::{
    Action, Allowed, Decision, DocFacts, FIELD_DENIED, FieldChange, FieldDecision, FieldLevel,
    FieldSet, FieldViolation, PermissionEngine, Violation,
};
pub use error::{PermError, PermErrors};
pub use model::{
    AppliesTo, AssignmentContext, FieldCondition, FieldFacts, FieldGrant, FieldRight, FieldTarget,
    GroupName, MaskFormat, Model, ModelTexts, Principal, ProtectionGroup, Right, Rights, Role,
    RoleGrant, Share, ShareRight, TENANT_ADMINISTRATOR, Tag, UserPermission, load, mask,
};
pub use phase::{Check, phase_requirements};
pub use reason::Reason;
pub use snapshot::{DoctypeFacts, DocumentData, LinkField, Snapshot};

/// The name of this crate, for diagnostics and the test matrix header.
pub const CRATE_NAME: &str = "spot-permissions";
