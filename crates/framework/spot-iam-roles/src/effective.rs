//! Effective rights: a role flattened with its duties and ancestors (ADR-070 sections 2 and 3.4).
//!
//! The result is deterministic: the same registry and role give the same rights in the same
//! order, whatever order the input was written in (grants are sorted by their key, field
//! grants by their value). Identical rights collapse into one, and every grant keeps **where it
//! came from**: the role path and the duty, so `explain` can say which assignment granted a
//! right and through which role (the assignment id comes from the context the evaluator
//! holds; the role path comes from here).
//!
//! A field `deny` anywhere in the closure stays a deny: a field grant is never merged with a
//! different one, so the engine, which treats a deny as final, sees it.

use std::collections::{BTreeMap, BTreeSet};

use spot_permissions::AppPermissionName;
use spot_permissions::model::{FieldGrant, Rights, Role as GrantRole, RoleGrant};

use crate::model::{DutyName, Role, RoleKind, RoleName};
use crate::registry::Registry;

/// Where a right came from.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GrantSource {
    /// The roles from the resolved role down to the one that holds the right: the resolved
    /// role first, the defining role last. One name when the role grants it itself.
    pub role_path: Vec<RoleName>,
    /// The duty that grants it, or `None` for a direct grant of the last role in the path.
    pub duty: Option<DutyName>,
}

/// A DocType grant of a resolved role, with where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedGrant {
    /// The grant, keyed by the resolved role's name.
    pub grant: RoleGrant,
    /// Every place that grants it, in the order they were found.
    pub sources: Vec<GrantSource>,
}

/// A field grant of a resolved role, with where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedFieldGrant {
    /// The grant, keyed by the resolved role's name.
    pub grant: FieldGrant,
    /// Every place that grants it, in the order they were found.
    pub sources: Vec<GrantSource>,
}

/// A named app permission of a resolved role, with where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAppPermission {
    /// The permission.
    pub permission: AppPermissionName,
    /// Every place that grants it, in the order they were found.
    pub sources: Vec<GrantSource>,
}

/// What a resolved role reaches that Barrier 3 of ADR-083 cares about: gathered over the whole
/// closure (the role, its duties and every ancestor), each list sorted and without repeats.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutsideProfile {
    /// Roles in the closure (the role itself included) whose kind is not `Portal`.
    pub desk_roles: Vec<RoleName>,
    /// Duties in the closure that are not marked outside-exposed.
    pub unexposed_duties: Vec<DutyName>,
    /// Roles in the closure that grant rights directly. A direct right carries no duty and so
    /// no exposure mark: it counts as not exposed.
    pub direct_right_roles: Vec<RoleName>,
}

/// A role with inheritance and duties flattened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRole {
    /// The role.
    pub name: RoleName,
    /// The role's own kind.
    pub kind: RoleKind,
    /// What the closure reaches for the outside-user rule.
    pub outside: OutsideProfile,
    /// DocType grants, sorted by doctype, then `if_owner`.
    pub grants: Vec<ResolvedGrant>,
    /// Field grants, sorted.
    pub field_grants: Vec<ResolvedFieldGrant>,
    /// Named app permissions, sorted by name.
    pub app_permissions: Vec<ResolvedAppPermission>,
}

impl ResolvedRole {
    /// The places that grant the app permission `name`, for `explain`.
    pub fn sources_of_app_permission(&self, name: &AppPermissionName) -> &[GrantSource] {
        self.app_permissions
            .iter()
            .find(|p| &p.permission == name)
            .map_or(&[], |p| p.sources.as_slice())
    }

    /// The places that grant `doctype` (with `if_owner` as given), for `explain`.
    pub fn sources_of(&self, doctype: &str, if_owner: bool) -> &[GrantSource] {
        self.grants
            .iter()
            .find(|g| g.grant.doctype == doctype && g.grant.if_owner == if_owner)
            .map_or(&[], |g| g.sources.as_slice())
    }
}

