//! The query seam: the hot index executor gives the same answers as the database path, and a
//! cold or stale graph falls back rather than serving a wrong read.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_graph_index::{Edge, EdgeKey, GraphIndex, IndexExecutor};
use spot_graph_query::contract::{Scenario, same_answers};
use spot_graph_query::{Direction, EdgeSelector, GraphQuery};
use support::{Chain, MapExecutor, MapSource, graph_key, graph_model, node};

fn scenarios(chain: &Chain) -> Vec<Scenario> {
    let model = graph_model();
    let queries = [
        GraphQuery::builder()
            .from(vec![node("po", chain.p1)])
            .follow(vec![
                EdgeSelector::new("grn.po"),
                EdgeSelector::new("invoice.grn"),
                EdgeSelector::new("payment.invoice"),
            ])
            .direction(Direction::In)
            .max_depth(5)
            .build()
            .unwrap(),
        GraphQuery::builder()
            .from(vec![node("payment", chain.y1)])
            .follow(vec![EdgeSelector::new("payment.invoice")])
            .direction(Direction::Out)
            .max_depth(5)
            .build()
            .unwrap(),
        GraphQuery::builder()
            .from(vec![node("grn", chain.g1)])
            .follow(vec![
                EdgeSelector::new("grn.po"),
                EdgeSelector::new("invoice.grn"),
            ])
            .direction(Direction::Both)
            .max_depth(3)
            .build()
            .unwrap(),
    ];
    let _ = model;
    let engine = support::engine();
    let mut scenarios = Vec::new();
    for principal in [
        support::who("auditor", &["Auditor"]),
        support::who("clerk", &["Reader"]),
        support::who("alice", &["Reader", "Owner"]),
        support::who("bob", &["Reader", "Owner"]),
    ] {
        for (i, query) in queries.iter().enumerate() {
            scenarios.push(Scenario {
                name: format!("{} / query {i}", principal.user),
                query: query.clone(),
                principal: principal.clone(),
                engine: engine.clone(),
            });
        }
    }
    scenarios
}

fn index_for(chain: &Chain) -> Arc<GraphIndex> {
    let model = graph_model();
    let records = chain.records();
    let edges = support::edges_of(&model, &records);
    Arc::new(GraphIndex::build(graph_key(), 5, edges).unwrap())
}

#[test]
fn a_hot_index_agrees_with_the_database_path() {
    let chain = Chain::new();
    let model = graph_model();
    let records = chain.records();
    let index = index_for(&chain);
    let mut hot = IndexExecutor::hot(
        MapSource {
            records: records.clone(),
        },
        model.clone(),
        index,
        Arc::new(spot_graph_query::UnrestrictedEdgeFields),
    );
    let mut database = MapExecutor {
        model,
        records: records.clone(),
    };
    let scenarios = scenarios(&chain);
    support::block_on(same_answers(&mut hot, &mut database, &scenarios))
        .expect("the hot index agrees with the database path");
}

#[test]
fn a_cold_executor_uses_the_database_path() {
    let chain = Chain::new();
    let model = graph_model();
    let records = chain.records();
    let mut cold = IndexExecutor::cold(
        MapSource {
            records: records.clone(),
        },
        model.clone(),
        Arc::new(spot_graph_query::UnrestrictedEdgeFields),
    );
    let mut database = MapExecutor {
        model,
        records: records.clone(),
    };
    let scenarios = scenarios(&chain);
    support::block_on(same_answers(&mut cold, &mut database, &scenarios))
        .expect("a cold executor equals the database path");
}

#[test]
fn a_stale_graph_falls_back_to_the_database_path() {
    let chain = Chain::new();
    let model = graph_model();
    let records = chain.records();
    let mut index = GraphIndex::build(graph_key(), 5, support::edges_of(&model, &records)).unwrap();
    index.mark_stale();
    let mut stale = IndexExecutor::hot(
        MapSource {
            records: records.clone(),
        },
        model.clone(),
        Arc::new(index),
        Arc::new(spot_graph_query::UnrestrictedEdgeFields),
    );
    let mut database = MapExecutor {
        model,
        records: records.clone(),
    };
    let scenarios = scenarios(&chain);
    support::block_on(same_answers(&mut stale, &mut database, &scenarios))
        .expect("a stale graph never answers a read");
}

#[test]
fn a_swapped_index_changes_the_answers_to_match_it() {
    let chain = Chain::new();
    let model = graph_model();
    let records = chain.records();

    // A hot index that knows only the receipt-to-order edge.
    let partial = Arc::new(
        GraphIndex::build(
            graph_key(),
            1,
            vec![Edge::bare(EdgeKey::new(
                "grn.po",
                node("grn", chain.g1),
                node("po", chain.p1),
            ))],
        )
        .unwrap(),
    );
    let mut executor = IndexExecutor::hot(
        MapSource {
            records: records.clone(),
        },
        model.clone(),
        partial,
        Arc::new(spot_graph_query::UnrestrictedEdgeFields),
    );
    let query = GraphQuery::builder()
        .from(vec![node("grn", chain.g1)])
        .follow(vec![EdgeSelector::new("invoice.grn")])
        .direction(Direction::In)
        .max_depth(1)
        .build()
        .unwrap();
    let principal = support::who("auditor", &["Auditor"]);
    let before = support::block_on(support::run(&mut executor, &query, &principal)).unwrap();
    assert!(
        !before
            .nodes
            .iter()
            .any(|n| n.node.entity.as_str() == "invoice"),
        "the partial index knows no invoice"
    );

    // After a rebuild the index knows the invoice too, so the answer changes.
    let full =
        Arc::new(GraphIndex::build(graph_key(), 2, support::edges_of(&model, &records)).unwrap());
    executor.set_index(Some(full));
    let after = support::block_on(support::run(&mut executor, &query, &principal)).unwrap();
    assert!(
        after
            .nodes
            .iter()
            .any(|n| n.node.entity.as_str() == "invoice"),
        "the rebuilt index reaches the invoice"
    );
}
