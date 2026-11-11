//! The apply-path acceptance tests on the in-memory storage fake (WP-EDG-06.T1).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::{
    Owners, apply_once, backend, begin, dec, fields, gl_document, item_document, load, name,
    record, source,
};
use spot_replica_apply::{
    ApplyCtx, ApplyOutcome, Reason, RecordingAudit, RecordingNoticeSink, SideEffects,
};
use spot_storage_contract::{
    ChangeFeed, DocumentData, FeedPosition, RecordId, StorageBackend, Value,
};

fn ctx<'a>(owners: &'a Owners, audit: &'a RecordingAudit) -> ApplyCtx<'a> {
    ApplyCtx::new(owners, audit, 1_700_000_000_000_000)
}

#[tokio::test]
async fn stores_a_version_with_children_and_entries() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let item_id = RecordId::new_v7();
    let gl_id = RecordId::new_v7();
    let item = item_document(item_id, "Widget", "3.00", 7);
    let gl = gl_document(gl_id, "4000", "12.34");
    let received = record(item.clone(), 7, 1, vec![gl.clone()]);

    let outcome = apply_once(&backend, &received, &ctx(&owners, &audit)).await;
    assert_eq!(outcome, ApplyOutcome::Stored, "{outcome:?}");

    let stored = load(&backend, "item", item_id)
        .await
        .expect("the item is stored");
    assert_eq!(stored.data.entity, item.entity);
    assert_eq!(
        stored.data.fields.get(&name("title")),
        Some(&Value::Text("Widget".to_owned()))
    );
    assert_eq!(
        stored.data.children.get(&name("lines")).map(Vec::len),
        Some(1)
    );
    let gl_stored = load(&backend, "gl_entry", gl_id)
        .await
        .expect("the ledger entry is stored");
    assert_eq!(
        gl_stored.data.fields.get(&name("account")),
        Some(&Value::Text("4000".to_owned()))
    );
}

#[tokio::test]
async fn marks_a_mirror_with_its_origin() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let item_id = RecordId::new_v7();
    let received = record(item_document(item_id, "Widget", "3.00", 4), 4, 1, vec![]);

    assert!(
        apply_once(&backend, &received, &ctx(&owners, &audit))
            .await
            .is_stored()
    );

    let stored = load(&backend, "item", item_id).await.expect("stored");
    assert_eq!(
        stored.data.fields.get(&name("origin_node")),
        Some(&Value::Text(source().as_str().to_owned()))
    );
    assert_eq!(
        stored.data.fields.get(&name("origin_id")),
        Some(&Value::Text(item_id.to_string()))
    );
    assert_eq!(
        stored.data.fields.get(&name("origin_version")),
        Some(&Value::Int(4))
    );
}

#[tokio::test]
async fn an_owned_record_is_not_marked_a_mirror() {
    let backend = backend();
    let owners = Owners {
        owner: source().as_str().to_owned(),
        owned: true,
    };
    let audit = RecordingAudit::new();
    let item_id = RecordId::new_v7();
    let received = record(item_document(item_id, "Widget", "3.00", 1), 1, 1, vec![]);

    assert!(
        apply_once(&backend, &received, &ctx(&owners, &audit))
            .await
            .is_stored()
    );
    let stored = load(&backend, "item", item_id).await.expect("stored");
    assert_eq!(
        stored.data.fields.get(&name("origin_node")),
        Some(&Value::Null),
        "a locally-owned record keeps the fields it arrived with"
    );
    assert_eq!(stored.version.0, 1);
}

#[tokio::test]
async fn duplicate_by_name_and_version() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let item_id = RecordId::new_v7();
    let received = record(item_document(item_id, "Widget", "3.00", 5), 5, 1, vec![]);

    assert!(
        apply_once(&backend, &received, &ctx(&owners, &audit))
            .await
            .is_stored()
    );
    let first = load(&backend, "item", item_id).await.expect("stored");
    assert_eq!(
        apply_once(&backend, &received, &ctx(&owners, &audit)).await,
        ApplyOutcome::Duplicate
    );
    let second = load(&backend, "item", item_id).await.expect("stored");
    assert_eq!(
        first, second,
        "a duplicate changes nothing, not even the storage version"
    );
}

