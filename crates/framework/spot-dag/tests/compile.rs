//! Compiler tests (brief §3 "Tests"): `ready`/`is_finished` over the
//! sanctioned shapes (linear chain; `All` join; `Any` join; conditional split
//! with a never-taken branch; started-but-not-completed), deterministic
//! `topological_order`, `depth`, and the sorted successor/predecessor
//! indexes.

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used)]

use spot_dag::{CompiledDag, DagDefinition, Edge, JoinRule, Limits, Node, NodeId, Progress};

/// A node with default join (`All`) and empty config.
fn n(id: &str) -> Node {
    Node {
        id: NodeId::new(id).unwrap(),
        kind: "step".to_owned(),
        join: JoinRule::All,
        config: serde_json::Value::Null,
    }
}

/// A node with `Any` join.
fn any_n(id: &str) -> Node {
    Node {
        id: NodeId::new(id).unwrap(),
        kind: "step".to_owned(),
        join: JoinRule::Any,
        config: serde_json::Value::Null,
    }
}

/// An unconditional edge.
fn e(from: &str, to: &str) -> Edge {
    Edge {
        from: NodeId::new(from).unwrap(),
        to: NodeId::new(to).unwrap(),
        condition: None,
    }
}

/// An edge with a condition.
fn ec(from: &str, to: &str, condition: &str) -> Edge {
    Edge {
        from: NodeId::new(from).unwrap(),
        to: NodeId::new(to).unwrap(),
        condition: Some(condition.to_owned()),
    }
}

/// Shorthand for a [`NodeId`].
fn id(raw: &str) -> NodeId {
    NodeId::new(raw).unwrap()
}

/// Ids as string slices, for assertions.
fn names(ids: &[NodeId]) -> Vec<&str> {
    ids.iter().map(NodeId::as_str).collect()
}

/// Diamond `a -> (b, c) -> d`, join node `d` with the given rule.
fn diamond(join_d: JoinRule) -> DagDefinition {
    DagDefinition::new(
        "t",
        1,
        vec![
            n("a"),
            n("b"),
            n("c"),
            Node {
                join: join_d,
                ..n("d")
            },
        ],
        vec![e("a", "b"), e("a", "c"), e("b", "d"), e("c", "d")],
    )
}

/// Linear chain `a -> b -> c`.
fn chain() -> DagDefinition {
    DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("c")],
        vec![e("a", "b"), e("b", "c")],
    )
}

/// Conditional split `a --approved--> c`, `a --rejected--> r`.
fn conditional_split() -> DagDefinition {
    DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("c"), n("r")],
        vec![ec("a", "c", "approved"), ec("a", "r", "rejected")],
    )
}

#[test]
fn ready_on_fresh_progress_is_exactly_the_entries() {
    let dag = CompiledDag::compile(&diamond(JoinRule::All), &Limits::new()).unwrap();
    assert_eq!(dag.ready(&Progress::default()), vec![id("a")]);
}

#[test]
fn ready_walks_a_linear_chain_one_step_at_a_time() {
    let dag = CompiledDag::compile(&chain(), &Limits::new()).unwrap();

    // Nothing taken: only `a` may start.
    let fresh = Progress::default();
    assert_eq!(dag.ready(&fresh), vec![id("a")]);

    // `a` completed, edge a->b taken: `b` is next.
    let state = Progress {
        completed: [id("a")].into_iter().collect(),
        taken: [(id("a"), id("b"))].into_iter().collect(),
        ..Progress::default()
    };
    assert_eq!(dag.ready(&state), vec![id("b")]);

    // `b` completed too: `c` is next.
    let state = Progress {
        completed: [id("a"), id("b")].into_iter().collect(),
        taken: [(id("a"), id("b")), (id("b"), id("c"))]
            .into_iter()
            .collect(),
        ..Progress::default()
    };
    assert_eq!(dag.ready(&state), vec![id("c")]);

    // Everything completed: nothing is ready.
    let state = Progress {
        completed: [id("a"), id("b"), id("c")].into_iter().collect(),
        ..Progress::default()
    };
    assert!(dag.ready(&state).is_empty());
}

