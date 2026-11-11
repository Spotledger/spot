//! Spotledger node roles: the typed set, its valid combinations, and a role change as a
//! settings diff plus a service restart plan.
//!
//! A node serves a set of roles — `web`, `jobs`, `realtime`, `database`, `branch-edge`. Roles
//! are configuration (the `runtime.node.roles` setting), never files written by hand: a change
//! is a [`ChangeSet`](spot_settings::ChangeSet) applied through the shared store. Before the
//! diff is written the module validates the combination and previews what a human should
//! expect to move (queue affinity, WebSocket connections).
//!
//! The one combination that is always wrong is `database` together with `branch-edge`: a
//! branch edge must keep working offline while a database role is the canonical data owner, so
//! one node cannot be both.

use spot_settings::{
    Change, ChangeSet, ScopeRef, SettingsRegistry, StoredValue, ValueStore, apply_changes,
};

use crate::error::{RuntimeError, RuntimeResult};
use crate::settings::NODE_ROLES_KEY;

/// One Spotledger node role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeRole {
    /// Serves the web UI and API.
    Web,
    /// Runs background jobs.
    Jobs,
    /// Serves realtime subscriptions and WebSocket connections.
    Realtime,
    /// Owns the PostgreSQL data.
    Database,
    /// Serves a branch edge with offline capability.
    BranchEdge,
}

impl NodeRole {
    /// Every role, in the order the console lists them.
    pub const ALL: [NodeRole; 5] = [
        NodeRole::Web,
        NodeRole::Jobs,
        NodeRole::Realtime,
        NodeRole::Database,
        NodeRole::BranchEdge,
    ];

    /// The lower-case wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Jobs => "jobs",
            Self::Realtime => "realtime",
            Self::Database => "database",
            Self::BranchEdge => "branch-edge",
        }
    }

    /// The role named `name`, if any.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.name() == name)
    }

    /// The service a node running this role restarts when the role set changes.
    #[must_use]
    pub fn service(self) -> &'static str {
        match self {
            Self::Web => "spot-web",
            Self::Jobs => "spot-jobs",
            Self::Realtime => "spot-realtime",
            Self::Database => "postgresql",
            Self::BranchEdge => "spot-edge",
        }
    }
}

/// One service restart a role change schedules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceRestart {
    /// The service name.
    pub service: String,
    /// Why it restarts.
    pub reason: String,
}

/// A validated role set, in role order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoleSet {
    roles: Vec<NodeRole>,
}

impl RoleSet {
    /// Validates and builds a set from names, refusing an unknown role, a duplicate, an empty
    /// set, or `database` together with `branch-edge`.
    ///
    /// # Errors
    /// [`RuntimeError::UnknownRole`], [`RuntimeError::RoleCombination`] or
    /// [`RuntimeError::InvalidArgument`].
    pub fn parse(names: &[String]) -> RuntimeResult<Self> {
        let mut roles = Vec::with_capacity(names.len());
        for name in names {
            let role = NodeRole::from_name(name)
                .ok_or_else(|| RuntimeError::UnknownRole { name: name.clone() })?;
            if roles.contains(&role) {
                return Err(RuntimeError::RoleCombination {
                    detail: format!("role {} is listed twice", role.name()),
                });
            }
            roles.push(role);
        }
        roles.sort_unstable();
        let set = Self { roles };
        set.validate()?;
        Ok(set)
    }

    /// Validates this set's combination.
    ///
    /// # Errors
    /// [`RuntimeError::RoleCombination`] when the set is empty or mixes `database` with
    /// `branch-edge`.
    pub fn validate(&self) -> RuntimeResult<()> {
        if self.roles.is_empty() {
            return Err(RuntimeError::RoleCombination {
                detail: "a node needs at least one role".to_owned(),
            });
        }
        if self.roles.contains(&NodeRole::Database) && self.roles.contains(&NodeRole::BranchEdge) {
            return Err(RuntimeError::RoleCombination {
                detail: "database and branch-edge cannot run on the same node: a branch edge keeps working offline while a database node is the canonical owner".to_owned(),
            });
        }
        Ok(())
    }

    /// The roles, in role order.
    #[must_use]
    pub fn roles(&self) -> &[NodeRole] {
        &self.roles
    }

    /// Whether the set contains `role`.
    #[must_use]
    pub fn contains(&self, role: NodeRole) -> bool {
        self.roles.contains(&role)
    }

