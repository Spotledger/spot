//! Barrier 3 of ADR-083: what a **portal** user may hold.
//!
//! A portal user (an outside person with a user record: a customer's accountant, a supplier's
//! clerk) may hold only a role whose kind is [`RoleKind::Portal`] **and** whose whole closure
//! (the role, every ancestor, every duty) carries nothing that is not marked outside-exposed.
//! A consumer principal is not a user and never reaches this code.
//!
//! The rule is an **allow-list on duties**, not a deny-list on a label: a `Portal` kind with
//! one unmarked duty is refused, a `Portal` role inheriting a desk role is refused, and so is a
//! `Portal` role with a direct right (a direct right carries no duty and so no mark). Every
//! unknown is closed: an unknown role, an unmarked duty, a stored duty from before the mark
//! existed.
//!
//! # One guard, every path
//!
//! [`OutsideGuard`] is built once from the registry and the set of portal user ids, and every
//! path that can make or keep an assignment asks it: the grant gate, assignment rules, IdP
//! group mapping, SCIM, the emergency grant, invitation acceptance and the snapshot build. A
//! guard with no portal users allows everything, so a caller that must protect portal users
//! passes the real set.
//!
//! # Role and duty edits
//!
//! An edit that would make a role held by a portal user carry a desk kind, an unmarked duty or
//! a direct right is **refused**, not applied-and-ended: [`Registry::with_role_guarded`] and
//! [`Registry::install_pack_guarded`] check every role a portal user holds in the registry the
//! edit would produce (so an edit to an ancestor or to a shared duty is caught too) and name
//! the holders by count, never by name.

use std::collections::{BTreeMap, BTreeSet};

use time::OffsetDateTime;

use crate::effective::{ResolvedRole, effective};
use crate::error::{self as e, IamError, IamErrors, Result};
use crate::model::{Assignment, Role, RoleKind, RoleName};
use crate::registry::Registry;

impl ResolvedRole {
    /// Whether a portal user may hold this role: `Ok` only when its kind and its whole closure
    /// pass the allow-list. The error names the role and the duty (or parent), never a user.
    pub fn check_for_portal(&self) -> Result<()> {
        let name = self.name.as_str();
        if self.kind != RoleKind::Portal {
            return Err(IamErrors::one(IamError::about(
                e::OUTSIDE_ROLE_KIND,
                name,
                format!("role `{name}` is a desk role; a portal user can hold only a portal role"),
            )));
        }
        if let Some(parent) = self.outside.desk_roles.iter().find(|r| **r != self.name) {
            return Err(IamErrors::one(IamError::about(
                e::OUTSIDE_ROLE_KIND,
                name,
                format!(
                    "portal role `{name}` inherits from `{}`, which is not a portal role",
                    parent.as_str()
                ),
            )));
        }
        if let Some(duty) = self.outside.unexposed_duties.first() {
            return Err(IamErrors::one(IamError::about(
                e::OUTSIDE_DUTY_NOT_EXPOSED,
                name,
                format!(
                    "portal role `{name}` includes the duty `{}`, which is not marked \
                     outside-exposed",
                    duty.as_str()
                ),
            )));
        }
        if let Some(holder) = self.outside.direct_right_roles.first() {
            return Err(IamErrors::one(IamError::about(
                e::OUTSIDE_DUTY_NOT_EXPOSED,
                name,
                format!(
                    "portal role `{name}` reaches rights granted directly by `{}`; a direct \
                     right belongs to no duty and so carries no outside-exposure mark",
                    holder.as_str()
                ),
            )));
        }
        Ok(())
    }
}

/// The portal users, and for every role of a registry whether a portal user may hold it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutsideGuard {
    portal_users: BTreeSet<String>,
    verdicts: BTreeMap<RoleName, std::result::Result<(), IamError>>,
}

impl OutsideGuard {
    /// A guard that protects no one: every check passes. Only for a caller that has no portal
    /// users (and for tests of the unguarded paths).
    pub fn none() -> OutsideGuard {
        OutsideGuard::default()
    }

    /// The guard for `portal_users` over `registry`.
    pub fn new(
        registry: &Registry,
        portal_users: impl IntoIterator<Item = impl Into<String>>,
    ) -> OutsideGuard {
        let verdicts = registry
            .roles()
            .map(|role| {
                let verdict = effective(role, registry)
                    .check_for_portal()
                    .map_err(first_error);
                (role.name.clone(), verdict)
            })
            .collect();
        OutsideGuard {
            portal_users: portal_users.into_iter().map(Into::into).collect(),
            verdicts,
        }
    }

    /// The guard when only the role records are at hand, not the duties (a host that exposes
    /// no registry). **Fail closed:** a role that includes any duty is refused for a portal
    /// user, because its marks cannot be seen; so is a role with a parent outside the list,
    /// with a direct right, or of desk kind.
    pub fn from_roles(
        roles: &[Role],
        portal_users: impl IntoIterator<Item = impl Into<String>>,
    ) -> OutsideGuard {
        let by_name: BTreeMap<&RoleName, &Role> = roles.iter().map(|r| (&r.name, r)).collect();
        let verdicts = roles
            .iter()
            .map(|role| (role.name.clone(), blind_verdict(role, &by_name)))
            .collect();
        OutsideGuard {
            portal_users: portal_users.into_iter().map(Into::into).collect(),
            verdicts,
        }
    }

    /// Whether `user` is a portal user this guard protects.
    pub fn is_portal(&self, user: &str) -> bool {
        self.portal_users.contains(user)
    }

    /// The portal users, sorted.
    pub fn portal_users(&self) -> impl Iterator<Item = &str> {
        self.portal_users.iter().map(String::as_str)
    }

