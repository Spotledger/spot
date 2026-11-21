//! What a filter and a flag may reveal: filters see only the caller's readable view of a record,
//! the permission check runs before the edge filter, shares and field grants decide what is
//! visible and what is masked.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_graph_query::{
    Direction, EdgeSelector, FactFields, GraphQuery, GraphResult, Include, StorageExecutor,
};
use spot_storage_contract::{
    CmpOp, Expr, FieldRef, Isolation, NodeRef, RecordId, StorageBackend, TxOptions, Value,
};
use support::*;

/// The chain, with the first payment's `kind` as given (the second is `cash`).
fn docs(
    y1_kind: &str,
) -> (
    Vec<spot_storage_contract::DocumentData>,
    RecordId,
    RecordId,
    RecordId,
) {
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
                ("kind", y1_kind.into()),
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
    (docs, p1, g1, i1)
}

fn at(entity: &str, id: RecordId) -> NodeRef {
    NodeRef {
        entity: n(entity),
        id,
    }
}

fn kind_is(value: &str) -> Expr {
    Expr::Cmp(
        Box::new(Expr::Field(FieldRef {
            alias: n("payment"),
            field: n("kind"),
        })),
        CmpOp::Eq,
        Box::new(Expr::Value(Value::Text(value.into()))),
    )
}

/// Runs `query` for `principal` against a store built from `docs`, under `engine`.
fn answer(
    docs: &[spot_storage_contract::DocumentData],
    query: &GraphQuery,
    principal: &spot_permissions::Principal,
    engine: &std::sync::Arc<spot_permissions::PermissionEngine>,
) -> GraphResult {
    let (backend, _) = block_on(store(docs));
    let tx = block_on(backend.begin(
        catalog(),
        TxOptions::new(n("protection"), Isolation::RepeatableRead).read_only(),
    ))
    .unwrap();
    let mut executor = StorageExecutor::new(
        tx,
        graph_model(),
        FactFields {
            state: None,
            created_by: Some(n("owner")),
        },
        std::sync::Arc::new(spot_graph_query::UnrestrictedEdgeFields),
    );
    let who = invocation(principal, engine);
    block_on(spot_graph_query::GraphExecutor::execute(
        &mut executor,
        query,
        &who,
    ))
    .unwrap()
}

/// The first payment (alice's) of a store built from `docs`.
fn y1_of(docs: &[spot_storage_contract::DocumentData]) -> RecordId {
    docs.iter()
        .find(|d| {
            d.entity.as_str() == "payment"
                && d.fields.get(&n("owner")) == Some(&Value::Text("alice".into()))
        })
        .map(|d| d.id)
        .unwrap()
}

/// From the payment itself, its invoice next: a node filter on `kind` reads the payment's own
/// `kind`, which is the value under test.
fn from_payment(y1: RecordId, filter: Expr) -> GraphQuery {
    GraphQuery::builder()
        .from(vec![at("payment", y1)])
        .follow(vec![EdgeSelector::directed(
            "payment.invoice",
            Direction::Out,
        )])
        .max_depth(1)
        .node_filter(filter)
        .build()
        .unwrap()
}

/// A node filter on a hidden field: the result is the same whatever the hidden value is.
#[test]
fn a_node_filter_on_a_hidden_field_does_not_depend_on_the_hidden_value() {
    let engine = protected_engine(&[]);
    let plain = spot_permissions::Principal::with_roles("plain", &["Plain"]);
    let (bank, _, _, _) = docs("bank");
    let (cash, _, _, _) = docs("cash");
    let query = from_payment(y1_of(&bank), kind_is("bank"));
    let with_bank = answer(&bank, &query, &plain, &engine);
    let with_cash = answer(&cash, &query, &plain, &engine);
    assert_eq!(
        with_bank, with_cash,
        "a hidden field must not decide which nodes are kept"
    );
}

