//! The topology the router chooses between: nodes, their roles, labels and capacity (ADR-24).
//!
//! A [`NodeProfile`] is assembled by the host from what the topology already knows — whether the
//! node is present in `spot-coordinator` (a node that is not present is not a target) plus the
//! node's role settings — and handed to the router. The router never discovers nodes itself; it
//! chooses among the profiles it is given, so a test can pass a fixed topology.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The longest node id, in bytes.
pub const MAX_NODE_ID_BYTES: usize = 200;

/// A node's identity in the topology.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    /// A node id; ids are short identifiers, not free text.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the id is well formed: non-empty, at most [`MAX_NODE_ID_BYTES`] bytes and
    /// printable.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.0.is_empty()
            && self.0.len() <= MAX_NODE_ID_BYTES
            && !self.0.chars().any(char::is_control)
    }
}

impl From<&str> for NodeId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a node is for. The role is the coarse, settings-driven part of a node's profile; labels
/// are the fine-grained part.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    /// Terminates HTTP and forwards; runs no heavy work.
    Gateway,
    /// Serves interactive requests and the interactive job queue.
    Interactive,
    /// Serves reports and dashboards; the dedicated report node.
    Report,
    /// Serves model inference and retrieval.
    Ai,
    /// Serves bulk imports and exports.
    Bulk,
}

impl NodeRole {
    /// Every role, in declaration order.
    pub const ALL: [NodeRole; 5] = [
        NodeRole::Gateway,
        NodeRole::Interactive,
        NodeRole::Report,
        NodeRole::Ai,
        NodeRole::Bulk,
    ];

    /// The role's lower-case name, as it appears in a policy document or a role setting.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NodeRole::Gateway => "gateway",
            NodeRole::Interactive => "interactive",
            NodeRole::Report => "report",
            NodeRole::Ai => "ai",
            NodeRole::Bulk => "bulk",
        }
    }

    /// Parse a role name.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownClass`] when the name is not one of [`NodeRole::ALL`].
    pub fn parse(name: &str) -> Result<Self> {
        NodeRole::ALL
            .into_iter()
            .find(|role| role.as_str() == name)
            .ok_or_else(|| Error::UnknownClass {
                name: name.to_owned(),
            })
    }
}

impl std::fmt::Display for NodeRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A node's capacity: how many jobs it runs at once and how deep its queue may grow before the
/// router should prefer another node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capacity {
    /// Concurrent jobs (workers) the node runs.
    pub workers: u32,
    /// The queue depth beyond which the node is considered saturated.
    pub max_queue_depth: u32,
}

impl Capacity {
    /// A capacity with the given workers and queue depth.
    #[must_use]
    pub fn new(workers: u32, max_queue_depth: u32) -> Self {
        Self {
            workers,
            max_queue_depth,
        }
    }
}

impl Default for Capacity {
    fn default() -> Self {
        Self {
            workers: 1,
            max_queue_depth: 1000,
        }
    }
}

/// One node's routing profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProfile {
    /// The node's id in the topology.
    pub id: NodeId,
    /// The roles the node serves.
    pub roles: Vec<NodeRole>,
    /// Free-form labels an operator or a path rule can target (for example `report-west`).
    #[serde(default)]
    pub labels: Vec<String>,
    /// How much work the node can hold.
    #[serde(default)]
    pub capacity: Capacity,
}

impl NodeProfile {
    /// Build a profile from what the topology knows: the coordinator's view of the node plus its
    /// role settings. Labels are sorted and de-duplicated so two equal profiles compare equal.
    #[must_use]
    pub fn from_topology(
        id: impl Into<NodeId>,
        roles: Vec<NodeRole>,
        labels: Vec<String>,
        capacity: Capacity,
    ) -> Self {
        let mut labels = labels;
        labels.sort();
        labels.dedup();
        Self {
            id: id.into(),
            roles,
            labels,
            capacity,
        }
    }

    /// Whether the node serves a role.
    #[must_use]
    pub fn has_role(&self, role: NodeRole) -> bool {
        self.roles.contains(&role)
    }

    /// Whether the node carries a label.
    #[must_use]
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }

    /// Whether the profile is well formed: a valid id, at least one worker, and labels that are
    /// short printable identifiers.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.id.is_valid()
            && self.capacity.workers > 0
            && self
                .labels
                .iter()
                .all(|l| !l.is_empty() && l.len() <= 100 && !l.chars().any(char::is_control))
    }
}

/// A selector over the topology: a path rule targets *these* roles or labels. An empty selector
/// matches every node (the default pool).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSelector {
    /// Roles any of which qualifies a node.
    #[serde(default)]
    pub roles: Vec<NodeRole>,
    /// Labels any of which qualifies a node.
    #[serde(default)]
    pub labels: Vec<String>,
}

impl NodeSelector {
    /// A selector over roles.
    #[must_use]
    pub fn roles(roles: Vec<NodeRole>) -> Self {
        Self {
            roles,
            labels: Vec::new(),
        }
    }

    /// A selector over labels.
    #[must_use]
    pub fn labels(labels: Vec<String>) -> Self {
        Self {
            roles: Vec::new(),
            labels,
        }
    }

    /// Whether the selector names nothing (and therefore matches every node).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roles.is_empty() && self.labels.is_empty()
    }

    /// Whether a node is selected: an empty selector matches all; otherwise a node qualifies when
    /// it serves one of the roles or carries one of the labels.
    #[must_use]
    pub fn matches(&self, node: &NodeProfile) -> bool {
        if self.is_empty() {
            return true;
        }
        self.roles.iter().any(|role| node.has_role(*role))
            || self.labels.iter().any(|label| node.has_label(label))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn node(id: &str, roles: Vec<NodeRole>, labels: &[&str]) -> NodeProfile {
        NodeProfile::from_topology(
            id,
            roles,
            labels.iter().map(|l| (*l).to_owned()).collect(),
            Capacity::default(),
        )
    }

    #[test]
    fn selector_matches_role_or_label() {
        let report = node("r1", vec![NodeRole::Report], &["west"]);
        assert!(NodeSelector::roles(vec![NodeRole::Report]).matches(&report));
        assert!(NodeSelector::labels(vec!["west".to_owned()]).matches(&report));
        assert!(!NodeSelector::labels(vec!["east".to_owned()]).matches(&report));
        assert!(NodeSelector::default().matches(&report));
    }

    #[test]
    fn labels_are_sorted_and_unique() {
        let profile = node("n1", vec![NodeRole::Interactive], &["b", "a", "b"]);
        assert_eq!(profile.labels, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn invalid_id_and_zero_workers_are_invalid() {
        assert!(!NodeId::new("").is_valid());
        assert!(!NodeId::new("bad\nid").is_valid());
        let mut profile = node("n1", vec![NodeRole::Interactive], &[]);
        profile.capacity.workers = 0;
        assert!(!profile.is_valid());
    }

    #[test]
    fn role_names_round_trip() {
        for role in NodeRole::ALL {
            assert_eq!(NodeRole::parse(role.as_str()).expect("parses"), role);
        }
    }
}
