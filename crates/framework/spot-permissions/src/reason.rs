//! Every decision carries every rule that granted or denied it, in a deterministic order.
//!
//! A [`Reason`] is one rule's contribution: an assignment that covered (or did not cover) the
//! document, a grant that gave a right, an owner rule that applied or did not, a user
//! permission that blocked a value, the posted state, a field grant or denial, a group with no
//! grant. `Decision::reasons` lists them all so "why was I denied" is a read, not a
//! conversation (the goal of WP-PLT-03). Every reason names the **assignment id** and the role
//! that granted or denied.
//!
//! The stable machine form is the reason *code* (`kind(detail)`); the support form is the
//! plain-language sentence [`Reason::sentence`] writes. Codes never change in a breaking way.

use std::fmt;

use crate::model::{Role, ShareRight};

/// Why a decision came out as it did: one entry per rule that spoke.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// An assignment was evaluated: it contributes when its scope covers the document, and
    /// contributes nothing (no grant, no denial) when it does not.
    Assignment {
        /// The assignment's id.
        id: String,
        /// Whether its scope covers the document.
        covers: bool,
    },

    /// A role grant gave rights.
    RoleGrant {
        /// The assignment whose role it is.
        assignment: String,
        /// The role whose grant spoke.
        role: Role,
        /// The grant applies only to the owner, and the acting user is the owner.
        owner_applies: bool,
        /// The right the grant gave on this decision.
        rights: Vec<String>,
    },

    /// An `if_owner` grant exists but did not apply: the acting user is not the creator.
    OwnerOnly {
        /// The assignment whose role it is.
        assignment: String,
        /// The role carrying the owner-only grant.
        role: Role,
        /// What the grant would have allowed, had the user been the owner.
        would_allow: Vec<String>,
    },

    /// A user permission in an assignment's scope blocked one field's value.
    UserPermissionBlocked {
        /// The assignment whose scope blocked.
        assignment: String,
        /// The field path (`"doctype.field"`).
        field: String,
        /// The value that is outside the allowed set, as its text form.
        value: String,
    },

    /// A share added rights on this document.
    Share {
        /// The rights the share gave.
        rights: Vec<ShareRight>,
    },

    /// A document-level state rule: posted documents are edited only through `update_posted`;
    /// cancelled ones are read-only (ADR-42, ADR-51).
    PostedState {
        /// The state the document was in.
        state: String,
        /// The action that state refuses.
        refused: String,
    },

    /// The principal is the platform acting as itself: every check passes.
    System,

    /// No role of the principal carries the right (a plain absence, listed so a denial is
    /// never unexplained).
    NoGrant {
        /// The DocType with no grant.
        doctype: String,
        /// What was looked for.
        wanted: String,
    },

    /// A field grant gave rights on a field or group.
    FieldGrant {
        /// The assignment whose role it is.
        assignment: String,
        /// The role.
        role: Role,
        /// `field:<path>` or `group:<name>`.
        target: String,
        /// The rights the grant carries.
        access: Vec<String>,
    },

    /// An explicit field `deny` took rights away; nothing lifts it.
    FieldDeny {
        /// The assignment whose role it is.
        assignment: String,
        /// The role.
        role: Role,
        /// `field:<path>` or `group:<name>`.
        target: String,
        /// The rights taken away (all of them when empty).
        access: Vec<String>,
    },

    /// A field grant exists but its condition failed on this document, so it did not apply.
    ConditionFailed {
        /// The assignment whose role it is.
        assignment: String,
        /// The role.
        role: Role,
        /// `field:<path>` or `group:<name>`.
        target: String,
    },

    /// A protected field, and no grant reaches it: closed by default.
    FieldClosed {
        /// The field.
        path: String,
    },

    /// One of several groups of the field has no grant, so the strictest group closes the
    /// field (the intersection rule).
    GroupNoGrant {
        /// The field.
        path: String,
        /// The group with no grant.
        group: String,
    },

    /// A field right is capped by the DocType right: it never exceeds it.
    DoctypeCap {
        /// The field.
        path: String,
        /// The DocType right that is missing.
        missing: String,
    },

    /// The field is in no group and has no grant on it: it follows the DocType right.
    FollowsDoctype {
        /// The field.
        path: String,
    },
}