#[tokio::test]
async fn stale_version_is_refused() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let item_id = RecordId::new_v7();

    let newer = record(item_document(item_id, "Newer", "3.00", 6), 6, 2, vec![]);
    assert!(
        apply_once(&backend, &newer, &ctx(&owners, &audit))
            .await
            .is_stored()
    );

    let older = record(item_document(item_id, "Older", "9.99", 5), 5, 1, vec![]);
    assert_eq!(
        apply_once(&backend, &older, &ctx(&owners, &audit)).await,
        ApplyOutcome::Stale
    );
    let stored = load(&backend, "item", item_id).await.expect("stored");
    assert_eq!(
        stored.data.fields.get(&name("title")),
        Some(&Value::Text("Newer".to_owned())),
        "a stale record never overwrites a newer one"
    );
}

#[tokio::test]
async fn out_of_order_versions_converge() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let item_id = RecordId::new_v7();

    for (version, title) in [(1_u64, "v1"), (3, "v3"), (2, "v2")] {
        let received = record(
            item_document(item_id, title, "1.00", version as i64),
            version,
            version,
            vec![],
        );
        let outcome = apply_once(&backend, &received, &ctx(&owners, &audit)).await;
        if version == 2 {
            assert_eq!(outcome, ApplyOutcome::Stale, "v2 arrives after v3");
        } else {
            assert_eq!(outcome, ApplyOutcome::Stored);
        }
    }
    let stored = load(&backend, "item", item_id).await.expect("stored");
    assert_eq!(
        stored.data.fields.get(&name("origin_version")),
        Some(&Value::Int(3)),
        "the newest version wins"
    );
}

#[tokio::test]
async fn a_non_owner_is_rejected_and_nothing_is_stored() {
    let backend = backend();
    let owners = Owners {
        owner: "other.node.app#hub".to_owned(),
        owned: false,
    };
    let audit = RecordingAudit::new();
    let item_id = RecordId::new_v7();
    let received = record(item_document(item_id, "Widget", "3.00", 1), 1, 1, vec![]);

    let outcome = apply_once(&backend, &received, &ctx(&owners, &audit)).await;
    assert_eq!(
        outcome.reason_code(),
        Some("replica_apply.not_owner"),
        "{outcome:?}"
    );
    assert!(load(&backend, "item", item_id).await.is_none());
}

#[tokio::test]
async fn structural_problems_are_rejected() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let ctx = ctx(&owners, &audit);

    // An unknown entity.
    let mut unknown = item_document(RecordId::new_v7(), "x", "1.00", 1);
    unknown.entity = name("nope");
    let outcome = apply_once(&backend, &record(unknown, 1, 1, vec![]), &ctx).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.unknown_entity"));

    // A missing declared field.
    let mut missing = item_document(RecordId::new_v7(), "x", "1.00", 1);
    missing.fields.remove(&name("title"));
    let outcome = apply_once(&backend, &record(missing, 1, 1, vec![]), &ctx).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.missing_field"));

    // An undeclared field.
    let mut extra = item_document(RecordId::new_v7(), "x", "1.00", 1);
    extra
        .fields
        .insert(name("secret"), Value::Text("nope".to_owned()));
    let outcome = apply_once(&backend, &record(extra, 1, 1, vec![]), &ctx).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.unknown_field"));

    // A value of the wrong type.
    let mut wrong = item_document(RecordId::new_v7(), "x", "1.00", 1);
    wrong.fields.insert(name("title"), Value::Int(3));
    let outcome = apply_once(&backend, &record(wrong, 1, 1, vec![]), &ctx).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.invalid_value"));

    // Rows under an undeclared child table.
    let mut bad_table = item_document(RecordId::new_v7(), "x", "1.00", 1);
    bad_table.children.insert(name("nope"), vec![]);
    let outcome = apply_once(&backend, &record(bad_table, 1, 1, vec![]), &ctx).await;
    assert_eq!(
        outcome.reason_code(),
        Some("replica_apply.unknown_child_table")
    );
}

#[tokio::test]
async fn metadata_skew_is_rejected() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let item_id = RecordId::new_v7();
    let mut received = record(item_document(item_id, "Widget", "3.00", 1), 1, 1, vec![]);
    received.meta_version = 9;

    let outcome = apply_once(&backend, &received, &ctx(&owners, &audit)).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.metadata_skew"));
    assert!(load(&backend, "item", item_id).await.is_none());

    // Within the accepted skew it stores.
    let ctx = ctx(&owners, &audit).with_max_skew(10);
    assert!(apply_once(&backend, &received, &ctx).await.is_stored());
}

