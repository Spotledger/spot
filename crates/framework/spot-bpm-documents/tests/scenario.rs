//! The Document node's scenarios: order -> delivery -> invoice, a validation failure, a
//! permission denial, idempotent replay, submit and cancel, and the Case file's completeness.
//!
//! Every operation runs through the real document engine over the in-memory storage fake, so a
//! step changes a document exactly as a user's click would.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;

use common::{block_on, engine, n, run_node, run_node_with_edges, site};
use serde_json::json;
use spot_app_api::DocState;
use spot_app_api::prelude::RecordId;
use spot_bpm_documents::CaseFileStore;
use spot_bpm_documents::case_file::{CaseFile, Touch};
use spot_bpm_documents::{DocumentNodeHandler, EngineDocumentOps, MemoryCaseFile, Operation};
use spot_bpm_engine::{CaseId, NodeOutcome, OutgoingEdge};
use spot_dag::NodeId;

/// The record id a successful node output names.
fn id_of(outcome: &NodeOutcome) -> RecordId {
    match outcome {
        NodeOutcome::Done {
            output: Some(output),
            ..
        } => {
            let text = output["id"].as_str().unwrap();
            spot_bpm_documents::ids::parse_record_id(text).unwrap()
        }
        other => panic!("expected a done outcome, got {other:?}"),
    }
}

#[test]
fn order_to_delivery_to_invoice_records_the_case_file() {
    let engine = engine();
    let case_file = Arc::new(MemoryCaseFile::new());
    let ops = Arc::new(EngineDocumentOps::new(
        engine.clone(),
        site(),
        case_file.clone(),
    ));
    let handler = DocumentNodeHandler::new(ops);
    let case = CaseId::new("case_order").unwrap();
    let mut variables = json!({ "title": "SO-0001" });

    // 1. The order.
    let order_config = json!({
        "operation": "create_from",
        "doctype": "sales_order",
        "mapping": { "title": "doc.title" },
    });
    let order = run_node(
        &handler,
        &case,
        "order",
        1,
        &order_config,
        None,
        &mut variables,
    );
    let order_id = id_of(&order);

    // 2. The delivery, linking the order.
    let delivery_config = json!({
        "operation": "create_from",
        "doctype": "delivery_note",
        "mapping": {
            "sales_order": { "expr": "doc.order.id", "as": "link" },
            "note": "doc.delivery_note",
        },
    });
    variables["delivery_note"] = json!("first delivery");
    let delivery = run_node(
        &handler,
        &case,
        "delivery",
        1,
        &delivery_config,
        None,
        &mut variables,
    );
    let delivery_id = id_of(&delivery);

    // 3. The invoice, linking the delivery.
    let invoice_config = json!({
        "operation": "create_from",
        "doctype": "sales_invoice",
        "mapping": {
            "title": "doc.invoice_title",
            "delivery_note": { "expr": "doc.delivery.id", "as": "link" },
        },
    });
    variables["invoice_title"] = json!("SI-0001");
    let invoice = run_node(
        &handler,
        &case,
        "invoice",
        1,
        &invoice_config,
        None,
        &mut variables,
    );
    let invoice_id = id_of(&invoice);

    // The documents exist, with the links a user's click would have made.
    assert!(
        block_on(engine.peek("sales_order", order_id))
            .unwrap()
            .is_some()
    );
    let delivery_doc = block_on(engine.peek("delivery_note", delivery_id))
        .unwrap()
        .unwrap();
    assert_eq!(delivery_doc.link("sales_order").unwrap(), Some(order_id));
    let invoice_doc = block_on(engine.peek("sales_invoice", invoice_id))
        .unwrap()
        .unwrap();
    assert_eq!(
        invoice_doc.link("delivery_note").unwrap(),
        Some(delivery_id)
    );

    // The Case file lists exactly the three touched documents.
    let entries = case_file.entries(&case).unwrap();
    assert_eq!(entries.len(), 3);
    let expected = vec![
        Touch::new(n("sales_order"), order_id),
        Touch::new(n("delivery_note"), delivery_id),
        Touch::new(n("sales_invoice"), invoice_id),
    ];
    let completeness = CaseFile::check(&entries, &expected);
    assert!(completeness.is_complete(), "{completeness:?}");
    assert!(entries.iter().any(|entry| entry.node.as_str() == "order"));
    assert!(
        entries
            .iter()
            .all(|entry| entry.receipt["record"]["id"].is_string())
    );
}

#[test]
fn a_validation_failure_takes_the_error_branch() {
    let engine = engine();
    let case_file = Arc::new(MemoryCaseFile::new());
    let ops = Arc::new(EngineDocumentOps::new(
        engine.clone(),
        site(),
        case_file.clone(),
    ));
    let handler = DocumentNodeHandler::new(ops);
    let case = CaseId::new("case_invalid").unwrap();
    let mut variables = json!({ "note": "no title" });

    // The order needs a title; the mapping sets none.
    let config = json!({
        "operation": "create_from",
        "doctype": "sales_order",
        "mapping": { "note": "doc.note" },
    });
    let outcome = run_node(&handler, &case, "order", 1, &config, None, &mut variables);
    match outcome {
        NodeOutcome::Fail { code, reason } => {
            assert_eq!(code, "document.validation_failed");
            assert!(reason.contains("title"), "the field path is named");
            assert!(!reason.contains("no title"), "the reason names no value");
        }
        other => panic!("expected the error branch, got {other:?}"),
    }
    // Nothing was written and nothing was recorded.
    assert!(case_file.entries(&case).unwrap().is_empty());
}

