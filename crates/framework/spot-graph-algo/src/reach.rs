//! Reachability ([`reachable`]) and BFS depth groups ([`distance_levels`]).

use crate::error::GraphError;
use crate::limits::{Limits, ensure_node_count, ensure_visited};
use crate::traversal::ensure_node_in_range;
use crate::view::{GraphView, NodeIx};

/// Every node reachable from one of the `from` nodes (inclusive), sorted
/// ascending.
///
/// Breadth-first with an ascending work-list, so the expansion order (and
/// with it any internal state) depends only on the graph's edge multiset —
/// the *result* is `sorted`, hence identical on every run and platform and
/// independent of edge insertion order.
///
/// `max_depth` bounds the BFS depth: edges off nodes at depth `max_depth`
/// are not followed, so only nodes within `max_depth` hops of a `from` node
/// (that pass is inclusive) appear.
///
/// # Errors
///
/// * [`GraphError::LimitExceeded`] (kind `Nodes`): `graph.node_count()`
///   exceeds `limits.max_nodes`;
/// * [`GraphError::NodeOutOfRange`]: a `from` node or an edge endpoint is
///   not a dense node of the graph;
/// * [`GraphError::LimitExceeded`] (kind `Visited`): the search expands more
///   than `limits.max_visited` distinct nodes (the `from` set counts).
#[must_use = "the caller wants the reachable set"]
pub fn reachable<G: GraphView>(
    graph: &G,
    from: &[NodeIx],
    max_depth: Option<u32>,
    limits: &Limits,
) -> Result<Vec<NodeIx>, GraphError> {
    distance_levels_inner(graph, from, max_depth, limits).map(|levels| {
        // Levels are per-depth sorted but not globally sorted (a depth-2 node
        // may have a lower index than a depth-1 node), so sort the union.
        let mut nodes: Vec<NodeIx> = levels.into_iter().flatten().collect();
        nodes.sort_unstable();
        nodes
    })
}

/// Nodes grouped by BFS depth: `result[d]` holds every node at distance `d`
/// from the nearest `from` node, each group sorted ascending, groups in
/// depth order starting at `0` (the `from` set itself).
///
/// Groups appear only while they are non-empty; `result.len()` - 1 is the
/// eccentricity of the `from` set. Unreached nodes are absent.
///
/// Determinism: groups are sorted sets, so the output depends only on the
/// edge multiset, never on the insertion order.
///
/// # Errors
///
/// Same contract as [`reachable`], including `max_depth`: depth beyond the
/// bound produces no groups.
pub fn distance_levels<G: GraphView>(
    graph: &G,
    from: &[NodeIx],
    max_depth: Option<u32>,
    limits: &Limits,
) -> Result<Vec<Vec<NodeIx>>, GraphError> {
    distance_levels_inner(graph, from, max_depth, limits)
}

fn distance_levels_inner<G: GraphView>(
    graph: &G,
    from: &[NodeIx],
    max_depth: Option<u32>,
    limits: &Limits,
) -> Result<Vec<Vec<NodeIx>>, GraphError> {
    let node_count = ensure_node_count(graph.node_count(), limits)?;
    for &start in from {
        ensure_node_in_range(graph, start)?;
    }

    let mut scratch: Vec<(NodeIx, u64)> = Vec::new();
    let mut visited = vec![false; node_count];
    let mut frontier: Vec<NodeIx> = Vec::new();
    for &start in from {
        if !visited[start as usize] {
            visited[start as usize] = true;
            frontier.push(start);
        }
    }
    let mut expanded = frontier.len();
    ensure_visited(expanded, limits)?;

    let mut levels: Vec<Vec<NodeIx>> = Vec::new();
    let mut depth: u32 = 0;
    while !frontier.is_empty() && max_depth.is_none_or(|bound| depth <= bound) {
        levels.push({
            let mut group = frontier.clone();
            group.sort_unstable();
            group
        });
        let bound_crossed = max_depth.is_some_and(|bound| depth >= bound);
        if bound_crossed {
            break;
        }
        let mut next: Vec<NodeIx> = Vec::new();
        for &node in &frontier {
            scratch.clear();
            scratch.extend(graph.successors(node));
            scratch.sort_unstable();
            for &(target, _) in &scratch {
                if !visited[target as usize] {
                    visited[target as usize] = true;
                    next.push(target);
                }
            }
        }
        expanded += next.len();
        ensure_visited(expanded, limits)?;
        frontier = next;
        depth += 1;
    }
    Ok(levels)
}