#[tokio::test]
async fn a_failed_bundle_leaves_nothing_and_redelivery_succeeds() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let ctx = ctx(&owners, &audit);
    let item_id = RecordId::new_v7();
    let gl_id = RecordId::new_v7();

    // The bundle's entry names an entity that does not exist.
    let mut broken_entry = gl_document(gl_id, "4000", "12.34");
    broken_entry.entity = name("nope");
    let broken = record(
        item_document(item_id, "Widget", "3.00", 1),
        1,
        1,
        vec![broken_entry],
    );
    let outcome = apply_once(&backend, &broken, &ctx).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.unknown_entity"));
    assert!(
        load(&backend, "item", item_id).await.is_none(),
        "the root must not be stored when the bundle fails"
    );
    assert!(load(&backend, "gl_entry", gl_id).await.is_none());

    // The same record, fixed, applies.
    let fixed = record(
        item_document(item_id, "Widget", "3.00", 1),
        1,
        1,
        vec![gl_document(gl_id, "4000", "12.34")],
    );
    assert!(apply_once(&backend, &fixed, &ctx).await.is_stored());
    assert!(load(&backend, "item", item_id).await.is_some());
    assert!(load(&backend, "gl_entry", gl_id).await.is_some());
}

#[tokio::test]
async fn a_bundle_that_fails_at_commit_leaves_nothing() {
    // A failure *after* the root is written (here a child row that already belongs to another
    // document) must still leave nothing behind: the commit is the boundary.
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let ctx = ctx(&owners, &audit);

    // Store document A with a child row.
    let a_id = RecordId::new_v7();
    let a = record(item_document(a_id, "A", "1.00", 1), 1, 1, vec![]);
    assert!(apply_once(&backend, &a, &ctx).await.is_stored());
    let stored_a = load(&backend, "item", a_id).await.expect("A is stored");
    let a_child = stored_a.data.children[&name("lines")][0].id;

    // Bundle B's entry reuses A's child row id under a different parent.
    let b_id = RecordId::new_v7();
    let mut entry = item_document(RecordId::new_v7(), "entry", "2.00", 1);
    entry.children.insert(
        name("lines"),
        vec![spot_storage_contract::ChildRow {
            id: a_child,
            fields: fields(&[
                ("qty", dec("2.00")),
                ("note", Value::Text("row".to_owned())),
            ]),
        }],
    );
    let b = record(item_document(b_id, "B", "3.00", 1), 1, 1, vec![entry]);

    let outcome = apply_once(&backend, &b, &ctx).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.write_failed"));
    assert!(
        load(&backend, "item", b_id).await.is_none(),
        "the root must not survive a failed commit"
    );
}

/// A controller/hook/script/emitter that panics if the apply path ever calls it.
struct PanicSideEffects;

impl SideEffects for PanicSideEffects {
    fn run_hooks(&self, _entity: &str) {
        panic!("a lifecycle hook ran during apply");
    }
    fn allocate_name(&self, _entity: &str) -> String {
        panic!("a naming series ran during apply");
    }
    fn run_script(&self, _entity: &str) {
        panic!("a server script ran during apply");
    }
    fn emit_event(&self, _entity: &str) {
        panic!("the outbox emitter ran during apply");
    }
}

#[tokio::test]
async fn side_effects_are_never_triggered() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let sentinel = PanicSideEffects;
    let ctx = ctx(&owners, &audit).with_side_effects(&sentinel);
    let item_id = RecordId::new_v7();
    let received = record(
        item_document(item_id, "Widget", "3.00", 1),
        1,
        1,
        vec![gl_document(RecordId::new_v7(), "4000", "12.34")],
    );

    // If any business logic ran, this panics.
    assert!(apply_once(&backend, &received, &ctx).await.is_stored());

    // A replicated record is not a new change: no outbox event was enqueued.
    let outbox = backend
        .read_outbox(FeedPosition::START, 100)
        .await
        .expect("the outbox reads");
    assert!(
        outbox.is_empty(),
        "apply must never emit an outbox event, got {outbox:?}"
    );

    // The stored content is exactly what arrived, not what a controller would have written.
    let stored = load(&backend, "item", item_id).await.expect("stored");
    assert_eq!(
        stored.data.fields.get(&name("title")),
        Some(&Value::Text("Widget".to_owned()))
    );
}

