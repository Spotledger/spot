//! The compiler (brief §3): validate, then build an immutable [`CompiledDag`]
//! with the indexes a runtime needs.
//!
//! [`CompiledDag`] is [`Arc`]-wrapped so cloning it is cheap: a running
//! instance can keep one version alive while the registry publishes the next
//! (ADR-13: definitions live in memory, immutable once compiled).
//!
//! Everything here is pure and synchronous: no conditions are evaluated (the
//! engine BPM-01 does that), no `kind`/`config` is interpreted, no clock, no
//! randomness. Traversals are iterative: `topological_order` and `depth` walk
//! explicit stacks/heaps and handle a 10,000-node chain.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::error::DagError;
use crate::model::{DagDefinition, Edge, JoinRule, Node, NodeId};
use crate::validate::{Limits, validate};

/// One compiled edge as seen from a node: the other endpoint plus the edge's
/// (opaque) condition. Ordering is by `(node, condition)` — the sort order of
/// the successor/predecessor lists.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CompiledEdge {
    /// The node at the other end of the edge.
    pub node: NodeId,
    /// The edge's condition, when it has one.
    pub condition: Option<String>,
}

/// The run-state a [`CompiledDag::ready`] / [`CompiledDag::is_finished`] call
/// reads. Set data (`BTreeSet`) so iteration is sorted and deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// Node ids whose work has completed.
    pub completed: BTreeSet<NodeId>,
    /// Node ids whose work has started but may not be complete.
    pub started: BTreeSet<NodeId>,
    /// Edges the engine has taken (after evaluating conditions), as
    /// `(from, to)`.
    pub taken: BTreeSet<(NodeId, NodeId)>,
}

/// The compiled, immutable form of a definition: indexes for the runtime.
///
/// Clone is cheap (the data sits in one [`Arc`]). All accessors are stable:
/// `entries`/`terminals` and `successors`/`predecessors` are sorted, and
/// `topological_order` is a deterministic function of the graph, not of the
/// definition's listing order.
#[derive(Debug, Clone)]
pub struct CompiledDag {
    inner: Arc<Inner>,
}

/// The shared payload behind the [`Arc`].
#[derive(Debug)]
struct Inner {
    key: String,
    version: u32,
    hash: crate::version::ContentHash,
    // Nodes in definition order.
    nodes: Vec<Node>,
    // id -> definition-order index.
    index: BTreeMap<String, usize>,
    entries: Vec<NodeId>,
    terminals: Vec<NodeId>,
    successors: Vec<Vec<CompiledEdge>>,
    predecessors: Vec<Vec<CompiledEdge>>,
    topological: Vec<NodeId>,
    depths: Vec<Option<u32>>,
}

use std::collections::BTreeMap;

impl CompiledDag {
    /// Compiles a definition: validates first (nothing compiles that does not
    /// validate), then builds the indexes.
    ///
    /// # Errors
    /// The same [`Vec<DagError>`] as [`validate`].
    pub fn compile(def: &DagDefinition, limits: &Limits) -> Result<Self, Vec<DagError>> {
        validate(def, limits)?;
        Ok(Self::unchecked(def))
    }