/// The same for a masked field: the filter sees the masked text, not the stored value.
#[test]
fn a_node_filter_on_a_masked_field_does_not_depend_on_the_hidden_value() {
    let engine = protected_engine(&[]);
    let masked = spot_permissions::Principal::with_roles("masked", &["Masked"]);
    let (bank, _, _, _) = docs("bank");
    let (cash, _, _, _) = docs("cash");
    let query = from_payment(y1_of(&bank), kind_is("bank"));
    let with_bank = answer(&bank, &query, &masked, &engine);
    let with_cash = answer(&cash, &query, &masked, &engine);
    assert_eq!(
        with_bank, with_cash,
        "a masked field must not decide which nodes are kept"
    );
}

/// An edge filter that passes or fails on a denied node: the flag is set both ways.
#[test]
fn an_edge_filter_on_a_denied_node_does_not_change_the_flag() {
    let engine = protected_engine(&[]);
    let bob = spot_permissions::Principal::with_roles("bob", &["Reader", "Owner"]);
    let (docs_bank, _, _, i1) = docs("bank");
    let query = |kind: &str| {
        GraphQuery::builder()
            .from(vec![at("invoice", i1)])
            .follow(vec![EdgeSelector::directed(
                "payment.invoice",
                Direction::In,
            )])
            .max_depth(1)
            .edge_filter(kind_is(kind))
            .build()
            .unwrap()
    };
    let passes = answer(&docs_bank, &query("bank"), &bob, &engine);
    let fails = answer(&docs_bank, &query("cash"), &bob, &engine);
    assert!(
        passes.truncated_by_permission,
        "a denied node sets the flag"
    );
    assert_eq!(
        passes.truncated_by_permission, fails.truncated_by_permission,
        "the edge filter must not decide the flag"
    );
}

/// A share makes exactly the shared record visible; its neighbours stay hidden.
#[test]
fn a_share_makes_exactly_one_node_visible() {
    let (docs_all, _, g1, _) = docs("bank");
    let engine = protected_engine(&[(g1, "outsider")]);
    let outsider = spot_permissions::Principal::with_roles("outsider", &[]);
    let query = GraphQuery::builder()
        .from(vec![at("grn", g1)])
        .follow(vec![
            EdgeSelector::new("grn.po"),
            EdgeSelector::new("invoice.grn"),
        ])
        .direction(Direction::Both)
        .max_depth(1)
        .build()
        .unwrap();
    let result = answer(&docs_all, &query, &outsider, &engine);
    assert_eq!(result.nodes.len(), 1, "only the shared receipt is visible");
    assert_eq!(result.nodes[0].node, at("grn", g1));
    assert!(result.truncated_by_permission);
}

/// Requested fields come back masked through the masked grant, hidden without one, and in clear
/// with the full grant.
#[test]
fn a_field_grant_masks_a_field_in_the_result() {
    let engine = protected_engine(&[]);
    let (docs_bank, _, _, _) = docs("bank");
    let y1 = y1_of(&docs_bank);
    let query = GraphQuery::builder()
        .from(vec![at("payment", y1)])
        .follow(vec![EdgeSelector::new("payment.invoice")])
        .direction(Direction::Out)
        .max_depth(1)
        .include(Include {
            fields: vec![n("kind")],
            edge_properties: false,
        })
        .build()
        .unwrap();
    let field_of = |principal: &spot_permissions::Principal| {
        let result = answer(&docs_bank, &query, principal, &engine);
        result
            .nodes
            .iter()
            .find(|x| x.node == at("payment", y1))
            .and_then(|x| x.fields.get(&n("kind")).cloned())
    };
    assert_eq!(
        field_of(&spot_permissions::Principal::with_roles(
            "auditor",
            &["Auditor"]
        )),
        Some(Value::Text("bank".into()))
    );
    assert_eq!(
        field_of(&spot_permissions::Principal::with_roles(
            "masked",
            &["Masked"]
        )),
        Some(Value::Text("***k".into()))
    );
    assert_eq!(
        field_of(&spot_permissions::Principal::with_roles(
            "plain",
            &["Plain"]
        )),
        None
    );
}