#[tokio::test]
async fn a_notify_topic_appends_exactly_one_notice_per_stored_record() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let notices = RecordingNoticeSink::new();
    let ctx = ctx(&owners, &audit).with_notify(true).with_notice(&notices);
    let item_id = RecordId::new_v7();
    let mut received = record(item_document(item_id, "Widget", "3.00", 1), 1, 1, vec![]);
    received.topic = Some(name("items"));

    assert!(apply_once(&backend, &received, &ctx).await.is_stored());
    assert_eq!(
        notices.notices().len(),
        1,
        "one notice for one stored record"
    );
    assert_eq!(notices.notices()[0].record_id, item_id);
    assert_eq!(notices.notices()[0].topic, Some(name("items")));

    // A duplicate adds no notice.
    assert_eq!(
        apply_once(&backend, &received, &ctx).await,
        ApplyOutcome::Duplicate
    );
    assert_eq!(notices.notices().len(), 1);

    // A rejected record adds no notice.
    let owners = Owners {
        owner: "other.node.app#hub".to_owned(),
        owned: false,
    };
    let rejected_ctx = ApplyCtx::new(&owners, &audit, 0)
        .with_notify(true)
        .with_notice(&notices);
    let rejected = record(
        item_document(RecordId::new_v7(), "x", "1.00", 1),
        1,
        1,
        vec![],
    );
    assert!(
        apply_once(&backend, &rejected, &rejected_ctx)
            .await
            .reason_code()
            .is_some()
    );
    assert_eq!(notices.notices().len(), 1);
}

#[tokio::test]
async fn a_replicated_audit_marker_is_written() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let item_id = RecordId::new_v7();
    let received = record(item_document(item_id, "Widget", "3.00", 4), 4, 9, vec![]);

    assert!(
        apply_once(&backend, &received, &ctx(&owners, &audit))
            .await
            .is_stored()
    );
    let markers = audit.markers();
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].source, source());
    assert_eq!(markers[0].seq, 9);
    assert_eq!(markers[0].version, 4);
    assert!(markers[0].mirror);

    // A duplicate writes no second marker.
    assert_eq!(
        apply_once(&backend, &received, &ctx(&owners, &audit)).await,
        ApplyOutcome::Duplicate
    );
    assert_eq!(audit.markers().len(), 1);
}

#[tokio::test]
async fn the_chain_audit_adapter_appends_a_replicated_entry() {
    use std::sync::Arc;

    use spot_audit_chain::{CheckpointSigner, InMemoryStore, LocalSigner};
    use spot_replica_apply::ChainAudit;

    let backend = backend();
    let owners = Owners::mirror();
    let signer: Arc<dyn CheckpointSigner> = Arc::new(LocalSigner::from_seed([7; 32]));
    let audit: ChainAudit<InMemoryStore> = ChainAudit::new(signer);
    let ctx = ApplyCtx::new(&owners, &audit, 0);
    let item_id = RecordId::new_v7();
    let received = record(item_document(item_id, "Widget", "3.00", 4), 4, 9, vec![]);

    assert!(apply_once(&backend, &received, &ctx).await.is_stored());

    let head = audit
        .head(source().site())
        .await
        .expect("the chain reads")
        .expect("an entry was appended");
    assert_eq!(head.record.kind, "replicated");
    assert_eq!(
        head.record.subject.as_ref().map(|s| s.id.clone()),
        Some(item_id.to_string())
    );
}

#[tokio::test]
async fn an_audit_failure_rolls_the_record_back() {
    struct FailingAudit;
    impl spot_replica_apply::ReplicaAudit for FailingAudit {
        fn replicated<'a>(
            &'a self,
            _marker: &'a spot_replica_apply::ReplicaMarker,
        ) -> futures::future::BoxFuture<'a, Result<(), Reason>> {
            Box::pin(async move {
                Err(Reason::AuditFailed {
                    why: "down".to_owned(),
                })
            })
        }
    }
    let backend = backend();
    let owners = Owners::mirror();
    let failing = FailingAudit;
    let ctx = ApplyCtx::new(&owners, &failing, 0);
    let item_id = RecordId::new_v7();
    let received = record(item_document(item_id, "Widget", "3.00", 1), 1, 1, vec![]);

    let outcome = apply_once(&backend, &received, &ctx).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.audit_failed"));
    assert!(
        load(&backend, "item", item_id).await.is_none(),
        "a failed audit rolls the whole apply back"
    );
}

