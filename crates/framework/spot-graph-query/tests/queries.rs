//! Queries over the storage fake: directions, depth, budget, cycles, edge types, filters, the
//! builder's problems and the convenience wrappers.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_graph_query::{
    Direction, EdgeSelector, FactFields, GraphQuery, Include, Order, StorageExecutor,
};
use spot_storage_contract::{CmpOp, Expr, Isolation, NodeRef, RecordId, TxOptions, Value};
use spot_storage_contract::{StorageBackend, Transaction};
use support::*;

/// The chain: P1 <- G1 <- I1 <- {Y1 (alice, bank), Y2 (bob, cash)}, and P2 superseding P1 and
/// back, which makes a cycle.
struct Chain {
    p1: RecordId,
    p2: RecordId,
    g1: RecordId,
    i1: RecordId,
    y1: RecordId,
    y2: RecordId,
    docs: Vec<spot_storage_contract::DocumentData>,
}

fn chain() -> Chain {
    let (p1, p2, g1, i1, y1, y2) = (
        RecordId::new_v7(),
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
            &[
                ("title", "PO-1".into()),
                ("owner", "alice".into()),
                ("superseded_by", Value::Id(p2)),
            ],
        ),
        record(
            "po",
            p2,
            &[
                ("title", "PO-2".into()),
                ("owner", "alice".into()),
                ("superseded_by", Value::Id(p1)),
            ],
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
    Chain {
        p1,
        p2,
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

const CHAIN_EDGES: [&str; 3] = ["grn.po", "invoice.grn", "payment.invoice"];

fn chain_edges(direction: Direction) -> Vec<EdgeSelector> {
    CHAIN_EDGES
        .iter()
        .map(|e| EdgeSelector::directed(*e, direction))
        .collect()
}

/// Opens a storage executor over a fresh read-only transaction.
async fn executor(backend: &spot_storage_fake::FakeBackend) -> StorageExecutor<impl Transaction> {
    let cat = catalog();
    let tx = backend
        .begin(
            cat,
            TxOptions::new(n("query"), Isolation::RepeatableRead).read_only(),
        )
        .await
        .expect("begin");
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

fn query(from: NodeRef, follow: Vec<EdgeSelector>, depth: u16) -> GraphQuery {
    GraphQuery::builder()
        .from(vec![from])
        .follow(follow)
        .direction(Direction::Out)
        .max_depth(depth)
        .build()
        .expect("a valid query")
}

#[test]
fn walks_in_from_the_purchase_order_by_depth_and_via() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let q = GraphQuery::builder()
        .from(vec![at("po", c.p1)])
        .follow(chain_edges(Direction::In))
        .max_depth(5)
        .build()
        .unwrap();
    let who = who("auditor", &["Auditor"]);
    let result = block_on(run(&mut ex, &q, &who)).unwrap();
    let got = depths(&result);
    assert_eq!(got.get(&at("po", c.p1)), Some(&0));
    assert_eq!(got.get(&at("grn", c.g1)), Some(&1));
    assert_eq!(got.get(&at("invoice", c.i1)), Some(&2));
    assert_eq!(got.get(&at("payment", c.y1)), Some(&3));
    assert_eq!(got.get(&at("payment", c.y2)), Some(&3));
    // The second purchase order is reached through the cycle only from p1's own superseded_by;
    // it is not on this chain (no edge type named po.superseded_by is followed).
    assert!(!got.contains_key(&at("po", c.p2)));
    let grn = result
        .nodes
        .iter()
        .find(|x| x.node == at("grn", c.g1))
        .unwrap();
    assert_eq!(grn.via, Some((at("po", c.p1), "grn.po".to_owned())));
    assert!(!result.truncated_by_permission);
    assert_eq!(result.edges.len(), result.nodes.len() - 1);
}

#[test]
fn walks_out_from_a_payment_to_the_purchase_order() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let q = GraphQuery::builder()
        .from(vec![at("payment", c.y1)])
        .follow(chain_edges(Direction::Out))
        .max_depth(5)
        .build()
        .unwrap();
    let result = block_on(run(&mut ex, &q, &who("auditor", &["Auditor"]))).unwrap();
    let got = depths(&result);
    assert_eq!(got.get(&at("invoice", c.i1)), Some(&1));
    assert_eq!(got.get(&at("grn", c.g1)), Some(&2));
    assert_eq!(got.get(&at("po", c.p1)), Some(&3));
    assert_eq!(got.len(), 4);
}

#[test]
fn both_ways_from_the_receipt_reaches_both_sides() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let q = GraphQuery::builder()
        .from(vec![at("grn", c.g1)])
        .follow(chain_edges(Direction::Both))
        .max_depth(1)
        .build()
        .unwrap();
    let result = block_on(run(&mut ex, &q, &who("auditor", &["Auditor"]))).unwrap();
    let got = depths(&result);
    assert_eq!(got.len(), 3);
    assert_eq!(got.get(&at("po", c.p1)), Some(&1));
    assert_eq!(got.get(&at("invoice", c.i1)), Some(&1));
}

#[test]
fn depth_cuts_the_walk() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let q = GraphQuery::builder()
        .from(vec![at("po", c.p1)])
        .follow(chain_edges(Direction::In))
        .max_depth(2)
        .build()
        .unwrap();
    let result = block_on(run(&mut ex, &q, &who("auditor", &["Auditor"]))).unwrap();
    let got = depths(&result);
    assert_eq!(got.len(), 3);
    assert!(!got.contains_key(&at("payment", c.y1)));
}

