//! Validation tests (brief "Tests" §: cycles, reachability/shape, complete
//! sorted errors, float rejection).

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used)]

use spot_dag::{DagDefinition, DagError, Edge, JoinRule, Limits, Node, NodeId, validate};

/// A node with default join and empty config.
fn n(id: &str) -> Node {
    Node {
        id: NodeId::new(id).unwrap(),
        kind: "step".to_owned(),
        join: JoinRule::All,
        config: serde_json::Value::Null,
    }
}

/// A node with a config value.
///
/// The float check belongs to `validate` (so `validate` is self-sufficient
/// for compiled values): here the value may carry floats and the constructor
/// accepts them.
fn n_cfg(id: &str, config: serde_json::Value) -> Node {
    // `Node::new` rejects floats; build the raw node directly instead and let
    // `validate` do the float check (that is what the test wants to probe).
    Node {
        id: NodeId::new(id).unwrap(),
        kind: "step".to_owned(),
        join: JoinRule::All,
        config,
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
fn e_c(from: &str, to: &str, condition: &str) -> Edge {
    Edge::new(
        NodeId::new(from).unwrap(),
        NodeId::new(to).unwrap(),
        Some(condition.to_owned()),
    )
}

fn codes(errors: &[DagError]) -> Vec<&'static str> {
    errors.iter().map(DagError::code).collect()
}

/// The 2-cycle `a -> b -> a`.
///
/// On a cyclic graph the shape checks are skipped (see the `validate` doc):
/// the cycle is the diagnosis, not one Unreachable/DeadEnd row per node.
#[test]
fn two_cycle_reports_one_cycle_with_concrete_path() {
    let def = DagDefinition::new("t", 1, vec![n("a"), n("b")], vec![e("a", "b"), e("b", "a")]);
    let errors = validate(&def, &Limits::new()).unwrap_err();
    let cycles: Vec<&DagError> = errors
        .iter()
        .filter(|e| matches!(e, DagError::Cycle { .. }))
        .collect();
    assert_eq!(codes(&errors), vec!["dag.cycle"]);
    assert_eq!(cycles.len(), 1, "one cycle per SCC");
    match errors.first() {
        Some(DagError::Cycle { path }) => {
            let names: Vec<&str> = path.iter().map(NodeId::as_str).collect();
            assert_eq!(names, vec!["a", "b", "a"], "starts and ends on the same id");
        }
        other => panic!("expected cycle first, got {other:?}"),
    }
}

/// The 3-cycle `a -> b -> c -> a`. Shape rows are skipped on cyclic graphs.
#[test]
fn three_cycle_reports_one_cycle() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("c")],
        vec![e("a", "b"), e("b", "c"), e("c", "a")],
    );
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert_eq!(codes(&errors), vec!["dag.cycle"]);
    match errors.first() {
        Some(DagError::Cycle { path }) => {
            let names: Vec<&str> = path.iter().map(NodeId::as_str).collect();
            assert_eq!(names.len(), 4, "three members plus the repeated start");
            assert_eq!(names[0], names[3]);
        }
        other => panic!("expected cycle, got {other:?}"),
    }
}

/// A cycle sitting behind an otherwise-fine diamond `a -> b -> c -> b` where
/// `b -> c` closes back onto `b`: entry `a` exists, so no `NoEntry` fires, but
/// the cycle does.
#[test]
fn cycle_behind_diamond() {
    // diamond a -> (b, c) -> d, plus a back edge d -> b making the b/c/d part cyclic.
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("c"), n("d")],
        vec![
            e("a", "b"),
            e("a", "c"),
            e("b", "d"),
            e("c", "d"),
            e("d", "b"),
        ],
    );
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert!(
        codes(&errors).contains(&"dag.cycle"),
        "cycle expected, got {errors:?}"
    );
    // Exactly one SCC is cyclic (b, c, d); a is not part of it.
    assert_eq!(
        codes(&errors).iter().filter(|c| **c == "dag.cycle").count(),
        1
    );
    // The cycle path stays inside the SCC {b, c, d}: it never contains `a`.
    match errors.iter().find(|e| matches!(e, DagError::Cycle { .. })) {
        Some(DagError::Cycle { path }) => {
            assert!(
                !path.iter().any(|p| p.as_str() == "a"),
                "a is not in the SCC"
            );
            assert_eq!(path.first(), path.last(), "path is closed");
        }
        other => panic!("expected cycle, got {other:?}"),
    }
}

