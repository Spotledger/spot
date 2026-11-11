//! The snapshot input: what the permission engine is built from (ADR-070 sections 3 and 4).
//!
//! [`build_snapshot_input`] turns the registry and the assignments into
//!
//! - the **grants** of every role, flattened (so the engine's grant table is keyed by role name
//!   and holds the duties and inherited rights already), and
//! - per user, the **active** assignments as `spot_permissions::AssignmentContext`, each with
//!   its own resolved role and **its own scope**.
//!
//! The engine evaluates assignment by assignment, so a user who is Accounting Manager for
//! Company A and Accounting Viewer for Company B has two entries, and a Company B document
//! sees only Viewer rights. This crate never combines one assignment's role with another's
//! scope. The instant is a parameter: nothing here reads a clock.

use std::collections::BTreeMap;

use spot_permissions::AppPermissionGrant;
use spot_permissions::model::{
    AssignmentContext, FieldGrant, Model, Principal, Role as GrantRole, RoleGrant,
};
use time::OffsetDateTime;

use crate::effective::{ResolvedRole, effective};
use crate::error::Result;
use crate::model::{Assignment, RoleName};
use crate::outside::OutsideGuard;
use crate::registry::{Registry, validate_assignments};

/// An assignment that is no longer in force because its end has passed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiryNotice {
    /// The assignment.
    pub assignment: String,
    /// The user it was for.
    pub user: String,
    /// The role it gave.
    pub role: RoleName,
    /// When it ended.
    pub valid_to: OffsetDateTime,
}

/// An assignment the snapshot build left out because its user is a portal user and the role
/// fails the outside allow-list (ADR-083 Barrier 3, the last line of defence).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusedAssignment {
    /// The assignment.
    pub assignment: String,
    /// The user it was for.
    pub user: String,
    /// The role it gave.
    pub role: RoleName,
    /// The stable `iam_outside.*` code that refused it.
    pub code: &'static str,
}

/// What the permission engine's snapshot is built from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotInput {
    /// Every role's flattened DocType grants, sorted by role, then DocType.
    pub grants: Vec<RoleGrant>,
    /// Every role's flattened field grants, sorted by role, then grant.
    pub field_grants: Vec<FieldGrant>,
    /// Every role's flattened named app permissions (WP-PLT-03.T3), sorted by role, then
    /// name. The engine keeps only those an installed app declares
    /// (`Snapshot::build_with_apps`); the rest are its orphans.
    pub app_grants: Vec<AppPermissionGrant>,
    /// Per user, the active assignments, ordered by assignment id.
    pub assignments: BTreeMap<String, Vec<AssignmentContext>>,
    /// Every role flattened, with where each right comes from, for `explain`.
    pub resolved: BTreeMap<RoleName, ResolvedRole>,
    /// Assignments that ended at or before the instant, for the caller to turn into events
    /// (it knows which it already raised).
    pub expired: Vec<ExpiryNotice>,
    /// Active assignments left out because a portal user may not hold the role. Empty unless
    /// built by [`build_snapshot_input_guarded`]; a non-empty list means another path let
    /// one through, and the caller should raise it.
    pub refused: Vec<RefusedAssignment>,
}

impl SnapshotInput {
    /// The model the engine loads: the grants of every role. Protection groups and shares come
    /// from elsewhere and are passed in.
    pub fn model(
        &self,
        groups: Vec<spot_permissions::ProtectionGroup>,
        shares: Vec<spot_permissions::Share>,
    ) -> Model {
        Model {
            grants: self.grants.clone(),
            field_grants: self.field_grants.clone(),
            groups,
            shares,
        }
    }

    /// The principal for `user`: their active assignments, each with its own scope. A user
    /// with none is a principal that can do nothing.
    pub fn principal(&self, user: &str) -> Principal {
        Principal::user(
            user,
            self.assignments.get(user).cloned().unwrap_or_default(),
        )
    }
}

