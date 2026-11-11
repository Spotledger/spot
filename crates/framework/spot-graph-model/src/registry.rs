//! The [`EdgeTypeRegistry`]: indexed views over a `GraphModel`'s edges.
//!
//! Built once per model (cheap: it holds `Rc`-ed sorted `Vec`s of references
//! into the model's own map), it answers the four lookups the brief fixes:
//!
//! - by [`EdgeTypeId`], the stable handle the index and the API use;
//! - by canonical name;
//! - by `from` node ("every edge leaving `invoice`");
//! - by `to` node — the **reverse** lookup, so "what links to `party`" is one
//!   call, sorted and deterministic.
//!
//! Duplicates cannot be introduced here: the registry is built from a model,
//! whose own map already keys by canonical name, and every index points into
//! that map.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::model::{EdgeType, EdgeTypeId, GraphModel, NodeType, NodeTypeId};

/// The four lookups over one model's edge types.
///
/// Every result is sorted by canonical name, so the same model always answers
/// the same list. The registry borrows the model it was built from: keep the
/// model alive (or derive a fresh one) while a registry is held.
#[derive(Debug, Clone)]
pub struct EdgeTypeRegistry {
    model: Rc<GraphModel>,
    by_edge_id: BTreeMap<EdgeTypeId, String>,
    by_from: BTreeMap<String, Vec<String>>,
    by_to_single: BTreeMap<String, Vec<String>>,
    by_to_dynamic: BTreeMap<String, Vec<String>>,
}

impl EdgeTypeRegistry {
    /// Builds the registry over `model`.
    #[must_use]
    pub fn new(model: &GraphModel) -> Self {
        let model = Rc::new(model.clone());
        let mut by_edge_id = BTreeMap::new();
        let mut by_from: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut by_to_single: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut by_to_dynamic: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for edge in model.edges.values() {
            by_edge_id.insert(edge.edge_id.clone(), edge.name.clone());
            by_from
                .entry(edge.from.clone())
                .or_default()
                .push(edge.name.clone());
            match &edge.to {
                crate::model::TargetShape::One(to) => {
                    by_to_single
                        .entry(to.clone())
                        .or_default()
                        .push(edge.name.clone());
                }
                crate::model::TargetShape::AnyOf(targets) => {
                    for to in targets {
                        by_to_dynamic
                            .entry(to.clone())
                            .or_default()
                            .push(edge.name.clone());
                    }
                }
                crate::model::TargetShape::Dynamic => {}
            }
        }
        for list in by_from.values_mut() {
            list.sort();
        }
        for list in by_to_single.values_mut() {
            list.sort();
        }
        for list in by_to_dynamic.values_mut() {
            list.sort();
        }
        Self {
            model,
            by_edge_id,
            by_from,
            by_to_single,
            by_to_dynamic,
        }
    }

    /// The model the registry indexes.
    #[must_use]
    pub fn model(&self) -> &GraphModel {
        &self.model
    }

    /// The edge type by its stable [`EdgeTypeId`].
    #[must_use]
    pub fn by_id(&self, id: &EdgeTypeId) -> Option<&EdgeType> {
        self.by_edge_id
            .get(id)
            .and_then(|name| self.model.edges.get(name))
    }

    /// The edge type by canonical name.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&EdgeType> {
        self.model.edges.get(name)
    }

    /// Every edge type leaving `from`, sorted by name.
    #[must_use]
    pub fn from_node(&self, from: &str) -> Vec<&EdgeType> {
        self.names_of(&self.by_from, from)
    }

    /// Every edge type pointing at `to` — the reverse lookup. Dynamic targets
    /// are resolved through their `AnyOf` set, so "what links to `party`"
    /// includes a dynamic link whose field may name `party`.
    #[must_use]
    pub fn to_node(&self, to: &str) -> Vec<&EdgeType> {
        self.names_of(&self.by_to_single, to)
    }

    /// Every edge type that may reach `to` only dynamically.
    #[must_use]
    pub fn to_node_dynamic(&self, to: &str) -> Vec<&EdgeType> {
        self.names_of(&self.by_to_dynamic, to)
    }

    /// Resolves names into edge references, in the sorted order of the index.
    fn names_of(&self, index: &BTreeMap<String, Vec<String>>, node: &str) -> Vec<&EdgeType> {
        index
            .get(node)
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| self.model.edges.get(name))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The node type by DocType name, for callers walking edges to their ends.
    #[must_use]
    pub fn node(&self, name: &str) -> Option<&NodeType> {
        self.model.nodes.get(name)
    }

    /// The node type by its [`NodeTypeId`].
    #[must_use]
    pub fn node_by_id(&self, id: &NodeTypeId) -> Option<&NodeType> {
        self.model.nodes.values().find(|node| &node.node_id == id)
    }
}