    /// Builds the indexes from a definition that just validated.
    fn unchecked(def: &DagDefinition) -> Self {
        let nodes = def.nodes.clone();
        let index: BTreeMap<String, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str().to_owned(), i))
            .collect();

        // successors/predecessors per node, each sorted by (other, condition).
        let mut successors: Vec<Vec<CompiledEdge>> = vec![Vec::new(); nodes.len()];
        let mut predecessors: Vec<Vec<CompiledEdge>> = vec![Vec::new(); nodes.len()];
        for edge in &def.edges {
            let (Some(from), Some(to)) = (
                index.get(edge.from.as_str()).copied(),
                index.get(edge.to.as_str()).copied(),
            ) else {
                // Unreachable: validation already rejected unknown endpoints.
                continue;
            };
            successors[from].push(CompiledEdge {
                node: edge.to.clone(),
                condition: edge.condition.clone(),
            });
            predecessors[to].push(CompiledEdge {
                node: edge.from.clone(),
                condition: edge.condition.clone(),
            });
        }
        for list in successors.iter_mut() {
            list.sort();
        }
        for list in predecessors.iter_mut() {
            list.sort();
        }

        let entries: Vec<NodeId> = nodes
            .iter()
            .filter(|n| predecessors[index_of(n, &index)].is_empty())
            .map(|n| n.id.clone())
            .collect();
        let terminals: Vec<NodeId> = nodes
            .iter()
            .filter(|n| successors[index_of(n, &index)].is_empty())
            .map(|n| n.id.clone())
            .collect();

        let topological = kahn(&nodes, &successors, &index);
        let depths = depths_from(&nodes, &successors, &entries, &index);

        Self {
            inner: Arc::new(Inner {
                key: def.key.clone(),
                version: def.version,
                hash: crate::version::ContentHash::of(def),
                nodes,
                index,
                entries,
                terminals,
                successors,
                predecessors,
                topological,
                depths,
            }),
        }
    }

    /// The definition's key (the DAG's name).
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// The definition's version number.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.inner.version
    }

    /// The content hash of the compiled definition.
    #[must_use]
    pub fn hash(&self) -> &crate::version::ContentHash {
        &self.inner.hash
    }

    /// The node with this id, when it exists.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.inner
            .index
            .get(id.as_str())
            .map(|&i| &self.inner.nodes[i])
    }

    /// All nodes in **definition order** (not sorted).
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.inner.nodes
    }

    /// Entry nodes (no incoming edge), sorted by id.
    #[must_use]
    pub fn entries(&self) -> &[NodeId] {
        &self.inner.entries
    }

    /// Terminal nodes (no outgoing edge), sorted by id.
    #[must_use]
    pub fn terminals(&self) -> &[NodeId] {
        &self.inner.terminals
    }

    /// The outgoing edges of a node, sorted by `(other id, condition)`.
    #[must_use]
    pub fn successors(&self, id: &NodeId) -> &[CompiledEdge] {
        self.inner
            .index
            .get(id.as_str())
            .map(|&i| self.inner.successors[i].as_slice())
            .unwrap_or(&[])
    }

    /// The incoming edges of a node, sorted by `(other id, condition)`.
    #[must_use]
    pub fn predecessors(&self, id: &NodeId) -> &[CompiledEdge] {
        self.inner
            .index
            .get(id.as_str())
            .map(|&i| self.inner.predecessors[i].as_slice())
            .unwrap_or(&[])
    }

    /// The nodes in deterministic topological order (Kahn's algorithm, the
    /// smallest ready id first). The same graph always yields the same order,
    /// regardless of how the definition listed its nodes and edges.
    #[must_use]
    pub fn topological_order(&self) -> &[NodeId] {
        &self.inner.topological
    }

    /// The length of the longest path from any entry to this node; entries
    /// themselves have depth 0. `None` when the id is unknown.
    #[must_use]
    pub fn depth(&self, id: &NodeId) -> Option<u32> {
        let i = *self.inner.index.get(id.as_str())?;
        self.inner.depths[i]
    }

    /// The nodes that may start now.
    ///
    /// A node is ready when all of these hold:
    ///
    /// 1. it is **not** in `Progress::completed`;
    /// 2. it is **not** in `Progress::started` (a node starts once, so a node
    ///    that is already started is not offered again);
    /// 3. its join rule is satisfied by the edges in `Progress::taken`:
    ///    - an **entry node** (no incoming edges at all) is ready from the
    ///      start;
    ///    - [`JoinRule::All`]: every incoming edge is in `taken`;
    ///    - [`JoinRule::Any`]: at least one incoming edge is in `taken`.
    ///
    /// An edge becomes `taken` only when the engine says so (for example
    /// after evaluating a condition), so a conditional branch that was not
    /// taken simply never makes its target ready. `ready` never evaluates
    /// conditions and never mutates the [`Progress`]. The result is sorted by
    /// node id.
    #[must_use]
    pub fn ready(&self, state: &Progress) -> Vec<NodeId> {
        let mut result = Vec::new();
        for (i, node) in self.inner.nodes.iter().enumerate() {
            if state.completed.contains(&node.id) || state.started.contains(&node.id) {
                continue;
            }
            let incoming = &self.inner.predecessors[i];
            let satisfied = if incoming.is_empty() {
                true
            } else {
                match node.join {
                    JoinRule::All => incoming
                        .iter()
                        .all(|e| state.taken.contains(&(e.node.clone(), node.id.clone()))),
                    JoinRule::Any => incoming
                        .iter()
                        .any(|e| state.taken.contains(&(e.node.clone(), node.id.clone()))),
                }
            };
            if satisfied {
                result.push(node.id.clone());
            }
        }
        // Definition order equals id order only by luck; sort for the
        // documented contract.
        result.sort();
        result
    }

    /// True when the run may end (brief §3, the exact rule):
    ///
    /// 1. no node is started-but-not-completed (work in flight always
    ///    blocks, whichever branch it sits on);
    /// 2. every terminal node **reachable through taken edges** from the
    ///    entry nodes is completed — a conditional branch the engine never
    ///    took does not block finishing.
    ///
    /// `ready` is the forward companion: a node is ready when it is neither
    /// completed nor started and its join rule is satisfied by the
    /// `Progress::taken` edges into it. A node that is ready-but-not-completed
    /// is terminal-blocked only if it is reachable *and* a terminal, so the
    /// rule above is exactly "the taken flow has run out".
    ///
    /// Walks forward, iteratively (no recursion), from the entry nodes along
    /// taken edges only, and checks terminal completion on what it reaches.
    #[must_use]
    pub fn is_finished(&self, state: &Progress) -> bool {
        // Rule 1: work in flight always blocks, even outside the taken flow.
        if state.started.iter().any(|id| !state.completed.contains(id)) {
            return false;
        }
        // Rule 2: forward walk over taken edges from the entries — purely
        // structural; completed/uncompleted states do not seed the walk, so
        // a node the taken flow cannot actually reach stays unreached even
        // if the state labels it completed.
        let mut reached: Vec<bool> = vec![false; self.inner.nodes.len()];
        let mut stack: Vec<usize> = Vec::new();
        for (i, _node) in self.inner.nodes.iter().enumerate() {
            if self.inner.predecessors[i].is_empty() {
                reached[i] = true;
                stack.push(i);
            }
        }
        while let Some(i) = stack.pop() {
            for out in &self.inner.successors[i] {
                if state
                    .taken
                    .contains(&(self.inner.nodes[i].id.clone(), out.node.clone()))
                    && let Some(&j) = self.inner.index.get(out.node.as_str())
                    && !reached[j]
                {
                    reached[j] = true;
                    stack.push(j);
                }
            }
        }
        for (i, node) in self.inner.nodes.iter().enumerate() {
            if reached[i]
                && self.inner.successors[i].is_empty()
                && !state.completed.contains(&node.id)
            {
                return false;
            }
        }
        // Every reachable terminal completed: finished.
        true
    }
}

