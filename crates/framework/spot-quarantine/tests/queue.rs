//! The quarantine-queue acceptance tests (WP-EDG-06.T2) on the in-memory storage backend.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{
    backend, catalog, editor, engine, ghost, note, note_with_extra, options, outsider, record,
    source, system,
};
use spot_quarantine::{
    Access, AllowAll, AuditAction, CatalogSource, ChainAudit, Condition, EngineGuard, FixedCatalog,
    InMemoryStore, Intake, IntakeOutcome, ItemFilter, ItemId, OperationResult, QuarantineError,
    QuarantineGuard, QuarantineStore, RecordingAudit, RetryPolicy, Severity, SharedCatalog, State,
    admin,
};
use spot_replica_apply::{MirrorAll, RecordingAudit as ReplicaRecording};
use spot_storage_contract::RecordId;
use spot_storage_fake::FakeBackend;
use spot_sync_protocol::SourceId;

/// The pieces one test needs, so each test reads as a story.
struct Harness {
    intake: Intake<FakeBackend>,
    store: Arc<dyn QuarantineStore>,
    audit: Arc<RecordingAudit>,
}

fn harness(
    catalog: Arc<dyn CatalogSource>,
    policy: RetryPolicy,
    guard: Arc<dyn QuarantineGuard>,
) -> Harness {
    let store: Arc<dyn QuarantineStore> = Arc::new(InMemoryStore::new());
    let audit = Arc::new(RecordingAudit::new());
    let intake = Intake::new(
        backend(),
        catalog,
        Arc::clone(&store),
        guard,
        audit.clone(),
        Arc::new(MirrorAll),
        Arc::new(ReplicaRecording::new()),
        options(),
    )
    .with_retry(policy);
    Harness {
        intake,
        store,
        audit,
    }
}

fn fixed(with_ghost: bool) -> Arc<dyn CatalogSource> {
    Arc::new(FixedCatalog::new(Arc::new(catalog(with_ghost))))
}

fn quarantined(outcome: IntakeOutcome) -> spot_quarantine::QuarantineItem {
    match outcome {
        IntakeOutcome::Quarantined(item) => item,
        other => panic!("expected a quarantined item, got {other:?}"),
    }
}

fn site() -> SourceId {
    source()
}