#[test]
fn ready_all_join_waits_for_both_branches() {
    let dag = CompiledDag::compile(&diamond(JoinRule::All), &Limits::new()).unwrap();

    // `a` done, only the left edge taken: `b` may run, `c` may not, and the
    // join node `d` waits for BOTH branches.
    let state = Progress {
        completed: [id("a")].into_iter().collect(),
        taken: [(id("a"), id("b"))].into_iter().collect(),
        ..Progress::default()
    };
    assert_eq!(dag.ready(&state), vec![id("b")]);

    // Both branch edges taken (b->d and c->d reach INTO the join): `d` may
    // start. The branch edges a->b/a->c are also recorded as taken; `ready`
    // never looks at `completed`.
    let state = Progress {
        completed: [id("a"), id("b"), id("c")].into_iter().collect(),
        taken: [
            (id("a"), id("b")),
            (id("a"), id("c")),
            (id("b"), id("d")),
            (id("c"), id("d")),
        ]
        .into_iter()
        .collect(),
        ..Progress::default()
    };
    assert_eq!(dag.ready(&state), vec![id("d")]);
}

#[test]
fn ready_any_join_first_branch_is_enough() {
    let dag = CompiledDag::compile(&diamond(JoinRule::Any), &Limits::new()).unwrap();

    // Only the left branch started: `b` is ready (its edge taken); the
    // `Any` join node `d` is not yet — none of ITS incoming edges is in
    // `taken`, and `ready` reads the edges into the node, not the flow.
    let state = Progress {
        completed: [id("a")].into_iter().collect(),
        taken: [(id("a"), id("b"))].into_iter().collect(),
        ..Progress::default()
    };
    assert_eq!(dag.ready(&state), vec![id("b")]);

    // The left branch finished and its edge INTO `d` was taken: one taken
    // incoming edge satisfies the `Any` rule, so `d` is ready even though
    // the right branch never ran. `All` would not be.
    let state = Progress {
        completed: [id("a"), id("b")].into_iter().collect(),
        taken: [(id("a"), id("b")), (id("b"), id("d"))]
            .into_iter()
            .collect(),
        ..Progress::default()
    };
    assert_eq!(dag.ready(&state), vec![id("d")]);

    // After `d` completes it is not offered again; with every edge taken the
    // unfinished right-branch node `c` is ready.
    let state = Progress {
        completed: [id("a"), id("b"), id("d")].into_iter().collect(),
        taken: [
            (id("a"), id("b")),
            (id("a"), id("c")),
            (id("b"), id("d")),
            (id("c"), id("d")),
        ]
        .into_iter()
        .collect(),
        ..Progress::default()
    };
    assert_eq!(dag.ready(&state), vec![id("c")]);
}

#[test]
fn ready_conditional_split_only_taken_branch_progresses() {
    let dag = CompiledDag::compile(&conditional_split(), &Limits::new()).unwrap();

    // `a` completed, the engine evaluated its outgoing conditions and took
    // only the `approved` edge: `c` becomes ready, `r` never does — a branch
    // not taken makes its target never ready (brief §3).
    let state = Progress {
        completed: [id("a")].into_iter().collect(),
        taken: [(id("a"), id("c"))].into_iter().collect(),
        ..Progress::default()
    };
    assert_eq!(dag.ready(&state), vec![id("c")]);
}

#[test]
fn started_node_is_not_offered_again() {
    let dag = CompiledDag::compile(&chain(), &Limits::new()).unwrap();
    let state = Progress {
        started: [id("b")].into_iter().collect(),
        taken: [(id("a"), id("b"))].into_iter().collect(),
        ..Progress::default()
    };
    // `b` started: not re-offered. Entry `a` is (still) ready; `c` blocked
    // behind `b`.
    assert_eq!(dag.ready(&state), vec![id("a")]);
}

