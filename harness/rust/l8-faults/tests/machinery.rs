//! The audit's classification on hand-built histories and stores.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_l7_invariants::history::{History, HistoryEntry, Level, OpKind, Outcome, Role, Touched};
use spot_l7_invariants::invariant::CheckContext;
use spot_l7_invariants::model::Model;
use spot_l7_invariants::oracle::{Effects, Oracle};
use spot_l8_faults::audit::DurabilityAudit;
use spot_storage_contract::{
    DocumentData, DocumentStore, Fields, Isolation, Name, RecordId, StorageBackend, Transaction,
    TxOptions, Value,
};
use spot_storage_fake::FakeBackend;

#[test]
fn sequence_gaps_are_found_and_clean_series_pass() {
    let mut oracle = Oracle::new();
    let mut clean = Effects::new(1);
    clean.numbers = vec![
        ("s".to_owned(), 1),
        ("s".to_owned(), 2),
        ("s".to_owned(), 3),
    ];
    oracle.apply(&clean);
    assert!(DurabilityAudit::sequence_gaps(&oracle).is_empty());

    let mut gap = Effects::new(2);
    gap.numbers = vec![("s".to_owned(), 4), ("s".to_owned(), 6)];
    oracle.apply(&gap);
    let violations = DurabilityAudit::sequence_gaps(&oracle);
    assert_eq!(violations.len(), 1);
    assert!(
        violations[0].detail.contains("missing=5"),
        "{}",
        violations[0].detail
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unacknowledged_operation_that_is_wholly_present_is_a_violation() {
    let model = Arc::new(Model::new().unwrap());
    let backend = FakeBackend::new();
    backend.prepare(&model.catalog).await.unwrap();

    // A real document in the store.
    let options = TxOptions::new(Name::new("t").unwrap(), Isolation::ReadCommitted);
    let mut tx = backend.begin(model.catalog.clone(), options).await.unwrap();
    let id = RecordId::new_v7();
    let mut fields = Fields::new();
    fields.insert(model.name.clone(), Value::Text("party".to_owned()));
    fields.insert(model.marker.clone(), Value::Text("m1".to_owned()));
    let data = DocumentData {
        entity: model.party.clone(),
        id,
        fields,
        children: BTreeMap::new(),
    };
    let version = tx.insert(&data).await.unwrap();
    tx.commit().await.unwrap();

    // A history that claims the operation's commit outcome was resolved as absent.
    let mut history = History::new(1, 1, "smoke");
    history.entries = vec![HistoryEntry {
        worker: 0,
        seq: 0,
        start_us: 0,
        end_us: 0,
        kind: OpKind::CreateParty,
        isolation: Level::ReadCommitted,
        outcome: Outcome::Indeterminate {
            resolved: Some(false),
        },
        attempts: 1,
        touched: vec![Touched {
            entity: model.party.to_string(),
            id: id.to_string(),
            role: Role::Created,
            version: Some(version.0),
        }],
        values: BTreeMap::new(),
    }];

    let backend = Arc::new(backend);
    let ctx = CheckContext::for_backend(
        backend,
        model.catalog.clone(),
        Arc::new(history),
        Arc::new(Oracle::new()),
    );
    let audit = DurabilityAudit::run(&ctx).await;
    println!("{}", audit.table());
    assert!(!audit.ok);
    assert!(
        audit
            .violations
            .iter()
            .any(|v| v.detail.starts_with("unacknowledged_effect_present")),
        "got {:?}",
        audit.violations
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unresolved_indeterminate_commit_is_a_violation() {
    let model = Arc::new(Model::new().unwrap());
    let backend = Arc::new(FakeBackend::new());
    backend.prepare(&model.catalog).await.unwrap();
    let mut history = History::new(1, 1, "smoke");
    history.entries = vec![HistoryEntry {
        worker: 0,
        seq: 0,
        start_us: 0,
        end_us: 0,
        kind: OpKind::Read,
        isolation: Level::ReadCommitted,
        outcome: Outcome::Indeterminate { resolved: None },
        attempts: 1,
        touched: Vec::new(),
        values: BTreeMap::new(),
    }];
    let ctx = CheckContext::for_backend(
        backend,
        model.catalog.clone(),
        Arc::new(history),
        Arc::new(Oracle::new()),
    );
    let audit = DurabilityAudit::run(&ctx).await;
    assert!(!audit.ok);
    assert!(
        audit
            .violations
            .iter()
            .any(|v| v.detail == "unresolved_indeterminate_commit")
    );
}