/// Two separate cycles produce two `Cycle` errors (one per SCC).
#[test]
fn two_separate_cycles_two_errors() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("c"), n("d")],
        vec![e("a", "b"), e("b", "a"), e("c", "d"), e("d", "c")],
    );
    let errors = validate(&def, &Limits::new()).unwrap_err();
    let mut cycle_count = 0;
    for error in &errors {
        if let DagError::Cycle { path } = error {
            cycle_count += 1;
            let names: Vec<&str> = path.iter().map(NodeId::as_str).collect();
            assert!(
                names.iter().all(|n| *n == "a" || *n == "b")
                    || names.iter().all(|n| *n == "c" || *n == "d"),
                "each cycle stays inside its SCC, got {names:?}"
            );
        }
    }
    assert_eq!(cycle_count, 2, "one cycle error per SCC, got {errors:?}");
}

/// A 10,000-node chain validates quickly and without a stack overflow.
#[test]
fn chain_of_ten_thousand_is_valid() {
    let count = 10_000usize;
    let mut nodes: Vec<Node> = Vec::with_capacity(count);
    for i in 0..count {
        let name = format!("n{i}");
        nodes.push(n(&name));
    }
    let mut edges: Vec<Edge> = Vec::with_capacity(count - 1);
    for i in 0..count - 1 {
        edges.push(e(&format!("n{i}"), &format!("n{}", i + 1)));
    }
    let def = DagDefinition::new("chain", 1, nodes, edges);
    assert!(
        validate(&def, &Limits::new()).is_ok(),
        "a plain chain is valid"
    );
}

/// The same chain closed into a cycle: exactly one `Cycle` error with a path
/// closing on itself. Shape rows are skipped on cyclic graphs.
#[test]
fn chain_closed_into_cycle() {
    let count = 10_000usize;
    let mut nodes: Vec<Node> = Vec::with_capacity(count);
    for i in 0..count {
        nodes.push(n(&format!("n{i}")));
    }
    let mut edges: Vec<Edge> = Vec::with_capacity(count);
    for i in 0..count - 1 {
        edges.push(e(&format!("n{i}"), &format!("n{}", i + 1)));
    }
    edges.push(e("n9999", "n0"));
    let def = DagDefinition::new("chain", 1, nodes, edges);
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert_eq!(
        codes(&errors),
        vec!["dag.cycle"],
        "the ring is one cyclic SCC; shape rows are skipped on cyclic graphs"
    );
    match errors.first() {
        Some(DagError::Cycle { path }) => {
            let names: Vec<&str> = path.iter().map(NodeId::as_str).collect();
            assert_eq!(names[0], "n0", "the walk started at the smallest id");
            assert_eq!(names[0], names[names.len() - 1], "closed on itself");
            assert_eq!(
                names.len(),
                10_001,
                "the whole ring plus the repeated start"
            );
        }
        other => panic!("expected cycle, got {other:?}"),
    }
}

/// Unknown endpoint: both direction ends are reported with edge index and id.
#[test]
fn unknown_endpoint() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b")],
        vec![e("a", "ghost"), e("phantom", "b")],
    );
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert_eq!(codes(&errors), vec!["dag.unknown_node", "dag.unknown_node"]);
    match errors.first() {
        Some(DagError::UnknownNode { edge_index, id }) => {
            assert_eq!((*edge_index, id.as_str()), (0, "ghost"));
        }
        other => panic!("expected unknown_node first, got {other:?}"),
    }
    match errors.last() {
        Some(DagError::UnknownNode { edge_index, id }) => {
            assert_eq!((*edge_index, id.as_str()), (1, "phantom"));
        }
        other => panic!("expected unknown_node second, got {other:?}"),
    }
}

