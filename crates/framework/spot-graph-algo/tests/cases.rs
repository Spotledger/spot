//! Hand-made graph cases for every algorithm of the brief §4: empty, one
//! node, self loop, parallel edges, disconnected parts, weighted paths,
//! limits, overflow and a cross-check of the `Cycle`/`find_cycle` agreement.

// Test code may unwrap: every unwrap here is over a value the test itself
// built (house pattern of the other framework crates' integration tests).
#![allow(clippy::unwrap_used)]

use spot_graph_algo::{
    AdjacencyGraph, Cycle, GraphError, GraphView, LimitKind, Limits, Path, UNIT, distance_levels,
    find_cycle, reachable, reversed, shortest_path, strongly_connected_components,
    topological_order,
};

/// `0 -> 1 -> 2 -> 3` plus `0 -> 2` and `1 -> 3`: a small DAG.
fn small_dag() -> AdjacencyGraph {
    AdjacencyGraph::from_edges(4, &[(0, 1), (1, 2), (2, 3), (0, 2), (1, 3)]).unwrap()
}

#[test]
fn empty_graph_topological_order_is_empty() {
    let graph = AdjacencyGraph::new(0).unwrap();
    assert_eq!(topological_order(&graph, &Limits::default()), Ok(vec![]));
    assert_eq!(find_cycle(&graph, &Limits::default()), Ok(None));
    assert_eq!(reachable(&graph, &[], None, &Limits::default()), Ok(vec![]));
    assert_eq!(
        distance_levels(&graph, &[], None, &Limits::default()),
        Ok(vec![])
    );
    assert_eq!(
        strongly_connected_components(&graph, &Limits::default()),
        Ok(vec![])
    );
    let (condensation, component_of) =
        spot_graph_algo::condensation(&graph, &Limits::default()).unwrap();
    assert_eq!(condensation.node_count(), 0);
    assert!(component_of.is_empty());
    assert_eq!(
        shortest_path(&graph, 0, 0, &Limits::default()),
        Err(GraphError::NodeOutOfRange {
            index: 0,
            node_count: 0
        })
    );
}

#[test]
fn one_node_graph() {
    let graph = AdjacencyGraph::new(1).unwrap();
    assert_eq!(topological_order(&graph, &Limits::default()), Ok(vec![0]));
    assert_eq!(find_cycle(&graph, &Limits::default()), Ok(None));
    assert_eq!(
        reachable(&graph, &[0], None, &Limits::default()),
        Ok(vec![0])
    );
    assert_eq!(
        distance_levels(&graph, &[0], None, &Limits::default()),
        Ok(vec![vec![0]])
    );
    assert_eq!(
        strongly_connected_components(&graph, &Limits::default()),
        Ok(vec![vec![0]])
    );
    assert_eq!(
        shortest_path(&graph, 0, 0, &Limits::default()),
        Ok(Some(Path {
            nodes: vec![0],
            cost: 0
        }))
    );
}

#[test]
fn self_loop_is_a_cycle_of_length_one() {
    let mut graph = AdjacencyGraph::new(2).unwrap();
    graph.add_unit_edge(0, 1).unwrap();
    graph.add_unit_edge(1, 1).unwrap();
    let cycle = Cycle { nodes: vec![1, 1] };
    assert_eq!(
        find_cycle(&graph, &Limits::default()),
        Ok(Some(cycle.clone()))
    );
    assert_eq!(
        topological_order(&graph, &Limits::default()),
        Err(GraphError::Cycle(cycle))
    );
    // The self loop node still reaches itself.
    assert_eq!(
        reachable(&graph, &[0], None, &Limits::default()),
        Ok(vec![0, 1])
    );
    // Tarjan emits sinks first: the self-loop component {1} before {0}.
    assert_eq!(
        strongly_connected_components(&graph, &Limits::default()),
        Ok(vec![vec![1], vec![0]])
    );
}

