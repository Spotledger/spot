//! Copy on customise, and the upgrade diff (ADR-070 section 2).
//!
//! A reference role is read-only. To change one, a tenant **copies** it ([`copy_role`]): the
//! copy is a custom role that remembers its source and version. When a new version of the
//! reference role arrives, [`diff_reference`] reports what changed and what conflicts with the
//! tenant's customisation. **The copy is never rewritten**: the diff only reports, and the
//! administrator decides.
//!
//! [`diff_reference`] compares the copy with the new reference role (it has no record of the
//! old one). When the old reference role is at hand, [`diff_reference_from_base`] does the exact
//! three-way comparison and names the items both sides changed.

use std::collections::BTreeMap;

use spot_permissions::AppPermissionName;
use spot_permissions::model::{Rights, Role as GrantRole};

use crate::model::{CopiedFrom, DutyName, DutyRef, Origin, Role, RoleName, Version};

/// What a customisation changed that an upgrade also changes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Conflict {
    /// The duty is in both, at different versions.
    Duty {
        /// The duty.
        name: DutyName,
    },
    /// The DocType grant (`doctype`, `if_owner`) is in both, with different rights.
    Grant {
        /// The DocType.
        doctype: String,
        /// Whether it is the owner-only grant.
        if_owner: bool,
    },
}

/// A DocType grant, by its key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GrantKey {
    /// The DocType.
    pub doctype: String,
    /// Whether it is the owner-only grant.
    pub if_owner: bool,
}

/// What an upgrade changes, and what it would collide with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoleDiff {
    /// Duties the new reference role includes.
    pub duties_added: Vec<DutyName>,
    /// Duties the new reference role no longer includes.
    pub duties_removed: Vec<DutyName>,
    /// DocType grants the new reference role adds.
    pub grants_added: Vec<GrantKey>,
    /// DocType grants the new reference role drops.
    pub grants_removed: Vec<GrantKey>,
    /// App permissions the new reference role adds.
    pub app_permissions_added: Vec<AppPermissionName>,
    /// App permissions the new reference role drops.
    pub app_permissions_removed: Vec<AppPermissionName>,
    /// Parents the new reference role adds.
    pub parents_added: Vec<RoleName>,
    /// Parents the new reference role drops.
    pub parents_removed: Vec<RoleName>,
    /// Where the customisation and the upgrade disagree.
    pub conflicts_with_customisation: Vec<Conflict>,
}

impl RoleDiff {
    /// Whether the upgrade changes nothing.
    pub fn is_empty(&self) -> bool {
        self.duties_added.is_empty()
            && self.duties_removed.is_empty()
            && self.grants_added.is_empty()
            && self.grants_removed.is_empty()
            && self.app_permissions_added.is_empty()
            && self.app_permissions_removed.is_empty()
            && self.parents_added.is_empty()
            && self.parents_removed.is_empty()
            && self.conflicts_with_customisation.is_empty()
    }
}

/// A custom copy of `source` under `new_name`, recording where it came from.
///
/// Copying a reference role records that role and its pack version. Copying a custom role
/// keeps the lineage of the original (a copy of a copy of "Sales Manager" v3 still upgrades
/// against "Sales Manager" v3). Duties, grants and parents are copied as they are, with the
/// grants re-keyed to the new name.
pub fn copy_role(source: &Role, new_name: &str) -> Role {
    let copied_from = match &source.origin {
        Origin::Reference { version, .. } => Some(CopiedFrom {
            role: source.name.clone(),
            version: *version,
        }),
        Origin::Custom { copied_from } => copied_from.clone(),
    };
    let key = GrantRole(new_name.to_owned());
    Role {
        name: RoleName::from(new_name),
        origin: Origin::Custom { copied_from },
        duties: source.duties.clone(),
        direct_grants: source
            .direct_grants
            .iter()
            .map(|g| spot_permissions::RoleGrant {
                role: key.clone(),
                ..g.clone()
            })
            .collect(),
        direct_field_grants: source
            .direct_field_grants
            .iter()
            .map(|g| spot_permissions::FieldGrant {
                role: key.clone(),
                ..g.clone()
            })
            .collect(),
        direct_app_permissions: source.direct_app_permissions.clone(),
        parents: source.parents.clone(),
        kind: source.kind,
    }
}