    /// Whether `user` may hold `role`. Anyone who is not a portal user passes; a portal user
    /// passes only for a known role that passes the allow-list.
    pub fn check(&self, user: &str, role: &RoleName) -> Result<()> {
        if !self.is_portal(user) {
            return Ok(());
        }
        self.check_role(role)
    }

    /// Whether a portal user may hold `role`, whoever the user is.
    pub fn check_role(&self, role: &RoleName) -> Result<()> {
        match self.verdicts.get(role) {
            Some(Ok(())) => Ok(()),
            Some(Err(error)) => Err(IamErrors::one(error.clone())),
            None => Err(IamErrors::one(IamError::about(
                e::OUTSIDE_ROLE_KIND,
                role.as_str(),
                format!(
                    "role `{}` is not a known portal role; a portal user can hold only a portal role",
                    role.as_str()
                ),
            ))),
        }
    }

    /// Filters `assignments` to those the guard lets stand and returns the refused ones with
    /// the error that refused each. Used where a path wants "keep what is allowed".
    pub fn partition<'a>(
        &self,
        assignments: &'a [Assignment],
    ) -> (Vec<&'a Assignment>, Vec<(&'a Assignment, IamError)>) {
        let mut kept = Vec::new();
        let mut refused = Vec::new();
        for a in assignments {
            match self.check(&a.user, &a.role) {
                Ok(()) => kept.push(a),
                Err(errors) => {
                    if let Some(error) = errors.errors().first() {
                        refused.push((a, error.clone()));
                    }
                }
            }
        }
        (kept, refused)
    }
}

fn first_error(errors: IamErrors) -> IamError {
    errors.errors().first().cloned().unwrap_or_else(|| {
        IamError::new(e::OUTSIDE_ROLE_KIND, "a portal user cannot hold this role")
    })
}

fn blind_verdict(
    role: &Role,
    by_name: &BTreeMap<&RoleName, &Role>,
) -> std::result::Result<(), IamError> {
    let name = role.name.as_str();
    let mut seen = BTreeSet::new();
    let mut stack = vec![role];
    while let Some(at) = stack.pop() {
        if !seen.insert(at.name.clone()) {
            continue;
        }
        if at.kind != RoleKind::Portal {
            return Err(IamError::about(
                e::OUTSIDE_ROLE_KIND,
                name,
                format!("`{}` is not a portal role", at.name.as_str()),
            ));
        }
        if !at.duties.is_empty()
            || !at.direct_grants.is_empty()
            || !at.direct_field_grants.is_empty()
            || !at.direct_app_permissions.is_empty()
        {
            return Err(IamError::about(
                e::OUTSIDE_DUTY_NOT_EXPOSED,
                name,
                format!(
                    "portal role `{name}` reaches duties or rights whose outside-exposure marks \
                     cannot be seen here; they are treated as not exposed"
                ),
            ));
        }
        for parent in &at.parents {
            match by_name.get(parent) {
                Some(p) => stack.push(p),
                None => {
                    return Err(IamError::about(
                        e::OUTSIDE_ROLE_KIND,
                        name,
                        format!("portal role `{name}` inherits from an unknown role"),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// How many portal users hold each role.
pub type PortalHolders = BTreeMap<RoleName, usize>;

/// The roles portal users hold, counted over the assignments **not ended before `at`** (an
/// assignment that has not started yet counts: it will take effect).
pub fn portal_holders(
    guard: &OutsideGuard,
    assignments: &[Assignment],
    at: OffsetDateTime,
) -> PortalHolders {
    let mut holders = PortalHolders::new();
    for a in assignments {
        if guard.is_portal(&a.user) && a.valid_to.is_none_or(|end| end > at) {
            *holders.entry(a.role.clone()).or_insert(0) += 1;
        }
    }
    holders
}

impl Registry {
    /// [`Registry::with_role`], refusing the edit when a role a portal user holds would no
    /// longer pass the allow-list in the registry the edit produces.
    pub fn with_role_guarded(&self, role: Role, holders: &PortalHolders) -> Result<Registry> {
        let next = self.with_role(role)?;
        check_edit(&next, holders)?;
        Ok(next)
    }

    /// [`Registry::install_pack`], with the same refusal.
    pub fn install_pack_guarded(
        &self,
        duties: Vec<crate::model::Duty>,
        roles: Vec<Role>,
        holders: &PortalHolders,
    ) -> Result<Registry> {
        let next = self.install_pack(duties, roles)?;
        check_edit(&next, holders)?;
        Ok(next)
    }
}

/// Refuses `next` when a role in `holders` does not pass the allow-list there. The message
/// names the role, what is wrong and the number of portal holders, never a holder.
pub fn check_edit(next: &Registry, holders: &PortalHolders) -> Result<()> {
    let mut problems = Vec::new();
    for (name, count) in holders {
        let verdict = match next.role(name) {
            Some(role) => effective(role, next).check_for_portal(),
            // The role is gone: its holders have nothing, which the other paths report.
            None => continue,
        };
        if let Err(errors) = verdict {
            let why = errors
                .errors()
                .first()
                .map(|x| x.message.clone())
                .unwrap_or_default();
            problems.push(IamError::about(
                e::OUTSIDE_ROLE_EDIT_REFUSED,
                name.as_str(),
                format!(
                    "the edit is refused: role `{}` is held by {count} portal user(s) and, \
                     after it, {why}",
                    name.as_str()
                ),
            ));
        }
    }
    match IamErrors::from_list(problems) {
        Some(errors) => Err(errors),
        None => Ok(()),
    }
}