#[test]
fn linear_chain_finishes_step_by_step() {
    let dag = CompiledDag::compile(&chain(), &Limits::new()).unwrap();

    // Fresh run: no edge is taken, so the walk stays on the entries and owes
    // nothing — the literal rule reads the engine's `taken` state, and an
    // engine that took nothing has nothing owed on the taken flow.
    assert!(dag.is_finished(&Progress::default()));

    // Work in flight: `b` started-but-not-completed on a taken edge — rule 1
    // blocks even though the terminal is not yet reached.
    let in_flight = Progress {
        started: [id("b")].into_iter().collect(),
        taken: [(id("a"), id("b"))].into_iter().collect(),
        ..Progress::default()
    };
    assert!(!dag.is_finished(&in_flight));

    // The flow reached the terminal `c` through taken edges but the engine
    // has not completed it: not finished — the taken frontier is unworked.
    let at_terminal = Progress {
        taken: [(id("a"), id("b")), (id("b"), id("c"))]
            .into_iter()
            .collect(),
        ..Progress::default()
    };
    assert!(!dag.is_finished(&at_terminal));

    // Whole chain completed with every edge taken: finished.
    let state = Progress {
        completed: [id("a"), id("b"), id("c")].into_iter().collect(),
        taken: [(id("a"), id("b")), (id("b"), id("c"))]
            .into_iter()
            .collect(),
        ..Progress::default()
    };
    assert!(dag.is_finished(&state));
}

#[test]
fn all_join_diamond_finishes_only_after_both_branches() {
    let dag = CompiledDag::compile(&diamond(JoinRule::All), &Limits::new()).unwrap();

    // Only the left branch ran (engine took a->b and b->d): the `All` join
    // node `d` is reachable through the taken edge b->d but not completed,
    // so the run is not finished — taking only one branch into an `All`
    // join cannot finish the diamond.
    let state = Progress {
        completed: [id("a"), id("b")].into_iter().collect(),
        taken: [(id("a"), id("b")), (id("b"), id("d"))]
            .into_iter()
            .collect(),
        ..Progress::default()
    };
    assert!(!dag.is_finished(&state));

    // Everything completed with every edge taken: finished.
    let state = Progress {
        completed: [id("a"), id("b"), id("c"), id("d")].into_iter().collect(),
        taken: [
            (id("a"), id("b")),
            (id("a"), id("c")),
            (id("b"), id("d")),
            (id("c"), id("d")),
        ]
        .into_iter()
        .collect(),
        ..Progress::default()
    };
    assert!(dag.is_finished(&state));
}

#[test]
fn any_join_diamond_finishes_after_one_branch() {
    let dag = CompiledDag::compile(&diamond(JoinRule::Any), &Limits::new()).unwrap();

    // Left branch completed and the join node ran through it: the right
    // branch (c) was never taken, so it must NOT block finishing.
    let state = Progress {
        completed: [id("a"), id("b"), id("d")].into_iter().collect(),
        taken: [(id("a"), id("b")), (id("b"), id("d"))]
            .into_iter()
            .collect(),
        ..Progress::default()
    };
    assert!(dag.is_finished(&state));
}

#[test]
fn conditional_split_with_never_taken_branch_still_finishes() {
    let dag = CompiledDag::compile(&conditional_split(), &Limits::new()).unwrap();

    // The engine took only `approved`; `c` completed. The `rejected` branch
    // (r) was never taken, so it is not reachable through taken edges and
    // does not block finishing (brief §3: the exact rule).
    let state = Progress {
        completed: [id("a"), id("c")].into_iter().collect(),
        taken: [(id("a"), id("c"))].into_iter().collect(),
        ..Progress::default()
    };
    assert!(dag.is_finished(&state));
}

#[test]
fn untaken_terminal_edge_after_completed_entry_finishes() {
    // Single conditional edge a --ok--> t, entry `a` completed but the
    // engine has not evaluated the condition yet (nothing taken): per the
    // documented rule, `t` is not reachable through taken edges, so it does
    // not block; `is_finished` reflects the engine's own taken-set state.
    let def = DagDefinition::new("t", 1, vec![n("a"), n("t")], vec![ec("a", "t", "ok")]);
    let dag = CompiledDag::compile(&def, &Limits::new()).unwrap();
    let state = Progress {
        completed: [id("a")].into_iter().collect(),
        ..Progress::default()
    };
    assert!(dag.is_finished(&state));
}

#[test]
fn started_but_not_completed_blocks_finishing() {
    let dag = CompiledDag::compile(&chain(), &Limits::new()).unwrap();

    // `b` started, not completed: work in flight always blocks.
    let state = Progress {
        started: [id("b")].into_iter().collect(),
        completed: [id("a")].into_iter().collect(),
        taken: [(id("a"), id("b"))].into_iter().collect(),
    };
    assert!(!dag.is_finished(&state));

    // Even a started node whose incoming path was never taken blocks (rule 2
    // is checked before reachability).
    let orphan = Progress {
        started: [id("c")].into_iter().collect(),
        ..Progress::default()
    };
    assert!(!dag.is_finished(&orphan));
}

