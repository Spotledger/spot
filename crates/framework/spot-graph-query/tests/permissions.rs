//! What a caller learns: hidden nodes take their paths with them, a hidden start looks like a
//! missing one, owner rules decide per record, and two users see two consistent graphs.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_graph_query::{
    Direction, EdgeSelector, FactFields, GraphQuery, GraphResult, StorageExecutor,
};
use spot_storage_contract::{Isolation, NodeRef, RecordId, StorageBackend, Transaction, TxOptions};
use support::*;

struct Chain {
    p1: RecordId,
    g1: RecordId,
    i1: RecordId,
    y1: RecordId,
    y2: RecordId,
    docs: Vec<spot_storage_contract::DocumentData>,
}

fn chain() -> Chain {
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
            &[
                ("owner", "alice".into()),
                ("po", spot_storage_contract::Value::Id(p1)),
            ],
        ),
        record(
            "invoice",
            i1,
            &[
                ("owner", "alice".into()),
                ("grn", spot_storage_contract::Value::Id(g1)),
            ],
        ),
        record(
            "payment",
            y1,
            &[
                ("owner", "alice".into()),
                ("invoice", spot_storage_contract::Value::Id(i1)),
                ("kind", "bank".into()),
            ],
        ),
        record(
            "payment",
            y2,
            &[
                ("owner", "bob".into()),
                ("invoice", spot_storage_contract::Value::Id(i1)),
                ("kind", "cash".into()),
            ],
        ),
    ];
    Chain {
        p1,
        g1,
        i1,
        y1,
        y2,
        docs,
    }
}

fn at(entity: &str, id: RecordId) -> NodeRef {
    NodeRef {
        entity: n(entity),
        id,
    }
}

fn chain_query(from: NodeRef, direction: Direction, depth: u16) -> GraphQuery {
    GraphQuery::builder()
        .from(vec![from])
        .follow(
            ["grn.po", "invoice.grn", "payment.invoice"]
                .iter()
                .map(|e| EdgeSelector::directed(*e, direction))
                .collect(),
        )
        .max_depth(depth)
        .build()
        .unwrap()
}

async fn executor(backend: &spot_storage_fake::FakeBackend) -> StorageExecutor<impl Transaction> {
    let tx = backend
        .begin(
            catalog(),
            TxOptions::new(n("perm"), Isolation::RepeatableRead).read_only(),
        )
        .await
        .unwrap();
    StorageExecutor::new(
        tx,
        graph_model(),
        FactFields {
            state: None,
            created_by: Some(n("owner")),
        },
        std::sync::Arc::new(spot_graph_query::UnrestrictedEdgeFields),
    )
}

/// Every edge of a result joins two returned nodes.
fn edges_are_inside(result: &GraphResult) -> bool {
    let nodes: Vec<&NodeRef> = result.nodes.iter().map(|x| &x.node).collect();
    result
        .edges
        .iter()
        .all(|e| nodes.contains(&&e.from) && nodes.contains(&&e.to))
}

#[test]
fn a_hidden_middle_node_takes_the_nodes_behind_it() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    // The clerk reads purchase orders and invoices, not receipts: the receipt is hidden, so the
    // invoice and the payments behind it are not reachable and not named.
    let clerk = who("clerk", &["Reader"]);
    let result = block_on(run(
        &mut ex,
        &chain_query(at("po", c.p1), Direction::In, 5),
        &clerk,
    ))
    .unwrap();
    assert_eq!(depths(&result).len(), 1);
    assert!(result.truncated_by_permission);
    assert!(result.edges.is_empty());
    assert!(edges_are_inside(&result));
}

#[test]
fn a_hidden_start_looks_exactly_like_a_missing_one() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let clerk = who("clerk", &["Reader"]);
    let hidden = block_on(run(
        &mut ex,
        &chain_query(at("grn", c.g1), Direction::In, 5),
        &clerk,
    ))
    .unwrap();
    let missing = block_on(run(
        &mut ex,
        &chain_query(at("grn", RecordId::new_v7()), Direction::In, 5),
        &clerk,
    ))
    .unwrap();
    assert!(hidden.nodes.is_empty());
    assert!(!hidden.truncated_by_permission);
    assert_eq!(hidden, missing);
}

#[test]
fn an_owner_rule_decides_record_by_record() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let from_invoice = |depth| {
        GraphQuery::builder()
            .from(vec![at("invoice", c.i1)])
            .follow(vec![EdgeSelector::directed(
                "payment.invoice",
                Direction::In,
            )])
            .max_depth(depth)
            .build()
            .unwrap()
    };
    // Bob reads the invoice and the payments he created. Alice's payment is hidden.
    let bob = who("bob", &["Reader", "Owner"]);
    let result = block_on(run(&mut ex, &from_invoice(3), &bob)).unwrap();
    let got = depths(&result);
    assert_eq!(got.get(&at("payment", c.y2)), Some(&1));
    assert!(!got.contains_key(&at("payment", c.y1)));
    assert!(result.truncated_by_permission);

    let alice = who("alice", &["Reader", "Owner"]);
    let result = block_on(run(&mut ex, &from_invoice(3), &alice)).unwrap();
    let got = depths(&result);
    assert!(got.contains_key(&at("payment", c.y1)));
    assert!(!got.contains_key(&at("payment", c.y2)));
}

#[test]
fn two_users_get_two_consistent_graphs() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let q = chain_query(at("invoice", c.i1), Direction::In, 3);
    let alice = block_on(run(&mut ex, &q, &who("alice", &["Reader", "Owner"]))).unwrap();
    let bob = block_on(run(&mut ex, &q, &who("bob", &["Reader", "Owner"]))).unwrap();
    assert_ne!(alice, bob);
    assert!(edges_are_inside(&alice));
    assert!(edges_are_inside(&bob));
    let auditor = block_on(run(&mut ex, &q, &who("auditor", &["Auditor"]))).unwrap();
    assert!(!auditor.truncated_by_permission);
    assert!(depths(&auditor).contains_key(&at("payment", c.y1)));
    assert!(depths(&auditor).contains_key(&at("payment", c.y2)));
    // The invoice is the start for everyone the query can see; the auditor sees both payments.
    assert!(depths(&alice).contains_key(&at("invoice", c.i1)));
    let _ = (c.g1, c.p1, c.y1, c.y2);
}
