//! The index itself: one graph per `(tenant, graph name)`, on a `petgraph` `StableGraph`.
//!
//! Node weights are record keys ([`NodeRef`]); edge weights are the canonical edge-type name plus
//! the edge's typed properties. Edges run in **storage orientation** (holder → target), so the
//! walk over the index and the walk over storage visit the same records.
//!
//! [`GraphIndex::apply`] is the incremental path: it applies strictly increasing event sequences,
//! ignores duplicates and older events, and marks the graph stale on a hole. [`GraphIndex::build`]
//! is the rebuild path: a fresh index from a stream of edges and a watermark, built off to the
//! side so a reader never sees a half-built graph.

use std::collections::{BTreeMap, BTreeSet};

use petgraph::Directed;
use petgraph::stable_graph::{EdgeIndex, NodeIndex, StableGraph};
use petgraph::visit::EdgeRef;
use spot_graph_query::{GraphQueryError, Step};
use spot_storage_contract::{Direction, NodeRef, Value};

use crate::error::GraphIndexError;
use crate::event::{Applied, Edge, EdgeEvent, EdgeKey, EdgeOp};
use crate::key::GraphKey;

/// Fixed bytes per node slot (the stable-graph slot plus its map entry), before the key's own
/// bytes. A deterministic estimate, not an allocator's measurement.
const NODE_OVERHEAD_BYTES: u64 = 48;
/// Fixed bytes per edge slot (the stable-graph slot plus its map entry), before the name and
/// property bytes.
const EDGE_OVERHEAD_BYTES: u64 = 64;
/// The bytes of a fixed-size [`Value`] (a record id, a decimal, a date or time).
const SCALAR_VALUE_BYTES: u64 = 16;

/// The bytes a record key contributes.
fn node_bytes(node: &NodeRef) -> u64 {
    NODE_OVERHEAD_BYTES + node.entity.as_str().len() as u64 + 16
}

/// The bytes a value contributes: text, JSON and bytes by length, everything else fixed.
fn value_bytes(value: &Value) -> u64 {
    match value {
        Value::Null => 0,
        Value::Text(text) | Value::Json(text) => text.len() as u64,
        Value::Bytes(bytes) => bytes.len() as u64,
        _ => SCALAR_VALUE_BYTES,
    }
}

/// The bytes one edge weight contributes: its name, its properties and the fixed slot.
fn weight_bytes(edge_type: &str, properties: &BTreeMap<String, Value>) -> u64 {
    let props: u64 = properties
        .iter()
        .map(|(name, value)| name.len() as u64 + value_bytes(value))
        .sum();
    EDGE_OVERHEAD_BYTES + edge_type.len() as u64 + props
}

/// One edge weight: the canonical edge-type name and the edge's typed properties.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EdgeWeight {
    edge_type: String,
    properties: BTreeMap<String, Value>,
}

/// One named graph, in memory.
#[derive(Clone)]
pub struct GraphIndex {
    key: GraphKey,
    graph: StableGraph<NodeRef, EdgeWeight, Directed>,
    nodes: BTreeMap<NodeRef, NodeIndex>,
    edges: BTreeMap<EdgeKey, EdgeIndex>,
    watermark: u64,
    stale: bool,
    bytes: u64,
}

impl GraphIndex {
    /// An empty index for `key`, with watermark 0 and not stale.
    #[must_use]
    pub fn new(key: GraphKey) -> Self {
        Self {
            key,
            graph: StableGraph::new(),
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            watermark: 0,
            stale: false,
            bytes: 0,
        }
    }

    /// Builds a fresh index from a stream of edges and the source's watermark.
    ///
    /// Used by a rebuild: the result is complete before it is swapped in, so no reader ever sees
    /// a partly built graph. Duplicate edges (same identity, delivered twice) collapse to one.
    ///
    /// # Errors
    ///
    /// [`GraphIndexError::InvalidEdge`] when an edge's type name is empty.
    pub fn build(
        key: GraphKey,
        watermark: u64,
        edges: impl IntoIterator<Item = Edge>,
    ) -> Result<Self, GraphIndexError> {
        let mut index = Self::new(key);
        for edge in edges {
            index.upsert_edge(edge.key, edge.properties)?;
        }
        index.watermark = watermark;
        index.stale = false;
        Ok(index)
    }

    /// The graph's identity.
    #[must_use]
    pub fn key(&self) -> &GraphKey {
        &self.key
    }

    /// The highest applied event sequence. 0 before any event.
    #[must_use]
    pub fn watermark(&self) -> u64 {
        self.watermark
    }

