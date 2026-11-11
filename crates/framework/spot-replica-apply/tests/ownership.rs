//! Receive-side ownership: `spot-partition`'s owner check over a received record
//! (WP-EDG-06.T3, ADR-062 G1, ADR-063).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{apply_once, backend, begin, dec, fields, item_document, load, name, record, source};
use spot_partition::{AssignmentRequest, MemoryPartitionRegistry, PartitionRegistry, ScopeKey};
use spot_replica_apply::{
    ApplyCtx, ApplyOutcome, PartitionOwners, Reason, ReceivedRecord, RecordingAudit,
    RecordingNoticeSink, RecordingQuarantine, ScopeFields, apply_quarantining,
};
use spot_storage_contract::{ChangeFeed, DocumentData, FeedPosition, RecordId, Value};
use spot_sync_protocol::{NodeId, SourceId};
use time::OffsetDateTime;

/// Unix microseconds, the unit [`ApplyCtx::at`] carries.
fn at(seconds: i64) -> i64 {
    seconds * 1_000_000
}

fn instant(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

fn node(id: &str) -> NodeId {
    NodeId::new(id).unwrap()
}

fn hub() -> NodeId {
    node("head.office.app#hub")
}

fn branch() -> NodeId {
    node("edge-khi#branch_07")
}

fn warehouse(value: &str) -> ScopeKey {
    ScopeKey::new("warehouse", value).unwrap()
}

/// A registry where `(warehouse, W7)` belongs to `owner` over `[from, to)`.
fn registry(owner: &NodeId, from: i64, to: Option<i64>) -> Arc<MemoryPartitionRegistry> {
    let registry = MemoryPartitionRegistry::new();
    registry
        .assign(AssignmentRequest {
            scope: warehouse("W7"),
            node: owner.clone(),
            effective_from: instant(from),
            effective_to: to.map(instant),
            actor: "admin".to_owned(),
            reason: "the warehouse's branch".to_owned(),
            at: instant(from),
        })
        .unwrap();
    Arc::new(registry)
}

/// A [`PartitionOwners`] over `registry`, reading the warehouse scope from the `warehouse` field.
fn owners(registry: Arc<MemoryPartitionRegistry>) -> PartitionOwners {
    PartitionOwners::new(
        registry,
        Arc::new(ScopeFields::new().with("warehouse", "warehouse")),
        hub(),
    )
}

fn stock(id: RecordId, warehouse_value: Value) -> DocumentData {
    DocumentData {
        entity: name("stock_entry"),
        id,
        fields: fields(&[
            ("warehouse", warehouse_value),
            ("qty", dec("2.00")),
            ("origin_node", Value::Null),
            ("origin_id", Value::Null),
            ("origin_version", Value::Int(0)),
        ]),
        children: BTreeMap::new(),
    }
}

fn stock_record(sender: &SourceId, document: DocumentData, version: u64) -> ReceivedRecord {
    let mut received = record(document, version, version, vec![]);
    received.source = sender.clone();
    received
}

fn from_branch() -> SourceId {
    SourceId::new(branch().as_str()).unwrap()
}

#[tokio::test]
async fn the_owner_is_accepted() {
    let backend = backend();
    let audit = RecordingAudit::new();
    let owners = owners(registry(&branch(), 1_000, None));
    let ctx = ApplyCtx::new(&owners, &audit, at(2_000));
    let id = RecordId::new_v7();
    let received = stock_record(&from_branch(), stock(id, Value::Text("W7".to_owned())), 1);

    let outcome = apply_once(&backend, &received, &ctx).await;
    assert_eq!(outcome, ApplyOutcome::Stored, "{outcome:?}");

    let stored = load(&backend, "stock_entry", id).await.expect("stored");
    assert_eq!(
        stored.data.fields.get(&name("origin_node")),
        Some(&Value::Text(branch().to_string())),
        "a record from another node is a mirror of it"
    );
}

#[tokio::test]
async fn a_non_owner_is_refused_and_quarantined() {
    let backend = backend();
    let audit = RecordingAudit::new();
    let owners = owners(registry(&branch(), 1_000, None));
    let port = RecordingQuarantine::new();
    let ctx = ApplyCtx::new(&owners, &audit, at(2_000));
    let id = RecordId::new_v7();
    let received = stock_record(&source(), stock(id, Value::Text("W7".to_owned())), 1);

    let tx = begin(&backend).await;
    let outcome = apply_quarantining(tx, &received, &ctx, &port).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.not_owner"));
    assert_eq!(port.len(), 1, "the refusal is handed to the queue");
    assert_eq!(port.held()[0].0.name, received.name);
    assert_eq!(port.held()[0].1.code(), "replica_apply.not_owner");
    assert!(load(&backend, "stock_entry", id).await.is_none());
}

#[tokio::test]
async fn ownership_moves_between_items_by_the_items_instant() {
    // W7 belongs to branch 07 until t=2000, then to the hub.
    let registry = MemoryPartitionRegistry::new();
    registry
        .assign(AssignmentRequest {
            scope: warehouse("W7"),
            node: branch(),
            effective_from: instant(1_000),
            effective_to: Some(instant(2_000)),
            actor: "admin".to_owned(),
            reason: "branch 7 runs the warehouse".to_owned(),
            at: instant(1_000),
        })
        .unwrap();
    registry
        .assign(AssignmentRequest {
            scope: warehouse("W7"),
            node: hub(),
            effective_from: instant(2_000),
            effective_to: None,
            actor: "admin".to_owned(),
            reason: "the warehouse moves to head office".to_owned(),
            at: instant(2_000),
        })
        .unwrap();
    let owners = owners(Arc::new(registry));
    let backend = backend();

    let audit = RecordingAudit::new();
    let port = RecordingQuarantine::new();
    let early = ApplyCtx::new(&owners, &audit, at(1_500));
    let before = stock_record(
        &source(),
        stock(RecordId::new_v7(), Value::Text("W7".to_owned())),
        1,
    );
    let tx = begin(&backend).await;
    assert_eq!(
        apply_quarantining(tx, &before, &early, &port)
            .await
            .reason_code(),
        Some("replica_apply.not_owner"),
        "before the move the hub does not own W7"
    );

    let late = ApplyCtx::new(&owners, &audit, at(2_500));
    let after = stock_record(
        &source(),
        stock(RecordId::new_v7(), Value::Text("W7".to_owned())),
        1,
    );
    let tx = begin(&backend).await;
    assert_eq!(
        apply_quarantining(tx, &after, &late, &port).await,
        ApplyOutcome::Stored,
        "after the move the same sender owns W7"
    );
    assert_eq!(port.len(), 1, "only the early item was quarantined");
}

#[tokio::test]
async fn a_document_without_a_partition_scope_is_accepted() {
    let backend = backend();
    let audit = RecordingAudit::new();
    // A configured scope field that this document leaves empty.
    let owners = owners(registry(&branch(), 1_000, None));
    let ctx = ApplyCtx::new(&owners, &audit, at(2_000));
    let id = RecordId::new_v7();
    let received = stock_record(&source(), stock(id, Value::Null), 1);

    let outcome = apply_once(&backend, &received, &ctx).await;
    assert_eq!(outcome, ApplyOutcome::Stored, "{outcome:?}");
    assert!(load(&backend, "stock_entry", id).await.is_some());
}

#[tokio::test]
async fn a_refusal_writes_nothing_and_emits_nothing() {
    let backend = backend();
    let audit = RecordingAudit::new();
    let notices = RecordingNoticeSink::new();
    let owners = owners(registry(&branch(), 1_000, None));
    let port = RecordingQuarantine::new();
    let ctx = ApplyCtx::new(&owners, &audit, at(2_000))
        .with_notify(true)
        .with_notice(&notices);
    let id = RecordId::new_v7();
    let mut received = stock_record(&source(), stock(id, Value::Text("W7".to_owned())), 1);
    received.topic = Some(name("stock"));

    let tx = begin(&backend).await;
    let outcome = apply_quarantining(tx, &received, &ctx, &port).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.not_owner"));

    assert!(load(&backend, "stock_entry", id).await.is_none());
    let outbox = backend
        .read_outbox(FeedPosition::START, 100)
        .await
        .expect("the outbox reads");
    assert!(outbox.is_empty(), "no outbox event: {outbox:?}");
    assert!(audit.is_empty(), "no replicated marker");
    assert!(notices.is_empty(), "no delivery notice");
}

/// A controller/hook/script/emitter that panics if the refusal path ever calls it.
struct PanicSideEffects;

impl spot_replica_apply::SideEffects for PanicSideEffects {
    fn run_hooks(&self, _entity: &str) {
        panic!("a lifecycle hook ran during a refusal");
    }
    fn allocate_name(&self, _entity: &str) -> String {
        panic!("a naming series ran during a refusal");
    }
    fn run_script(&self, _entity: &str) {
        panic!("a server script ran during a refusal");
    }
    fn emit_event(&self, _entity: &str) {
        panic!("the outbox emitter ran during a refusal");
    }
}

#[tokio::test]
async fn a_refusal_runs_no_business_logic() {
    let backend = backend();
    let audit = RecordingAudit::new();
    let owners = owners(registry(&branch(), 1_000, None));
    let port = RecordingQuarantine::new();
    let sentinel = PanicSideEffects;
    let ctx = ApplyCtx::new(&owners, &audit, at(2_000)).with_side_effects(&sentinel);
    let id = RecordId::new_v7();
    let received = stock_record(&source(), stock(id, Value::Text("W7".to_owned())), 1);

    let tx = begin(&backend).await;
    // If any business logic ran, this panics.
    let outcome = apply_quarantining(tx, &received, &ctx, &port).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.not_owner"));
    assert_eq!(port.len(), 1);
}

#[tokio::test]
async fn a_refusal_does_not_block_the_next_record() {
    let backend = backend();
    let audit = RecordingAudit::new();
    let owners = owners(registry(&branch(), 1_000, None));
    let port = RecordingQuarantine::new();
    let ctx = ApplyCtx::new(&owners, &audit, at(2_000));

    // A record the sender does not own...
    let refused = stock_record(
        &source(),
        stock(RecordId::new_v7(), Value::Text("W7".to_owned())),
        1,
    );
    let tx = begin(&backend).await;
    assert_eq!(
        apply_quarantining(tx, &refused, &ctx, &port)
            .await
            .reason_code(),
        Some("replica_apply.not_owner")
    );

    // ...and the next record of another document, which the owner sends, stores.
    let id = RecordId::new_v7();
    let accepted = stock_record(&from_branch(), stock(id, Value::Text("W7".to_owned())), 1);
    let tx = begin(&backend).await;
    assert_eq!(
        apply_quarantining(tx, &accepted, &ctx, &port).await,
        ApplyOutcome::Stored
    );
    assert!(load(&backend, "stock_entry", id).await.is_some());
    assert_eq!(port.len(), 1);
}

#[tokio::test]
async fn redelivery_of_a_refused_item_is_refused_again() {
    let backend = backend();
    let audit = RecordingAudit::new();
    let owners = owners(registry(&branch(), 1_000, None));
    let port = RecordingQuarantine::new();
    let ctx = ApplyCtx::new(&owners, &audit, at(2_000));
    let id = RecordId::new_v7();
    let received = stock_record(&source(), stock(id, Value::Text("W7".to_owned())), 1);

    for _ in 0..2 {
        let tx = begin(&backend).await;
        let outcome = apply_quarantining(tx, &received, &ctx, &port).await;
        assert_eq!(outcome.reason_code(), Some("replica_apply.not_owner"));
    }
    assert_eq!(port.len(), 2, "each delivery is held; none is dropped");
    assert!(load(&backend, "stock_entry", id).await.is_none());
}

#[tokio::test]
async fn a_scope_that_cannot_be_read_is_refused_and_quarantined() {
    let backend = backend();
    let audit = RecordingAudit::new();
    // A configured field name that is not a storage name: the configuration is broken, so
    // ownership cannot be decided and the record is refused rather than guessed at.
    let owners = PartitionOwners::new(
        registry(&branch(), 1_000, None),
        Arc::new(ScopeFields::new().with("warehouse", "not a field")),
        hub(),
    );
    let port = RecordingQuarantine::new();
    let ctx = ApplyCtx::new(&owners, &audit, at(2_000));
    let id = RecordId::new_v7();
    let received = stock_record(&source(), stock(id, Value::Text("W7".to_owned())), 1);

    let tx = begin(&backend).await;
    let outcome = apply_quarantining(tx, &received, &ctx, &port).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.invalid_scope"));
    assert_eq!(port.len(), 1);
    assert!(load(&backend, "stock_entry", id).await.is_none());
}

#[tokio::test]
async fn a_failing_port_is_reported_not_swallowed() {
    struct FailingPort;

    impl spot_replica_apply::QuarantinePort for FailingPort {
        fn hold<'a>(
            &'a self,
            _record: &'a ReceivedRecord,
            _reason: &'a Reason,
            _at: i64,
        ) -> futures::future::BoxFuture<'a, Result<(), Reason>> {
            Box::pin(async move {
                Err(Reason::QuarantineFailed {
                    why: "the queue is down".to_owned(),
                })
            })
        }
    }

    let backend = backend();
    let audit = RecordingAudit::new();
    let owners = owners(registry(&branch(), 1_000, None));
    let ctx = ApplyCtx::new(&owners, &audit, at(2_000));
    let id = RecordId::new_v7();
    let received = stock_record(&source(), stock(id, Value::Text("W7".to_owned())), 1);

    let tx = begin(&backend).await;
    let outcome = apply_quarantining(tx, &received, &ctx, &FailingPort).await;
    assert_eq!(
        outcome.reason_code(),
        Some("replica_apply.quarantine_failed"),
        "a lost refusal must be visible, not silent"
    );
    assert!(load(&backend, "stock_entry", id).await.is_none());
}