#[tokio::test]
async fn a_thousand_records_with_five_percent_invalid_flow_through() {
    let h = harness(
        fixed(false),
        RetryPolicy::sync_default(),
        Arc::new(AllowAll),
    );
    let mut applied = 0_u32;
    let mut quarantined = 0_u32;

    for seq in 1..=1_000_u64 {
        // Every twentieth record names an entity the catalog does not have.
        let document = if seq % 20 == 0 {
            ghost(RecordId::new_v7(), "bad", 1)
        } else {
            note(RecordId::new_v7(), "ok", 1)
        };
        let received = record(document, 1, seq, vec![]);
        match h
            .intake
            .receive(&received, 1_000_000 + i64::try_from(seq).unwrap())
            .await
            .unwrap()
        {
            IntakeOutcome::Applied => applied += 1,
            IntakeOutcome::Quarantined(_) => quarantined += 1,
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    assert_eq!(applied, 950, "every valid record flowed through");
    assert_eq!(quarantined, 50, "every invalid record was quarantined");
    assert_eq!(
        h.store.list(&ItemFilter::all()).await.unwrap().len(),
        50,
        "the queue holds exactly the invalid records"
    );
    assert_eq!(
        h.intake
            .checkpoint(&site())
            .expect("a checkpoint")
            .last_applied_seq,
        1_000,
        "the checkpoint advanced past every quarantined record"
    );
}

#[tokio::test]
async fn a_refused_record_does_not_block_the_next_one() {
    let h = harness(
        fixed(false),
        RetryPolicy::sync_default(),
        Arc::new(AllowAll),
    );

    let bad = record(ghost(RecordId::new_v7(), "bad", 1), 1, 1, vec![]);
    assert!(matches!(
        h.intake.receive(&bad, 0).await.unwrap(),
        IntakeOutcome::Quarantined(_)
    ));

    let good = record(note(RecordId::new_v7(), "good", 1), 1, 2, vec![]);
    assert_eq!(
        h.intake.receive(&good, 0).await.unwrap(),
        IntakeOutcome::Applied
    );

    assert_eq!(
        h.intake
            .checkpoint(&site())
            .expect("a checkpoint")
            .last_applied_seq,
        2
    );
}

#[tokio::test]
async fn a_later_version_is_parked_and_released_in_order_after_a_metadata_upgrade() {
    let shared = Arc::new(SharedCatalog::new(Arc::new(catalog(false))));
    let catalog_source: Arc<dyn CatalogSource> = shared.clone();
    let h = harness(
        catalog_source,
        RetryPolicy::sync_default(),
        Arc::new(AllowAll),
    );

    let id = RecordId::new_v7();
    let v1 = record(ghost(id, "v1", 1), 1, 1, vec![]);
    assert!(matches!(
        h.intake.receive(&v1, 0).await.unwrap(),
        IntakeOutcome::Quarantined(_)
    ));

    // A different document keeps flowing.
    let other = record(note(RecordId::new_v7(), "other", 1), 1, 2, vec![]);
    assert_eq!(
        h.intake.receive(&other, 0).await.unwrap(),
        IntakeOutcome::Applied
    );

    // A later version of the quarantined document is parked behind it.
    let v2 = record(ghost(id, "v2", 2), 2, 3, vec![]);
    let parked = match h.intake.receive(&v2, 0).await.unwrap() {
        IntakeOutcome::Parked(item) => item,
        other => panic!("expected a parked item, got {other:?}"),
    };
    assert_eq!(parked.reason_code, "quarantine.dependent_held");
    assert_eq!(parked.state, State::Held);

    // The cause is fixed by installing the metadata that declares the entity.
    shared.replace(Arc::new(catalog(true)));

    let views = h
        .intake
        .retry_all(&system(), "after metadata upgrade", 5)
        .await
        .unwrap();
    assert_eq!(views.len(), 2, "both held items were retried");
    assert_eq!(views[0].result, OperationResult::Released);
    assert_eq!(views[0].item.version, 1, "released in order: v1 first");
    assert_eq!(views[1].result, OperationResult::Released);
    assert_eq!(
        views[1].item.version, 2,
        "then the version parked behind it"
    );
}

#[tokio::test]
async fn discard_is_audited_and_keeps_the_payload() {
    let h = harness(
        fixed(false),
        RetryPolicy::sync_default(),
        Arc::new(AllowAll),
    );
    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let item = quarantined(h.intake.receive(&received, 10).await.unwrap());

    let view = h
        .intake
        .discard(&system(), item.id, "duplicate of an import", 11)
        .await
        .unwrap();
    assert_eq!(view.result, OperationResult::Discarded);
    assert_eq!(view.item.state, State::Discarded);

    let entries = h.audit.entries();
    assert_eq!(entries.len(), 1, "the discard is audited");
    assert_eq!(entries[0].action, AuditAction::Discarded);
    assert_eq!(entries[0].item, item.id);
    assert_eq!(entries[0].reason, "duplicate of an import");

    // The sender's data is never deleted or edited.
    let stored = h.store.record(item.id).await.unwrap().expect("the payload");
    assert_eq!(
        stored, received,
        "the stored payload is exactly what arrived"
    );

    // A reason is required, and a closed item cannot be discarded again.
    assert_eq!(
        h.intake
            .discard(&system(), item.id, "   ", 12)
            .await
            .unwrap_err(),
        QuarantineError::ReasonRequired { id: item.id }
    );
    assert_eq!(
        h.intake
            .discard(&system(), item.id, "again", 12)
            .await
            .unwrap_err(),
        QuarantineError::NotOpen {
            id: item.id,
            state: State::Discarded,
            access: Access::Discard.as_str(),
        }
    );
}

#[tokio::test]
async fn permissions_are_enforced() {
    let guard: Arc<dyn QuarantineGuard> = Arc::new(EngineGuard::new(engine()));
    let h = harness(
        fixed(false),
        RetryPolicy::sync_default(),
        Arc::clone(&guard),
    );

    // A `note`-entity item (an undeclared field), so the permission is asked against `note`.
    let received = record(note_with_extra(RecordId::new_v7(), 1), 1, 1, vec![]);
    let item = quarantined(h.intake.receive(&received, 0).await.unwrap());
    assert_eq!(item.record_ref.entity, "note");

    // The editor has `update` on `note`, so the retry is allowed (and fails again, still held).
    let view = h.intake.retry(&editor(), item.id, 1).await.unwrap();
    assert_eq!(view.result, OperationResult::StillHeld);

    // The editor has no `delete`, so a discard is refused.
    assert_eq!(
        h.intake
            .discard(&editor(), item.id, "why", 1)
            .await
            .unwrap_err(),
        QuarantineError::Denied {
            access: Access::Discard.as_str(),
            id: item.id,
        }
    );

    // A principal with no roles may not even retry.
    assert_eq!(
        h.intake.retry(&outsider(), item.id, 1).await.unwrap_err(),
        QuarantineError::Denied {
            access: Access::Retry.as_str(),
            id: item.id,
        }
    );

    // The admin list is filtered by `read` on the record's entity.
    let policy = RetryPolicy::sync_default();
    let for_editor = admin::list(&*h.store, &*guard, &editor(), &ItemFilter::all(), &policy)
        .await
        .unwrap();
    assert_eq!(for_editor.len(), 1);
    let for_outsider = admin::list(&*h.store, &*guard, &outsider(), &ItemFilter::all(), &policy)
        .await
        .unwrap();
    assert!(for_outsider.is_empty());

    // The platform itself may discard.
    let view = h
        .intake
        .discard(&system(), item.id, "operator decision", 2)
        .await
        .unwrap();
    assert_eq!(view.result, OperationResult::Discarded);
}

#[tokio::test]
async fn restart_keeps_the_queue() {
    let store: Arc<dyn QuarantineStore> = Arc::new(InMemoryStore::new());

    {
        let intake = Intake::new(
            backend(),
            fixed(false),
            Arc::clone(&store),
            Arc::new(AllowAll),
            Arc::new(RecordingAudit::new()),
            Arc::new(MirrorAll),
            Arc::new(ReplicaRecording::new()),
            options(),
        );
        for seq in 1..=3 {
            let received = record(ghost(RecordId::new_v7(), "x", 1), 1, seq, vec![]);
            assert!(matches!(
                intake.receive(&received, 0).await.unwrap(),
                IntakeOutcome::Quarantined(_)
            ));
        }
    }

    // A fresh intake over the same store sees the same queue, in the same order.
    let intake = Intake::new(
        backend(),
        fixed(false),
        Arc::clone(&store),
        Arc::new(AllowAll),
        Arc::new(RecordingAudit::new()),
        Arc::new(MirrorAll),
        Arc::new(ReplicaRecording::new()),
        options(),
    );
    let policy = RetryPolicy::sync_default();
    let items = admin::list(&*store, &AllowAll, &system(), &ItemFilter::all(), &policy)
        .await
        .unwrap();
    assert_eq!(items.len(), 3, "the queue survived the restart");
    assert_eq!(items[0].id, ItemId(1));
    assert_eq!(items[2].id, ItemId(3));

    // The id counter continues from the store.
    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 4, vec![]);
    let item = quarantined(intake.receive(&received, 0).await.unwrap());
    assert_eq!(item.id, ItemId(4));
}

#[tokio::test]
async fn automatic_retry_stops_at_the_cap_and_raises_a_condition() {
    let policy = RetryPolicy::new(vec![0, 10, 20], 2);
    let h = harness(fixed(false), policy.clone(), Arc::new(AllowAll));
    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let item = quarantined(h.intake.receive(&received, 1_000).await.unwrap());

    // The first attempt is due immediately.
    let first = h.intake.run_due(item.first_seen).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].result, OperationResult::StillHeld);
    assert_eq!(first[0].item.attempts, 1);
    assert!(first[0].condition.is_none());

    // The second after the backoff reaches the cap: the item stays Held and raises the condition.
    let second = h.intake.run_due(item.first_seen + 10).await.unwrap();
    assert_eq!(second[0].item.attempts, 2);
    assert_eq!(second[0].result, OperationResult::StillHeld);
    let raised = second[0]
        .condition
        .clone()
        .expect("the cap raises a condition");
    assert_eq!(raised.code, "QUARANTINED");
    assert_eq!(raised.severity, Severity::Amber);
    assert!(raised.sentence.contains("could not be applied"));

    // A later run reports the condition without attempting again.
    let capped = h.intake.run_due(item.first_seen + 100).await.unwrap();
    assert_eq!(capped[0].result, OperationResult::StillHeld);
    assert_eq!(capped[0].item.attempts, 2, "no attempt past the cap");
    assert_eq!(capped[0].item.state, State::Held);
    assert_eq!(
        capped[0].condition.clone().expect("a condition").code,
        "QUARANTINED"
    );
}

