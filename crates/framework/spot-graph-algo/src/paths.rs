//! Shortest path ([`shortest_path`], [`Path`]): Dijkstra with `u64` costs,
//! a unit-weight BFS fast path, and cost overflow as an explicit error.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use crate::error::GraphError;
use crate::limits::{Limits, ensure_node_count, ensure_visited};
use crate::traversal::{ensure_node_in_range, ensure_path_endpoints};
use crate::view::{GraphView, NodeIx, UNIT, Weight};

/// A minimum-cost path from `from` to `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    /// The node sequence, starting with `from` and ending with `to`. A
    /// `from == to` query is a one-node path (cost `0`) even when `from`
    /// sits on a cycle.
    pub nodes: Vec<NodeIx>,
    /// The sum of the edge weights along `nodes`; `0` for a one-node path.
    pub cost: Weight,
}

/// The minimum-cost path from `from` to `to`.
///
/// The *cost* is the classic Dijkstra minimum over `u64` edge weights and is
/// always the true minimum. The *node sequence* is pinned by a deterministic
/// tie-break, so the same graph yields the same path on every run and
/// platform, independent of edge insertion order — and of the `successors`
/// iteration order:
///
/// **Tie-break (forward-lexicographic).** Among all minimum-cost paths the
/// returned one is the *lexicographically smallest* read from `from`: its
/// second node is the lowest-index node that lies on any minimum-cost path
/// after `from`, its third node the lowest index given the second, and so
/// on. This falls out of a two-sided distance computation — `dist_f`
/// (distances *from* `from`) and `d2t` (distances *to* `to`): node `v` lies
/// on a minimum-cost path exactly when `dist_f[v] + d2t[v] == cost`, and the
/// next node after `v` is the lowest-index successor `w` with
/// `dist_f[v] + w(v, w) + d2t[w] == cost`. The "lowest predecessor index"
/// reconstruction of the brief is the backward restatement of the same rule:
/// each step could equivalently pick the lowest-index predecessor of the
/// current node on a minimum-cost path.
///
/// **Overflow.** A `u64` cost never silently wraps. Distances are capped at
/// the sentinel `u64::MAX - 1`: an edge weight of `u64::MAX` (or a partial
/// sum reaching it) means "representable cost exhausted", and a shortest
/// distance that lands at or beyond the sentinel is
/// [`GraphError::CostOverflow`] when it is the *answer* (`to` itself) — a
/// representable answer (cost < sentinel) never errors, even when other
/// branches of the graph would overflow mid-relaxation. Zero-weight edges
/// are rejected up front ([`GraphError::ZeroWeightEdge`]): the extraction
/// terminates because each step strictly increases `dist_f` for weights
/// `>= 1`, and `0` breaks that guarantee.
///
/// Performance: when every edge weight is [`UNIT`] the two single-source
/// runs are plain BFS (first discovery is final, each edge adds exactly 1);
/// otherwise both runs are lazy-deletion Dijkstra on a
/// `Reverse((distance, index))` binary heap, so nodes settle in ascending
/// `(distance, index)` order and never reopen. Both runs explore the whole
/// graph (the extraction needs *final* distances in both directions). Runs
/// and extraction are iterative — every queue, stack and heap lives on the
/// heap, so a 1,000,000-node chain cannot overflow the stack.
///
/// # Errors
///
/// * [`GraphError::LimitExceeded`] (kind `Nodes`): `graph.node_count()`
///   exceeds `limits.max_nodes`;
/// * [`GraphError::NodeOutOfRange`]: `from`, `to`, or an edge endpoint is
///   not a dense node of the graph;
/// * [`GraphError::LimitExceeded`] (kind `Visited`): the two runs discover
///   more than `limits.max_visited` *distinct* nodes combined;
/// * [`GraphError::ZeroWeightEdge`]: some edge has weight `0`;
/// * [`GraphError::CostOverflow`]: the shortest distance `from -> to`
///   reaches or passes the sentinel `u64::MAX - 1` — see **Overflow**;
/// * [`GraphError::InternalInvariant`]: defensive, unreachable for a
///   well-formed `GraphView`;
/// * `Ok(None)`: `to` is not reachable from `from`.
#[must_use = "the caller wants the path"]
pub fn shortest_path<G: GraphView>(
    graph: &G,
    from: NodeIx,
    to: NodeIx,
    limits: &Limits,
) -> Result<Option<Path>, GraphError> {
    let node_count = ensure_node_count(graph.node_count(), limits)?;
    ensure_path_endpoints(graph, from, to)?;
    if from == to {
        return Ok(Some(Path {
            nodes: vec![from],
            cost: 0,
        }));
    }

    /// Distances at or above the sentinel mean "unreached / not usable".
    const SENTINEL: Weight = u64::MAX - 1;
    let mut scratch: Vec<(NodeIx, Weight)> = Vec::new();

    // ---- Scan pass: endpoints validated, zero weights rejected ----
    let mut all_unit = true;
    for source in 0..node_count {
        scratch.clear();
        scratch.extend(graph.successors(source as NodeIx));
        for &(target, weight) in &scratch {
            ensure_node_in_range(graph, target)?;
            if weight == 0 {
                return Err(GraphError::ZeroWeightEdge {
                    from: source as NodeIx,
                    to: target,
                });
            }
            all_unit &= weight == UNIT;
        }
    }

    // ---- Run 1: distances from `from` ----
    let mut dist_f: Vec<Weight> = vec![SENTINEL; node_count];
    let mut visited = vec![false; node_count];
    let mut expanded = 0usize;
    if all_unit {
        bfs_distances(
            graph,
            from,
            &mut dist_f,
            SENTINEL,
            &mut visited,
            &mut expanded,
            limits,
        )?;
    } else {
        dijkstra_distances(
            graph,
            from,
            &mut dist_f,
            SENTINEL,
            &mut visited,
            &mut expanded,
            limits,
        )?;
    }
    if !visited[to as usize] {
        return Ok(None);
    }
    if dist_f[to as usize] >= SENTINEL {
        return Err(GraphError::CostOverflow { from, to });
    }
    let cost = dist_f[to as usize];

    // ---- Run 2: distances *to* `to` over the reversed graph ----
    let reversed = crate::view::reversed(graph);
    let mut d2t: Vec<Weight> = vec![SENTINEL; node_count];
    let mut visited_back = vec![false; node_count];
    let mut expanded_back = 0usize;
    if all_unit {
        bfs_distances(
            &reversed,
            to,
            &mut d2t,
            SENTINEL,
            &mut visited_back,
            &mut expanded_back,
            limits,
        )?;
    } else {
        dijkstra_distances(
            &reversed,
            to,
            &mut d2t,
            SENTINEL,
            &mut visited_back,
            &mut expanded_back,
            limits,
        )?;
    }
    ensure_visited(expanded.saturating_add(expanded_back), limits)?;

    // ---- Extract the forward-lexicographic minimum-cost path ----
    // Invariants: `d2t[v] == 0` iff `v == to` (weights >= 1), the path so
    // far costs exactly `dist_f[current]`, and every step strictly
    // increases `dist_f` — so the loop visits at most `node_count` distinct
    // nodes and terminates.
    let mut nodes = vec![from];
    let mut current = from;
    for _step in 0..node_count {
        if current == to {
            return Ok(Some(Path { nodes, cost }));
        }
        scratch.clear();
        scratch.extend(graph.successors(current));
        scratch.sort_unstable();
        let mut next: Option<NodeIx> = None;
        for &(target, weight) in &scratch {
            if d2t[target as usize] >= SENTINEL {
                continue;
            }
            if dist_f[current as usize]
                .checked_add(weight)
                .and_then(|via| via.checked_add(d2t[target as usize]))
                == Some(cost)
            {
                next = Some(target);
                break;
            }
        }
        current = match next {
            Some(next) => {
                nodes.push(next);
                next
            }
            None => {
                return Err(GraphError::InternalInvariant {
                    what: "minimum-cost path extraction found no on-path successor",
                });
            }
        };
    }
    Err(GraphError::InternalInvariant {
        what: "minimum-cost path extraction exceeded the node budget",
    })
}