#[tokio::test]
async fn a_structural_rejection_is_not_an_ownership_handoff() {
    let backend = backend();
    let audit = RecordingAudit::new();
    let owners = owners(registry(&branch(), 1_000, None));
    let port = RecordingQuarantine::new();
    let ctx = ApplyCtx::new(&owners, &audit, at(2_000));
    let mut unknown = stock(RecordId::new_v7(), Value::Text("W7".to_owned()));
    unknown.entity = name("nope");
    let received = stock_record(&from_branch(), unknown, 1);

    let tx = begin(&backend).await;
    let outcome = apply_quarantining(tx, &received, &ctx, &port).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.unknown_entity"));
    assert!(
        port.is_empty(),
        "the queue's intake adapter holds every rejection; the ownership port only holds G1"
    );
}

#[tokio::test]
async fn a_record_from_another_node_without_a_scope_is_still_a_mirror() {
    // An item has no configured warehouse field, so its scope is empty and its origin owns it.
    // The check accepts it from its sender; the node stores it read-only with its origin.
    let backend = backend();
    let audit = RecordingAudit::new();
    let owners = PartitionOwners::new(
        registry(&branch(), 1_000, None),
        Arc::new(ScopeFields::new()),
        hub(),
    );
    let ctx = ApplyCtx::new(&owners, &audit, at(2_000));
    let id = RecordId::new_v7();
    let mut received = record(item_document(id, "Widget", "1.00", 1), 1, 1, vec![]);
    received.source = from_branch();

    assert!(apply_once(&backend, &received, &ctx).await.is_stored());
    let stored = load(&backend, "item", id).await.expect("stored");
    assert_eq!(
        stored.data.fields.get(&name("origin_node")),
        Some(&Value::Text(branch().to_string()))
    );
}
