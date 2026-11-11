//! The role, duty and assignment model (WP-IAM-03.T1; ADR-070 sections 2 and 3).
//!
//! A pure, deterministic crate that compiles roles, duties and assignments into the input of
//! the permission engine (`spot-permissions`). The rule that makes it worth building:
//! **scope belongs to the assignment, never to the user** and never to a role.
//!
//! | Area | Where |
//! | --- | --- |
//! | Duties, roles, assignments, scope | [`Duty`], [`Role`], [`Assignment`], [`Scope`] |
//! | Validation: acyclic inheritance, depth 8, read-only reference roles | [`Registry`] |
//! | Copy on customise and the upgrade diff | [`copy_role`], [`diff_reference`] |
//! | Effective rights, with where each came from | [`effective`], [`ResolvedRole`] |
//! | Who may hold what: the portal rule (ADR-083 Barrier 3) | [`OutsideGuard`], [`RoleKind`] |
//! | The engine's input and expiry | [`build_snapshot_input`], [`SnapshotInput`], [`ExpiryNotice`] |
//!
//! Nothing here reads storage, a clock or any ambient state; instants are passed in. Every
//! problem is reported at once with a stable `iam.*` code ([`codes`]).

mod diff;
mod effective;
mod error;
mod model;
mod outside;
mod registry;
mod snapshot;

pub use diff::{Conflict, GrantKey, RoleDiff, copy_role, diff_reference, diff_reference_from_base};
pub use effective::{
    GrantSource, OutsideProfile, ResolvedAppPermission, ResolvedFieldGrant, ResolvedGrant,
    ResolvedRole, effective,
};
pub use error::{IamError, IamErrors, Result};
pub use model::{
    Assignment, CopiedFrom, Duty, DutyName, DutyRef, MAX_NAME, Origin, Role, RoleKind, RoleName,
    Scope, Source, Version, active_at,
};
pub use outside::{OutsideGuard, PortalHolders, check_edit, portal_holders};
pub use registry::{MAX_DEPTH, Registry, validate_assignments};
pub use snapshot::{
    ExpiryNotice, RefusedAssignment, SnapshotInput, build_snapshot_input,
    build_snapshot_input_guarded, expiry_notices,
};

/// `FieldGrant`, `RoleGrant` and `UserPermission` are the permission engine's types; they are
/// re-exported so a caller needs one import.
pub use spot_permissions::{
    AppPermissionGrant, AppPermissionName, FieldGrant, RoleGrant, UserPermission,
};

/// The stable error codes of this crate, as listed in the error catalogue.
pub mod codes {
    pub use crate::error::{
        ASSIGNMENT_WINDOW, DUPLICATE_ASSIGNMENT, DUPLICATE_NAME, DUTY_VERSION, GRANT_ROLE,
        INVALID_NAME, OUTSIDE_DUTY_NOT_EXPOSED, OUTSIDE_ROLE_EDIT_REFUSED, OUTSIDE_ROLE_KIND,
        REFERENCE_READONLY, ROLE_CYCLE, ROLE_DEPTH, UNKNOWN_DUTY, UNKNOWN_PARENT, UNKNOWN_ROLE,
    };
}