#[test]
fn the_node_budget_is_passed_through_not_emptied() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let q = GraphQuery::builder()
        .from(vec![at("po", c.p1)])
        .follow(chain_edges(Direction::In))
        .max_depth(5)
        .max_nodes(2)
        .build()
        .unwrap();
    let error = block_on(run(&mut ex, &q, &who("auditor", &["Auditor"]))).unwrap_err();
    assert_eq!(error.code(), "storage.traversal_too_large");
}

#[test]
fn a_cycle_terminates_with_each_node_once() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let q = GraphQuery::builder()
        .from(vec![at("po", c.p1)])
        .follow(vec![EdgeSelector::directed(
            "po.superseded_by",
            Direction::Both,
        )])
        .max_depth(64)
        .build()
        .unwrap();
    let result = block_on(run(&mut ex, &q, &who("auditor", &["Auditor"]))).unwrap();
    let got = depths(&result);
    assert_eq!(got.len(), 2);
    assert_eq!(got.get(&at("po", c.p2)), Some(&1));
}

#[test]
fn only_the_listed_edge_types_are_followed() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let q = GraphQuery::builder()
        .from(vec![at("po", c.p1)])
        .follow(vec![EdgeSelector::new("grn.po")])
        .direction(Direction::In)
        .max_depth(5)
        .build()
        .unwrap();
    let result = block_on(run(&mut ex, &q, &who("auditor", &["Auditor"]))).unwrap();
    assert_eq!(depths(&result).len(), 2);
    assert!(result.edges.iter().all(|e| e.edge == "grn.po"));
}

#[test]
fn an_edge_filter_reads_the_record_holding_the_link() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let bank_only = Expr::Cmp(
        Box::new(Expr::Field(spot_storage_contract::FieldRef {
            alias: n("payment"),
            field: n("kind"),
        })),
        CmpOp::Eq,
        Box::new(Expr::Value(Value::Text("bank".into()))),
    );
    let q = GraphQuery::builder()
        .from(vec![at("invoice", c.i1)])
        .follow(vec![EdgeSelector::directed(
            "payment.invoice",
            Direction::In,
        )])
        .max_depth(3)
        .edge_filter(bank_only)
        .build()
        .unwrap();
    let result = block_on(run(&mut ex, &q, &who("auditor", &["Auditor"]))).unwrap();
    let got = depths(&result);
    assert!(got.contains_key(&at("payment", c.y1)));
    assert!(!got.contains_key(&at("payment", c.y2)));
}