    /// The roles as their wire names.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.roles
            .iter()
            .map(|role| role.name().to_owned())
            .collect()
    }
}

/// What a role change will do, shown before the diff is written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RolePreview {
    /// The roles before the change.
    pub before: Vec<NodeRole>,
    /// The roles after the change.
    pub after: Vec<NodeRole>,
    /// Roles added by the change.
    pub added: Vec<NodeRole>,
    /// Roles removed by the change.
    pub removed: Vec<NodeRole>,
    /// The services that restart, with the reason.
    pub restarts: Vec<ServiceRestart>,
    /// What moves when the roles change (queue affinity, WebSocket connections, data ownership).
    pub moves: Vec<String>,
}

impl RolePreview {
    /// Whether the change alters the role set.
    #[must_use]
    pub fn is_change(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }
}

/// What applying a role change did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleReport {
    /// The preview that was applied.
    pub preview: RolePreview,
    /// Whether the settings store accepted the diff.
    pub applied: bool,
}

/// Reads the node's role set from the settings store, falling back to the registry default.
///
/// # Errors
/// [`RuntimeError::Settings`] when the stored value is not a list of known roles.
pub fn read_roles(
    store: &dyn ValueStore,
    registry: &SettingsRegistry,
    scope: &ScopeRef,
) -> RuntimeResult<RoleSet> {
    let stored = store
        .get(scope, NODE_ROLES_KEY)
        .or_else(|| default_value(registry, NODE_ROLES_KEY));
    let mut names = Vec::new();
    match stored {
        Some(StoredValue::Value(spot_settings::ParamValue::List(items))) => {
            for item in items {
                match item {
                    spot_settings::ParamValue::Text(name) => names.push(name),
                    _ => {
                        return Err(RuntimeError::Settings {
                            detail: format!("{NODE_ROLES_KEY} must be a list of role names"),
                        });
                    }
                }
            }
        }
        Some(_) => {
            return Err(RuntimeError::Settings {
                detail: format!("{NODE_ROLES_KEY} must be a list of role names"),
            });
        }
        None => {
            return Err(RuntimeError::Settings {
                detail: format!("{NODE_ROLES_KEY} has no value and no default"),
            });
        }
    }
    RoleSet::parse(&names)
}

/// Replaces the node's role set through the settings store and returns the restart plan.
///
/// The diff is validated against the registry and written as one batch; on a validation or
/// store error nothing is left half-written.
///
/// # Errors
/// [`RuntimeError::RoleCombination`] for an invalid set, [`RuntimeError::Settings`] when the
/// diff is refused.
pub fn set_roles(
    store: &dyn ValueStore,
    registry: &SettingsRegistry,
    scope: &ScopeRef,
    requested: &[String],
) -> RuntimeResult<RoleReport> {
    let after = RoleSet::parse(requested)?;
    let before = read_roles(store, registry, scope)?;
    let preview = preview(&before, &after);

    let value = spot_settings::ParamValue::List(
        after
            .names()
            .into_iter()
            .map(spot_settings::ParamValue::Text)
            .collect(),
    );
    let change_set =
        ChangeSet::new().push_change(Change::set(scope.clone(), NODE_ROLES_KEY, value));
    apply_changes(&change_set, registry, store).map_err(|error| RuntimeError::Settings {
        detail: format!("{error}"),
    })?;

    Ok(RoleReport {
        preview,
        applied: true,
    })
}

/// Builds the preview of moving from `before` to `after`.
#[must_use]
pub fn preview(before: &RoleSet, after: &RoleSet) -> RolePreview {
    let added: Vec<NodeRole> = after
        .roles()
        .iter()
        .copied()
        .filter(|role| !before.contains(*role))
        .collect();
    let removed: Vec<NodeRole> = before
        .roles()
        .iter()
        .copied()
        .filter(|role| !after.contains(*role))
        .collect();

    let mut restarts = Vec::new();
    if !added.is_empty() || !removed.is_empty() {
        for role in added.iter().chain(removed.iter()) {
            let reason = if added.contains(role) {
                "start the service for the added role"
            } else {
                "stop the service for the removed role"
            };
            restarts.push(ServiceRestart {
                service: role.service().to_owned(),
                reason: reason.to_owned(),
            });
        }
        restarts.sort_by(|left, right| left.service.cmp(&right.service));
        restarts.dedup_by(|left, right| left.service == right.service);
    }

    let mut moves = Vec::new();
    if touched(NodeRole::Jobs, &added, &removed) {
        moves.push("job-queue affinity moves to the node set that now runs jobs".to_owned());
    }
    if touched(NodeRole::Realtime, &added, &removed) {
        moves.push(
            "realtime WebSocket connections reconnect to the node set that now serves realtime"
                .to_owned(),
        );
    }
    if touched(NodeRole::Database, &added, &removed) {
        moves.push(
            "database ownership changes: take and verify a backup before moving the data role"
                .to_owned(),
        );
    }
    if touched(NodeRole::BranchEdge, &added, &removed) {
        moves.push(
            "branch-edge sync scope changes; offline nodes will re-scope on their next sync"
                .to_owned(),
        );
    }
    if touched(NodeRole::Web, &added, &removed) {
        moves.push("web traffic moves to the node set that now serves the UI and API".to_owned());
    }

    RolePreview {
        before: before.roles().to_vec(),
        after: after.roles().to_vec(),
        added,
        removed,
        restarts,
        moves,
    }
}