#[test]
fn a_case_actor_that_is_not_a_user_id_is_refused_not_run_as_system() {
    let engine = engine_without_permission();
    let case_file = Arc::new(MemoryCaseFile::new());
    let ops = Arc::new(EngineDocumentOps::new(engine, site(), case_file.clone()));
    let handler = DocumentNodeHandler::new(ops);
    let case = CaseId::new("case_bad_actor").unwrap();
    let mut variables = json!({ "t": "x" });
    let config = json!({
        "operation": "create_from",
        "doctype": "sales_order",
        "mapping": { "title": "doc.t" },
    });
    let outcome = run_node(
        &handler,
        &case,
        "order",
        1,
        &config,
        Some("alice"),
        &mut variables,
    );
    match outcome {
        NodeOutcome::Fail { code, .. } => assert_eq!(code, "bpm_documents.principal_invalid"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(case_file.entries(&case).unwrap().is_empty());
}

#[test]
fn a_permission_denial_does_not_leak() {
    let engine = engine_without_permission();
    let case_file = Arc::new(MemoryCaseFile::new());
    let ops = Arc::new(EngineDocumentOps::new(engine, site(), case_file.clone()));
    let handler = DocumentNodeHandler::new(ops);
    let case = CaseId::new("case_denied").unwrap();
    let mut variables = json!({ "secret": "CANARY-VALUE" });
    let config = json!({
        "operation": "create_from",
        "doctype": "sales_order",
        "mapping": { "title": "doc.secret" },
    });
    // A user the empty permission snapshot knows nothing about.
    let user = spot_bpm_documents::ids::record_id_for("user1").to_string();
    let outcome = run_node(
        &handler,
        &case,
        "order",
        1,
        &config,
        Some(&user),
        &mut variables,
    );
    match outcome {
        NodeOutcome::Fail { code, reason } => {
            assert_eq!(code, "permission.denied");
            assert!(!reason.contains("CANARY-VALUE"), "no field value leaks");
            assert!(!format!("{reason:?}").contains("CANARY-VALUE"));
        }
        other => panic!("expected a permission failure, got {other:?}"),
    }
    assert!(case_file.entries(&case).unwrap().is_empty());
}

/// An engine whose authorizer is the permission engine over an empty snapshot: every user check
/// refuses.
fn engine_without_permission() -> Arc<spot_document::engine::Engine<spot_storage_fake::FakeBackend>>
{
    use spot_document::engine::{Engine, MemoryPrincipals, PermissionAuthorizer, V7Ids};
    use spot_permissions::{PermissionEngine, Snapshot};

    let snapshot = Snapshot::empty(&common::meta()).unwrap();
    let authorizer = Arc::new(PermissionAuthorizer::new(
        Arc::new(PermissionEngine::new(Arc::new(snapshot))),
        Arc::new(MemoryPrincipals::new()),
    ));
    let seams = Arc::new(spot_seams::RegistryBuilder::new().build().unwrap());
    let engine = Engine::new(
        spot_storage_fake::FakeBackend::new(),
        common::meta(),
        seams,
        Arc::new(spot_core::ManualClock::new(common::now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_authorizer(authorizer);
    block_on(engine.prepare()).unwrap();
    Arc::new(engine)
}

#[test]
fn a_replay_returns_the_first_result() {
    let engine = engine();
    let case_file = Arc::new(MemoryCaseFile::new());
    let ops = Arc::new(EngineDocumentOps::new(
        engine.clone(),
        site(),
        case_file.clone(),
    ));
    let handler = DocumentNodeHandler::new(ops);
    let case = CaseId::new("case_replay").unwrap();
    let mut variables = json!({ "title": "SO-0002" });
    let config = json!({
        "operation": "create_from",
        "doctype": "sales_order",
        "mapping": { "title": "doc.title" },
    });
    let first = run_node(&handler, &case, "order", 1, &config, None, &mut variables);
    let second = run_node(&handler, &case, "order", 1, &config, None, &mut variables);
    assert_eq!(first, second, "a replay returns the first result");
    // Exactly one document and one Case-file entry.
    assert_eq!(case_file.entries(&case).unwrap().len(), 1);
    let id = id_of(&first);
    assert!(block_on(engine.peek("sales_order", id)).unwrap().is_some());
}

#[test]
fn submit_then_cancel_runs_through_the_lifecycle() {
    let engine = engine();
    let case_file = Arc::new(MemoryCaseFile::new());
    let ops = Arc::new(EngineDocumentOps::new(
        engine.clone(),
        site(),
        case_file.clone(),
    ));
    let handler = DocumentNodeHandler::new(ops);
    let case = CaseId::new("case_submit").unwrap();
    let mut variables = json!({ "title": "SO-0003" });

    let create = json!({
        "operation": "create_from",
        "doctype": "sales_order",
        "mapping": { "title": "doc.title" },
    });
    let created = run_node(&handler, &case, "order", 1, &create, None, &mut variables);
    let order_id = id_of(&created);
    assert_eq!(
        block_on(engine.peek("sales_order", order_id))
            .unwrap()
            .unwrap()
            .state(),
        Some(DocState::Draft)
    );

    let submit = json!({
        "operation": "submit",
        "doctype": "sales_order",
        "document": "doc.order.id",
    });
    let _ = run_node(&handler, &case, "submit", 1, &submit, None, &mut variables);
    assert_eq!(
        block_on(engine.peek("sales_order", order_id))
            .unwrap()
            .unwrap()
            .state(),
        Some(DocState::Posted)
    );

    let cancel = json!({
        "operation": "cancel",
        "doctype": "sales_order",
        "document": "doc.order.id",
    });
    let outcome = run_node(&handler, &case, "cancel", 1, &cancel, None, &mut variables);
    assert!(matches!(outcome, NodeOutcome::Done { .. }));
    assert_eq!(
        block_on(engine.peek("sales_order", order_id))
            .unwrap()
            .unwrap()
            .state(),
        Some(DocState::Cancelled)
    );
    // The Case file still lists the order once, with its latest operation.
    let entries = case_file.entries(&case).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].operation, Operation::Cancel);
}

#[test]
fn a_submit_declares_cancel_as_its_compensation() {
    assert_eq!(Operation::Submit.compensation(), Some(Operation::Cancel));
    let engine = engine();
    let case_file = Arc::new(MemoryCaseFile::new());
    let ops = Arc::new(EngineDocumentOps::new(engine, site(), case_file.clone()));
    let handler = DocumentNodeHandler::new(ops);
    let case = CaseId::new("case_comp").unwrap();
    let mut variables = json!({ "title": "SO-0004" });
    let create = json!({
        "operation": "create_from",
        "doctype": "sales_order",
        "mapping": { "title": "doc.title" },
    });
    let created = run_node(&handler, &case, "order", 1, &create, None, &mut variables);
    let order_id = id_of(&created);
    let submit = json!({
        "operation": "submit",
        "doctype": "sales_order",
        "document": "doc.order.id",
    });
    let outcome = run_node(&handler, &case, "submit", 1, &submit, None, &mut variables);
    match outcome {
        NodeOutcome::Done {
            output: Some(output),
            ..
        } => assert_eq!(output["compensation"], json!("cancel")),
        other => panic!("expected a done outcome, got {other:?}"),
    }
    let entry = case_file.entries(&case).unwrap();
    assert_eq!(entry[0].compensation, Some(Operation::Cancel));
    assert_eq!(entry[0].document, order_id);
}

#[test]
fn a_map_operation_writes_variables_only() {
    let engine = engine();
    let case_file = Arc::new(MemoryCaseFile::new());
    let ops = Arc::new(EngineDocumentOps::new(engine, site(), case_file.clone()));
    let handler = DocumentNodeHandler::new(ops);
    let case = CaseId::new("case_map").unwrap();
    let mut variables = json!({ "qty": 3 });
    let config = json!({
        "operation": "map",
        "doctype": "note",
        "mapping": { "total": "doc.qty * 2" },
    });
    let outcome = run_node(&handler, &case, "compute", 1, &config, None, &mut variables);
    assert!(matches!(outcome, NodeOutcome::Done { .. }));
    assert_eq!(variables["compute"]["total"], json!(6));
    assert!(case_file.entries(&case).unwrap().is_empty());
}

#[test]
fn a_success_takes_the_true_branch() {
    let engine = engine();
    let case_file = Arc::new(MemoryCaseFile::new());
    let ops = Arc::new(EngineDocumentOps::new(engine, site(), case_file.clone()));
    let handler = DocumentNodeHandler::new(ops);
    let case = CaseId::new("case_branch").unwrap();
    let mut variables = json!({ "title": "SO-0005", "large": true });
    let config = json!({
        "operation": "create_from",
        "doctype": "sales_order",
        "mapping": { "title": "doc.title" },
    });
    let outgoing = vec![
        OutgoingEdge {
            to: NodeId::new("approve").unwrap(),
            condition: Some("doc.large == true".to_owned()),
            error: false,
        },
        OutgoingEdge {
            to: NodeId::new("review").unwrap(),
            condition: Some("doc.large == false".to_owned()),
            error: false,
        },
    ];
    let outcome = run_node_with_edges(
        &handler,
        &case,
        "order",
        1,
        &config,
        None,
        &mut variables,
        &outgoing,
    );
    match outcome {
        NodeOutcome::Done { branches, .. } => {
            assert_eq!(branches, vec!["doc.large == true".to_owned()]);
        }
        other => panic!("expected a done outcome, got {other:?}"),
    }
}