/// Index lookups for edges (kept off the hot path; the compiler indexes by
/// node id once and reuses it).
impl Edge {
    /// The edge's target's index.
    #[must_use]
    pub fn to_index(&self, index: &BTreeMap<String, usize>) -> Option<usize> {
        index.get(self.to.as_str()).copied()
    }

    /// The edge's source's index.
    #[must_use]
    pub fn from_index(&self, index: &BTreeMap<String, usize>) -> Option<usize> {
        index.get(self.from.as_str()).copied()
    }
}

/// `index.get` convenience for node rows.
fn index_of(node: &Node, index: &BTreeMap<String, usize>) -> usize {
    match index.get(node.id.as_str()) {
        Some(&i) => i,
        // Only reachable if the index disagrees with `nodes`, which it cannot
        // (it was just built from them); a stable default keeps total.
        None => usize::MAX,
    }
}

/// Kahn's algorithm with the smallest ready id first (brief §3): a BinaryHeap
/// standing in for a min-heap via `Reverse` would be the textbook shape, but a
/// `BTreeSet` of ready ids does the same with plain data.
fn kahn(
    nodes: &[Node],
    successors: &[Vec<CompiledEdge>],
    index: &BTreeMap<String, usize>,
) -> Vec<NodeId> {
    let n = nodes.len();
    let mut indegree: Vec<usize> = vec![0; n];
    for succ in successors.iter().take(n) {
        for out in succ {
            if let Some(&to) = index.get(out.node.as_str()) {
                indegree[to] += 1;
            }
        }
    }
    let mut ready: BTreeSet<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    let mut order: Vec<NodeId> = Vec::with_capacity(n);
    while let Some(&i) = ready.iter().next() {
        ready.remove(&i);
        order.push(nodes[i].id.clone());
        for out in &successors[i] {
            if let Some(&to) = index.get(out.node.as_str()) {
                indegree[to] -= 1;
                if indegree[to] == 0 {
                    ready.insert(to);
                }
            }
        }
    }
    order
}