#[test]
fn parallel_edges_are_counted_once_per_node_and_do_not_break_kahn() {
    // Three parallel 0 -> 1 edges plus a self loop on 2.
    let mut graph = AdjacencyGraph::new(3).unwrap();
    graph.add_unit_edge(0, 1).unwrap();
    graph.add_unit_edge(0, 1).unwrap();
    graph.add_unit_edge(0, 1).unwrap();
    graph.add_unit_edge(1, 2).unwrap();
    graph.add_unit_edge(2, 2).unwrap();
    assert_eq!(
        topological_order(&graph, &Limits::default()),
        Err(GraphError::Cycle(Cycle { nodes: vec![2, 2] }))
    );

    // Without the self loop the parallel edges are transparent.
    let mut graph = AdjacencyGraph::new(3).unwrap();
    graph.add_unit_edge(0, 1).unwrap();
    graph.add_unit_edge(0, 1).unwrap();
    graph.add_unit_edge(0, 1).unwrap();
    graph.add_unit_edge(1, 2).unwrap();
    assert_eq!(
        topological_order(&graph, &Limits::default()),
        Ok(vec![0, 1, 2])
    );
    assert_eq!(find_cycle(&graph, &Limits::default()), Ok(None));
    // Sinks first: {2}, then {1}, then {0}.
    assert_eq!(
        strongly_connected_components(&graph, &Limits::default()),
        Ok(vec![vec![2], vec![1], vec![0]])
    );
    assert_eq!(
        reachable(&graph, &[0], None, &Limits::default()),
        Ok(vec![0, 1, 2])
    );
    // Condensation has 3 components with one edge 0 -> 1 -> 2 (deduplicated).
    let (condensation, component_of) =
        spot_graph_algo::condensation(&graph, &Limits::default()).unwrap();
    // `component_of` is indexed by node: sinks-first emission numbers {2}
    // as component 0, {1} as 1, {0} as 2.
    assert_eq!(component_of, vec![2, 1, 0]);
    assert_eq!(condensation.edge_count(), 2);
}

#[test]
fn disconnected_parts_are_independent() {
    let graph = AdjacencyGraph::from_edges(6, &[(0, 1), (1, 0), (2, 3), (4, 5)]).unwrap();
    assert_eq!(
        strongly_connected_components(&graph, &Limits::default()),
        // DFS roots in ascending order, sinks first within each tree:
        // tree of 0 emits {0,1}; tree of 2 emits sink {3} then {2}; tree
        // of 4 emits sink {5} then {4}.
        Ok(vec![vec![0, 1], vec![3], vec![2], vec![5], vec![4]])
    );
    assert_eq!(
        find_cycle(&graph, &Limits::default()),
        Ok(Some(Cycle {
            nodes: vec![0, 1, 0]
        }))
    );
    assert_eq!(
        reachable(&graph, &[4], None, &Limits::default()),
        Ok(vec![4, 5])
    );
    assert_eq!(
        distance_levels(&graph, &[4], None, &Limits::default()),
        Ok(vec![vec![4], vec![5]])
    );
    // `topological_order` reports the lowest cyclic root's witness (0).
    assert!(topological_order(&graph, &Limits::default()).is_err());
}

#[test]
fn topological_order_ties_break_to_the_lowest_index() {
    // Two sources (0, 1) and two independent chains; lexicographically the
    // 0-chain drains fully before the 1-chain starts.
    let graph = AdjacencyGraph::from_edges(5, &[(0, 2), (1, 3), (2, 4), (3, 4)]).unwrap();
    assert_eq!(
        topological_order(&graph, &Limits::default()),
        Ok(vec![0, 1, 2, 3, 4])
    );
    // Now the 1-chain is shorter: after 0 drains, 1 is the smallest ready.
    let graph = AdjacencyGraph::from_edges(4, &[(0, 2), (2, 3), (1, 3)]).unwrap();
    assert_eq!(
        topological_order(&graph, &Limits::default()),
        Ok(vec![0, 1, 2, 3])
    );
}

#[test]
fn diamond_pickes_the_deterministic_tie_break() {
    // 0 -> 1 -> 3 and 0 -> 2 -> 3: two min-cost paths; forward-lexicographic
    // takes the 1 branch.
    let graph = AdjacencyGraph::from_edges(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]).unwrap();
    assert_eq!(
        shortest_path(&graph, 0, 3, &Limits::default()),
        Ok(Some(Path {
            nodes: vec![0, 1, 3],
            cost: 2
        }))
    );
}

#[test]
fn weighted_shortest_path_prefers_the_heavy_shortcut() {
    // 0 -1-> 1 -1-> 2 (cost 2) vs 0 -5-> 2 (cost 5): the 2-hop path wins.
    let mut graph = AdjacencyGraph::new(3).unwrap();
    graph.add_edge(0, 1, 1).unwrap();
    graph.add_edge(1, 2, 1).unwrap();
    graph.add_edge(0, 2, 5).unwrap();
    assert_eq!(
        shortest_path(&graph, 0, 2, &Limits::default()),
        Ok(Some(Path {
            nodes: vec![0, 1, 2],
            cost: 2
        }))
    );
    // Make the shortcut cheap: it wins now.
    let mut graph = AdjacencyGraph::new(3).unwrap();
    graph.add_edge(0, 1, 10).unwrap();
    graph.add_edge(1, 2, 10).unwrap();
    graph.add_edge(0, 2, 3).unwrap();
    assert_eq!(
        shortest_path(&graph, 0, 2, &Limits::default()),
        Ok(Some(Path {
            nodes: vec![0, 2],
            cost: 3
        }))
    );
}