/// Self loop on an acyclic graph: one `SelfLoop` error, no extra `Cycle` row.
/// The shape rows fire only when the rest of the shape is broken; here
/// `a -> b` keeps an entry (a) and a terminal (b) and everything reachable
/// except the self-loop node… wait: the self edge makes `a` both reachable
/// (as entry) and shape-clean, so only the SelfLoop row remains.
#[test]
fn self_loop() {
    let def = DagDefinition::new("t", 1, vec![n("a"), n("b")], vec![e("a", "a"), e("a", "b")]);
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert_eq!(codes(&errors), vec!["dag.self_loop"], "{errors:?}");
}

/// A pure self-loop graph (only the self edge): the SelfLoop row only. The
/// self edge is skipped for in/out degree, making `s` an entry AND terminal
/// (it has no other edges): reachable (as entry), reaches terminal (itself),
/// so the only row is the root cause, the SelfLoop. A DeadEnd cannot fire:
/// `s` *is* the terminal by out-degree rules — the loop is its own row.
#[test]
fn pure_self_loop_graph() {
    let def = DagDefinition::new("t", 1, vec![n("s")], vec![e("s", "s")]);
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert_eq!(
        codes(&errors),
        vec!["dag.self_loop"],
        "no shape rows: s is both the only entry and the only terminal; {errors:?}"
    );
}

/// Duplicate node: one error naming the repeated id.
#[test]
fn duplicate_node() {
    let def = DagDefinition::new("t", 1, vec![n("a"), n("a"), n("b")], vec![e("a", "b")]);
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert_eq!(codes(&errors), vec!["dag.duplicate_node"]);
    assert!(matches!(errors.first(), Some(DagError::DuplicateNode(id)) if id.as_str() == "a"));
}

/// Duplicate edge: same `(from, to, condition)` twice.
#[test]
fn duplicate_edge() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b")],
        vec![e_c("a", "b", "yes"), e_c("a", "b", "yes")],
    );
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert_eq!(codes(&errors), vec!["dag.duplicate_edge"]);
}

/// The same `(from, to)` pair with different conditions is NOT a duplicate.
#[test]
fn same_pair_different_conditions_is_fine() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("ok"), n("ok2")],
        vec![
            e_c("a", "b", "yes"),
            e_c("a", "b", "no"),
            e("b", "ok"),
            e("b", "ok2"),
        ],
    );
    assert!(validate(&def, &Limits::new()).is_ok());
}

/// Version zero.
#[test]
fn version_zero() {
    let def = DagDefinition::new("t", 0, vec![n("a")], vec![]);
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert_eq!(codes(&errors), vec!["dag.version_zero"]);
}

/// Every node has an incoming edge (SCC {b, c}) plus entry `a`: `Cycle` and
/// `NoTerminal` fire; `NoEntry` must not (a is an entry).
#[test]
fn no_entry_every_node_has_incoming() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("c")],
        vec![e("a", "b"), e("b", "c"), e("c", "b")],
    );
    let errors = validate(&def, &Limits::new()).unwrap_err();
    let list = codes(&errors);
    assert!(list.contains(&"dag.cycle"), "{list:?}");
    assert!(!list.contains(&"dag.no_entry"), "{list:?}");
}

/// Every node has an outgoing edge and an incoming one (pure 2-cycle):
/// `NoTerminal` is skipped like every shape row, because the graph is cyclic;
/// the case where shape fires without a cycle is `chain_with_no_terminal`.
#[test]
fn no_terminal() {
    let def = DagDefinition::new("t", 1, vec![n("a"), n("b")], vec![e("a", "b"), e("b", "a")]);
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert!(codes(&errors).contains(&"dag.cycle"), "{errors:?}");
}

/// No terminal in an *acyclic* graph: `a -> b -> c` with `c` being… not a
/// terminal requires a cycle; the acyclic variant is a chain into a hub that
/// loops back — not acyclic. The acyclic no-terminal case cannot exist (an
/// acyclic graph always has at least one terminal; walk forward from any
/// node, the path must end). Assert exactly that: the cycle variant fires.
#[test]
fn chain_with_no_terminal_is_cyclic() {
    // The only way to remove every terminal is a cycle somewhere; on a
    // valid acyclic graph a terminal always exists.
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("c")],
        vec![e("a", "b"), e("b", "c"), e("c", "b")],
    );
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert!(codes(&errors).contains(&"dag.cycle"));
}