#[test]
fn an_unknown_edge_type_lists_the_similar_ones() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let q = query(at("po", c.p1), vec![EdgeSelector::new("grn.pp")], 1);
    let error = block_on(run(&mut ex, &q, &who("auditor", &["Auditor"]))).unwrap_err();
    assert_eq!(error.code(), "graphq.unknown_edge_type");
    assert!(error.to_string().contains("grn.po"), "{error}");
}

#[test]
fn the_builder_reports_every_problem_at_once() {
    let error = GraphQuery::builder()
        .max_depth(0)
        .max_nodes(0)
        .node_filter(Expr::Text(
            Box::new(Expr::Value(Value::Text("x".into()))),
            spot_storage_contract::TextMatch {
                pattern: "x".into(),
                mode: spot_storage_contract::TextMode::Contains,
                case_insensitive: false,
            },
        ))
        .build()
        .unwrap_err();
    let spot_graph_query::GraphQueryError::Invalid(problems) = error else {
        panic!("expected a list of problems");
    };
    let codes: Vec<&str> = problems.iter().map(|p| p.code).collect();
    assert_eq!(
        codes,
        vec![
            "graphq.no_start",
            "graphq.no_edges",
            "graphq.depth_out_of_range",
            "graphq.max_nodes_out_of_range",
            "graphq.unsupported_filter",
        ]
    );
}

#[test]
fn requested_fields_come_back_on_the_nodes() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let q = GraphQuery::builder()
        .from(vec![at("po", c.p1)])
        .follow(chain_edges(Direction::In))
        .max_depth(1)
        .include(Include {
            fields: vec![n("title")],
            edge_properties: false,
        })
        .order(Order::ByEntityThenId)
        .build()
        .unwrap();
    let result = block_on(run(&mut ex, &q, &who("auditor", &["Auditor"]))).unwrap();
    let po = result
        .nodes
        .iter()
        .find(|x| x.node == at("po", c.p1))
        .unwrap();
    assert_eq!(
        po.fields.get(&n("title")),
        Some(&Value::Text("PO-1".into()))
    );
    assert!(result.nodes.iter().all(|x| x.depth <= 1));
}

#[test]
fn convenience_wrappers_build_the_expected_walks() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let auditor = who("auditor", &["Auditor"]);
    let desc = GraphQuery::descendants(at("po", c.p1), "grn.po", 1).unwrap();
    let got = depths(&block_on(run(&mut ex, &desc, &auditor)).unwrap());
    assert_eq!(got.len(), 2);
    let anc = GraphQuery::ancestors(at("payment", c.y1), "payment.invoice", 1).unwrap();
    let got = depths(&block_on(run(&mut ex, &anc, &auditor)).unwrap());
    assert!(got.contains_key(&at("invoice", c.i1)));
    let lin = GraphQuery::lineage(at("grn", c.g1), &CHAIN_EDGES).unwrap();
    let got = depths(&block_on(run(&mut ex, &lin, &auditor)).unwrap());
    // The receipt, its purchase order, its invoice and both payments; the supersession link is
    // not one of the listed edges, so the second purchase order stays out.
    assert_eq!(got.len(), 5);
    assert!(got.contains_key(&at("payment", c.y2)));
    let _ = (c.p2, c.y1);
}

#[test]
fn the_result_is_a_graph_view_for_the_algorithms() {
    use spot_graph_algo::{GraphView, Limits, reachable};
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let mut ex = block_on(executor(&backend));
    let q = GraphQuery::builder()
        .from(vec![at("po", c.p1)])
        .follow(chain_edges(Direction::In))
        .max_depth(5)
        .build()
        .unwrap();
    let result = block_on(run(&mut ex, &q, &who("auditor", &["Auditor"]))).unwrap();
    let view = result.view();
    assert_eq!(view.node_count(), result.nodes.len());
    let start = result.nodes.iter().position(|x| x.depth == 0).unwrap() as u32;
    let reach = reachable(&view, &[start], None, &Limits::default()).unwrap();
    assert_eq!(reach.len(), result.nodes.len());
}