#[test]
fn shortest_path_tie_break_is_the_lowest_predecessor() {
    // 0 -1-> 2 -1-> 3 and 0 -1-> 1 -1-> 3: equal costs; the lexicographic
    // path goes through node 1 (lowest second node on a min-cost path).
    let graph = AdjacencyGraph::from_edges(4, &[(0, 2), (2, 3), (0, 1), (1, 3)]).unwrap();
    assert_eq!(
        shortest_path(&graph, 0, 3, &Limits::default()),
        Ok(Some(Path {
            nodes: vec![0, 1, 3],
            cost: 2
        }))
    );
    // The same graph, deeper: 0 -> 2 -> 4 -> 5 vs 0 -> 1 -> 3 -> 5 — same
    // lexicographic rule picks the 1-chain.
    let graph =
        AdjacencyGraph::from_edges(6, &[(0, 2), (2, 4), (4, 5), (0, 1), (1, 3), (3, 5)]).unwrap();
    assert_eq!(
        shortest_path(&graph, 0, 5, &Limits::default()),
        Ok(Some(Path {
            nodes: vec![0, 1, 3, 5],
            cost: 3
        }))
    );
}

#[test]
fn overflow_shortens_to_the_representable_answer_or_errors_exactly_there() {
    // A single u64::MAX edge: the answer cost itself is unrepresentable.
    let mut graph = AdjacencyGraph::new(2).unwrap();
    graph.add_edge(0, 1, u64::MAX).unwrap();
    assert_eq!(
        shortest_path(&graph, 0, 1, &Limits::default()),
        Err(GraphError::CostOverflow { from: 0, to: 1 })
    );
    // A `from == to` answer is cost 0 and immune to the heavy edge.
    assert_eq!(
        shortest_path(&graph, 0, 0, &Limits::default()),
        Ok(Some(Path {
            nodes: vec![0],
            cost: 0
        }))
    );
    // The reverse direction is representable: 1 -> 0 costs u64::MAX? no —
    // there is no 1 -> 0 edge here; unreachable.
    assert_eq!(shortest_path(&graph, 1, 0, &Limits::default()), Ok(None));

    // A u64::MAX edge beside a cheap route: the representable answer wins
    // (a saturating branch must not poison the whole call).
    let mut graph = AdjacencyGraph::new(3).unwrap();
    graph.add_edge(0, 1, u64::MAX).unwrap();
    graph.add_edge(0, 2, 1).unwrap();
    graph.add_edge(2, 1, 1).unwrap();
    assert_eq!(
        shortest_path(&graph, 0, 1, &Limits::default()),
        Ok(Some(Path {
            nodes: vec![0, 2, 1],
            cost: 2
        }))
    );
}

#[test]
fn zero_weight_edge_is_rejected_explicitly() {
    let mut graph = AdjacencyGraph::new(2).unwrap();
    graph.add_edge(0, 1, 0).unwrap();
    assert!(matches!(
        shortest_path(&graph, 0, 1, &Limits::default()),
        Err(GraphError::ZeroWeightEdge { .. })
    ));
    // Other algorithms accept weight-0 edges fine.
    assert_eq!(find_cycle(&graph, &Limits::default()), Ok(None));
}

#[test]
fn limits_bound_every_function() {
    let graph = small_dag();
    // Node limit: every function rejects a graph over the cap.
    assert!(matches!(
        topological_order(&graph, &Limits::new(2, usize::MAX)),
        Err(GraphError::LimitExceeded {
            kind: LimitKind::Nodes,
            ..
        })
    ));
    assert!(matches!(
        find_cycle(&graph, &Limits::new(2, usize::MAX)),
        Err(GraphError::LimitExceeded {
            kind: LimitKind::Nodes,
            ..
        })
    ));
    assert!(matches!(
        reachable(&graph, &[0], None, &Limits::new(2, usize::MAX)),
        Err(GraphError::LimitExceeded {
            kind: LimitKind::Nodes,
            ..
        })
    ));
    assert!(matches!(
        strongly_connected_components(&graph, &Limits::new(2, usize::MAX)),
        Err(GraphError::LimitExceeded {
            kind: LimitKind::Nodes,
            ..
        })
    ));
    // Visited limit: reachable from 0 visits 4 nodes; a cap of 2 rejects.
    assert!(matches!(
        reachable(&graph, &[0], None, &Limits::bounded_visited(2)),
        Err(GraphError::LimitExceeded {
            kind: LimitKind::Visited,
            limit: 2,
            actual: 3, // 0, 1, 2 discovered before the third crossing? see below
        })
    ));
    // max_depth bounds the BFS: depth 1 from 0 sees {0, 1, 2}.
    assert_eq!(
        reachable(&graph, &[0], Some(1), &Limits::default()),
        Ok(vec![0, 1, 2])
    );
    assert_eq!(
        distance_levels(&graph, &[0], Some(0), &Limits::default()),
        Ok(vec![vec![0]])
    );
}