/// Empty definition.
#[test]
fn empty_definition() {
    let def = DagDefinition::new("t", 1, vec![], vec![]);
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert_eq!(codes(&errors), vec!["dag.empty"]);
}

/// An isolated node (no edges touching it) is both unreachable and a dead end;
/// it is not an entry (nothing leads out) and never a terminal on its own.
/// Edges: `a -> b`; also `a -> b2` so `a` is not a terminal, `b2` is.
#[test]
fn isolated_node() {
    // `solo` has no edges at all: it cannot be reached (Unreachable) and
    // cannot reach a terminal (DeadEnd). `a -> b, a -> b2` keeps the graph
    // otherwise shaped (entry a, terminals b and b2).
    let a = n("a");
    let b = n("b");
    let solo = n("solo");
    let b2 = n("b2");
    let def = DagDefinition::new(
        "t",
        1,
        vec![a, b, solo, b2],
        vec![e("a", "b"), e("a", "b2")],
    );
    let errors = validate(&def, &Limits::new());
    // The isolated node earns both shape rows; assert on the value directly
    // (`unwrap_err` is not usable twice on the same moved value here).
    let errors = match errors {
        Err(errors) => errors,
        Ok(()) => panic!("the isolated node must be an Unreachable AND a DeadEnd"),
    };
    assert_eq!(
        codes(&errors),
        vec!["dag.dead_end", "dag.unreachable"],
        "{errors:?}"
    );
    assert!(matches!(errors.first(), Some(DagError::DeadEnd(id)) if id.as_str() == "solo"));
    assert!(matches!(errors.last(), Some(DagError::Unreachable(id)) if id.as_str() == "solo"));
}

/// An isolated self-loop on a node that also feeds a terminal: SelfLoop only,
/// because the node stays reachable and the graph keeps entry + terminal.
#[test]
fn self_loop_on_entry_node() {
    // `a` has a self loop and an outgoing edge; `b` is a plain terminal.
    let def = DagDefinition::new("t", 1, vec![n("a"), n("b")], vec![e("a", "a"), e("a", "b")]);
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert_eq!(codes(&errors), vec!["dag.self_loop"], "{errors:?}");
}

/// An isolated self-loop node next to a working chain: SelfLoop + shape rows
/// for the island (DeadEnd, Unreachable), not a Cycle row. `s`'s self edge is
/// skipped for in/out degree, so `s` has no in-degree (no entry row: it is an
/// entry by degree!) — reachable() from the real entry `a` does NOT reach it,
/// but being an entry it counts "reachable"? No: reachable() only walks from
/// entries; `s` IS an entry, so it becomes reachable through itself. Then
/// Unreachable must NOT fire; DeadEnd DOES (its only path leads to itself,
/// and `reaches_terminal` marks `s` as a terminal-reachable node — `s` is a
/// terminal by out-degree… and then it *does* reach a terminal, itself).
#[test]
fn isolated_self_loop_node() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("s")],
        vec![e("a", "b"), e("s", "s")],
    );
    let errors = validate(&def, &Limits::new()).unwrap_err();
    let list = codes(&errors);
    assert_eq!(
        list,
        vec!["dag.self_loop"],
        "s is its own entry+terminal by degree rules; the SelfLoop row is the diagnosis: {list:?}"
    );
}

/// One definition with four distinct problems returns four errors in the
/// documented sorted order (code, then node id). The self edge makes the
/// shape checks skip this definition (the SelfLoop row stands in for it), so
/// no Unreachable/DeadEnd noise dilutes the four rows.
#[test]
fn four_distinct_errors_in_sorted_order() {
    let def = DagDefinition::new(
        "t",
        0,
        vec![n("a"), n("a"), n_cfg("b", serde_json::json!({"x": 1.5}))],
        vec![e("a", "a")],
    );
    let errors = validate(&def, &Limits::new()).unwrap_err();
    let list = codes(&errors);
    assert_eq!(list.len(), 4, "distinct problems: {errors:?}");
    // Sorted by code then node id:
    // dag.duplicate_node(a) < dag.float_in_config(b) < dag.self_loop(a)
    //   < dag.version_zero
    assert_eq!(
        list,
        vec![
            "dag.duplicate_node",
            "dag.float_in_config",
            "dag.self_loop",
            "dag.version_zero"
        ],
        "{errors:?}"
    );
}