#[tokio::test]
async fn a_quarantined_record_is_not_stored_twice() {
    let h = harness(
        fixed(false),
        RetryPolicy::sync_default(),
        Arc::new(AllowAll),
    );
    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);

    let first = quarantined(h.intake.receive(&received, 0).await.unwrap());
    assert_eq!(
        h.intake.receive(&received, 0).await.unwrap(),
        IntakeOutcome::AlreadyQuarantined
    );
    assert_eq!(h.store.list(&ItemFilter::all()).await.unwrap().len(), 1);

    // A manual release re-applies, and once the cause is fixed it is released.
    let fixed_source = fixed(true);
    let h = Harness {
        intake: Intake::new(
            backend(),
            fixed_source,
            Arc::clone(&h.store),
            Arc::new(AllowAll),
            Arc::new(RecordingAudit::new()),
            Arc::new(MirrorAll),
            Arc::new(ReplicaRecording::new()),
            options(),
        ),
        store: h.store,
        audit: h.audit,
    };
    let view = h.intake.release(&system(), first.id, 9).await.unwrap();
    assert_eq!(view.result, OperationResult::Released);
}

#[tokio::test]
async fn a_view_serialises_for_the_console_screen() {
    let h = harness(
        fixed(false),
        RetryPolicy::sync_default(),
        Arc::new(AllowAll),
    );
    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let item = quarantined(h.intake.receive(&received, 0).await.unwrap());
    let view = admin::get(
        &*h.store,
        &AllowAll,
        &system(),
        item.id,
        &RetryPolicy::sync_default(),
    )
    .await
    .unwrap()
    .expect("the item");

    let json = serde_json::to_string(&view).unwrap();
    assert!(json.contains("\"reason_code\":\"replica_apply.unknown_entity\""));
    assert!(json.contains("\"state\":\"held\""));
    assert!(json.contains("\"retryable\":true"));
    let round_trip: spot_quarantine::ItemView = serde_json::from_str(&json).unwrap();
    assert_eq!(round_trip, view);
}