#[tokio::test]
async fn a_payload_round_trips_through_the_wire() {
    let item_id = RecordId::new_v7();
    let gl_id = RecordId::new_v7();
    let received = record(
        item_document(item_id, "Widget", "3.00", 4),
        4,
        9,
        vec![gl_document(gl_id, "4000", "12.34")],
    );

    let payload = received.payload_json().expect("encodes");
    let decoded = spot_replica_apply::ReceivedRecord::from_record(
        &source(),
        &spot_sync_protocol::Record::new(
            received.name.clone(),
            received.version,
            received.seq,
            payload,
        )
        .expect("a valid record"),
    )
    .expect("decodes");

    assert_eq!(decoded.document, received.document);
    assert_eq!(decoded.entries, received.entries);
    assert_eq!(decoded.meta_version, received.meta_version);
    assert_eq!(decoded.version, received.version);
}

#[tokio::test]
async fn a_bundle_with_a_reused_id_is_rejected() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let item_id = RecordId::new_v7();
    // The entry reuses the root's id.
    let mut entry = gl_document(item_id, "4000", "12.34");
    entry.entity = name("gl_entry");
    let received = record(
        item_document(item_id, "Widget", "3.00", 1),
        1,
        1,
        vec![entry],
    );

    let outcome = apply_once(&backend, &received, &ctx(&owners, &audit)).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.duplicate_id"));
}

#[tokio::test]
async fn a_concurrent_apply_of_the_same_record_is_idempotent() {
    // Two transactions begin on the same version; the fake serializes them on the row lock, so
    // one stores and the other sees the stored version as a duplicate.
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let ctx = ctx(&owners, &audit);
    let item_id = RecordId::new_v7();
    let received = record(item_document(item_id, "Widget", "3.00", 1), 1, 1, vec![]);

    let first_tx = begin(&backend).await;
    let first = spot_replica_apply::apply(first_tx, &received, &ctx).await;
    let second_tx = begin(&backend).await;
    let second = spot_replica_apply::apply(second_tx, &received, &ctx).await;

    assert_eq!(first, ApplyOutcome::Stored);
    assert_eq!(second, ApplyOutcome::Duplicate);
    assert_eq!(audit.markers().len(), 1);
}

#[tokio::test]
async fn a_missing_origin_field_rejects_a_mirror() {
    // A catalog whose entity has no origin fields cannot hold a mirror.
    use spot_storage_contract::{Collation, EntityDef, EntityKind, FieldDef, FieldType};
    let plain = EntityDef {
        name: name("plain"),
        kind: EntityKind::Document,
        fields: vec![FieldDef {
            name: name("title"),
            ty: FieldType::Text { max_chars: None },
            nullable: true,
        }],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    let catalog =
        spot_storage_contract::Catalog::new(1, Collation::new("en").unwrap(), vec![plain]).unwrap();
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let ctx = ctx(&owners, &audit);
    let id = RecordId::new_v7();
    let doc = DocumentData {
        entity: name("plain"),
        id,
        fields: fields(&[("title", Value::Text("x".to_owned()))]),
        children: BTreeMap::new(),
    };
    let received = record(doc, 1, 1, vec![]);

    let tx = backend
        .begin(
            std::sync::Arc::new(catalog),
            spot_storage_contract::TxOptions::new(
                name("apply"),
                spot_storage_contract::Isolation::ReadCommitted,
            ),
        )
        .await
        .unwrap();
    let outcome = spot_replica_apply::apply(tx, &received, &ctx).await;
    assert_eq!(outcome.reason_code(), Some("replica_apply.unknown_field"));
}

#[tokio::test]
async fn a_name_and_version_are_validated() {
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let ctx = ctx(&owners, &audit);
    let mut received = record(
        item_document(RecordId::new_v7(), "x", "1.00", 1),
        0,
        1,
        vec![],
    );
    received.version = 0;
    let outcome = apply_once(&backend, &received, &ctx).await;
    assert_eq!(
        outcome.reason_code(),
        Some("replica_apply.invalid_identifier")
    );

    let mut received = record(
        item_document(RecordId::new_v7(), "x", "1.00", 1),
        1,
        0,
        vec![],
    );
    received.seq = 0;
    let outcome = apply_once(&backend, &received, &ctx).await;
    assert_eq!(
        outcome.reason_code(),
        Some("replica_apply.invalid_identifier")
    );
}

#[tokio::test]
async fn the_default_origin_fields_are_the_adr058_reading() {
    let origin = spot_replica_apply::OriginFields::default();
    assert_eq!(origin.node, "origin_node");
    assert_eq!(origin.id, "origin_id");
    assert_eq!(origin.version, "origin_version");
    assert!(origin.names().is_ok());
}