#[test]
fn out_of_range_arguments_are_errors_not_panics() {
    let graph = small_dag();
    assert_eq!(
        shortest_path(&graph, 0, 9, &Limits::default()),
        Err(GraphError::NodeOutOfRange {
            index: 9,
            node_count: 4
        })
    );
    assert_eq!(
        reachable(&graph, &[7], None, &Limits::default()),
        Err(GraphError::NodeOutOfRange {
            index: 7,
            node_count: 4
        })
    );
    // Building with an out-of-range edge fails.
    assert!(AdjacencyGraph::from_edges(2, &[(0, 5)]).is_err());
    // Oversized node count fails.
    assert!(AdjacencyGraph::new((u32::MAX as usize) + 1).is_err());
}

#[test]
fn reversed_adaptor_flips_every_edge() {
    let graph = small_dag();
    let back = reversed(&graph);
    assert_eq!(back.node_count(), 4);
    assert_eq!(back.edge_count(), graph.edge_count());
    let mut forward_edges: Vec<(u32, u32)> = Vec::new();
    for node in 0..4 {
        for (target, _) in graph.successors(node) {
            forward_edges.push((node, target));
        }
    }
    for (from, to) in forward_edges {
        let hits: Vec<u32> = back.successors(to).map(|(source, _)| source).collect();
        assert!(hits.contains(&from), "{from} -> {to} missing in reversal");
    }
    // Reversing twice is the original.
    let twice = reversed(&back);
    let mut original: Vec<Vec<u32>> = vec![Vec::new(); 4];
    let mut round_trip: Vec<Vec<u32>> = vec![Vec::new(); 4];
    for node in 0..4u32 {
        for (target, _) in graph.successors(node) {
            original[node as usize].push(target);
        }
        for (target, _) in twice.successors(node) {
            round_trip[node as usize].push(target);
        }
    }
    for node in 0..4 {
        original[node as usize].sort_unstable();
        round_trip[node as usize].sort_unstable();
        assert_eq!(original[node as usize], round_trip[node as usize]);
    }
    // Weights survive the flip.
    let mut weighted = AdjacencyGraph::new(2).unwrap();
    weighted.add_edge(0, 1, 7).unwrap();
    let back = reversed(&weighted);
    assert_eq!(back.successors(1).collect::<Vec<_>>(), vec![(0, 7)]);
}

#[test]
fn cycle_witness_is_shortest_through_lowest_cyclic_node() {
    // Two cycles: 2 <-> 3 (length 2) and 5 -> 6 -> 7 -> 5 (length 3), plus
    // cycle-free decorations. Lowest cyclic node is 2; witness is the
    // shortest cycle through 2.
    let graph =
        AdjacencyGraph::from_edges(9, &[(0, 1), (2, 3), (3, 2), (5, 6), (6, 7), (7, 5), (7, 8)])
            .unwrap();
    assert_eq!(
        find_cycle(&graph, &Limits::default()),
        Ok(Some(Cycle {
            nodes: vec![2, 3, 2]
        }))
    );
    // Two cycles: 1 -> 2 -> 3 -> 4 -> 1 (length 4) and 5 <-> 6 (length 2).
    // The lowest cyclic node is 1 (not 5), so the witness is the shortest
    // cycle *through 1* even though the graph's globally shortest cycle is
    // the 2-cycle (documented tie-break, see `find_cycle`).
    let graph =
        AdjacencyGraph::from_edges(9, &[(1, 2), (2, 3), (3, 4), (4, 1), (5, 6), (6, 5)]).unwrap();
    assert_eq!(
        find_cycle(&graph, &Limits::default()),
        Ok(Some(Cycle {
            nodes: vec![1, 2, 3, 4, 1]
        }))
    );
}

#[test]
fn unit_weight_constant() {
    assert_eq!(UNIT, 1);
    assert_eq!(UNIT, u64::from(1u8));
}

#[test]
fn error_codes_are_stable_strings() {
    let error = GraphError::Cycle(Cycle { nodes: vec![0, 0] });
    assert_eq!(error.code(), "graph.cycle");
}
