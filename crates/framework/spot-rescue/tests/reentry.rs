#![allow(clippy::expect_used, clippy::unwrap_used)]
//! The re-entry planner: causal order, transforms, blocked items and refusals.

mod common;

use std::sync::Arc;

use common::principal;
use serde_json::json;
use spot_rescue::{
    Classification, CommittedOperation, DownTransform, Error, FixedOperations, OldDocType,
    OldSchema, OperationId, OperationKind, ReentryPlanner,
};

const T: i64 = 1_000;

fn op(id: &str, sequence: u64, doctype: &str, payload: serde_json::Value) -> CommittedOperation {
    CommittedOperation::new(
        OperationId::new(id).expect("a valid id"),
        sequence,
        OperationKind::Create,
        doctype,
        payload,
        principal("alice"),
        T + sequence as i64 + 1,
    )
}

fn planner() -> ReentryPlanner {
    ReentryPlanner::new(
        OldSchema::new()
            .with_doctype(
                "sales_invoice",
                OldDocType::new(["id", "customer", "amount", "note"])
                    .with_required(["id", "amount"]),
            )
            .with_doctype("customer", OldDocType::new(["id", "name"])),
        vec![
            DownTransform::Rename {
                doctype: "sales_invoice".to_owned(),
                from: "grand_total".to_owned(),
                to: "amount".to_owned(),
            },
            DownTransform::Drop {
                doctype: "sales_invoice".to_owned(),
                field: "internal_flag".to_owned(),
            },
        ],
    )
}

#[test]
fn replayable_operations_need_no_transform() {
    let plan = planner()
        .plan(
            T,
            "rescue-1",
            &[op(
                "invoice-1",
                1,
                "sales_invoice",
                json!({"id": 1, "customer": "acme", "amount": 500}),
            )],
        )
        .expect("a valid plan");
    assert_eq!(plan.counts().replayable, 1);
    assert_eq!(plan.counts().needs_transform, 0);
    assert_eq!(plan.items[0].classification, Classification::Replayable);
    assert_eq!(plan.items[0].command_id, "rescue-1:reentry:invoice-1");
}

#[test]
fn a_changed_field_shape_needs_the_declared_down_transform() {
    let plan = planner()
        .plan(
            T,
            "rescue-1",
            &[op(
                "invoice-1",
                1,
                "sales_invoice",
                json!({"id": 1, "grand_total": 500, "internal_flag": true}),
            )],
        )
        .expect("a valid plan");
    let item = &plan.items[0];
    assert!(matches!(
        item.classification,
        Classification::NeedsTransform { .. }
    ));
    assert_eq!(item.prepared, json!({"id": 1, "amount": 500}));
    let item = item.item("rescue-1", 2);
    assert_eq!(item.payload, json!({"id": 1, "amount": 500}));
    assert_eq!(
        item.original_payload,
        json!({"id": 1, "grand_total": 500, "internal_flag": true})
    );
    assert_eq!(item.principal.user, "alice");
    assert_eq!(item.command_id, "rescue-1:reentry:invoice-1");
}

#[test]
fn an_unknown_doctype_is_blocked_and_preserved() {
    let plan = planner()
        .plan(
            T,
            "rescue-1",
            &[op("widget-1", 1, "widget", json!({"id": 1}))],
        )
        .expect("a valid plan");
    let mut blocked = plan.blocked();
    assert_eq!(blocked.len(), 1);
    let item = blocked.pop().expect("one blocked item");
    assert!(matches!(
        item.classification,
        Classification::Blocked { .. }
    ));
    assert_eq!(item.operation.payload, json!({"id": 1}));
}

#[test]
fn a_field_with_no_transform_blocks_the_item() {
    let plan = planner()
        .plan(
            T,
            "rescue-1",
            &[op(
                "invoice-1",
                1,
                "sales_invoice",
                json!({"id": 1, "amount": 500, "surprise": "new"}),
            )],
        )
        .expect("a valid plan");
    assert_eq!(plan.counts().blocked, 1);
    let blocked = plan.blocked();
    assert!(matches!(
        blocked[0].classification,
        Classification::Blocked { .. }
    ));
}

#[test]
fn a_missing_required_field_blocks_the_item() {
    let plan = planner()
        .plan(
            T,
            "rescue-1",
            &[op(
                "invoice-1",
                1,
                "sales_invoice",
                json!({"id": 1, "customer": "acme"}),
            )],
        )
        .expect("a valid plan");
    assert_eq!(plan.counts().blocked, 1);
}

#[test]
fn a_delete_after_the_snapshot_is_always_blocked() {
    let mut operation = op(
        "invoice-1",
        1,
        "sales_invoice",
        json!({"id": 1, "amount": 500}),
    );
    operation.kind = OperationKind::Delete;
    let plan = planner()
        .plan(T, "rescue-1", &[operation])
        .expect("a valid plan");
    assert_eq!(plan.counts().blocked, 1);
}

#[test]
fn operations_before_the_snapshot_are_not_reentered() {
    let mut before = op(
        "invoice-1",
        1,
        "sales_invoice",
        json!({"id": 1, "amount": 500}),
    );
    before.committed_at = T;
    let after = op(
        "invoice-2",
        2,
        "sales_invoice",
        json!({"id": 2, "amount": 700}),
    );
    let plan = planner()
        .plan(T, "rescue-1", &[before, after])
        .expect("a valid plan");
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].key().as_str(), "invoice-2");
}

#[test]
fn commit_order_is_a_tie_break_and_causes_win() {
    let later = op(
        "invoice-a",
        1,
        "sales_invoice",
        json!({"id": 1, "amount": 100}),
    );
    let earlier = op(
        "invoice-b",
        2,
        "sales_invoice",
        json!({"id": 2, "amount": 200}),
    )
    .with_causes(vec![OperationId::new("invoice-a").expect("a valid id")]);
    let plan = planner()
        .plan(T, "rescue-1", &[earlier, later])
        .expect("a valid plan");
    let order: Vec<String> = plan
        .items
        .iter()
        .map(|item| item.key().as_str().to_owned())
        .collect();
    assert_eq!(order, vec!["invoice-a".to_owned(), "invoice-b".to_owned()]);
}

#[test]
fn a_causal_cycle_is_refused() {
    let first = op("a", 1, "customer", json!({"id": 1, "name": "a"}))
        .with_causes(vec![OperationId::new("b").expect("a valid id")]);
    let second = op("b", 2, "customer", json!({"id": 2, "name": "b"}))
        .with_causes(vec![OperationId::new("a").expect("a valid id")]);
    let error = planner()
        .plan(T, "rescue-1", &[first, second])
        .expect_err("a cycle has no causal order");
    assert!(matches!(error, Error::InvalidInput { .. }));
}

#[test]
fn duplicate_operation_ids_are_refused() {
    let first = op("a", 1, "customer", json!({"id": 1, "name": "a"}));
    let second = op("a", 2, "customer", json!({"id": 2, "name": "b"}));
    let error = planner()
        .plan(T, "rescue-1", &[first, second])
        .expect_err("duplicate ids are ambiguous");
    assert!(matches!(error, Error::InvalidInput { .. }));
}

#[tokio::test]
async fn the_fixed_source_filters_by_snapshot_time() {
    let source = FixedOperations::new(vec![
        op("before", 1, "customer", json!({"id": 1, "name": "before"})),
        op("after", 2, "customer", json!({"id": 2, "name": "after"})),
    ]);
    let source: Arc<dyn spot_rescue::OperationSource> = Arc::new(source);
    let operations = source
        .operations_after(T + 2)
        .await
        .expect("the read works");
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].key().as_str(), "after");
}
