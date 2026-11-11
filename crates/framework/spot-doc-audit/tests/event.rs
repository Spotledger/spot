//! The audit event: one per operation, atomic with the transaction, masked, capped, and never
//! written for a business DocType that opted out.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{Document, HookPoint, Operation, Phase, Target};
use spot_core::ManualClock;
use spot_doc_audit::{
    AUDIT_OFF_DOCTYPES, AUDIT_TOPIC, AuditSource, AuditStep, DiffChange, DiffValue, DocAuditEvent,
    DocOperation, EventSizePolicy, FailingWriter, NoBeforeImages, build_event, cap_event,
    enqueue_audit, is_audit_off, outbox_event_of, record_of,
};
use spot_storage_contract::feed::{ChangeFeed, FeedPosition};
use spot_storage_contract::{
    Catalog, Isolation, StorageBackend, Transaction, TxOptions, contract_tests,
};
use spot_storage_fake::FakeBackend;

async fn backend() -> (FakeBackend, Arc<Catalog>) {
    let b = FakeBackend::new();
    let cat = Arc::new(contract_tests::catalog().unwrap());
    b.prepare(&cat).await.unwrap();
    (b, cat)
}

async fn begin(b: &FakeBackend, cat: &Arc<Catalog>) -> <FakeBackend as StorageBackend>::Tx {
    b.begin(
        cat.clone(),
        TxOptions::new(
            Name::new("docaudit_test").unwrap(),
            Isolation::ReadCommitted,
        ),
    )
    .await
    .unwrap()
}

async fn audit_events(b: &FakeBackend) -> Vec<spot_storage_contract::feed::OutboxRecord> {
    b.read_outbox(FeedPosition::START, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.event.topic.as_str() == AUDIT_TOPIC)
        .collect()
}

fn operations() -> Vec<DocOperation> {
    vec![
        DocOperation::Create,
        DocOperation::Update,
        DocOperation::Delete,
        DocOperation::Post,
        DocOperation::Cancel,
        DocOperation::Rename,
        DocOperation::Correct,
        DocOperation::Method("submit".to_owned()),
    ]
}

/// Every operation writes exactly one audit event, in the same transaction as the change.
#[tokio::test]
async fn every_operation_writes_one_event_in_the_transaction() {
    let (b, cat) = backend().await;
    for (i, op) in operations().into_iter().enumerate() {
        let event = event(&id_of(u64::try_from(i).unwrap() + 1), 1, op, "note");
        let mut tx = begin(&b, &cat).await;
        enqueue_audit(&mut tx, &event).await.unwrap();
        tx.commit().await.unwrap();
    }
    let records = audit_events(&b).await;
    assert_eq!(
        records.len(),
        operations().len(),
        "exactly one audit event per operation"
    );
    for (record, op) in records.iter().zip(operations()) {
        let doc = spot_events_contract::payload::PayloadDocument::from_outbox_event(&record.event)
            .unwrap();
        let decoded: DocAuditEvent = serde_json::from_value(doc.data).unwrap();
        assert_eq!(decoded.operation, op);
        assert_eq!(record.event.topic.as_str(), AUDIT_TOPIC);
    }
}

/// A rolled-back transaction leaves no audit event: the event is atomic with the change.
#[tokio::test]
async fn a_rolled_back_transaction_writes_no_event() {
    let (b, cat) = backend().await;
    let event = event(&id_of(1), 1, DocOperation::Create, "note");
    let mut tx = begin(&b, &cat).await;
    enqueue_audit(&mut tx, &event).await.unwrap();
    tx.rollback().await.unwrap();
    assert!(
        audit_events(&b).await.is_empty(),
        "a rollback must write none"
    );

    let mut tx = begin(&b, &cat).await;
    enqueue_audit(&mut tx, &event).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        audit_events(&b).await.len(),
        1,
        "a commit writes exactly one"
    );
}

/// A failing outbox write fails the operation: the step returns an error and nothing is written.
#[test]
fn a_failing_write_fails_the_step() {
    let step = AuditStep::new(
        DOCTYPE,
        Arc::new(FailingWriter),
        masker(),
        Arc::new(NoBeforeImages),
        Arc::new(ManualClock::new(
            spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
        )),
    );
    let mut doc = Document::new(name(DOCTYPE), RecordId::new_v7());
    doc.set(name(TITLE), Value::Text("note".to_owned()));
    let point = HookPoint::of(Phase::After, Target::Op(Operation::Create));
    let err = step
        .build_and_write(&point, &doc, &invocation("alice"))
        .unwrap_err();
    assert_eq!(err.code(), spot_doc_audit::error::OUTBOX_WRITE);
}