/// Builds the engine's input at the instant `at`. Fails, naming each problem, when an
/// assignment is invalid or names a role that does not exist: a user never gets a role by
/// accident, and a broken assignment is never silently skipped.
///
/// **This build does not protect portal users** (it has no user types). A caller that knows
/// them must use [`build_snapshot_input_guarded`].
pub fn build_snapshot_input(
    registry: &Registry,
    assignments: &[Assignment],
    at: OffsetDateTime,
) -> Result<SnapshotInput> {
    build_inner(registry, assignments, at, &OutsideGuard::none())
}

/// [`build_snapshot_input`] with the portal rule (ADR-083 Barrier 3) applied last: an active
/// assignment of a portal user whose role fails the allow-list is **left out entirely** (not
/// trimmed to its exposed part) and recorded in [`SnapshotInput::refused`]. Defence in depth:
/// every assignment path already refuses it; this stops a forged or legacy record.
pub fn build_snapshot_input_guarded(
    registry: &Registry,
    assignments: &[Assignment],
    at: OffsetDateTime,
    guard: &OutsideGuard,
) -> Result<SnapshotInput> {
    build_inner(registry, assignments, at, guard)
}

fn build_inner(
    registry: &Registry,
    assignments: &[Assignment],
    at: OffsetDateTime,
    guard: &OutsideGuard,
) -> Result<SnapshotInput> {
    validate_assignments(registry, assignments)?;
    let mut input = SnapshotInput::default();
    for role in registry.roles() {
        let resolved = effective(role, registry);
        input
            .grants
            .extend(resolved.grants.iter().map(|g| g.grant.clone()));
        input
            .field_grants
            .extend(resolved.field_grants.iter().map(|g| g.grant.clone()));
        input
            .app_grants
            .extend(resolved.app_permissions.iter().map(|p| AppPermissionGrant {
                role: GrantRole(role.name.0.clone()),
                permission: p.permission.clone(),
            }));
        input.resolved.insert(role.name.clone(), resolved);
    }
    input.grants.sort();
    input.field_grants.sort();
    input.app_grants.sort();

    let mut ordered: Vec<&Assignment> = assignments.iter().collect();
    ordered.sort_by(|a, b| (&a.user, &a.id).cmp(&(&b.user, &b.id)));
    for a in ordered {
        if a.active_at(at) {
            if let Err(errors) = guard.check(&a.user, &a.role) {
                input.refused.push(RefusedAssignment {
                    assignment: a.id.clone(),
                    user: a.user.clone(),
                    role: a.role.clone(),
                    code: errors
                        .errors()
                        .first()
                        .map_or(crate::error::OUTSIDE_ROLE_KIND, |x| x.code),
                });
                continue;
            }
            input
                .assignments
                .entry(a.user.clone())
                .or_default()
                .push(AssignmentContext {
                    id: a.id.clone(),
                    roles: vec![GrantRole(a.role.0.clone())],
                    scope: a.scope.restrictions.clone(),
                });
        } else if let Some(valid_to) = a.valid_to
            && valid_to <= at
        {
            input.expired.push(ExpiryNotice {
                assignment: a.id.clone(),
                user: a.user.clone(),
                role: a.role.clone(),
                valid_to,
            });
        }
    }
    input
        .expired
        .sort_by(|a, b| (a.valid_to, &a.assignment).cmp(&(b.valid_to, &b.assignment)));
    Ok(input)
}

/// The assignments whose end falls in `(after, up_to]`, oldest first: what a periodic job
/// turns into "assignment expired" events. Pure: the caller supplies both instants.
pub fn expiry_notices(
    assignments: &[Assignment],
    after: OffsetDateTime,
    up_to: OffsetDateTime,
) -> Vec<ExpiryNotice> {
    let mut notices: Vec<ExpiryNotice> = assignments
        .iter()
        .filter_map(|a| {
            let valid_to = a.valid_to?;
            (valid_to > after && valid_to <= up_to).then(|| ExpiryNotice {
                assignment: a.id.clone(),
                user: a.user.clone(),
                role: a.role.clone(),
                valid_to,
            })
        })
        .collect();
    notices.sort_by(|a, b| (a.valid_to, &a.assignment).cmp(&(b.valid_to, &b.assignment)));
    notices
}
