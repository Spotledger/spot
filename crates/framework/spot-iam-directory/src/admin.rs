//! Last-administrator protection (ADR-070 sections 5.2 and 6.6).
//!
//! The system refuses to deactivate, delete, demote or expire the last enabled tenant
//! administrator, or the last enabled break-glass administrator; and while SSO is required at
//! least two break-glass administrators must remain, because the IdP must never be the only
//! way in.
//!
//! The only input is a [`DirectoryView`]: this crate assumes nothing about how administrators
//! are stored (a role assignment, a flag, a group).

use spot_storage_contract::RecordId;

use crate::error::{self as e, IamError, IamErrors, Result};

/// How many break-glass administrators must remain while SSO is required.
pub const BREAK_GLASS_MINIMUM: usize = 2;

/// What the check needs to know about the tenant's administrators.
pub trait DirectoryView {
    /// How many enabled tenant administrators exist (break-glass administrators count when they
    /// are also tenant administrators).
    fn enabled_tenant_admins(&self) -> usize;
    /// How many enabled break-glass administrators exist.
    fn enabled_break_glass_admins(&self) -> usize;
    /// Whether `user` is an enabled tenant administrator now.
    fn is_enabled_tenant_admin(&self, user: RecordId) -> bool;
    /// Whether `user` is an enabled break-glass administrator now.
    fn is_enabled_break_glass_admin(&self, user: RecordId) -> bool;
    /// Whether the tenant requires SSO for sign-in.
    fn sso_required(&self) -> bool;
}

/// What is about to happen to a user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// The user is disabled.
    Disable,
    /// The user is deleted.
    Delete,
    /// The user loses the administrator (or break-glass) role.
    Demote,
    /// The user's administrator assignment expires.
    Expire,
}

/// A change to check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    /// The user concerned.
    pub user: RecordId,
    /// What happens.
    pub kind: ChangeKind,
}

/// Refuses a change that would remove the last administrator of a kind, or break the
/// break-glass minimum. Reports every problem at once.
pub fn check_admin_invariant(view: &impl DirectoryView, change: &Change) -> Result<()> {
    let mut problems = Vec::new();
    if view.is_enabled_tenant_admin(change.user) && view.enabled_tenant_admins() <= 1 {
        problems.push(IamError::new(
            e::LAST_ADMIN,
            "this is the last tenant administrator; appoint another before this change",
        ));
    }
    if view.is_enabled_break_glass_admin(change.user) {
        let remaining = view.enabled_break_glass_admins().saturating_sub(1);
        if remaining == 0 {
            problems.push(IamError::new(
                e::LAST_ADMIN,
                "this is the last break-glass administrator; appoint another before this change",
            ));
        }
        if view.sso_required() && remaining < BREAK_GLASS_MINIMUM {
            problems.push(IamError::new(
                e::BREAK_GLASS_MINIMUM,
                format!(
                    "SSO is required, so at least {BREAK_GLASS_MINIMUM} break-glass administrators must remain"
                ),
            ));
        }
    }
    match IamErrors::from_list(problems) {
        Some(errors) => Err(errors),
        None => Ok(()),
    }
}