/// Whether `role` was added or removed.
fn touched(role: NodeRole, added: &[NodeRole], removed: &[NodeRole]) -> bool {
    added.contains(&role) || removed.contains(&role)
}

/// The registry default for `key`, as a stored value.
fn default_value(registry: &SettingsRegistry, key: &str) -> Option<StoredValue> {
    registry
        .get(key)
        .and_then(|spec| spec.default.clone())
        .map(StoredValue::Value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_settings::MemoryStore;
    use spot_settings::model::ScopeRef;

    use super::*;
    use crate::settings::runtime_registry;

    #[test]
    fn roles_round_trip_through_their_names() {
        for role in NodeRole::ALL {
            assert_eq!(NodeRole::from_name(role.name()), Some(role));
        }
        assert_eq!(NodeRole::from_name("admin"), None);
    }

    #[test]
    fn a_database_and_branch_edge_node_is_invalid() {
        let error = RoleSet::parse(&["database".to_owned(), "branch-edge".to_owned()]).unwrap_err();
        assert_eq!(error.code(), crate::error::ROLE_COMBINATION);
        assert!(RoleSet::parse(&["web".to_owned(), "database".to_owned()]).is_ok());
    }

    #[test]
    fn an_empty_or_duplicated_set_is_invalid() {
        assert_eq!(
            RoleSet::parse(&[]).unwrap_err().code(),
            crate::error::ROLE_COMBINATION
        );
        assert_eq!(
            RoleSet::parse(&["web".to_owned(), "web".to_owned()])
                .unwrap_err()
                .code(),
            crate::error::ROLE_COMBINATION
        );
        assert_eq!(
            RoleSet::parse(&["admin".to_owned()]).unwrap_err().code(),
            crate::error::UNKNOWN_ROLE
        );
    }

    #[test]
    fn a_change_is_a_settings_diff_and_reports_its_restarts_and_moves() {
        let registry = runtime_registry().unwrap();
        let store = MemoryStore::new();
        let scope = ScopeRef::Server("srv-1".to_owned());

        let before = read_roles(&store, &registry, &scope).unwrap();
        assert_eq!(before.names(), vec!["web", "database"]);

        let report = set_roles(
            &store,
            &registry,
            &scope,
            &["web".to_owned(), "jobs".to_owned(), "realtime".to_owned()],
        )
        .unwrap();
        assert!(report.applied);
        assert_eq!(
            report.preview.added,
            vec![NodeRole::Jobs, NodeRole::Realtime]
        );
        assert_eq!(report.preview.removed, vec![NodeRole::Database]);
        assert!(
            report
                .preview
                .restarts
                .iter()
                .any(|restart| restart.service == "spot-jobs")
        );
        assert!(
            report
                .preview
                .moves
                .iter()
                .any(|line| line.contains("job-queue"))
        );
        assert!(
            report
                .preview
                .moves
                .iter()
                .any(|line| line.contains("WebSocket"))
        );

        let after = read_roles(&store, &registry, &scope).unwrap();
        assert_eq!(after.names(), vec!["web", "jobs", "realtime"]);
    }

    #[test]
    fn an_unchanged_set_has_an_empty_preview() {
        let before = RoleSet::parse(&["web".to_owned()]).unwrap();
        let preview = preview(&before, &before);
        assert!(!preview.is_change());
        assert!(preview.restarts.is_empty());
        assert!(preview.moves.is_empty());
    }
}