/// Flattens `role` over `registry`: its duties' grants, its direct grants, then its parents'
/// (depth first, in the order the parents are listed; a role reached twice counts once).
pub fn effective(role: &Role, registry: &Registry) -> ResolvedRole {
    let mut found: Vec<(RoleGrant, GrantSource)> = Vec::new();
    let mut found_fields: Vec<(FieldGrant, GrantSource)> = Vec::new();
    let mut found_apps: Vec<(AppPermissionName, GrantSource)> = Vec::new();
    let mut seen: BTreeSet<RoleName> = BTreeSet::new();
    seen.insert(role.name.clone());
    let mut out = Found {
        grants: &mut found,
        fields: &mut found_fields,
        apps: &mut found_apps,
    };
    let mut profile = Profile::default();
    walk(
        role,
        registry,
        vec![role.name.clone()],
        &mut seen,
        &mut out,
        &mut profile,
    );

    let mut merged: BTreeMap<(String, bool), (Rights, Vec<GrantSource>)> = BTreeMap::new();
    for (grant, source) in found {
        let entry = merged
            .entry((grant.doctype, grant.if_owner))
            .or_insert_with(|| (Rights::new([]), Vec::new()));
        entry.0 = entry.0.union(&grant.rights);
        if !entry.1.contains(&source) {
            entry.1.push(source);
        }
    }
    let grants = merged
        .into_iter()
        .map(|((doctype, if_owner), (rights, sources))| ResolvedGrant {
            grant: RoleGrant {
                doctype,
                role: GrantRole(role.name.0.clone()),
                rights,
                if_owner,
            },
            sources,
        })
        .collect();

    let mut fields: BTreeMap<FieldGrant, Vec<GrantSource>> = BTreeMap::new();
    for (grant, source) in found_fields {
        let sources = fields.entry(grant).or_default();
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    let field_grants = fields
        .into_iter()
        .map(|(grant, sources)| ResolvedFieldGrant { grant, sources })
        .collect();

    let mut apps: BTreeMap<AppPermissionName, Vec<GrantSource>> = BTreeMap::new();
    for (permission, source) in found_apps {
        let sources = apps.entry(permission).or_default();
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    let app_permissions = apps
        .into_iter()
        .map(|(permission, sources)| ResolvedAppPermission {
            permission,
            sources,
        })
        .collect();

    ResolvedRole {
        name: role.name.clone(),
        kind: role.kind,
        outside: OutsideProfile {
            desk_roles: profile.desk.into_iter().collect(),
            unexposed_duties: profile.unexposed.into_iter().collect(),
            direct_right_roles: profile.direct.into_iter().collect(),
        },
        grants,
        field_grants,
        app_permissions,
    }
}

/// What the walk collects.
struct Found<'a> {
    grants: &'a mut Vec<(RoleGrant, GrantSource)>,
    fields: &'a mut Vec<(FieldGrant, GrantSource)>,
    apps: &'a mut Vec<(AppPermissionName, GrantSource)>,
}

#[derive(Default)]
struct Profile {
    desk: BTreeSet<RoleName>,
    unexposed: BTreeSet<DutyName>,
    direct: BTreeSet<RoleName>,
}

fn walk(
    role: &Role,
    registry: &Registry,
    path: Vec<RoleName>,
    seen: &mut BTreeSet<RoleName>,
    out: &mut Found<'_>,
    profile: &mut Profile,
) {
    let key = GrantRole(path[0].0.clone());
    if role.kind != RoleKind::Portal {
        profile.desk.insert(role.name.clone());
    }
    if !role.direct_grants.is_empty()
        || !role.direct_field_grants.is_empty()
        || !role.direct_app_permissions.is_empty()
    {
        profile.direct.insert(role.name.clone());
    }
    for used in &role.duties {
        let Some(duty) = registry.duty(&used.name) else {
            // Cannot happen in a validated registry; a duty we cannot see is not exposed.
            profile.unexposed.insert(used.name.clone());
            continue;
        };
        if !duty.outside_exposed {
            profile.unexposed.insert(duty.name.clone());
        }
        let source = GrantSource {
            role_path: path.clone(),
            duty: Some(duty.name.clone()),
        };
        for grant in &duty.grants {
            out.grants.push((rekey(grant, &key), source.clone()));
        }
        for grant in &duty.field_grants {
            out.fields.push((rekey_field(grant, &key), source.clone()));
        }
        for permission in &duty.app_permissions {
            out.apps.push((permission.clone(), source.clone()));
        }
    }
    let direct = GrantSource {
        role_path: path.clone(),
        duty: None,
    };
    for grant in &role.direct_grants {
        out.grants.push((rekey(grant, &key), direct.clone()));
    }
    for grant in &role.direct_field_grants {
        out.fields.push((rekey_field(grant, &key), direct.clone()));
    }
    for permission in &role.direct_app_permissions {
        out.apps.push((permission.clone(), direct.clone()));
    }
    for parent in &role.parents {
        if !seen.insert(parent.clone()) {
            continue;
        }
        let Some(parent_role) = registry.role(parent) else {
            // Cannot happen in a validated registry; a parent we cannot see is not portal.
            profile.desk.insert(parent.clone());
            continue;
        };
        let mut deeper = path.clone();
        deeper.push(parent.clone());
        walk(parent_role, registry, deeper, seen, out, profile);
    }
}

fn rekey(grant: &RoleGrant, role: &GrantRole) -> RoleGrant {
    RoleGrant {
        role: role.clone(),
        ..grant.clone()
    }
}

fn rekey_field(grant: &FieldGrant, role: &GrantRole) -> FieldGrant {
    FieldGrant {
        role: role.clone(),
        ..grant.clone()
    }
}