impl Reason {
    /// The stable machine code of this reason: `kind(details)`.
    #[must_use]
    pub fn code(&self) -> String {
        match self {
            Reason::Assignment { id, covers } => {
                format!(
                    "assignment({id}, {})",
                    if *covers { "covers" } else { "outside" }
                )
            }
            Reason::RoleGrant {
                assignment, role, ..
            } => format!("role_grant({assignment}, {})", role.as_str()),
            Reason::OwnerOnly {
                assignment, role, ..
            } => format!("owner_only({assignment}, {})", role.as_str()),
            Reason::UserPermissionBlocked {
                assignment,
                field,
                value,
            } => format!("user_permission_blocked({assignment}, {field}, {value})"),
            Reason::Share { rights } => {
                let names: Vec<&str> = rights.iter().map(|r| r.as_str()).collect();
                format!("share({})", names.join(","))
            }
            Reason::PostedState { state, refused } => format!("posted_state({state}, {refused})"),
            Reason::System => "system".to_owned(),
            Reason::NoGrant { .. } => "no_grant".to_owned(),
            Reason::FieldGrant {
                assignment,
                role,
                target,
                ..
            } => format!("field_grant({assignment}, {}, {target})", role.as_str()),
            Reason::FieldDeny {
                assignment,
                role,
                target,
                ..
            } => format!("field_deny({assignment}, {}, {target})", role.as_str()),
            Reason::ConditionFailed {
                assignment,
                role,
                target,
            } => format!(
                "condition_failed({assignment}, {}, {target})",
                role.as_str()
            ),
            Reason::FieldClosed { path } => format!("field_closed({path})"),
            Reason::GroupNoGrant { path, group } => format!("group_no_grant({path}, {group})"),
            Reason::DoctypeCap { path, missing } => format!("doctype_cap({path}, {missing})"),
            Reason::FollowsDoctype { path } => format!("follows_doctype({path})"),
        }
    }

    /// The short kind of the reason, as the matrices' `codes` column names it.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Reason::Assignment { .. } => "assignment",
            Reason::RoleGrant { .. } => "role_grant",
            Reason::OwnerOnly { .. } => "owner_only",
            Reason::UserPermissionBlocked { .. } => "user_permission_blocked",
            Reason::Share { .. } => "share",
            Reason::PostedState { .. } => "posted_state",
            Reason::System => "system",
            Reason::NoGrant { .. } => "no_grant",
            Reason::FieldGrant { .. } => "field_grant",
            Reason::FieldDeny { .. } => "field_deny",
            Reason::ConditionFailed { .. } => "condition_failed",
            Reason::FieldClosed { .. } => "field_closed",
            Reason::GroupNoGrant { .. } => "group_no_grant",
            Reason::DoctypeCap { .. } => "doctype_cap",
            Reason::FollowsDoctype { .. } => "follows_doctype",
        }
    }

    /// The support view: one plain-language sentence per reason.
    #[must_use]
    pub fn sentence(&self) -> String {
        match self {
            Reason::Assignment { id, covers: true } => {
                format!("assignment `{id}` covers this document")
            }
            Reason::Assignment { id, covers: false } => {
                format!("assignment `{id}` does not cover this document, so it contributes nothing")
            }
            Reason::RoleGrant {
                assignment,
                role,
                rights,
                ..
            } => format!(
                "assignment `{assignment}`: role `{}` grants {}",
                role.as_str(),
                list(rights)
            ),
            Reason::OwnerOnly {
                assignment,
                role,
                would_allow,
            } => format!(
                "assignment `{assignment}`: role `{}` grants {} only to the document's creator, and the acting user did not create it",
                role.as_str(),
                list(would_allow)
            ),
            Reason::UserPermissionBlocked {
                assignment,
                field,
                value,
            } => format!(
                "assignment `{assignment}` is scoped to an allowed list for `{field}`, and `{value}` is not on it"
            ),
            Reason::Share { rights } => {
                let names: Vec<String> = rights.iter().map(|r| r.as_str().to_owned()).collect();
                format!("a share on this document grants {}", list(&names))
            }
            Reason::PostedState { state, refused } => {
                format!("the document is {state}: {refused} is not how a {state} document changes")
            }
            Reason::System => "the platform acts as itself".to_owned(),
            Reason::NoGrant { doctype, wanted } => {
                format!("no role of this user carries `{wanted}` on `{doctype}`")
            }
            Reason::FieldGrant {
                assignment,
                role,
                target,
                access,
            } => format!(
                "assignment `{assignment}`: role `{}` grants {} on {target}",
                role.as_str(),
                list(access)
            ),
            Reason::FieldDeny {
                assignment,
                role,
                target,
                access,
            } => format!(
                "assignment `{assignment}`: role `{}` is explicitly denied {} on {target}; no other grant lifts it",
                role.as_str(),
                if access.is_empty() {
                    "everything".to_owned()
                } else {
                    list(access)
                }
            ),
            Reason::ConditionFailed {
                assignment,
                role,
                target,
            } => format!(
                "assignment `{assignment}`: role `{}` has a grant on {target}, but its condition does not hold on this document",
                role.as_str()
            ),
            Reason::FieldClosed { path } => {
                format!("`{path}` is protected and no grant reaches it: closed by default")
            }
            Reason::GroupNoGrant { path, group } => format!(
                "`{path}` is also in group `{group}`, which grants nothing: the strictest group decides"
            ),
            Reason::DoctypeCap { path, missing } => format!(
                "a field right on `{path}` never exceeds the DocType right, and `{missing}` is missing"
            ),
            Reason::FollowsDoctype { path } => {
                format!("`{path}` is in no group, so it follows the DocType right")
            }
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.sentence())
    }
}

/// `a`, `a and b`, `a, b and c`: the list form support text reads best in.
fn list(items: &[String]) -> String {
    match items.split_last() {
        None => "nothing".to_owned(),
        Some((last, [])) => last.clone(),
        Some((last, head)) => format!("{} and {}", head.join(", "), last),
    }
}
