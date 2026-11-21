//! Role views: one applies, chosen by priority, never merged (ADR-068 §7.2).
//!
//! Views are per role, never per user (a person who needs a special layout is given a
//! special role). Roles carry a priority order; a user gets the view of their
//! highest-priority role that has one for the DocType. A user with no role view gets the
//! site customisation or the standard view.

use serde::{Deserialize, Serialize};

use crate::viewdef::ViewDef;

/// A role name, as the views and the priority order refer to it. Opaque on purpose: whether
/// a role exists, and what it grants, is auth's business (WP-PLT-03), not the view model's.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoleRef(
    /// The role's name.
    pub String,
);

impl RoleRef {
    /// Wraps a role name.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The role's name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Picks the user's role view: the view of the highest-priority role the user has.
///
/// - `user_roles` are the roles the user holds;
/// - `role_priority` lists role names, best first; a role the list does not mention ranks
///   after every mentioned one (and keeps its relative order among the unmentioned);
/// - `views` are the DocType's view candidates — role views carry `role`; everything else
///   is skipped (the standard view is not `select_role_view`'s business).
///
/// Returns the first view of the winning role in list order, or `None` when none of the
/// user's roles has one. Deterministic for given inputs.
pub fn select_role_view<'a>(
    user_roles: &[RoleRef],
    role_priority: &[String],
    views: &'a [ViewDef],
) -> Option<&'a ViewDef> {
    let rank_of = |role: &str| {
        role_priority
            .iter()
            .position(|name| name.as_str() == role)
            .unwrap_or(role_priority.len())
    };
    let held = |role: &str| {
        user_roles
            .iter()
            .any(|user_role| user_role.as_str() == role)
    };
    let mut best: Option<(usize, usize, &ViewDef)> = None;
    for (position, view) in views.iter().enumerate() {
        let Some(role) = view.role.as_deref() else {
            continue;
        };
        if !held(role) {
            continue;
        }
        let rank = rank_of(role);
        if best.is_none_or(|(best_rank, best_position, _)| {
            (rank, position) < (best_rank, best_position)
        }) {
            best = Some((rank, position, view));
        }
    }
    best.map(|(_, _, view)| view)
}
