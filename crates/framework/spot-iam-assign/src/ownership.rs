//! Who may change which assignment (ADR-070 section 3): a source manages only its own.
//!
//! The same role from two sources is two assignments; access ends only when all of them end.
//! These checks run before a change is stored; the reconcilers only ever propose changes to
//! their own source's assignments.

use std::collections::BTreeMap;

use spot_iam_roles::{Assignment, RoleName, Source};

use crate::error::{self as e, AssignError};

/// Who is asking for a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    /// A source acting on its own assignments: an administrator (`Manual`), the rule
    /// reconciler (`Rule`), the IdP sync (`Idp`) or the emergency process (`Emergency`).
    Source(Source),
    /// The expiry job: it ends whatever has passed its `valid_to`, whatever the source.
    Expiry,
}

/// A change somebody wants to make to stored assignments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeRequest {
    /// Add an assignment.
    Add(Assignment),
    /// Edit an assignment.
    Edit {
        /// As stored.
        before: Assignment,
        /// As proposed.
        after: Assignment,
    },
    /// End an assignment.
    End(Assignment),
}

/// The roles the tenant declared IdP-managed, with the managing provider of each.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManagedRoles(BTreeMap<RoleName, String>);

impl ManagedRoles {
    /// No managed roles.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares `role` managed by `provider`.
    pub fn declare(&mut self, role: RoleName, provider: &str) {
        self.0.insert(role, provider.to_owned());
    }

    /// The managing provider of `role`, if it is managed.
    pub fn provider(&self, role: &RoleName) -> Option<&str> {
        self.0.get(role).map(String::as_str)
    }
}

fn idp_managed(role: &RoleName, provider: &str) -> AssignError {
    AssignError::about(
        e::IDP_MANAGED,
        role.as_str(),
        format!(
            "the role `{}` is managed by the identity provider `{provider}` (source idp): \
             change the user's group there, not here",
            role.as_str()
        ),
    )
}

fn not_owner(a: &Assignment, actor: Source) -> AssignError {
    AssignError::about(
        e::NOT_OWNER,
        &a.id,
        format!(
            "the assignment belongs to source {:?}; a {:?} change cannot touch it",
            a.source, actor
        ),
    )
}

/// Decides whether `actor` may make `change`.
///
/// - A source adds, edits and ends only its own assignments.
/// - A **manual** add, edit or end of an IdP-managed role is refused (`iamassign.idp_managed`,
///   naming the role and the provider), whichever source the assignment has.
/// - An emergency assignment is ended only by the emergency process or by expiry.
/// - An edit never changes the `source`.
pub fn authorise_change(
    change: &ChangeRequest,
    actor: Actor,
    managed: &ManagedRoles,
) -> Result<(), AssignError> {
    match change {
        ChangeRequest::Add(a) => {
            let Actor::Source(src) = actor else {
                return Err(AssignError::about(
                    e::WRONG_SOURCE,
                    &a.id,
                    "expiry only ends assignments",
                ));
            };
            if src == Source::Manual
                && let Some(p) = managed.provider(&a.role)
            {
                return Err(idp_managed(&a.role, p));
            }
            if a.source != src {
                return Err(AssignError::about(
                    e::WRONG_SOURCE,
                    &a.id,
                    format!(
                        "a {src:?} change cannot add an assignment of source {:?}",
                        a.source
                    ),
                ));
            }
            Ok(())
        }
        ChangeRequest::Edit { before, after } => {
            let Actor::Source(src) = actor else {
                return Err(AssignError::about(
                    e::WRONG_SOURCE,
                    &before.id,
                    "expiry only ends assignments",
                ));
            };
            if src == Source::Manual {
                for role in [&before.role, &after.role] {
                    if let Some(p) = managed.provider(role) {
                        return Err(idp_managed(role, p));
                    }
                }
            }
            if before.source != after.source {
                return Err(AssignError::about(
                    e::SOURCE_EDIT,
                    &before.id,
                    "the source of an assignment cannot be edited",
                ));
            }
            if before.source != src {
                return Err(not_owner(before, src));
            }
            Ok(())
        }
        ChangeRequest::End(a) => match actor {
            Actor::Expiry => Ok(()),
            Actor::Source(src) => {
                if src == Source::Manual
                    && let Some(p) = managed.provider(&a.role)
                {
                    return Err(idp_managed(&a.role, p));
                }
                if a.source == Source::Emergency && src != Source::Emergency {
                    return Err(AssignError::about(
                        e::EMERGENCY_END,
                        &a.id,
                        "an emergency assignment ends only through the emergency process or by expiry",
                    ));
                }
                if a.source != src {
                    return Err(not_owner(a, src));
                }
                Ok(())
            }
        },
    }
}