#[test]
fn topological_order_is_identical_for_a_shuffled_twin() {
    // Same diamond, definition order deliberately scrambled.
    let scrambled = DagDefinition::new(
        "t",
        1,
        vec![n("d"), n("b"), n("c"), n("a")],
        vec![e("a", "c"), e("b", "d"), e("a", "b"), e("c", "d")],
    );
    let plain = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("c"), n("d")],
        vec![e("a", "b"), e("a", "c"), e("b", "d"), e("c", "d")],
    );
    let left = CompiledDag::compile(&scrambled, &Limits::new()).unwrap();
    let right = CompiledDag::compile(&plain, &Limits::new()).unwrap();
    assert_eq!(names(left.topological_order()), vec!["a", "b", "c", "d"]);
    assert_eq!(left.topological_order(), right.topological_order());
    assert_eq!(left.hash(), right.hash());
}

#[test]
fn depth_on_diamond_and_chain() {
    let dag = CompiledDag::compile(&diamond(JoinRule::All), &Limits::new()).unwrap();
    assert_eq!(dag.depth(&id("a")), Some(0));
    assert_eq!(dag.depth(&id("b")), Some(1));
    assert_eq!(dag.depth(&id("c")), Some(1));
    assert_eq!(dag.depth(&id("d")), Some(2));

    let dag = CompiledDag::compile(&chain(), &Limits::new()).unwrap();
    assert_eq!(dag.depth(&id("c")), Some(2));

    // Unknown id: `None`.
    assert_eq!(dag.depth(&id("zz")), None);
}

#[test]
fn successors_and_predecessors_are_sorted_by_node_then_condition() {
    // `mid` gets two conditions from `a` and one plain edge from `m`;
    // definition order is scrambled on purpose.
    let def = DagDefinition::new(
        "t",
        1,
        vec![any_n("mid"), n("m"), n("a"), n("end"), n("end2")],
        vec![
            ec("a", "mid", "second"),
            e("m", "mid"),
            ec("a", "mid", "first"),
            e("mid", "end2"),
            e("a", "end"),
            e("m", "end"),
        ],
    );
    let dag = CompiledDag::compile(&def, &Limits::new()).unwrap();

    // Successors of `a` sorted by (other id, condition): both edges reach
    // `mid`, conditions ascending; `end` after.
    let succ: Vec<(String, Option<String>)> = dag
        .successors(&id("a"))
        .iter()
        .map(|ce| (ce.node.as_str().to_owned(), ce.condition.clone()))
        .collect();
    assert_eq!(
        succ,
        vec![
            ("end".to_owned(), None),
            ("mid".to_owned(), Some("first".to_owned())),
            ("mid".to_owned(), Some("second".to_owned())),
        ]
    );

    // Predecessors of `mid` sorted the same way.
    let pred: Vec<(String, Option<String>)> = dag
        .predecessors(&id("mid"))
        .iter()
        .map(|ce| (ce.node.as_str().to_owned(), ce.condition.clone()))
        .collect();
    assert_eq!(
        pred,
        vec![
            ("a".to_owned(), Some("first".to_owned())),
            ("a".to_owned(), Some("second".to_owned())),
            ("m".to_owned(), None),
        ]
    );
}

#[test]
fn accessor_indexes_follow_their_documented_order() {
    let def = DagDefinition::new(
        "t",
        7,
        vec![n("d"), n("b"), n("c"), n("a")],
        vec![e("a", "b"), e("a", "c"), e("b", "d"), e("c", "d")],
    );
    let dag = CompiledDag::compile(&def, &Limits::new()).unwrap();
    assert_eq!(dag.key(), "t");
    assert_eq!(dag.version(), 7);
    // Nodes stay in *definition* order (a is last here).
    let def_order: Vec<&str> = dag.nodes().iter().map(|n| n.id.as_str()).collect();
    assert_eq!(def_order, vec!["d", "b", "c", "a"]);
    // Entries and terminals are sorted.
    assert_eq!(names(dag.entries()), vec!["a"]);
    assert_eq!(names(dag.terminals()), vec!["d"]);
    assert!(dag.node(&id("b")).is_some());
    assert!(dag.node(&id("zz")).is_none());
}
