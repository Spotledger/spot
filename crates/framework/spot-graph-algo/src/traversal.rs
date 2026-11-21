//! Topological order ([`topological_order`]) and cycle detection
//! ([`find_cycle`]), iterative and deterministic, sharing one Kahn kernel.
//!
//! The two functions answer different questions but must agree: when
//! [`topological_order`] rejects a graph with [`GraphError::Cycle`],
//! [`find_cycle`] returns exactly that witness cycle. Both therefore share
//! [`kahn_outcome`] and [`witness`]. This module also hosts the crate-wide
//! determinism primitive ([`sorted_successors`]) and the argument checks
//! every algorithm reuses.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use crate::error::{Cycle, GraphError};
use crate::limits::{Limits, ensure_node_count, ensure_visited};
use crate::scc;
use crate::view::{GraphView, NodeIx, Weight};

/// Checks that `node` is a valid dense index of `graph`.
pub(crate) fn ensure_node_in_range(graph: &impl GraphView, node: NodeIx) -> Result<(), GraphError> {
    if node as usize >= graph.node_count() {
        Err(GraphError::NodeOutOfRange {
            index: node,
            node_count: graph.node_count(),
        })
    } else {
        Ok(())
    }
}

/// Checks `from`/`to` for [`crate::shortest_path`].
pub(crate) fn ensure_path_endpoints(
    graph: &impl GraphView,
    from: NodeIx,
    to: NodeIx,
) -> Result<(), GraphError> {
    ensure_node_in_range(graph, from)?;
    ensure_node_in_range(graph, to)
}

/// Collects a node's outgoing edges into an ascending, deduplicated
/// `(target, weight)` vector: the canonical scan order of every algorithm in
/// this crate. Determinism primitive: the graph's own iteration order never
/// leaks into any output.
pub(crate) fn sorted_successors(
    graph: &impl GraphView,
    node: NodeIx,
    scratch: &mut Vec<(NodeIx, Weight)>,
) {
    scratch.clear();
    scratch.extend(graph.successors(node));
    scratch.sort_unstable();
    scratch.dedup();
}

/// The topological order of an acyclic graph (Kahn's algorithm; ties broken
/// by the lowest [`NodeIx`]).
///
/// Determinism: among the ready set (in-degree 0 in the working graph) the
/// algorithm always emits the lowest index first (a min-heap), so the output
/// is the lexicographically smallest topological order — identical on every
/// run and platform, and independent of the edge insertion order and of the
/// `successors` iteration order.
///
/// # Errors
///
/// * [`GraphError::LimitExceeded`] (kind `Nodes`) when `graph.node_count()`
///   exceeds `limits.max_nodes`;
/// * [`GraphError::NodeOutOfRange`] when an edge endpoint is not a dense
///   node of the graph;
/// * [`GraphError::LimitExceeded`] (kind `Visited`) when Kahn or the witness
///   search expands more than `limits.max_visited` distinct nodes;
/// * [`GraphError::Cycle`] when the graph has a cycle: the payload is one
///   *shortest-found witness cycle* (exact contract at [`find_cycle`]),
///   first node repeated at the end.
#[must_use = "the caller wants the order or the cycle"]
pub fn topological_order<G: GraphView>(
    graph: &G,
    limits: &Limits,
) -> Result<Vec<NodeIx>, GraphError> {
    let node_count = graph.node_count();
    ensure_node_count(node_count, limits)?;
    match kahn_outcome(graph, node_count, limits)? {
        KahnOutcome::Acyclic(order) => Ok(order),
        KahnOutcome::Cyclic => Err(GraphError::Cycle(witness(graph, node_count, limits)?)),
    }
}

/// One witness cycle, or `None` when the graph is acyclic.
///
/// Determinism — the witness contract (the same cycle [`topological_order`]
/// rejects a cyclic graph with):
///
/// 1. The witness is the *shortest* (fewest edges) cycle that contains the
///    *lowest-index node that lies on any cycle*. Cyclic nodes are read off
///    the SCCs (members of a component of size >= 2, plus singleton members
///    with a self loop), so the lowest-index cyclic node `r` is a property
///    of the edge multiset alone.
/// 2. The shortest cycle through `r` is found by a breadth-first search from
///    `r` (FIFO queue, successors scanned ascending, first discovery wins
///    the parent slot — a deterministic BFS tree), then the closing edge
///    `u -> r` minimising `dist[u] + 1`, ties on `u` by lowest index. A self
///    loop on `r` is the length-1 candidate (`u == r`).
/// 3. The result is `[r, .., u, r]`: the unique BFS-tree path from `r` to
///    `u`, with the first node repeated at the end.
///
/// The search is iterative; its queue lives on the heap, so graphs of any
/// size cannot overflow the stack.
///
/// # Errors
///
/// * [`GraphError::LimitExceeded`] (kind `Nodes`/`Visited`) as for
///   [`topological_order`];
/// * [`GraphError::NodeOutOfRange`] for an edge endpoint outside the graph.
#[must_use = "the caller wants the cycle or the absence of one"]
pub fn find_cycle<G: GraphView>(graph: &G, limits: &Limits) -> Result<Option<Cycle>, GraphError> {
    let node_count = graph.node_count();
    ensure_node_count(node_count, limits)?;
    match kahn_outcome(graph, node_count, limits)? {
        KahnOutcome::Acyclic(_) => Ok(None),
        KahnOutcome::Cyclic => Ok(Some(witness(graph, node_count, limits)?)),
    }
}

/// The outcome of Kahn's algorithm over `graph`.
enum KahnOutcome {
    /// The order is complete: the graph is acyclic.
    Acyclic(Vec<NodeIx>),
    /// The order is incomplete: the graph has a cycle.
    Cyclic,
}

