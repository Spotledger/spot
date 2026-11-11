//! The [`GraphView`] trait, the reference [`AdjacencyGraph`] implementation
//! and the [`reversed`] adaptor.
//!
//! WP-GRA-05: the algorithms of this crate are written **once** against this
//! minimal trait, so native, WASM and TypeScript callers observe the same
//! results from the same graph. A `GraphView` promises:
//!
//! - nodes are dense: every node is one of `0..node_count()`, addressable as
//!   a [`NodeIx`] (`u32`);
//! - `successors` yields `(target, weight)` pairs, with [`UNIT`] (`1`) as the
//!   weight of an unweighted edge; parallel edges and self loops are legal;
//! - `node_count()` never exceeds `u32::MAX as usize` (the [`NodeIx`] cast in
//!   the algorithms would truncate otherwise);
//! - implementations do not panic on an out-of-range node argument — the
//!   reference implementation yields an empty iterator.

use crate::error::GraphError;

/// A dense node index: one of `0..node_count()`.
pub type NodeIx = u32;

/// An edge weight; unweighted edges use [`UNIT`].
pub type Weight = u64;

/// The weight of an unweighted edge.
pub const UNIT: Weight = 1;

/// Read-only access to a directed graph with dense `u32` node ids and `u64`
/// edge weights.
///
/// The trait is deliberately tiny so any representation (adjacency lists, a
/// petgraph-style store, a database view) can implement it, and so the WASM
/// and TypeScript bindings of WP-GRA-05 can expose exactly these two calls.
pub trait GraphView {
    /// The number of nodes; never more than `u32::MAX as usize`.
    fn node_count(&self) -> usize;

    /// The outgoing edges of `node` as `(target, weight)` pairs.
    ///
    /// Implementations own the iteration order; the algorithms of this crate
    /// never let it leak into their output (every returned value is sorted or
    /// computed by an explicit, documented tie-break).
    fn successors(&self, node: NodeIx) -> impl Iterator<Item = (NodeIx, Weight)>;
}

/// Adjacency-list directed multigraph: the reference [`GraphView`].
///
/// Determinism: `successors` keeps edge insertion order, `node_count` and
/// `edge_count` are exact, and the same edge multiset inserted in the same
/// order always yields the same graph. The algorithms' outputs do not depend
/// on the insertion order (verified by `tests/determinism.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdjacencyGraph {
    adjacency: Vec<Vec<(NodeIx, Weight)>>,
    edge_count: usize,
}

impl AdjacencyGraph {
    /// A graph with `node_count` nodes and no edges.
    ///
    /// # Errors
    ///
    /// [`GraphError::NodeIndexOverflow`] when `node_count` exceeds
    /// `u32::MAX as usize`: the nodes could not be addressed as [`NodeIx`].
    pub fn new(node_count: usize) -> Result<Self, GraphError> {
        if node_count > u32::MAX as usize {
            return Err(GraphError::NodeIndexOverflow { count: node_count });
        }
        Ok(Self {
            adjacency: vec![Vec::new(); node_count],
            edge_count: 0,
        })
    }

    /// A graph built from `(from, to)` pairs, all with weight [`UNIT`].
    ///
    /// # Errors
    ///
    /// [`GraphError::NodeIndexOverflow`] for an oversized `node_count`,
    /// [`GraphError::NodeOutOfRange`] for an edge endpoint outside
    /// `0..node_count`.
    pub fn from_edges(node_count: usize, edges: &[(NodeIx, NodeIx)]) -> Result<Self, GraphError> {
        Self::from_weighted_edges(
            node_count,
            edges
                .iter()
                .map(|&(from, to)| (from, to, UNIT))
                .collect::<Vec<_>>()
                .as_slice(),
        )
    }

    /// A graph built from `(from, to, weight)` triples.
    ///
    /// # Errors
    ///
    /// Same as [`AdjacencyGraph::from_edges`].
    pub fn from_weighted_edges(
        node_count: usize,
        edges: &[(NodeIx, NodeIx, Weight)],
    ) -> Result<Self, GraphError> {
        let mut graph = Self::new(node_count)?;
        for &(from, to, weight) in edges {
            graph.add_edge(from, to, weight)?;
        }
        Ok(graph)
    }

    /// Adds one edge; parallel edges and self loops are legal and counted
    /// individually by [`AdjacencyGraph::edge_count`].
    ///
    /// # Errors
    ///
    /// [`GraphError::NodeOutOfRange`] when an endpoint is not a dense node of
    /// the graph.
    pub fn add_edge(&mut self, from: NodeIx, to: NodeIx, weight: Weight) -> Result<(), GraphError> {
        let node_count = self.adjacency.len();
        if from as usize >= node_count {
            return Err(GraphError::NodeOutOfRange {
                index: from,
                node_count,
            });
        }
        if to as usize >= node_count {
            return Err(GraphError::NodeOutOfRange {
                index: to,
                node_count,
            });
        }
        self.adjacency[from as usize].push((to, weight));
        self.edge_count += 1;
        Ok(())
    }

    /// Adds one unweighted edge (weight [`UNIT`]).
    ///
    /// # Errors
    ///
    /// Same as [`AdjacencyGraph::add_edge`].
    pub fn add_unit_edge(&mut self, from: NodeIx, to: NodeIx) -> Result<(), GraphError> {
        self.add_edge(from, to, UNIT)
    }

    /// The number of edges, counting parallel edges and self loops.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edge_count
    }

    /// Assembles the graph from an already-built adjacency structure
    /// (crate-internal: bypasses the `u32::MAX` check trusted to the caller).
    pub(crate) fn from_adjacency(adjacency: Vec<Vec<(NodeIx, Weight)>>) -> Self {
        let edge_count = adjacency.iter().map(Vec::len).sum();
        Self {
            adjacency,
            edge_count,
        }
    }
}

impl GraphView for AdjacencyGraph {
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

/// Adaptor: the same graph with every edge direction flipped, weights kept.
///
/// Returns an owned [`AdjacencyGraph`]; node ids are unchanged. A node's
/// successors in the reversal appear in ascending source order, so the
/// result depends only on the edge multiset — not on the insertion order.
#[must_use]
pub fn reversed<G: GraphView>(graph: &G) -> AdjacencyGraph {
    let node_count = graph.node_count();
    let mut adjacency = vec![Vec::new(); node_count];
    for source in 0..node_count {
        let source = source as NodeIx;
        for (target, weight) in graph.successors(source) {
            adjacency[target as usize].push((source, weight));
        }
    }
    AdjacencyGraph::from_adjacency(adjacency)
}