/// What the new reference role changes compared with the customised copy.
///
/// Added and removed are relative to the copy: an item the new reference role has and the copy
/// lacks is *added*, one the copy has and the new reference role lacks is *removed* (which may
/// be the tenant's own addition; the administrator decides). An item in both with a different
/// version or different rights is a conflict. The copy is not touched.
pub fn diff_reference(copy: &Role, new_reference: &Role) -> RoleDiff {
    diff(None, copy, new_reference)
}

/// As [`diff_reference`], with the old reference role the copy was made from: added and removed
/// are then what the *upgrade* changes (new against old), and a conflict is an item both the
/// upgrade and the tenant changed, differently.
pub fn diff_reference_from_base(base: &Role, copy: &Role, new_reference: &Role) -> RoleDiff {
    diff(Some(base), copy, new_reference)
}

fn duty_map(role: &Role) -> BTreeMap<DutyName, Option<Version>> {
    role.duties
        .iter()
        .map(|d: &DutyRef| (d.name.clone(), d.version))
        .collect()
}

fn grant_map(role: &Role) -> BTreeMap<GrantKey, Rights> {
    let mut map: BTreeMap<GrantKey, Rights> = BTreeMap::new();
    for g in &role.direct_grants {
        let key = GrantKey {
            doctype: g.doctype.clone(),
            if_owner: g.if_owner,
        };
        let rights = match map.get(&key) {
            Some(existing) => existing.union(&g.rights),
            None => g.rights.clone(),
        };
        map.insert(key, rights);
    }
    map
}

fn app_permission_map(role: &Role) -> BTreeMap<AppPermissionName, ()> {
    role.direct_app_permissions
        .iter()
        .map(|p| (p.clone(), ()))
        .collect()
}

fn parent_map(role: &Role) -> BTreeMap<RoleName, ()> {
    role.parents.iter().map(|p| (p.clone(), ())).collect()
}

/// The keys added, removed and in conflict. `base` is the old reference role's map for a
/// three-way comparison.
fn compare<K: Ord + Clone, V: PartialEq>(
    base: Option<&BTreeMap<K, V>>,
    copy: &BTreeMap<K, V>,
    new: &BTreeMap<K, V>,
) -> (Vec<K>, Vec<K>, Vec<K>) {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut conflicts = Vec::new();
    match base {
        None => {
            for (k, v) in new {
                match copy.get(k) {
                    None => added.push(k.clone()),
                    Some(c) if c != v => conflicts.push(k.clone()),
                    Some(_) => {}
                }
            }
            for k in copy.keys() {
                if !new.contains_key(k) {
                    removed.push(k.clone());
                }
            }
        }
        Some(base) => {
            for (k, v) in new {
                match base.get(k) {
                    None => {
                        added.push(k.clone());
                        if copy.get(k).is_some_and(|c| c != v) {
                            conflicts.push(k.clone());
                        }
                    }
                    Some(b) if b != v => {
                        if copy.get(k).is_some_and(|c| c != b && c != v) {
                            conflicts.push(k.clone());
                        }
                    }
                    Some(_) => {}
                }
            }
            for (k, b) in base {
                if !new.contains_key(k) {
                    removed.push(k.clone());
                    if copy.get(k).is_some_and(|c| c != b) {
                        conflicts.push(k.clone());
                    }
                }
            }
        }
    }
    (added, removed, conflicts)
}

fn diff(base: Option<&Role>, copy: &Role, new: &Role) -> RoleDiff {
    let (duties_added, duties_removed, duty_conflicts) =
        compare(base.map(duty_map).as_ref(), &duty_map(copy), &duty_map(new));
    let (grants_added, grants_removed, grant_conflicts) = compare(
        base.map(grant_map).as_ref(),
        &grant_map(copy),
        &grant_map(new),
    );
    let (app_permissions_added, app_permissions_removed, _) = compare(
        base.map(app_permission_map).as_ref(),
        &app_permission_map(copy),
        &app_permission_map(new),
    );
    let (parents_added, parents_removed, _) = compare(
        base.map(parent_map).as_ref(),
        &parent_map(copy),
        &parent_map(new),
    );
    let mut conflicts: Vec<Conflict> = duty_conflicts
        .into_iter()
        .map(|name| Conflict::Duty { name })
        .chain(grant_conflicts.into_iter().map(|k| Conflict::Grant {
            doctype: k.doctype,
            if_owner: k.if_owner,
        }))
        .collect();
    conflicts.sort();
    RoleDiff {
        duties_added,
        duties_removed,
        grants_added,
        grants_removed,
        app_permissions_added,
        app_permissions_removed,
        parents_added,
        parents_removed,
        conflicts_with_customisation: conflicts,
    }
}
