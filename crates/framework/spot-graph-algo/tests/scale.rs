//! Brief §4 scale: a 1,000,000-node chain and a 1,000,000-node star complete
//! within the default test stack with no recursion (every traversal here is
//! iterative).

// Test code may unwrap: every unwrap here is over a value the test itself
// built (house pattern of the other framework crates' integration tests).
#![allow(clippy::unwrap_used)]

use spot_graph_algo::{
    AdjacencyGraph, Limits, distance_levels, find_cycle, reachable, shortest_path,
    strongly_connected_components, topological_order,
};

const MILLION: usize = 1_000_000;

/// `0 -> 1 -> ... -> N-1`, built without unwrap panic paths (the builder is
/// infallible by construction; `expect` in tests is allowed by the lint).
fn chain(node_count: usize) -> AdjacencyGraph {
    let edges: Vec<(u32, u32)> = (0..node_count - 1)
        .map(|i| (i as u32, i as u32 + 1))
        .collect();
    AdjacencyGraph::from_edges(node_count, &edges).unwrap()
}

/// A star: the centre 0 feeds every other node (out-degree N-1).
fn star(node_count: usize) -> AdjacencyGraph {
    let edges: Vec<(u32, u32)> = (1..node_count).map(|i| (0, i as u32)).collect();
    AdjacencyGraph::from_edges(node_count, &edges).unwrap()
}

#[test]
fn million_node_chain_completes_without_recursion() {
    let graph = chain(MILLION);
    let limits = Limits::default();

    // Topological order: the chain in index order.
    let order = topological_order(&graph, &limits).unwrap();
    assert_eq!(order.len(), MILLION);
    assert_eq!(order[MILLION - 1], MILLION as u32 - 1);

    // Cycle detection: acyclic, and the answer arrives instantly (the
    // Kahn pass is the whole work).
    assert_eq!(find_cycle(&graph, &limits).unwrap(), None);

    // SCCs: one per node; sinks-first => [N-1], [N-2], ..., [0].
    let components = strongly_connected_components(&graph, &limits).unwrap();
    assert_eq!(components.len(), MILLION);
    assert_eq!(components[0], vec![MILLION as u32 - 1]);
    assert_eq!(components[MILLION - 1], vec![0]);

    // Reachability from the head: everything, one level group.
    assert_eq!(
        reachable(&graph, &[0], None, &limits).unwrap().len(),
        MILLION
    );
    let levels = distance_levels(&graph, &[0], None, &limits).unwrap();
    assert_eq!(levels.len(), MILLION); // one depth per chain node
    assert_eq!(levels[0], vec![0]);
    assert_eq!(levels[MILLION - 1], vec![MILLION as u32 - 1]);

    // Shortest path: the full chain.
    let path = shortest_path(&graph, 0, MILLION as u32 - 1, &limits)
        .unwrap()
        .unwrap();
    assert_eq!(path.cost, MILLION as u64 - 1);
    assert_eq!(path.nodes.len(), MILLION);
    assert_eq!(path.nodes[0], 0);
    assert_eq!(path.nodes[MILLION - 1], MILLION as u32 - 1);
}

#[test]
fn million_node_star_completes_without_recursion() {
    let graph = star(MILLION);
    let limits = Limits::default();

    let order = topological_order(&graph, &limits).unwrap();
    assert_eq!(order[0], 0);
    assert_eq!(order.len(), MILLION);

    assert_eq!(find_cycle(&graph, &limits).unwrap(), None);

    // SCCs: sinks first — leaves ascending, then the centre.
    let components = strongly_connected_components(&graph, &limits).unwrap();
    assert_eq!(components.len(), MILLION);
    assert_eq!(components[0], vec![1]);
    assert_eq!(components[MILLION - 1], vec![0]);

    // Everything is exactly one hop away.
    let levels = distance_levels(&graph, &[0], None, &limits).unwrap();
    assert_eq!(levels.len(), 2);
    assert_eq!(levels[0], vec![0]);
    assert_eq!(levels[1].len(), MILLION - 1);

    // Shortest path to the last leaf.
    let path = shortest_path(&graph, 0, MILLION as u32 - 1, &limits)
        .unwrap()
        .unwrap();
    assert_eq!(path.cost, 1);
    assert_eq!(path.nodes, vec![0, MILLION as u32 - 1]);
}

#[test]
fn million_node_chain_under_an_explicit_visited_bound_stops_cleanly() {
    let graph = chain(MILLION);
    // A bound well under the chain length: a clean LimitExceeded, no crash
    // and no runaway.
    let bounded = Limits::bounded_visited(1_000);
    let error = reachable(&graph, &[0], None, &bounded).unwrap_err();
    assert!(matches!(
        error,
        spot_graph_algo::GraphError::LimitExceeded {
            kind: spot_graph_algo::LimitKind::Visited,
            ..
        }
    ));
}
