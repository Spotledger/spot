//! The [`ModelDiff`] between two derives, for the incremental index later.
//!
//! [`GraphModel::diff`] compares the **edge types** by canonical name (the
//! model's identity key), and node types by DocType name. A change in a
//! field's `required` is an edge change; the metadata version rides along but
//! is deliberately not part of equality — two derives of the same content at
//! different versions diff clean.
//!
//! Deterministic order: added, removed and changed edges are sorted by name,
//! so a diff is stable for golden dumps and logs.

use std::collections::BTreeSet;

use crate::model::EdgeType;

/// The result of `new.diff(old)`: what the incremental index must apply.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelDiff {
    /// Edge types that exist now and did not before, sorted by name.
    pub added_edges: Vec<String>,
    /// Edge types that existed before and do not now, sorted by name.
    pub removed_edges: Vec<String>,
    /// Edge types that exist in both and changed, sorted by name, with the
    /// old and new types.
    pub changed_edges: Vec<EdgeChange>,
    /// DocTypes that are nodes now and were not before, sorted by name.
    pub added_nodes: Vec<String>,
    /// DocTypes that were nodes before and are not now (a DocType removed, or
    /// one that became a relation or a child), sorted by name.
    pub removed_nodes: Vec<String>,
}

/// One changed edge type: old and new side by side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeChange {
    /// The edge's canonical name.
    pub name: String,
    /// The edge type as the old model had it.
    pub old: EdgeType,
    /// The edge type as the new model has it.
    pub new: EdgeType,
}

impl ModelDiff {
    /// Diffs `new` (the receiver in `GraphModel::diff`) against `old`.
    #[must_use]
    pub fn between(old: &crate::model::GraphModel, new: &crate::model::GraphModel) -> Self {
        let mut added_edges = Vec::new();
        let mut removed_edges = Vec::new();
        let mut changed_edges = Vec::new();
        let mut old_names = BTreeSet::new();
        for (name, old_edge) in &old.edges {
            old_names.insert(name.clone());
            match new.edges.get(name) {
                None => removed_edges.push(name.clone()),
                Some(new_edge) if new_edge != old_edge => {
                    changed_edges.push(EdgeChange {
                        name: name.clone(),
                        old: old_edge.clone(),
                        new: new_edge.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        for name in new.edges.keys() {
            if !old_names.contains(name) {
                added_edges.push(name.clone());
            }
        }
        let old_node_names: BTreeSet<&str> = old.nodes.keys().map(String::as_str).collect();
        let new_node_names: BTreeSet<&str> = new.nodes.keys().map(String::as_str).collect();
        let added_nodes = new_node_names
            .difference(&old_node_names)
            .map(|name| (*name).to_owned())
            .collect();
        let removed_nodes = old_node_names
            .difference(&new_node_names)
            .map(|name| (*name).to_owned())
            .collect();
        Self {
            added_edges,
            removed_edges,
            changed_edges,
            added_nodes,
            removed_nodes,
        }
    }

    /// Whether nothing differs (same edge types and node types).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added_edges.is_empty()
            && self.removed_edges.is_empty()
            && self.changed_edges.is_empty()
            && self.added_nodes.is_empty()
            && self.removed_nodes.is_empty()
    }
}
