//! The forward graph of a process and the ranges of its backward edges.
//!
//! Everything here works on node indexes: the node table is built once, so no walk looks a node
//! up by scanning the node list.

use std::collections::{BTreeMap, VecDeque};

use spot_bpm_commit::KIND_COMMIT;
use spot_bpm_model::{Edge, Node, NodeId, NodeKind, ProcessVersion};

use crate::meta::{EdgeMode, EffectMeta};

/// The most nodes a process may have for the checks.
pub const MAX_NODES: usize = 10_000;
/// The most edges a process may have for the checks.
pub const MAX_EDGES: usize = 50_000;
/// The most backward edges (undo or rewind) the checks walk. Each walk is linear in the graph.
pub const MAX_BACK_EDGES: usize = 256;
/// The most commit points the checks walk. Each region walk is linear in the graph.
pub const MAX_COMMIT_POINTS: usize = 256;

/// A backward edge and the nodes it sends the request back across.
pub(crate) struct BackEdge<'a> {
    pub(crate) edge: &'a Edge,
    pub(crate) mode: EdgeMode,
    /// Indexes of the nodes after the target up to and including the source, on forward paths.
    /// A self-loop (source is target) holds the node itself: its own effects are reversed.
    pub(crate) range: Vec<usize>,
}

/// Forward adjacency (edges whose mode is `Forward`) and the marked backward edges, over node
/// indexes.
pub(crate) struct Shape<'a> {
    pub(crate) nodes: &'a [Node],
    back: Vec<(usize, usize, &'a Edge, EdgeMode)>,
    succ: Vec<Vec<usize>>,
    pred: Vec<Vec<usize>>,
    /// Indexes of the commit points, in node order.
    pub(crate) commits: Vec<usize>,
    is_commit: Vec<bool>,
}

impl<'a> Shape<'a> {
    /// `None` when the process exceeds the bounds.
    pub(crate) fn of(process: &'a ProcessVersion, meta: &dyn EffectMeta) -> Option<Self> {
        let nodes = process.nodes.as_slice();
        if nodes.len() > MAX_NODES || process.edges.len() > MAX_EDGES {
            return None;
        }
        let mut index: BTreeMap<&NodeId, usize> = BTreeMap::new();
        for (i, node) in nodes.iter().enumerate() {
            index.entry(&node.id).or_insert(i);
        }
        let is_commit: Vec<bool> = nodes
            .iter()
            .map(|n| matches!(&n.kind, NodeKind::Unknown(u) if u.kind == KIND_COMMIT))
            .collect();
        let commits: Vec<usize> = (0..nodes.len()).filter(|i| is_commit[*i]).collect();
        if commits.len() > MAX_COMMIT_POINTS {
            return None;
        }
        let mut shape = Shape {
            nodes,
            back: Vec::new(),
            succ: vec![Vec::new(); nodes.len()],
            pred: vec![Vec::new(); nodes.len()],
            commits,
            is_commit,
        };
        for edge in &process.edges {
            let (Some(&from), Some(&to)) = (index.get(&edge.from), index.get(&edge.to)) else {
                continue;
            };
            match meta.edge_mode(edge) {
                EdgeMode::Forward => {
                    shape.succ[from].push(to);
                    shape.pred[to].push(from);
                }
                mode => shape.back.push((from, to, edge, mode)),
            }
        }
        if shape.back.len() > MAX_BACK_EDGES {
            return None;
        }
        Some(shape)
    }

    pub(crate) fn back_edges(&self) -> Vec<BackEdge<'a>> {
        self.back
            .iter()
            .map(|&(from, to, edge, mode)| {
                // From the target forward, and to the source backward: the nodes on a path
                // between them. The target keeps its own effects, except in a self-loop, where
                // the target is the source and the loop reverses it (fail closed).
                let from_target = walk(&self.succ, to);
                let to_source = walk(&self.pred, from);
                let range = (0..self.nodes.len())
                    .filter(|&n| from_target[n] && to_source[n] && (n != to || from == to))
                    .collect();
                BackEdge { edge, mode, range }
            })
            .collect()
    }

    /// Every node (as a flag per index) from which the commit point is reachable on forward edges
    /// without passing through another commit point.
    pub(crate) fn region(&self, commit: usize) -> Vec<bool> {
        let mut region = vec![false; self.nodes.len()];
        let mut queue = VecDeque::from([commit]);
        while let Some(current) = queue.pop_front() {
            for &before in &self.pred[current] {
                if self.is_commit[before] || region[before] {
                    continue;
                }
                region[before] = true;
                queue.push_back(before);
            }
        }
        region
    }
}

/// The nodes reachable from `start` (including it) along `adjacency`, as a flag per index.
fn walk(adjacency: &[Vec<usize>], start: usize) -> Vec<bool> {
    let mut seen = vec![false; adjacency.len()];
    seen[start] = true;
    let mut queue = VecDeque::from([start]);
    while let Some(current) = queue.pop_front() {
        for &next in &adjacency[current] {
            if seen[next] {
                continue;
            }
            seen[next] = true;
            queue.push_back(next);
        }
    }
    seen
}
