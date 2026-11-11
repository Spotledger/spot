//! The group-to-role mapping table (ADR-070 7.2 and 7.3): `(idp_group -> role, scope)`.
//!
//! A SCIM `Group` never grants a right by itself. A row of this table says that membership of
//! one identity-provider group gives one role in one scope; a group with no row is stored and
//! listed, but grants nothing, and the administration view flags it. The table decides
//! **assignments with source `idp`** through [`spot_iam_assign`]; it never edits a role.
//!
//! [`MappingStore`] is the table's persistence contract; the in-memory implementation lives in
//! [`crate::store`]. Validation reuses [`GroupRoleMap::load`], so a bad row is refused with the
//! assignment crate's own `iamassign.*` reasons, mapped to a clean SCIM `400`.

use serde::{Deserialize, Serialize};
use spot_iam_assign::{GroupRole, GroupRoleMap, RoleDirectory, StaticRestriction};
use spot_iam_directory::UserType;
use spot_iam_roles::{OutsideGuard, Role, RoleName};

use crate::TenantId;
use crate::error::{ScimError, ScimResult};

/// One row of the mapping table.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupMapping {
    /// The identity-provider group's name, as the IdP sends it.
    pub idp_group: String,
    /// The role it gives.
    pub role: RoleName,
    /// Where the role applies; empty means everywhere.
    #[serde(default)]
    pub scope: Vec<StaticRestriction>,
}

impl GroupMapping {
    /// The row as the assignment reconciler's [`GroupRole`].
    pub fn to_group_role(&self) -> GroupRole {
        GroupRole {
            group: self.idp_group.clone(),
            role: self.role.clone(),
            scope: self.scope.clone(),
        }
    }
}

/// The table's persistence contract. Every method is scoped by tenant, so a SCIM call can never
/// read or write another tenant's mapping.
pub trait MappingStore {
    /// The tenant's rows, ordered by group then role.
    fn mappings(&self, tenant: &TenantId) -> Vec<GroupMapping>;
    /// Inserts or replaces the row for a group, validating it first.
    fn upsert_mapping(&mut self, tenant: &TenantId, mapping: GroupMapping) -> ScimResult<()>;
    /// Removes the row for a group; `true` when one was there.
    fn remove_mapping(&mut self, tenant: &TenantId, idp_group: &str) -> bool;
    /// Replaces the whole table for a tenant, validating every row first.
    fn replace_mappings(
        &mut self,
        tenant: &TenantId,
        mappings: Vec<GroupMapping>,
    ) -> ScimResult<()>;
    /// The roles that exist, for validating a row and building the mapping table.
    fn role_directory(&self) -> RoleDirectory;
    /// The tenant's role records. SCIM **reads** these to validate a mapping and never writes
    /// them; a test asserts a group edit leaves them unchanged.
    fn role_records(&self) -> Vec<Role>;
}

/// Validates a row against the roles that exist, reporting the first problem as a SCIM `400`.
pub fn validate_mapping(mapping: &GroupMapping, roles: &RoleDirectory) -> ScimResult<()> {
    GroupRoleMap::load(vec![mapping.to_group_role()], roles)
        .map(|_| ())
        .map_err(|errors| ScimError::mapping(errors.to_string()))
}

/// Builds the reconciler's validated table from the tenant's rows.
pub fn build_group_role_map(
    mappings: &[GroupMapping],
    roles: &RoleDirectory,
) -> ScimResult<GroupRoleMap> {
    GroupRoleMap::load(
        mappings.iter().map(GroupMapping::to_group_role).collect(),
        roles,
    )
    .map_err(|errors| ScimError::mapping(errors.to_string()))
}

/// A mapping row that would give a portal role-holder-to-be a role Barrier 3 of ADR-083
/// refuses: the group, the role, how many portal members the group has, and why. Names no
/// member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalMappingRefusal {
    /// The identity-provider group.
    pub idp_group: String,
    /// The role the row gives.
    pub role: RoleName,
    /// How many of the group's members are portal users.
    pub portal_members: usize,
    /// Why the role is refused for them (a stable `iam_outside.*` message).
    pub message: String,
}

/// The rows of `mappings` that [`reconcile`](crate::request) would have to refuse because a
/// member of the mapped group is a portal user and `guard` refuses the row's role for portal
/// users (ADR-083 Barrier 3). The apply path refuses them regardless; this lets validation and
/// the administration view say so before a group edit. `guard` should be built from the
/// registry when the host has one (`OutsideGuard::new`); [`OutsideGuard::from_roles`] is the
/// fail-closed fallback.
pub fn portal_mapping_refusals(
    mappings: &[GroupMapping],
    groups: &[crate::resource::StoredGroup],
    users: &[crate::resource::StoredUser],
    guard: &OutsideGuard,
) -> Vec<PortalMappingRefusal> {
    let mut out = Vec::new();
    for mapping in mappings {
        let portal_members = groups
            .iter()
            .filter(|g| g.display_name == mapping.idp_group)
            .flat_map(|g| g.members.iter())
            .filter(|member| {
                users
                    .iter()
                    .any(|u| &u.user.id == *member && u.user.user_type == UserType::Portal)
            })
            .count();
        if portal_members == 0 {
            continue;
        }
        if let Err(errors) = guard.check_role(&mapping.role) {
            out.push(PortalMappingRefusal {
                idp_group: mapping.idp_group.clone(),
                role: mapping.role.clone(),
                portal_members,
                message: errors.to_string(),
            });
        }
    }
    out.sort_by(|a, b| (&a.idp_group, &a.role).cmp(&(&b.idp_group, &b.role)));
    out
}

/// One row of the administration view: what a group grants, or that it grants nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAdminRow {
    /// The SCIM group id.
    pub id: String,
    /// The group's name.
    pub display_name: String,
    /// The roles the mapping table gives it, in order; empty when it is unmapped.
    pub roles: Vec<String>,
    /// Whether the group has no mapping row and therefore grants nothing.
    pub unmapped: bool,
    /// How many members it has.
    pub members: usize,
}

/// The administration view of a tenant's groups: every group, what it grants, and whether it is
/// unmapped. A group whose name has no mapping row is listed with `unmapped = true`.
pub fn group_admin_view(
    groups: &[crate::resource::StoredGroup],
    mappings: &[GroupMapping],
) -> Vec<GroupAdminRow> {
    let mut rows: Vec<GroupAdminRow> = groups
        .iter()
        .map(|g| {
            let mut roles: Vec<String> = mappings
                .iter()
                .filter(|m| m.idp_group == g.display_name)
                .map(|m| m.role.as_str().to_owned())
                .collect();
            roles.sort();
            roles.dedup();
            GroupAdminRow {
                id: g.id.to_string(),
                display_name: g.display_name.clone(),
                unmapped: roles.is_empty(),
                roles,
                members: g.members.len(),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    rows
}
