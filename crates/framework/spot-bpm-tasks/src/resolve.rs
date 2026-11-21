//! Assignee resolution: the directory, the approver resolver and the resolution rule.
//!
//! A Decision node names its assignee as a user, a role, or an approver rule. Resolution turns
//! that name into a set of users, and **never guesses**: when the set comes out empty the Case
//! parks with `bpm_tasks.no_assignee` rather than picking someone. A role resolves through the
//! [`Directory`] (its members), and a rule through an [`ApproverResolver`] — the rule-graph or
//! approver-resolution function a host wires in.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use crate::task::AssigneeSpec;

/// Who the platform can ask: role membership and the manager of a user.
///
/// The directory is a port: the IAM layer provides the real one, and [`MapDirectory`] is the
/// in-memory reference the tests use.
pub trait Directory: Send + Sync {
    /// The users who hold `role`; empty when the role is unknown or empty.
    fn users_in_role(&self, role: &str) -> Vec<String>;

    /// The manager of `user`, when the directory knows one.
    fn manager_of(&self, user: &str) -> Option<String>;
}

/// A rule-graph or approver-resolution function: turns an approver rule's name into users.
pub trait ApproverResolver: Send + Sync {
    /// The users `rule` resolves to for the `(case, node)` pair and the node's form payload, or
    /// `None` when the rule is unknown.
    fn resolve(&self, rule: &str, case: &str, node: &str, form: &Json) -> Option<Vec<String>>;
}

/// An approver resolver that knows no rules: an `approver:` spec then resolves to nobody and the
/// Case parks, which is the honest answer when no rule engine is wired.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoApprovers;

impl ApproverResolver for NoApprovers {
    fn resolve(&self, _rule: &str, _case: &str, _node: &str, _form: &Json) -> Option<Vec<String>> {
        None
    }
}

/// An in-memory [`Directory`]: roles to members, and users to managers.
#[derive(Debug, Clone, Default)]
pub struct MapDirectory {
    roles: BTreeMap<String, Vec<String>>,
    managers: BTreeMap<String, String>,
}

impl MapDirectory {
    /// An empty directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `user` to `role`'s members.
    #[must_use]
    pub fn with_role(mut self, role: &str, user: &str) -> Self {
        self.roles
            .entry(role.to_owned())
            .or_default()
            .push(user.to_owned());
        self
    }

    /// Records that `manager` manages `user`.
    #[must_use]
    pub fn with_manager(mut self, user: &str, manager: &str) -> Self {
        self.managers.insert(user.to_owned(), manager.to_owned());
        self
    }
}

impl Directory for MapDirectory {
    fn users_in_role(&self, role: &str) -> Vec<String> {
        self.roles.get(role).cloned().unwrap_or_default()
    }

    fn manager_of(&self, user: &str) -> Option<String> {
        self.managers.get(user).cloned()
    }
}

/// The resolution of an assignee spec into users and the role names it named.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Resolved {
    /// The resolved users.
    pub users: BTreeSet<String>,
    /// The role names the spec named, kept so a member added later may still act.
    pub roles: BTreeSet<String>,
}

/// Resolves `spec` to its users, through `directory` and `resolver`.
#[must_use]
pub fn resolve_assignees(
    spec: &AssigneeSpec,
    directory: &dyn Directory,
    resolver: &dyn ApproverResolver,
    case: &str,
    node: &str,
    form: &Json,
) -> Resolved {
    let mut resolved = Resolved::default();
    match spec {
        AssigneeSpec::User(user) => {
            if !user.is_empty() {
                resolved.users.insert(user.clone());
            }
        }
        AssigneeSpec::Any(users) => {
            resolved
                .users
                .extend(users.iter().filter(|u| !u.is_empty()).cloned());
        }
        AssigneeSpec::Role(role) => {
            if !role.is_empty() {
                resolved.roles.insert(role.clone());
                resolved.users.extend(
                    directory
                        .users_in_role(role)
                        .into_iter()
                        .filter(|u| !u.is_empty()),
                );
            }
        }
        AssigneeSpec::Approver(rule) => {
            if let Some(users) = resolver.resolve(rule, case, node, form) {
                resolved
                    .users
                    .extend(users.into_iter().filter(|u| !u.is_empty()));
            }
        }
    }
    resolved
}