/// Single-source distances with plain BFS (all weights unit).
///
/// Fills `dist` with shortest edge counts (`sentinel` = unreachable) and
/// marks every discovered node in `visited`; `expanded` accumulates the
/// distinct discoveries across runs for [`Limits::max_visited`]. The
/// relaxation guards `next < sentinel`, so unit graphs beyond the sentinel
/// saturate exactly like the Dijkstra path.
fn bfs_distances<G: GraphView>(
    graph: &G,
    source: NodeIx,
    dist: &mut [Weight],
    sentinel: Weight,
    visited: &mut [bool],
    expanded: &mut usize,
    limits: &Limits,
) -> Result<(), GraphError> {
    let mut scratch: Vec<(NodeIx, Weight)> = Vec::new();
    dist[source as usize] = 0;
    if !visited[source as usize] {
        visited[source as usize] = true;
        *expanded += 1;
    }
    ensure_visited(*expanded, limits)?;
    let mut queue: VecDeque<NodeIx> = VecDeque::new();
    queue.push_back(source);
    while let Some(node) = queue.pop_front() {
        scratch.clear();
        scratch.extend(graph.successors(node));
        // Ascending scan keeps the discovery (parent) assignment
        // deterministic regardless of the graph's own iteration order.
        scratch.sort_unstable();
        for &(target, weight) in &scratch {
            // Same first-touch/improve split as `dijkstra_distances`: unit
            // graphs can also saturate (beyond `sentinel` hops).
            let next = dist[node as usize].saturating_add(weight).min(sentinel);
            if dist[target as usize] == sentinel && next == sentinel {
                if !visited[target as usize] {
                    visited[target as usize] = true;
                    *expanded += 1;
                }
                ensure_visited(*expanded, limits)?;
            } else if next < dist[target as usize] {
                dist[target as usize] = next;
                if !visited[target as usize] {
                    visited[target as usize] = true;
                    *expanded += 1;
                }
                ensure_visited(*expanded, limits)?;
                queue.push_back(target);
            }
        }
    }
    Ok(())
}