/// The stored form masks a secured field and keeps a plain one (ADR-070 section 12 rule 9).
#[test]
fn the_stored_form_masks_secured_fields() {
    let m = masker();
    let after = image(&id_of(1), 1, "hello", "4111111111111111");
    let event = build_event(
        &*m,
        "test.spotledger.app",
        DocOperation::Create,
        &after,
        None,
        user_actor("alice"),
        1_700_000_000_000_000,
        None,
        AuditSource::Api,
        &EventSizePolicy::default(),
    )
    .unwrap();
    let secret = event
        .diff
        .iter()
        .find(|c| c.path == SECRET)
        .expect("the secret changed");
    assert_eq!(secret.after, Some(DiffValue::Text("***".to_owned())));
    let title = event
        .diff
        .iter()
        .find(|c| c.path == TITLE)
        .expect("the title changed");
    assert_eq!(title.after, Some(DiffValue::Text("hello".to_owned())));
    // The chain record asserts the mask and carries the masked event.
    let record = record_of(&event).unwrap();
    assert!(record.masked);
    assert_eq!(record.kind, "doc.create");
}

/// The truncation rule: a diff over the cap keeps the first N changes, the digest of the full
/// diff, and sets `truncated`.
#[test]
fn a_large_event_is_truncated_with_the_full_diff_digest() {
    let mut event = event(&id_of(1), 1, DocOperation::Update, "note");
    event.diff = (0..20)
        .map(|i| DiffChange {
            path: format!("field_{i}"),
            before: None,
            after: Some(DiffValue::Text("x".repeat(50))),
        })
        .collect();
    let policy = EventSizePolicy {
        max_bytes: 400,
        max_changes: 3,
        long_value_bytes: 256,
    };
    cap_event(&mut event, &policy).unwrap();
    assert!(event.truncated);
    assert_eq!(event.diff.len(), 3);
    assert!(event.diff_digest.is_some(), "the full diff digest is kept");
}

/// Impersonation records both identities, on the event and in the chain record.
#[test]
fn impersonation_records_both_identities() {
    let mut event = event(&id_of(1), 1, DocOperation::Update, "note");
    event.actor = spot_doc_audit::ActorRef {
        actor: format!("user:{}", id_of(1_001)),
        on_behalf_of: Some(format!("user:{}", id_of(1_002))),
        component: None,
    };
    let record = record_of(&event).unwrap();
    assert_eq!(record.actor, format!("user:{}", id_of(1_001)));
    assert_eq!(record.on_behalf_of, Some(format!("user:{}", id_of(1_002))));
    let outbox = outbox_event_of(&event).unwrap();
    let doc = spot_events_contract::payload::PayloadDocument::from_outbox_event(&outbox).unwrap();
    match doc.meta.actor {
        spot_events_contract::Actor::User { via, .. } => {
            assert!(via.is_some(), "the impersonated identity is in the meta");
        }
        other => panic!("unexpected actor {other:?}"),
    }
}

/// The audit-off list holds only framework-internal DocTypes, and it may not change silently.
#[test]
fn the_audit_off_list_is_framework_internal_only() {
    let expected: std::collections::BTreeSet<&str> = [
        "audit_chain",
        "audit_checkpoint",
        "doc_audit_event",
        "sys_telemetry",
    ]
    .into_iter()
    .collect();
    let actual: std::collections::BTreeSet<&str> = AUDIT_OFF_DOCTYPES.iter().copied().collect();
    assert_eq!(
        actual, expected,
        "adding a DocType to the audit-off list is a deliberate review decision"
    );
    for business in ["note", "sales_invoice", "customer", "user", "role"] {
        assert!(
            !is_audit_off(business),
            "a business DocType `{business}` may not opt out"
        );
    }
}

/// An audit-off DocType produces no event.
#[test]
fn an_audit_off_doctype_produces_no_event() {
    let writer = RecordingWriter::new();
    let step = AuditStep::new(
        "audit_chain",
        writer.clone(),
        masker(),
        Arc::new(NoBeforeImages),
        Arc::new(ManualClock::new(
            spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
        )),
    );
    let mut doc = Document::new(name("audit_chain"), RecordId::new_v7());
    doc.set(name(TITLE), Value::Text("x".to_owned()));
    let point = HookPoint::of(Phase::After, Target::Op(Operation::Create));
    step.build_and_write(&point, &doc, &invocation("alice"))
        .unwrap();
    assert!(writer.events().is_empty());
}

/// A step writes exactly one event for a normal operation.
#[test]
fn the_step_writes_one_event() {
    let writer = RecordingWriter::new();
    let step = AuditStep::new(
        DOCTYPE,
        writer.clone(),
        masker(),
        Arc::new(NoBeforeImages),
        Arc::new(ManualClock::new(
            spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
        )),
    );
    let mut doc = Document::new(name(DOCTYPE), RecordId::new_v7());
    doc.set(name(TITLE), Value::Text("note".to_owned()));
    doc.set(name(SECRET), Value::Text("4111111111111111".to_owned()));
    let point = HookPoint::of(Phase::After, Target::Op(Operation::Create));
    step.build_and_write(&point, &doc, &invocation("alice"))
        .unwrap();
    let events = writer.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].operation, DocOperation::Create);
    let secret = events[0].diff.iter().find(|c| c.path == SECRET).unwrap();
    assert_eq!(secret.after, Some(DiffValue::Text("***".to_owned())));
}
