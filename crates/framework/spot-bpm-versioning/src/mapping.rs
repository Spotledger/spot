//! The explicit node-to-node mapping a Case migration carries.
//!
//! A mapping is deliberately not a guess. A node is resolved in this order:
//!
//! 1. an explicit pair in [`MigrationMapping::with_node`];
//! 2. the same id, when the target version still defines it (the "unchanged id" default);
//! 3. nothing, which the planner refuses with
//!    [`VersioningError::UnmappedNode`](crate::VersioningError::UnmappedNode) when the Case is
//!    still standing on that node.
//!
//! The mapping also carries the target's variable contract: the dotted variable paths the
//! target version needs, and the defaults a migration may supply for a path the Case does not
//! carry. Completed nodes are never mapped: their history is left exactly as stored.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;
use spot_dag::NodeId;

/// The explicit node mapping and variable contract of one migration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MigrationMapping {
    nodes: BTreeMap<NodeId, NodeId>,
    required: BTreeSet<String>,
    defaults: BTreeMap<String, Json>,
}

impl MigrationMapping {
    /// An empty mapping: every unchanged id resolves to itself, and no variable is required.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one explicit node pair (builder form).
    #[must_use]
    pub fn with_node(mut self, from: NodeId, to: NodeId) -> Self {
        self.nodes.insert(from, to);
        self
    }

    /// Adds one explicit node pair in place.
    pub fn map_node(&mut self, from: NodeId, to: NodeId) {
        self.nodes.insert(from, to);
    }

    /// Declares a dotted variable path the target version requires (builder form).
    #[must_use]
    pub fn requiring(mut self, path: impl Into<String>) -> Self {
        self.required.insert(path.into());
        self
    }

    /// Supplies a default for a variable path the Case may lack (builder form).
    #[must_use]
    pub fn with_variable_default(mut self, path: impl Into<String>, value: Json) -> Self {
        self.defaults.insert(path.into(), value);
        self
    }

    /// The target node an explicit pair names, when there is one.
    #[must_use]
    pub fn target_of(&self, from: &NodeId) -> Option<&NodeId> {
        self.nodes.get(from)
    }

    /// The explicit pairs, in source-id order.
    #[must_use]
    pub fn pairs(&self) -> &BTreeMap<NodeId, NodeId> {
        &self.nodes
    }

    /// The dotted variable paths the target requires.
    #[must_use]
    pub fn required(&self) -> &BTreeSet<String> {
        &self.required
    }

    /// The defaults the mapping may write into the Case.
    #[must_use]
    pub fn defaults(&self) -> &BTreeMap<String, Json> {
        &self.defaults
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn explicit_pairs_win_and_unlisted_ids_default_to_themselves() {
        let from = NodeId::new("review").unwrap();
        let to = NodeId::new("approve").unwrap();
        let mapping = MigrationMapping::new().with_node(from.clone(), to.clone());
        assert_eq!(mapping.target_of(&from), Some(&to));
        assert_eq!(mapping.target_of(&NodeId::new("other").unwrap()), None);
    }
}