/// Single-source distances with lazy-deletion Dijkstra.
///
/// Settles nodes in ascending `(distance, index)` order, at most once.
/// The relaxation saturates at `sentinel` (never wraps); distances at the
/// sentinel are "not usable" and simply settle last.
fn dijkstra_distances<G: GraphView>(
    graph: &G,
    source: NodeIx,
    dist: &mut [Weight],
    sentinel: Weight,
    visited: &mut [bool],
    expanded: &mut usize,
    limits: &Limits,
) -> Result<(), GraphError> {
    let mut scratch: Vec<(NodeIx, Weight)> = Vec::new();
    let mut settled = vec![false; dist.len()];
    dist[source as usize] = 0;
    if !visited[source as usize] {
        visited[source as usize] = true;
        *expanded += 1;
    }
    ensure_visited(*expanded, limits)?;
    let mut heap: BinaryHeap<Reverse<(Weight, NodeIx)>> = BinaryHeap::new();
    heap.push(Reverse((0, source)));
    while let Some(Reverse((d, node))) = heap.pop() {
        if settled[node as usize] || d > dist[node as usize] {
            continue;
        }
        settled[node as usize] = true;
        if !visited[node as usize] {
            visited[node as usize] = true;
            *expanded += 1;
        }
        ensure_visited(*expanded, limits)?;
        scratch.clear();
        scratch.extend(graph.successors(node));
        scratch.sort_unstable();
        for &(target, weight) in &scratch {
            // Saturating relaxation: distances at/over `sentinel` mean
            // "cost not representable" and never wrap. A *first touch* by
            // an overflowing edge still marks the node visited (so the
            // caller can tell overflow from unreachability) but never
            // enqueues it — nothing beyond the sentinel can improve.
            let candidate = d.saturating_add(weight).min(sentinel);
            if dist[target as usize] == sentinel && candidate == sentinel {
                if !visited[target as usize] {
                    visited[target as usize] = true;
                    *expanded += 1;
                }
                ensure_visited(*expanded, limits)?;
            } else if candidate < dist[target as usize] {
                dist[target as usize] = candidate;
                if !visited[target as usize] {
                    visited[target as usize] = true;
                    *expanded += 1;
                }
                ensure_visited(*expanded, limits)?;
                heap.push(Reverse((candidate, target)));
            }
        }
    }
    Ok(())
}