/// Kahn's algorithm: in-degrees in one `O(V + E)` scan, then a min-heap
/// ready set emitting the lowest index first.
fn kahn_outcome<G: GraphView>(
    graph: &G,
    node_count: usize,
    limits: &Limits,
) -> Result<KahnOutcome, GraphError> {
    let mut scratch: Vec<(NodeIx, Weight)> = Vec::new();
    let mut in_degrees = vec![0u32; node_count];
    for source in 0..node_count {
        scratch.clear();
        scratch.extend(graph.successors(source as NodeIx));
        for &(target, _) in &scratch {
            ensure_node_in_range(graph, target)?;
            in_degrees[target as usize] += 1;
        }
    }

    let mut ready: BinaryHeap<Reverse<NodeIx>> = BinaryHeap::new();
    for (index, &degree) in in_degrees.iter().enumerate() {
        if degree == 0 {
            ready.push(Reverse(index as NodeIx));
        }
    }
    let mut order = Vec::with_capacity(node_count);
    while let Some(Reverse(node)) = ready.pop() {
        order.push(node);
        ensure_visited(order.len(), limits)?;
        // Decrement over the *raw* successors: parallel edges are counted
        // (and must be decremented) once per edge. The heap keeps the
        // emission deterministic regardless of decrement order.
        scratch.clear();
        scratch.extend(graph.successors(node));
        for &(target, _) in &scratch {
            let degree = &mut in_degrees[target as usize];
            *degree -= 1;
            if *degree == 0 {
                ready.push(Reverse(target));
            }
        }
    }
    if order.len() == node_count {
        Ok(KahnOutcome::Acyclic(order))
    } else {
        Ok(KahnOutcome::Cyclic)
    }
}

/// Computes the witness cycle of a cyclic graph (see [`find_cycle`]).
///
/// Guards against misuse (an acyclic graph reaching here) with
/// [`GraphError::InternalInvariant`] instead of a panic.
fn witness<G: GraphView>(
    graph: &G,
    node_count: usize,
    limits: &Limits,
) -> Result<Cycle, GraphError> {
    let (components, _) = scc::compute(graph, limits)?;

    // Lowest-index node that lies on any cycle: a member of an SCC of size
    // >= 2 (components are sorted, `component[0]` is the smallest member) or
    // a singleton with a self loop — whichever is lowest overall.
    let mut scratch: Vec<(NodeIx, Weight)> = Vec::new();
    let mut lowest: Option<NodeIx> = None;
    let consider = |node: NodeIx, lowest: &mut Option<NodeIx>| {
        *lowest = Some(match *lowest {
            Some(seen) => seen.min(node),
            None => node,
        });
    };
    for component in &components {
        if component.len() >= 2 {
            consider(component[0], &mut lowest);
        }
    }
    for source in 0..node_count as NodeIx {
        scratch.clear();
        scratch.extend(graph.successors(source));
        if scratch.iter().any(|&(target, _)| target == source) {
            consider(source, &mut lowest);
        }
    }
    let root = match lowest {
        Some(root) => root,
        None => {
            return Err(GraphError::InternalInvariant {
                what: "kahn found a cycle but no SCC membership or self loop did",
            });
        }
    };

    // Deterministic BFS from `root`: FIFO queue, successors scanned in
    // ascending order, first discovery wins the parent slot. Every
    // discovered node counts against `limits.max_visited`.
    let mut parents = vec![NodeIx::MAX; node_count];
    let mut distances = vec![NodeIx::MAX; node_count];
    let mut queue: VecDeque<NodeIx> = VecDeque::new();
    parents[root as usize] = root;
    distances[root as usize] = 0;
    let mut reached = 1usize;
    ensure_visited(reached, limits)?;
    queue.push_back(root);
    while let Some(node) = queue.pop_front() {
        sorted_successors(graph, node, &mut scratch);
        for &(target, _) in &scratch {
            if parents[target as usize] == NodeIx::MAX {
                parents[target as usize] = node;
                distances[target as usize] = distances[node as usize] + 1;
                reached += 1;
                ensure_visited(reached, limits)?;
                queue.push_back(target);
            }
        }
    }

    // Shortest cycle through `root`: the closing edge `u -> root` with the
    // smallest `dist[u] + 1`; ties on `u` by lowest index (ascending scan,
    // first win). A self loop on `root` is the length-1 candidate.
    let mut best: Option<(u64, NodeIx)> = None; // (length, closing source)
    for u in 0..node_count as NodeIx {
        if distances[u as usize] == NodeIx::MAX {
            continue;
        }
        scratch.clear();
        scratch.extend(graph.successors(u));
        if scratch.iter().any(|&(target, _)| target == root) {
            let length = u64::from(distances[u as usize]) + 1;
            if best.is_none_or(|(seen_length, _)| length < seen_length) {
                best = Some((length, u));
            }
        }
    }
    let (length, u) = match best {
        Some(closing) => closing,
        None => {
            return Err(GraphError::InternalInvariant {
                what: "cyclic node has no closing edge in the witness BFS",
            });
        }
    };

    // Rebuild the BFS-tree path `root .. u` (parents make it unique), then
    // close the cycle with `root` again: `length` nodes plus the repeat.
    let mut chain: Vec<NodeIx> = Vec::new();
    let mut current = u;
    loop {
        chain.push(current);
        if current == root {
            break;
        }
        current = parents[current as usize];
    }
    chain.reverse(); // now `root .. u`, ending on `u`
    chain.push(root);
    debug_assert_eq!(
        chain.len(),
        usize::try_from(length).map_or(0, |l| l + 1),
        "witness is the documented shortest-through-lowest cycle"
    );
    Ok(Cycle { nodes: chain })
}