#[tokio::test]
async fn the_chain_audit_adapter_appends_a_discard_entry() {
    use spot_audit_chain::{CheckpointSigner, InMemoryStore as AuditStore, LocalSigner};

    let signer: Arc<dyn CheckpointSigner> = Arc::new(LocalSigner::from_seed([7; 32]));
    let audit: Arc<ChainAudit<AuditStore>> = Arc::new(ChainAudit::new(signer));
    let store: Arc<dyn QuarantineStore> = Arc::new(InMemoryStore::new());
    let intake = Intake::new(
        backend(),
        fixed(false),
        store,
        Arc::new(AllowAll),
        audit.clone(),
        Arc::new(MirrorAll),
        Arc::new(ReplicaRecording::new()),
        options(),
    );
    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let item = quarantined(intake.receive(&received, 0).await.unwrap());
    intake
        .discard(&system(), item.id, "not needed", 5)
        .await
        .unwrap();

    let head = audit
        .head("head.office.app")
        .await
        .unwrap()
        .expect("an entry was appended");
    assert_eq!(head.record.kind, "quarantine_discarded");
    assert_eq!(head.record.actor, "system");
}

/// The default dependency seam parks a later version and lets an earlier or equal one through.
#[test]
fn the_default_dependents_seam_parks_only_later_versions() {
    use spot_quarantine::{Dependents, QuarantineItem, RecordRef, SameDocument};

    let seam = SameDocument;
    let ref_of = |version: u64| RecordRef {
        name: spot_sync_protocol::RecordName::new("note:abc").unwrap(),
        version,
        entity: "note".to_owned(),
        id: "abc".to_owned(),
    };
    let item = QuarantineItem {
        id: ItemId(1),
        source: source(),
        seq: 1,
        record_ref: ref_of(3),
        reason_code: "replica_apply.unknown_entity".to_owned(),
        detail: String::new(),
        first_seen: 0,
        attempts: 0,
        state: State::Held,
    };

    let later = record(note(RecordId::new_v7(), "x", 5), 5, 2, vec![]);
    let mut later = later;
    later.name = spot_sync_protocol::RecordName::new("note:abc").unwrap();
    assert!(seam.waits_behind(&item, &later));

    let mut earlier = record(note(RecordId::new_v7(), "x", 2), 2, 3, vec![]);
    earlier.name = spot_sync_protocol::RecordName::new("note:abc").unwrap();
    assert!(!seam.waits_behind(&item, &earlier));
}

/// The condition constructor is available to the future health surface.
#[test]
fn a_condition_names_the_item_and_suggests_an_action() {
    let item = spot_quarantine::QuarantineItem {
        id: ItemId(7),
        source: source(),
        seq: 1,
        record_ref: spot_quarantine::RecordRef {
            name: spot_sync_protocol::RecordName::new("note:abc").unwrap(),
            version: 2,
            entity: "note".to_owned(),
            id: "abc".to_owned(),
        },
        reason_code: "replica_apply.unknown_entity".to_owned(),
        detail: String::new(),
        first_seen: 0,
        attempts: 8,
        state: State::Held,
    };
    let info = spot_quarantine::reason::describe(&item.reason_code);
    let condition: Condition = Condition::quarantined(&item, &info);
    assert_eq!(condition.code, "QUARANTINED");
    assert_eq!(condition.subject, "note:abc@2");
    assert!(!condition.action.is_empty());
}
