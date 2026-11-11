//! The same-answers contract: the storage executor and an executor over explicit values give
//! equal results on every scenario, and the harness reports a divergence when they differ.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_graph_model::GraphModel;
use spot_graph_query::contract::{Scenario, same_answers};
use spot_graph_query::{Direction, EdgeSelector, FactFields, GraphQuery, StorageExecutor};
use spot_storage_contract::{Isolation, NodeRef, RecordId, StorageBackend, TxOptions, Value};
use support::*;

#[test]
fn storage_and_explicit_values_agree_on_every_scenario() {
    let (p1, g1, i1, y1, y2) = (
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
    );
    let docs = vec![
        record(
            "po",
            p1,
            &[("title", "PO-1".into()), ("owner", "alice".into())],
        ),
        record(
            "grn",
            g1,
            &[("owner", "alice".into()), ("po", Value::Id(p1))],
        ),
        record(
            "invoice",
            i1,
            &[("owner", "alice".into()), ("grn", Value::Id(g1))],
        ),
        record(
            "payment",
            y1,
            &[
                ("owner", "alice".into()),
                ("invoice", Value::Id(i1)),
                ("kind", "bank".into()),
            ],
        ),
        record(
            "payment",
            y2,
            &[
                ("owner", "bob".into()),
                ("invoice", Value::Id(i1)),
                ("kind", "cash".into()),
            ],
        ),
    ];
    let (backend, _) = block_on(store(&docs));
    let tx = block_on(backend.begin(
        catalog(),
        TxOptions::new(n("contract"), Isolation::RepeatableRead).read_only(),
    ))
    .unwrap();
    let mut storage = StorageExecutor::new(
        tx,
        graph_model(),
        FactFields {
            state: None,
            created_by: Some(n("owner")),
        },
        Arc::new(spot_graph_query::UnrestrictedEdgeFields),
    );
    let mut memory = MemExecutor {
        model: graph_model(),
        nodes: node_data(&docs),
    };
    let node = |entity: &str, id: RecordId| NodeRef {
        entity: n(entity),
        id,
    };
    let queries = [
        GraphQuery::builder()
            .from(vec![node("po", p1)])
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
            .from(vec![node("payment", y1)])
            .follow(vec![EdgeSelector::new("payment.invoice")])
            .direction(Direction::Out)
            .max_depth(5)
            .build()
            .unwrap(),
        GraphQuery::builder()
            .from(vec![node("grn", g1)])
            .follow(vec![
                EdgeSelector::new("grn.po"),
                EdgeSelector::new("invoice.grn"),
            ])
            .direction(Direction::Both)
            .max_depth(3)
            .build()
            .unwrap(),
    ];
    let mut scenarios = Vec::new();
    for principal in [
        who("auditor", &["Auditor"]),
        who("clerk", &["Reader"]),
        who("alice", &["Reader", "Owner"]),
        who("bob", &["Reader", "Owner"]),
    ] {
        for (i, query) in queries.iter().enumerate() {
            scenarios.push(Scenario {
                name: format!("{} / query {i}", principal.user),
                query: query.clone(),
                principal: principal.clone(),
                engine: engine(),
            });
        }
    }
    block_on(same_answers(&mut storage, &mut memory, &scenarios)).expect("the executors agree");
}

#[test]
fn a_different_model_is_reported_as_a_divergence() {
    let p1 = RecordId::new_v7();
    let docs = vec![record(
        "po",
        p1,
        &[("title", "PO-1".into()), ("owner", "alice".into())],
    )];
    let (backend, _) = block_on(store(&docs));
    let tx = block_on(backend.begin(
        catalog(),
        TxOptions::new(n("contract"), Isolation::RepeatableRead).read_only(),
    ))
    .unwrap();
    let mut storage = StorageExecutor::new(
        tx,
        graph_model(),
        FactFields::default(),
        Arc::new(spot_graph_query::UnrestrictedEdgeFields),
    );
    // An empty model knows no edge types, so it answers with an error where storage answers.
    let empty = Arc::new(GraphModel::derive(
        &spot_meta::Registry::new(vec![]).unwrap().effective(),
    ));
    let mut memory = MemExecutor {
        model: empty,
        nodes: node_data(&docs),
    };
    let scenario = Scenario {
        name: "grn.po from a purchase order".into(),
        query: GraphQuery::builder()
            .from(vec![NodeRef {
                entity: n("po"),
                id: p1,
            }])
            .follow(vec![spot_graph_query::EdgeSelector::new("grn.po")])
            .max_depth(1)
            .build()
            .unwrap(),
        principal: who("auditor", &["Auditor"]),
        engine: engine(),
    };
    let divergence = block_on(same_answers(&mut storage, &mut memory, &[scenario])).unwrap_err();
    assert_eq!(divergence.scenario, "grn.po from a purchase order");
    assert!(divergence.right.contains("error"), "{divergence:?}");
}
