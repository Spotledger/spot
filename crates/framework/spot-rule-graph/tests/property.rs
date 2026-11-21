//! A differential property test: the engine's transitive closure equals a naive fixed-point
//! oracle over random graphs.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod support;

use std::collections::BTreeSet;

use proptest::prelude::*;
use spot_rule_graph::{Datum, Engine, MemoryFacts, Query, RuleSet};
use spot_storage_contract::NodeRef;

use support::{meta, rec, seed_of};

fn node(datum: &Datum) -> &NodeRef {
    match datum {
        Datum::Record(node) => node,
        other => panic!("expected a record, got {other:?}"),
    }
}

/// The naive transitive closure of `base`, to a fixed point.
fn closure(base: &BTreeSet<(u8, u8)>) -> BTreeSet<(u8, u8)> {
    let mut set = base.clone();
    loop {
        let snapshot: Vec<(u8, u8)> = set.iter().copied().collect();
        let mut added = false;
        for &(a, b) in &snapshot {
            for &(c, d) in &snapshot {
                if b == c && set.insert((a, d)) {
                    added = true;
                }
            }
        }
        if !added {
            return set;
        }
    }
}

proptest! {
    #[test]
    fn transitive_closure_matches_a_naive_oracle(edges in prop::collection::vec((0_u8..6, 0_u8..6), 0..24)) {
        let meta = meta("bom");
        let source = "reach(X, Y) :- item.component(X, Y).\n\
                      reach(X, Y) :- item.component(X, Z), reach(Z, Y).";
        let compiled = RuleSet::compile(source, &meta).unwrap();

        let mut facts = MemoryFacts::new();
        let mut base: BTreeSet<(u8, u8)> = BTreeSet::new();
        for (a, b) in &edges {
            facts.add_edge("item.component", rec("item", *a), rec("item", *b));
            base.insert((*a, *b));
        }

        let rows = Engine::new()
            .evaluate(&compiled, &facts, &Query::new("reach"))
            .unwrap();
        let got: BTreeSet<(u8, u8)> = rows
            .rows
            .iter()
            .map(|row| (seed_of(node(&row[0])), seed_of(node(&row[1]))))
            .collect();
        prop_assert_eq!(got, closure(&base));
    }
}