/// Longest-path depth from entries (brief §3): iterative longest-path DP on a
/// forward topological walk.
fn depths_from(
    nodes: &[Node],
    successors: &[Vec<CompiledEdge>],
    entries: &[NodeId],
    index: &BTreeMap<String, usize>,
) -> Vec<Option<u32>> {
    let mut depths: Vec<Option<u32>> = vec![None; nodes.len()];
    // Process in topological order: every predecessor's depth is final before
    // a node's own becomes.
    let mut queue: BTreeSet<usize> = entries
        .iter()
        .filter_map(|id| index.get(id.as_str()).copied())
        .collect();
    for &i in &queue {
        depths[i] = Some(0);
    }
    while let Some(&i) = queue.iter().next() {
        queue.remove(&i);
        for out in &successors[i] {
            if let Some(to) = index.get(out.node.as_str()).copied() {
                let next = depths[i].unwrap_or(0) + 1;
                if depths[to].is_none_or(|d| next > d) {
                    depths[to] = Some(next);
                }
                queue.insert(to);
            }
        }
    }
    depths
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::{JoinRule, Node, NodeId};

    fn node(id: &str) -> Node {
        Node::new(
            NodeId::new(id).unwrap(),
            "step",
            JoinRule::All,
            serde_json::Value::Null,
        )
        .unwrap()
    }

    fn edge(from: &str, to: &str) -> Edge {
        Edge::new(NodeId::new(from).unwrap(), NodeId::new(to).unwrap(), None)
    }

    #[test]
    fn topological_follows_smallest_ready() {
        // diamond: a; b, c; d. Definition order deliberately scrambled.
        let def = DagDefinition::new(
            "t",
            1,
            vec![node("d"), node("b"), node("c"), node("a")],
            vec![
                edge("a", "c"),
                edge("b", "d"),
                edge("a", "b"),
                edge("c", "d"),
            ],
        );
        let dag = CompiledDag::compile(&def, &Limits::new()).unwrap();
        let ids: Vec<&str> = dag.topological_order().iter().map(NodeId::as_str).collect();
        assert_eq!(ids, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn depth_on_diamond() {
        let def = DagDefinition::new(
            "t",
            1,
            vec![node("a"), node("b"), node("c"), node("d")],
            vec![
                edge("a", "b"),
                edge("a", "c"),
                edge("b", "d"),
                edge("c", "d"),
            ],
        );
        let dag = CompiledDag::compile(&def, &Limits::new()).unwrap();
        assert_eq!(dag.depth(&NodeId::new("a").unwrap()), Some(0));
        assert_eq!(dag.depth(&NodeId::new("b").unwrap()), Some(1));
        assert_eq!(dag.depth(&NodeId::new("d").unwrap()), Some(2));
    }

    #[test]
    fn successors_and_predecessors_sorted() {
        // mid has two successors (z, a) and two predecessors (m, a); the
        // definition order is scrambled on purpose.
        let def = DagDefinition::new(
            "t",
            1,
            vec![
                node("mid"),
                node("z"),
                node("a"),
                node("m"),
                node("b2"),
                node("end"),
                node("end2"),
            ],
            vec![
                Edge::new(NodeId::new("mid").unwrap(), NodeId::new("z").unwrap(), None),
                Edge::new(NodeId::new("m").unwrap(), NodeId::new("mid").unwrap(), None),
                Edge::new(NodeId::new("a").unwrap(), NodeId::new("mid").unwrap(), None),
                Edge::new(
                    NodeId::new("mid").unwrap(),
                    NodeId::new("b2").unwrap(),
                    None,
                ),
                edge("z", "end"),
                edge("m", "end"),
                edge("a", "end2"),
                edge("b2", "end"),
            ],
        );
        let dag = CompiledDag::compile(&def, &Limits::new()).unwrap();
        let mid = NodeId::new("mid").unwrap();
        let succ: Vec<&str> = dag
            .successors(&mid)
            .iter()
            .map(|e| e.node.as_str())
            .collect();
        assert_eq!(succ, vec!["b2", "z"]);
        let pred: Vec<&str> = dag
            .predecessors(&mid)
            .iter()
            .map(|e| e.node.as_str())
            .collect();
        assert_eq!(pred, vec!["a", "m"]);
    }

    #[test]
    fn entry_node_ready_from_start() {
        let def = DagDefinition::new("t", 1, vec![node("a"), node("b")], vec![edge("a", "b")]);
        let dag = CompiledDag::compile(&def, &Limits::new()).unwrap();
        let ready = dag.ready(&Progress::default());
        assert_eq!(ready, vec![NodeId::new("a").unwrap()]);
    }
}
