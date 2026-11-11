//! The `graph-index` feature: edges answered from a hot in-memory index (WP-GRA-02).
#![cfg(feature = "graph-index")]
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod support;

use spot_graph_index::{Edge, EdgeKey, GraphIndex, GraphKey};
use spot_rule_graph::graph_facts::GraphIndexFacts;
use spot_rule_graph::{Datum, Engine, Query, RuleSet};

use support::{meta, rec};

#[test]
fn edges_come_from_the_hot_index() {
    let key = GraphKey::new("tenant", "boms");
    let edges = vec![
        Edge::bare(EdgeKey::new(
            "item.component",
            rec("item", 0),
            rec("item", 1),
        )),
        Edge::bare(EdgeKey::new(
            "item.component",
            rec("item", 1),
            rec("item", 2),
        )),
        Edge::bare(EdgeKey::new(
            "bom_change.item",
            rec("bom_change", 0),
            rec("item", 2),
        )),
    ];
    let index = GraphIndex::build(key, 3, edges).unwrap();
    let source = "uses(I, C) :- item.component(I, C).\n\
                  affected(C) :- bom_change.item(_, C).\n\
                  affected(U) :- uses(U, D), affected(D).";
    let compiled = RuleSet::compile(source, &meta("bom")).unwrap();
    let facts = GraphIndexFacts::edges_only(&index);
    let rows = Engine::new()
        .evaluate(&compiled, &facts, &Query::new("affected"))
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![
            vec![Datum::Record(rec("item", 0))],
            vec![Datum::Record(rec("item", 1))],
            vec![Datum::Record(rec("item", 2))],
        ]
    );
}
