//! What a graph query returns, and the [`GraphView`] of it.

use std::collections::BTreeMap;

use spot_graph_algo::{GraphView, NodeIx, UNIT, Weight};
use spot_storage_contract::{Name, NodeRef, Value};

/// A node the caller may see, reached by the query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphNode {
    /// The record.
    pub node: NodeRef,
    /// Its distance from the nearest start node, within the nodes the caller may see.
    pub depth: u16,
    /// The node it was reached from, and the edge type used, for depth above 0.
    pub via: Option<(NodeRef, String)>,
    /// The requested fields the caller may read, masked where the grant says so. A field the
    /// caller may not read is absent, not null.
    pub fields: BTreeMap<Name, Value>,
    /// The hops from the start node to this node, shallowest first. Filled only when the query
    /// asked for `Include::edge_properties` or a rollup; empty otherwise and at depth 0.
    pub path: Vec<PathHop>,
    /// The product of the rolled-up edge property along `path` (the query's `rollup`), or `None`
    /// at depth 0 or when the query asked for no rollup. An integer when every factor is an
    /// integer, otherwise a decimal whose scale is the sum of the factors' scales.
    pub rolled_up: Option<Value>,
    /// With `rollup_sum`: how many simple paths contributed to `rolled_up`, saturating at
    /// `u32::MAX` (then `path_count_saturated` is true). 0 when no summed rollup was asked and at
    /// a start node.
    pub path_count: u32,
    /// Whether the real number of paths exceeds `u32::MAX`, so `path_count` is a floor.
    pub path_count_saturated: bool,
}

/// One hop of a path: the node reached, the edge type crossed and that edge's properties.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathHop {
    /// The node this hop reaches.
    pub node: NodeRef,
    /// The canonical edge-type name crossed.
    pub edge_type: String,
    /// The edge's properties, by name; empty unless `Include::edge_properties` is set. Typed
    /// values only: null, bool, integer, decimal, text, date and link.
    pub properties: BTreeMap<Name, Value>,
}

/// One followed edge between two returned nodes, in traversal orientation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEdge {
    /// The canonical edge-type name.
    pub edge: String,
    /// The node the traversal left.
    pub from: NodeRef,
    /// The node it reached.
    pub to: NodeRef,
}

/// The answer to a query.
///
/// The caller learns only about nodes it may read. A node it may not read is removed together
/// with every path through it, and `truncated_by_permission` says that something was removed
/// without naming it. Counts and depths are those of the visible graph, so they reveal nothing
/// hidden.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GraphResult {
    /// The visible nodes, in the query's order.
    pub nodes: Vec<GraphNode>,
    /// The edges that reached each returned node (one per node at depth above 0), in the
    /// query's order.
    pub edges: Vec<GraphEdge>,
    /// Whether a node reached from a visible node was removed for the caller's permissions or
    /// filters. A hidden start node never sets this.
    pub truncated_by_permission: bool,
    /// Whether the query asked for a `max_paths` above the ceiling and it was clamped.
    pub max_paths_clamped: bool,
}

impl GraphResult {
    /// The result as a [`GraphView`]: node `i` is `nodes[i]`, and each edge runs from its `from`
    /// to its `to`. The shortest-path, reachability and topological algorithms of
    /// `spot-graph-algo` run on it directly.
    #[must_use]
    pub fn view(&self) -> ResultGraph {
        let index: BTreeMap<&NodeRef, NodeIx> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| NodeIx::try_from(i).ok().map(|ix| (&n.node, ix)))
            .collect();
        let mut adjacency: Vec<Vec<(NodeIx, Weight)>> = vec![Vec::new(); self.nodes.len()];
        for edge in &self.edges {
            if let (Some(&from), Some(&to)) = (index.get(&edge.from), index.get(&edge.to)) {
                adjacency[from as usize].push((to, UNIT));
            }
        }
        ResultGraph { adjacency }
    }
}

/// The [`GraphView`] of a [`GraphResult`]. Built by [`GraphResult::view`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultGraph {
    adjacency: Vec<Vec<(NodeIx, Weight)>>,
}

impl GraphView for ResultGraph {
    fn node_count(&self) -> usize {
        self.adjacency.len()
    }

    fn successors(&self, node: NodeIx) -> impl Iterator<Item = (NodeIx, Weight)> {
        self.adjacency
            .get(node as usize)
            .into_iter()
            .flatten()
            .copied()
    }
}
