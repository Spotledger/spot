//! `edges_of` and `diff`: every link kind becomes the edge the graph model names, and a
//! document change produces the right adds, removes and updates.

// Test code may unwrap and expect what the test itself built.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use common::{doc, name, rid, row, schema, set};
use spot_app_api::prelude::Value;
use spot_edge_sync::{Edge, EdgeSchema, diff, edges_of};
use spot_graph_model::{EdgeTypeId, contains_name};

fn edge(name: &str, from: (&str, u64), to: (&str, u64)) -> Edge {
    Edge::new(
        EdgeTypeId::of(name),
        spot_storage_contract::NodeRef {
            entity: common::name(from.0),
            id: rid(from.1),
        },
        spot_storage_contract::NodeRef {
            entity: common::name(to.0),
            id: rid(to.1),
        },
    )
}

fn set_of(edges: impl IntoIterator<Item = Edge>) -> BTreeSet<Edge> {
    edges.into_iter().collect()
}

#[test]
fn a_link_field_is_an_edge() {
    let schema = schema();
    let mut order = doc("order", rid(1));
    set(&mut order, "customer", Value::Id(rid(2)));
    let expected = set_of([edge("order.customer", ("order", 1), ("party", 2))]);
    assert_eq!(edges_of(&schema, &order), expected);
}

#[test]
fn a_null_link_is_no_edge() {
    let schema = schema();
    let order = doc("order", rid(1));
    assert!(edges_of(&schema, &order).is_empty());
}

#[test]
fn a_dynamic_link_targets_the_named_doctype() {
    let schema = schema();
    let mut order = doc("order", rid(1));
    set(&mut order, "ref_kind", Value::Text("project".into()));
    set(&mut order, "ref", Value::Id(rid(7)));
    let expected = set_of([edge("order.ref", ("order", 1), ("project", 7))]);
    assert_eq!(edges_of(&schema, &order), expected);
}

#[test]
fn a_dynamic_link_without_a_type_is_no_edge() {
    let schema = schema();
    let mut order = doc("order", rid(1));
    set(&mut order, "ref", Value::Id(rid(7)));
    assert!(edges_of(&schema, &order).is_empty());
}

#[test]
fn a_table_is_containment_plus_the_childs_links_from_the_parent() {
    let schema = schema();
    let mut order = doc("order", rid(1));
    order.rows_mut(name("lines")).push(row(
        rid(5),
        &[
            ("item", Value::Id(rid(2))),
            ("qty", Value::Text("3".into())),
        ],
    ));
    let expected = set_of([
        edge(
            &contains_name("order", "order_line"),
            ("order", 1),
            ("order_line", 5),
        ),
        edge("order_line.item", ("order", 1), ("party", 2)),
    ]);
    assert_eq!(edges_of(&schema, &order), expected);
}

#[test]
fn a_table_multiselect_is_one_edge_from_the_parent_to_the_pick() {
    let schema = schema();
    let mut order = doc("order", rid(1));
    order
        .rows_mut(name("watchers"))
        .push(row(rid(6), &[("party", Value::Id(rid(2)))]));
    let expected = set_of([edge("order.watchers", ("order", 1), ("party", 2))]);
    assert_eq!(edges_of(&schema, &order), expected);
}

#[test]
fn a_relation_doctype_is_one_typed_edge_with_properties() {
    let schema = schema();
    let mut task = doc("task_link", rid(9));
    set(&mut task, "order", Value::Id(rid(1)));
    set(&mut task, "project", Value::Id(rid(7)));
    set(&mut task, "role", Value::Text("owner".into()));
    let mut expected = edge("task_link.relates", ("order", 1), ("project", 7)).owned_by(
        spot_storage_contract::NodeRef {
            entity: common::name("task_link"),
            id: rid(9),
        },
    );
    expected.properties.insert(
        "role".into(),
        spot_edge_sync::PropValue::Text("owner".into()),
    );
    assert_eq!(edges_of(&schema, &task), set_of([expected]));
}

#[test]
fn a_relation_without_both_ends_is_no_edge() {
    let schema = schema();
    let mut task = doc("task_link", rid(9));
    set(&mut task, "order", Value::Id(rid(1)));
    assert!(edges_of(&schema, &task).is_empty());
}

#[test]
fn a_custom_link_field_added_at_runtime_gets_edges_with_no_code_change() {
    let meta = common::meta_with_custom_link();
    let schema = EdgeSchema::derive(&meta, &[common::relation_spec()]);
    let mut order = doc("order", rid(1));
    set(&mut order, "custom_ref", Value::Id(rid(2)));
    let expected = set_of([edge("order.custom_ref", ("order", 1), ("party", 2))]);
    assert_eq!(edges_of(&schema, &order), expected);
}

#[test]
fn create_adds_every_edge_and_delete_removes_it() {
    let schema = schema();
    let mut order = doc("order", rid(1));
    set(&mut order, "customer", Value::Id(rid(2)));
    let created = diff(&schema, None, Some(&order));
    assert_eq!(created.added.len(), 1);
    assert!(created.removed.is_empty() && created.updated.is_empty());

    let deleted = diff(&schema, Some(&order), None);
    assert_eq!(deleted.removed.len(), 1);
    assert!(deleted.added.is_empty());
}

#[test]
fn updating_a_link_value_removes_the_old_edge_and_adds_the_new() {
    let schema = schema();
    let mut before = doc("order", rid(1));
    set(&mut before, "customer", Value::Id(rid(2)));
    let mut after = doc("order", rid(1));
    set(&mut after, "customer", Value::Id(rid(3)));
    let changes = diff(&schema, Some(&before), Some(&after));
    assert_eq!(
        changes.added,
        set_of([edge("order.customer", ("order", 1), ("party", 3))])
    );
    assert_eq!(
        changes.removed,
        set_of([edge("order.customer", ("order", 1), ("party", 2))])
    );
    assert!(changes.updated.is_empty());
}

#[test]
fn changing_a_relation_property_updates_the_edge() {
    let schema = schema();
    let mut before = doc("task_link", rid(9));
    set(&mut before, "order", Value::Id(rid(1)));
    set(&mut before, "project", Value::Id(rid(7)));
    set(&mut before, "role", Value::Text("owner".into()));
    let mut after = before.clone();
    set(&mut after, "role", Value::Text("approver".into()));
    let changes = diff(&schema, Some(&before), Some(&after));
    assert_eq!(changes.updated.len(), 1);
    assert!(changes.added.is_empty() && changes.removed.is_empty());
    let update = &changes.updated[0];
    assert_eq!(
        update.after.properties.get("role"),
        Some(&spot_edge_sync::PropValue::Text("approver".into()))
    );
}

#[test]
fn cancelling_does_not_change_the_edge_set() {
    let schema = schema();
    let mut order = doc("order", rid(1));
    set(&mut order, "customer", Value::Id(rid(2)));
    let cancelled = order.clone();
    // Cancelling changes only the state; the links (and so the edges) are untouched.
    let changes = diff(&schema, Some(&order), Some(&cancelled));
    assert!(changes.is_empty(), "cancel changes no edge: {changes:?}");
    // A second create/delete pair still round-trips through the same set.
    assert_eq!(edges_of(&schema, &order), edges_of(&schema, &cancelled));
}