/// The documented order on an acyclic graph with several problem families:
/// DuplicateNode(a) then TooManyNodes (idless rows have no anchor). Shape
/// rows do not fire here: no island node is present, and the graph is fully
/// shaped (a -> mid -> end2 with entry a and terminal end2).
#[test]
fn four_errors_sorted_with_shape_rows() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("a"), n("mid"), n("end2")],
        vec![e("a", "mid"), e("mid", "end2")],
    );
    let limits = Limits {
        max_nodes: 3,
        ..Limits::new()
    };
    let errors = validate(&def, &limits).unwrap_err();
    let list = codes(&errors);
    assert_eq!(
        list,
        vec![
            "dag.duplicate_node", // anchored at a
            "dag.too_many_nodes", // idless (4 nodes > 3)
        ],
        "{errors:?}"
    );
}

/// Node limits produce `TooManyNodes`.
#[test]
fn too_many_nodes_limit() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("c")],
        vec![e("a", "b"), e("b", "c")],
    );
    let limits = Limits {
        max_nodes: 2,
        ..Limits::new()
    };
    let errors = validate(&def, &limits).unwrap_err();
    assert_eq!(codes(&errors), vec!["dag.too_many_nodes"]);
    assert!(
        matches!(
            errors.first(),
            Some(DagError::TooManyNodes {
                limit: 2,
                actual: 3
            })
        ),
        "{errors:?}"
    );
}

/// Edge limits produce `TooManyEdges`.
#[test]
fn too_many_edges_limit() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("c"), n("d")],
        vec![e("a", "b"), e("a", "c"), e("a", "d"), e("b", "d")],
    );
    let limits = Limits {
        max_edges: 3,
        ..Limits::new()
    };
    let errors = validate(&def, &limits).unwrap_err();
    assert_eq!(codes(&errors), vec!["dag.too_many_edges"]);
}

/// Config byte limit produces `ConfigTooLarge` only for the offending node.
#[test]
fn config_too_large() {
    let big = "x".repeat(70_000);
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n_cfg("b", serde_json::json!({ "blob": big }))],
        vec![e("a", "b")],
    );
    let errors = validate(&def, &Limits::new()).unwrap_err();
    assert_eq!(codes(&errors), vec!["dag.config_too_large"]);
    assert!(
        matches!(errors.first(), Some(DagError::ConfigTooLarge { node, .. }) if node.as_str() == "b"),
        "{errors:?}"
    );
}

/// A config exactly at the limit passes.
#[test]
fn config_at_limit_passes() {
    // ~60k of padding is safe under the 65_536 default.
    let pad = "x".repeat(60_000);
    let def = DagDefinition::new(
        "t",
        1,
        vec![n_cfg("a", serde_json::json!({ "blob": pad }))],
        vec![],
    );
    assert!(validate(&def, &Limits::new()).is_ok());
}

mod floats {
    use super::*;

    #[test]
    fn top_level_float_rejected() {
        let def = DagDefinition::new(
            "t",
            1,
            vec![n_cfg("a", serde_json::json!({ "x": 1.5 }))],
            vec![],
        );
        let errors = validate(&def, &Limits::new()).unwrap_err();
        assert_eq!(codes(&errors), vec!["dag.float_in_config"]);
        match errors.first() {
            Some(DagError::FloatInConfig { node, path }) => {
                assert_eq!(node.as_str(), "a");
                assert_eq!(&**path, "x");
            }
            other => panic!("expected float_in_config, got {other:?}"),
        }
    }

