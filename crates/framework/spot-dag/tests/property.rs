//! Property test (brief §"Compiler"): for 64 random DAGs — edges only from a
//! lower to a higher index, node and edge order shuffled — a definition and
//! its shuffled twin must agree on `topological_order` and `hash`.

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used)]

use proptest::prelude::*;
use spot_dag::{CompiledDag, DagDefinition, Edge, JoinRule, Limits, Node, NodeId};

/// A node with default join (`All`) and empty config.
fn n(id: &str) -> Node {
    Node {
        id: NodeId::new(id).unwrap(),
        kind: "step".to_owned(),
        join: JoinRule::All,
        config: serde_json::Value::Null,
    }
}

/// An unconditional edge.
fn e(from: NodeId, to: NodeId) -> Edge {
    Edge {
        from,
        to,
        condition: None,
    }
}

/// Builds node ids `s0..s{n-1}` (the `s_` prefix keeps names id-grammar-safe
/// regardless of the index digits).
fn node_ids(count: usize) -> Vec<NodeId> {
    (0..count)
        .map(|i| NodeId::new(&format!("s{i}")).unwrap())
        .collect()
}

/// Shuffles `Vec<u32>` strategies don't exist; build a shuffle from the
/// random seed ourselves: sort indices by a random key.
fn shuffled<T>(mut items: Vec<T>, seed: u64) -> Vec<T> {
    // A tiny xorshift for a deterministic-enough Fisher-Yates key stream; the
    // proptest generator drives the seed, so every case is still random.
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for i in (1..items.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
    items
}

// The property: a random DAG and its shuffled twin must share
// `topological_order` and `hash`.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn shuffled_twin_has_identical_topo_and_hash(
        node_count in 1usize..=12,
        edge_seed in any::<u64>(),
        shuffle_seed in any::<u64>(),
    ) {
        let ids = node_ids(node_count);

        // Edges only from a lower to a higher index: acyclic by construction.
        // Every non-last node takes its "next" edge, so every node lies on
        // the chain (reachable from the entry, reaches the terminal) and the
        // extra cross edges only ever add shortcuts.
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for from in 0..node_count.saturating_sub(1) {
            edges.push((from, from + 1));
        }
        for from in 0..node_count.saturating_sub(1) {
            for to in (from + 2)..node_count {
                // Deterministic per (from, to) pick from the seed stream;
                // `pick` never degenerates: it is re-mixed once more.
                let pick = (edge_seed ^ (from as u64).wrapping_mul(0x9E3779B97F4A7C15))
                    ^ (to as u64).wrapping_mul(0xD1B54A32D192ED03);
                if pick.is_multiple_of(3) {
                    edges.push((from, to));
                }
            }
        }

        // Base definition: nodes and edges in construction order. At least
        // one node; the entry s0 exists even with no edges (single node is a
        // valid entry+terminal graph).
        let base = DagDefinition::new(
            "prop",
            1,
            ids.iter().map(|id| n(id.as_str())).collect(),
            edges.iter().map(|&(f, t)| e(ids[f].clone(), ids[t].clone())).collect(),
        );

        // Twin: same graph, node and edge order shuffled.
        let node_perm: Vec<usize> = shuffled((0..node_count).collect(), shuffle_seed);
        let edge_perm: Vec<usize> = shuffled((0..edges.len()).collect(), edge_seed);
        let twin = DagDefinition::new(
            "prop",
            1,
            node_perm.iter().map(|&i| n(ids[i].as_str())).collect(),
            edge_perm
                .iter()
                .map(|&i| {
                    let (f, t) = edges[i];
                    e(ids[f].clone(), ids[t].clone())
                })
                .collect(),
        );

        let left = CompiledDag::compile(&base, &Limits::new())
            .unwrap_or_else(|errs| panic!("base must compile: {errs:?}"));
        let right = CompiledDag::compile(&twin, &Limits::new())
            .unwrap_or_else(|errs| panic!("twin must compile: {errs:?}"));

        prop_assert_eq!(left.topological_order(), right.topological_order());
        prop_assert_eq!(left.hash().as_hex(), right.hash().as_hex());
    }
}
