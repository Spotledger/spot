//! The governance audit entry: its shape, the outbox event it becomes, and its atomicity with
//! the record write.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use spot_app_api::context::Actor;
use spot_app_api::prelude::{Decimal, RecordId};
use spot_bpm_governance::{GOVERNANCE_TOPIC, GovernanceAudit, enqueue_governance, outbox_event_of};
use spot_storage_contract::{
    Catalog, ChangeFeed, DocumentData, DocumentStore, FeedPosition, Fields, Isolation, RowLock,
    StorageBackend, Transaction, TxOptions, Value, contract_tests,
};
use spot_storage_fake::FakeBackend;

/// A `party` document with every optional field null (the contract catalog's entity).
fn party(name: &str) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("name"), Value::Text(name.to_owned()));
    fields.insert(n("credit_limit"), Value::Decimal(Decimal::new(10_000, 2)));
    fields.insert(n("parent_party"), Value::Null);
    for field in [
        "note", "since", "opens_at", "seen_at", "active", "visits", "profile", "logo",
    ] {
        fields.insert(n(field), Value::Null);
    }
    DocumentData {
        entity: n("party"),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    }
}

async fn backend() -> (FakeBackend, Arc<Catalog>) {
    let backend = FakeBackend::new();
    let catalog = Arc::new(contract_tests::catalog().unwrap());
    backend.prepare(&catalog).await.unwrap();
    (backend, catalog)
}

async fn begin(
    backend: &FakeBackend,
    catalog: &Arc<Catalog>,
) -> <FakeBackend as StorageBackend>::Tx {
    backend
        .begin(
            catalog.clone(),
            TxOptions::new(n("governance_test"), Isolation::ReadCommitted),
        )
        .await
        .unwrap()
}

async fn governance_events(backend: &FakeBackend) -> usize {
    backend
        .read_outbox(FeedPosition::START, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|record| record.event.topic.as_str() == GOVERNANCE_TOPIC)
        .count()
}

async fn party_exists(backend: &FakeBackend, catalog: &Arc<Catalog>, id: RecordId) -> bool {
    let mut tx = begin(backend, catalog).await;
    let loaded = tx.load(&n("party"), id, RowLock::None).await.unwrap();
    tx.rollback().await.unwrap();
    loaded.is_some()
}

fn entry(actor: &Actor, id: RecordId) -> GovernanceAudit {
    GovernanceAudit::outside_process(
        actor,
        "acme",
        "note",
        id,
        "approved by the controller",
        Some("command-1"),
        now(),
    )
}

#[test]
fn the_entry_carries_who_what_why_and_which_policy() {
    let actor = user_actor();
    let id = RecordId::new_v7();
    let audit = entry(&actor, id);
    let record = audit.to_record();
    assert_eq!(record.kind, "bpm.governance.outside_process");
    assert_eq!(record.actor, format!("user:{}", user_id()));
    let subject = record.subject.expect("the entry names the record");
    assert_eq!(subject.doctype, "note");
    assert_eq!(subject.id, id.to_string());
    let payload = record.payload;
    assert_eq!(
        payload.get("company"),
        Some(&spot_audit_chain::PayloadValue::Text("acme".to_owned()))
    );
    assert_eq!(
        payload.get("policy"),
        Some(&spot_audit_chain::PayloadValue::Text(
            "bpm.governance.policy".to_owned()
        ))
    );
    assert_eq!(
        payload.get("reason"),
        Some(&spot_audit_chain::PayloadValue::Text(
            "approved by the controller".to_owned()
        ))
    );
    assert!(record.masked, "no document value is ever put in an entry");
}

#[test]
fn the_outbox_event_uses_the_governance_topic() {
    let actor = user_actor();
    let audit = entry(&actor, RecordId::new_v7());
    let event = outbox_event_of(&audit).unwrap();
    assert_eq!(event.topic.as_str(), GOVERNANCE_TOPIC);
    assert_eq!(event.subject.entity.as_str(), "note");
}

#[test]
fn a_policy_change_event_has_a_subject_too() {
    let actor = user_actor();
    let audit = GovernanceAudit::policy_change(
        &actor,
        "acme",
        "note",
        spot_bpm_governance::GovernanceMode::Governed,
        "free to governed",
        now(),
    );
    let event = outbox_event_of(&audit).unwrap();
    assert_eq!(event.subject.entity.as_str(), "bpm_governance_policy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_audit_commits_with_the_record_and_rolls_back_with_it() {
    let (backend, catalog) = backend().await;
    let actor = user_actor();

    // One transaction writes the record and its governance audit; both commit together.
    let committed = party("Committed");
    let mut tx = begin(&backend, &catalog).await;
    tx.insert(&committed).await.unwrap();
    enqueue_governance(&mut tx, &entry(&actor, committed.id))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        party_exists(&backend, &catalog, committed.id).await,
        "the record committed"
    );
    assert_eq!(
        governance_events(&backend).await,
        1,
        "the audit committed with the record"
    );

    // A rolled-back transaction leaves neither the record nor the audit.
    let rolled_back = party("RolledBack");
    let mut tx = begin(&backend, &catalog).await;
    tx.insert(&rolled_back).await.unwrap();
    enqueue_governance(&mut tx, &entry(&actor, rolled_back.id))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(
        !party_exists(&backend, &catalog, rolled_back.id).await,
        "the record rolled back"
    );
    assert_eq!(
        governance_events(&backend).await,
        1,
        "no audit survived the rollback"
    );
}

#[test]
fn a_failed_audit_write_fails_the_operation_closed() {
    // The enqueue refuses an event with no valid subject, so a caller cannot silently skip its
    // audit.
    let actor = Actor::System {
        component: n("import"),
    };
    let audit = GovernanceAudit::outside_process(
        &actor,
        "acme",
        "not a storage name!",
        RecordId::new_v7(),
        "reason",
        None,
        now(),
    );
    assert!(outbox_event_of(&audit).is_err());
}

/// The subject version is absent at pre-create: the record has no version yet.
#[test]
fn the_create_event_has_no_subject_version() {
    let actor = user_actor();
    let audit = entry(&actor, RecordId::new_v7());
    let event = outbox_event_of(&audit).unwrap();
    assert_eq!(event.subject_version, None);
    assert_eq!(event.schema_version, 1);
}