    #[test]
    fn integer_passes() {
        let def = DagDefinition::new(
            "t",
            1,
            vec![n_cfg("a", serde_json::json!({ "x": 1 }))],
            vec![],
        );
        assert!(validate(&def, &Limits::new()).is_ok());
    }

    #[test]
    fn string_with_decimal_passes() {
        let def = DagDefinition::new(
            "t",
            1,
            vec![n_cfg("a", serde_json::json!({ "x": "1.50" }))],
            vec![],
        );
        assert!(validate(&def, &Limits::new()).is_ok());
    }

    #[test]
    fn float_nested_in_array() {
        let def = DagDefinition::new(
            "t",
            1,
            vec![n_cfg("a", serde_json::json!({ "xs": [1, 2, 1.5] }))],
            vec![],
        );
        let errors = validate(&def, &Limits::new()).unwrap_err();
        assert_eq!(codes(&errors), vec!["dag.float_in_config"]);
        match errors.first() {
            Some(DagError::FloatInConfig { path, .. }) => assert_eq!(&**path, "xs[2]"),
            other => panic!("expected float_in_config, got {other:?}"),
        }
    }

    #[test]
    fn float_nested_in_object() {
        let def = DagDefinition::new(
            "t",
            1,
            vec![n_cfg(
                "a",
                serde_json::json!({ "prices": { "amount": 0.5 } }),
            )],
            vec![],
        );
        let errors = validate(&def, &Limits::new()).unwrap_err();
        match errors.first() {
            Some(DagError::FloatInConfig { path, .. }) => assert_eq!(&**path, "prices.amount"),
            other => panic!("expected float_in_config, got {other:?}"),
        }
    }

    #[test]
    fn float_deeply_nested() {
        let def = DagDefinition::new(
            "t",
            1,
            vec![n_cfg(
                "a",
                serde_json::json!({ "rows": [ { "cols": [ { "v": 2.25 } ] } ] }),
            )],
            vec![],
        );
        let errors = validate(&def, &Limits::new()).unwrap_err();
        match errors.first() {
            Some(DagError::FloatInConfig { path, .. }) => assert_eq!(&**path, "rows[0].cols[0].v"),
            other => panic!("expected float_in_config, got {other:?}"),
        }
    }

    #[test]
    fn negative_float_rejected_negative_int_accepted() {
        let bad = DagDefinition::new(
            "t",
            1,
            vec![n_cfg("a", serde_json::json!({ "x": -1.5 }))],
            vec![],
        );
        assert!(validate(&bad, &Limits::new()).is_err());
        let good = DagDefinition::new(
            "t",
            1,
            vec![n_cfg("a", serde_json::json!({ "x": -1 }))],
            vec![],
        );
        assert!(validate(&good, &Limits::new()).is_ok());
    }

    #[test]
    fn whole_number_float_literal_is_still_a_float_number() {
        // JSON has no int/float type distinction at the syntax level; serde_json
        // parses `2.0` as f64 and it must be rejected even though its value is
        // whole. It parses 2 (no dot) as u64/i64 and accepts it.
        let def = DagDefinition::new(
            "t",
            1,
            vec![n_cfg("a", serde_json::json!({ "x": 2.0 }))],
            vec![],
        );
        let errors = validate(&def, &Limits::new()).unwrap_err();
        assert_eq!(codes(&errors), vec!["dag.float_in_config"]);
    }
}

/// JoinRule Any/All on a node with fewer than two incoming edges is allowed.
#[test]
fn join_rule_on_single_incoming_is_fine() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![
            Node::new(
                NodeId::new("a").unwrap(),
                "step",
                JoinRule::Any,
                serde_json::Value::Null,
            )
            .unwrap(),
            n("b"),
        ],
        vec![e("a", "b")],
    );
    assert!(validate(&def, &Limits::new()).is_ok());
}

/// Entries and terminals of a valid graph: exact sets.
#[test]
fn valid_graph_has_expected_entries_and_shape() {
    let def = DagDefinition::new(
        "t",
        1,
        vec![n("a"), n("b"), n("c"), n("d")],
        vec![e("a", "b"), e("a", "c"), e("b", "d"), e("c", "d")],
    );
    assert!(validate(&def, &Limits::new()).is_ok());
}