    /// Whether the index missed events and must not answer reads until rebuilt.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// How many records the graph holds.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// How many edges the graph holds.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// The estimated bytes the graph occupies, accounted as it changes.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        self.bytes
    }

    /// The bytes recomputed from the graph's current contents. Equal to
    /// [`GraphIndex::estimated_bytes`] when the accounting is correct.
    #[must_use]
    pub fn recompute_bytes(&self) -> u64 {
        let nodes: u64 = self.nodes.keys().map(node_bytes).sum();
        let edges: u64 = self
            .edges
            .keys()
            .filter_map(|key| {
                self.edges
                    .get(key)
                    .and_then(|ix| self.graph.edge_weight(*ix))
                    .map(|w| weight_bytes(&w.edge_type, &w.properties))
            })
            .sum();
        nodes + edges
    }

    /// Whether the record is an endpoint of some edge in the graph.
    #[must_use]
    pub fn contains_node(&self, node: &NodeRef) -> bool {
        self.nodes.contains_key(node)
    }

    /// Every record in the graph, in key order.
    pub fn nodes(&self) -> impl Iterator<Item = &NodeRef> {
        self.nodes.keys()
    }

    /// Every edge in the graph, in key order, with its properties.
    pub fn edges(&self) -> impl Iterator<Item = Edge> + '_ {
        self.edges.iter().map(|(key, ix)| Edge {
            key: key.clone(),
            properties: self
                .graph
                .edge_weight(*ix)
                .map(|w| w.properties.clone())
                .unwrap_or_default(),
        })
    }

    /// The properties of one edge, when it exists.
    #[must_use]
    pub fn edge_properties(&self, key: &EdgeKey) -> Option<&BTreeMap<String, Value>> {
        self.edges
            .get(key)
            .and_then(|ix| self.graph.edge_weight(*ix))
            .map(|w| &w.properties)
    }

    /// The properties of the edge a walk crossed from `from` to `to` along the compiled `step`,
    /// when the graph holds that edge. The edge is stored from its `from` end to its `to` end
    /// (for a relation edge: the record named by the relation's `from_field` to the one named by
    /// its `to_field`), so an incoming step looks the pair up the other way round; a `both` step
    /// tries the walk's orientation first.
    #[must_use]
    pub fn crossed_edge_properties(
        &self,
        from: &NodeRef,
        to: &NodeRef,
        step: &Step,
    ) -> Option<&BTreeMap<String, Value>> {
        let forward = EdgeKey::new(step.name.clone(), from.clone(), to.clone());
        let backward = EdgeKey::new(step.name.clone(), to.clone(), from.clone());
        match step.direction {
            Direction::Outgoing => self.edge_properties(&forward),
            Direction::Incoming => self.edge_properties(&backward),
            Direction::Both => self
                .edge_properties(&forward)
                .or_else(|| self.edge_properties(&backward)),
        }
    }

    /// Applies one event.
    ///
    /// A duplicate or an event at or below the watermark is [`Applied::Ignored`] (idempotent
    /// redelivery). The next sequence is applied and moves the watermark. A sequence beyond
    /// `watermark + 1` is a hole: the graph is marked stale and
    /// [`GraphIndexError::Gap`] is returned, so nothing is guessed.
    ///
    /// # Errors
    ///
    /// [`GraphIndexError::WrongGraph`] when the event names another graph,
    /// [`GraphIndexError::InvalidEdge`] for a malformed edge, and [`GraphIndexError::Gap`] on a
    /// hole.
    pub fn apply(&mut self, event: &EdgeEvent) -> Result<Applied, GraphIndexError> {
        if event.graph != self.key {
            return Err(GraphIndexError::WrongGraph {
                expected: self.key.clone(),
                got: event.graph.clone(),
            });
        }
        if event.edge.edge_type.is_empty() {
            return Err(GraphIndexError::invalid_edge("the edge type name is empty"));
        }
        if self.stale {
            return Ok(Applied::Ignored);
        }
        if event.sequence <= self.watermark {
            return Ok(Applied::Ignored);
        }
        let expected = self.watermark.saturating_add(1);
        if event.sequence > expected {
            self.stale = true;
            return Err(GraphIndexError::Gap {
                graph: self.key.clone(),
                expected,
                got: event.sequence,
            });
        }
        let applied = match event.op {
            EdgeOp::Add | EdgeOp::Update => {
                self.upsert_edge(event.edge.clone(), event.properties.clone())?
            }
            EdgeOp::Remove => self.remove_edge(&event.edge),
        };
        self.watermark = event.sequence;
        Ok(applied)
    }

    /// Marks the graph stale, as a source that cannot be trusted does.
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// The records one step from `node` along `edge_type`, excluding `node` itself, in storage
    /// orientation and sorted order. Empty when the record is not in the graph.
    #[must_use]
    pub fn neighbours(
        &self,
        node: &NodeRef,
        edge_type: &str,
        direction: Direction,
    ) -> Vec<NodeRef> {
        let Some(&ix) = self.nodes.get(node) else {
            return Vec::new();
        };
        let mut out: BTreeSet<NodeRef> = BTreeSet::new();
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for edge in self.graph.edges_directed(ix, petgraph::Direction::Outgoing) {
                if edge.weight().edge_type == edge_type
                    && let Some(weight) = self.graph.node_weight(edge.target())
                {
                    out.insert(weight.clone());
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            for edge in self.graph.edges_directed(ix, petgraph::Direction::Incoming) {
                if edge.weight().edge_type == edge_type
                    && let Some(weight) = self.graph.node_weight(edge.source())
                {
                    out.insert(weight.clone());
                }
            }
        }
        out.remove(node);
        out.into_iter().collect()
    }

    /// The [`NodeSource`](spot_graph_query::NodeSource) step over the index: the records one step
    /// from `node` along the compiled `step`.
    ///
    /// # Errors
    ///
    /// Never fails today; the signature matches the query seam so a future limit can be reported
    /// without changing callers.
    pub fn step(
        &self,
        node: &NodeRef,
        step: &Step,
        _max_nodes: u32,
    ) -> Result<Vec<NodeRef>, GraphQueryError> {
        Ok(self.neighbours(node, &step.name, step.direction))
    }

    /// Inserts or updates an edge, returning what changed.
    ///
    /// Both [`EdgeOp::Add`] and [`EdgeOp::Update`] set the edge to the event's state: a missing
    /// edge is created, an existing edge with different properties is updated, and an existing
    /// edge whose properties already match is a no-op. So redelivering either event never loses a
    /// change.
    fn upsert_edge(
        &mut self,
        key: EdgeKey,
        properties: BTreeMap<String, Value>,
    ) -> Result<Applied, GraphIndexError> {
        if key.edge_type.is_empty() {
            return Err(GraphIndexError::invalid_edge("the edge type name is empty"));
        }
        if let Some(&ix) = self.edges.get(&key) {
            let old = self
                .graph
                .edge_weight(ix)
                .map(|w| w.properties.clone())
                .unwrap_or_default();
            if old != properties {
                let weight = EdgeWeight {
                    edge_type: key.edge_type.clone(),
                    properties,
                };
                self.bytes -= weight_bytes(&key.edge_type, &old);
                self.bytes += weight_bytes(&weight.edge_type, &weight.properties);
                if let Some(slot) = self.graph.edge_weight_mut(ix) {
                    *slot = weight;
                }
                return Ok(Applied::Updated);
            }
            return Ok(Applied::Unchanged);
        }
        let from = self.ensure_node(&key.from);
        let to = self.ensure_node(&key.to);
        let weight = EdgeWeight {
            edge_type: key.edge_type.clone(),
            properties,
        };
        let ix = self.graph.add_edge(from, to, weight.clone());
        self.edges.insert(key, ix);
        self.bytes += weight_bytes(&weight.edge_type, &weight.properties);
        Ok(Applied::Added)
    }

    /// Removes an edge and any endpoint it isolates.
    fn remove_edge(&mut self, key: &EdgeKey) -> Applied {
        let Some(ix) = self.edges.remove(key) else {
            return Applied::Unchanged;
        };
        let endpoints = self.graph.edge_endpoints(ix);
        if let Some(weight) = self.graph.edge_weight(ix) {
            self.bytes -= weight_bytes(&weight.edge_type, &weight.properties);
        }
        self.graph.remove_edge(ix);
        if let Some((from, to)) = endpoints {
            self.drop_if_isolated(from);
            self.drop_if_isolated(to);
        }
        Applied::Removed
    }

    /// The node's stable index, adding it when new.
    fn ensure_node(&mut self, node: &NodeRef) -> NodeIndex {
        if let Some(&ix) = self.nodes.get(node) {
            return ix;
        }
        let ix = self.graph.add_node(node.clone());
        self.nodes.insert(node.clone(), ix);
        self.bytes += node_bytes(node);
        ix
    }

    /// Removes a node that has no edges left, so the graph's records are exactly the endpoints of
    /// its edges.
    fn drop_if_isolated(&mut self, ix: NodeIndex) {
        let isolated = self
            .graph
            .edges_directed(ix, petgraph::Direction::Outgoing)
            .next()
            .is_none()
            && self
                .graph
                .edges_directed(ix, petgraph::Direction::Incoming)
                .next()
                .is_none();
        if !isolated {
            return;
        }
        if let Some(node) = self.graph.node_weight(ix).cloned() {
            self.nodes.remove(&node);
            self.bytes -= node_bytes(&node);
        }
        self.graph.remove_node(ix);
    }
}

impl std::fmt::Debug for GraphIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphIndex")
            .field("key", &self.key)
            .field("nodes", &self.node_count())
            .field("edges", &self.edge_count())
            .field("watermark", &self.watermark)
            .field("stale", &self.stale)
            .finish()
    }
}
