//! What each assignment contributes to reading one DocType: the same questions
//! `PermissionEngine::can` asks for `Action::Read`, answered once for all rows.

use spot_permissions::{
    AssignmentContext, Principal, Right, ShareRight, Snapshot, TENANT_ADMINISTRATOR,
};
use spot_storage_contract::RecordId;

/// How an assignment's roles let it read the DocType.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadGrant {
    /// No role grants `read`.
    Nothing,
    /// Only `if_owner` grants: the assignment reads documents the user created.
    OwnerOnly,
    /// Some role grants `read` unconditionally (or is the tenant administrator).
    Everything,
}

/// The `read` an assignment's roles give on `doctype` (scope is judged separately).
pub(crate) fn read_grant(snapshot: &Snapshot, a: &AssignmentContext, doctype: &str) -> ReadGrant {
    let mut grant = ReadGrant::Nothing;
    for role in &a.roles {
        if role.as_str() == TENANT_ADMINISTRATOR {
            return ReadGrant::Everything;
        }
        for g in snapshot
            .grants()
            .iter()
            .filter(|g| g.doctype == doctype && &g.role == role && g.rights.contains(Right::Read))
        {
            if g.if_owner {
                grant = ReadGrant::OwnerOnly;
            } else {
                return ReadGrant::Everything;
            }
        }
    }
    grant
}

/// The assignments of a principal in the order the engine evaluates them.
pub(crate) fn sorted_assignments(principal: &Principal) -> Vec<&AssignmentContext> {
    let mut v: Vec<&AssignmentContext> = principal.assignments.iter().collect();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

/// The ids of documents shared with the user with the `read` right, sorted, without repeats.
pub(crate) fn shared_read_ids(snapshot: &Snapshot, principal: &Principal) -> Vec<RecordId> {
    let mut ids: Vec<RecordId> = snapshot
        .shares()
        .iter()
        .filter(|s| s.user == principal.user && s.rights.contains(&ShareRight::Read))
        .map(|s| s.doc)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}
