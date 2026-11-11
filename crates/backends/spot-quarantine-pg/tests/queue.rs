//! The scenarios of `spot-quarantine`'s `tests/queue.rs` that need only the `QuarantineStore`
//! trait, run through the intake over `PgQuarantineStore`: the stream is never blocked, a later
//! version is parked and released in order, a discard keeps the payload, a restart keeps the
//! queue, the retry cap raises a condition, and a refused record is not stored twice.
//!
//! Skips cleanly without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{catalog, ghost, note, options, record, source, store};
use spot_permissions::Principal;
use spot_postgres::testing::TestDb;
use spot_quarantine::{
    AllowAll, AuditAction, CatalogSource, FixedCatalog, Intake, IntakeOutcome, ItemFilter, ItemId,
    OperationResult, QuarantineError, QuarantineItem, QuarantineStore, RecordingAudit, RetryPolicy,
    Severity, SharedCatalog, State, admin,
};
use spot_replica_apply::{MirrorAll, RecordingAudit as ReplicaRecording};
use spot_storage_contract::RecordId;
use spot_storage_fake::FakeBackend;

fn system() -> Principal {
    Principal::system()
}

fn fixed(with_ghost: bool) -> Arc<dyn CatalogSource> {
    Arc::new(FixedCatalog::new(Arc::new(catalog(with_ghost))))
}

fn intake_over(
    store: &Arc<dyn QuarantineStore>,
    catalog: Arc<dyn CatalogSource>,
    policy: RetryPolicy,
    audit: Arc<RecordingAudit>,
) -> Intake<FakeBackend> {
    Intake::new(
        FakeBackend::new(),
        catalog,
        Arc::clone(store),
        Arc::new(AllowAll),
        audit,
        Arc::new(MirrorAll),
        Arc::new(ReplicaRecording::new()),
        options(),
    )
    .with_retry(policy)
}

async fn pg_store(db: &TestDb, app: &str) -> Arc<dyn QuarantineStore> {
    Arc::new(store(db, app).await)
}

fn quarantined(outcome: IntakeOutcome) -> QuarantineItem {
    match outcome {
        IntakeOutcome::Quarantined(item) => item,
        other => panic!("expected a quarantined item, got {other:?}"),
    }
}

#[tokio::test]
async fn a_thousand_records_with_five_percent_invalid_flow_through() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = pg_store(&db, "thousand").await;
    let intake = intake_over(
        &store,
        fixed(false),
        RetryPolicy::sync_default(),
        Arc::new(RecordingAudit::new()),
    );
    let (mut applied, mut held) = (0_u32, 0_u32);
    for seq in 1..=1_000_u64 {
        let document = if seq % 20 == 0 {
            ghost(RecordId::new_v7(), "bad", 1)
        } else {
            note(RecordId::new_v7(), "ok", 1)
        };
        let received = record(document, 1, seq, vec![]);
        match intake
            .receive(&received, 1_000_000 + i64::try_from(seq).unwrap())
            .await
            .unwrap()
        {
            IntakeOutcome::Applied => applied += 1,
            IntakeOutcome::Quarantined(_) => held += 1,
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    assert_eq!((applied, held), (950, 50));
    assert_eq!(store.list(&ItemFilter::all()).await.unwrap().len(), 50);
    assert_eq!(
        intake
            .checkpoint(&source())
            .expect("a checkpoint")
            .last_applied_seq,
        1_000
    );
}

#[tokio::test]
async fn a_later_version_is_parked_and_released_in_order_after_a_metadata_upgrade() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = pg_store(&db, "parked").await;
    let shared = Arc::new(SharedCatalog::new(Arc::new(catalog(false))));
    let source_of_catalog: Arc<dyn CatalogSource> = shared.clone();
    let intake = intake_over(
        &store,
        source_of_catalog,
        RetryPolicy::sync_default(),
        Arc::new(RecordingAudit::new()),
    );

    let id = RecordId::new_v7();
    let v1 = record(ghost(id, "v1", 1), 1, 1, vec![]);
    assert!(matches!(
        intake.receive(&v1, 0).await.unwrap(),
        IntakeOutcome::Quarantined(_)
    ));
    let other = record(note(RecordId::new_v7(), "other", 1), 1, 2, vec![]);
    assert_eq!(
        intake.receive(&other, 0).await.unwrap(),
        IntakeOutcome::Applied
    );

    let v2 = record(ghost(id, "v2", 2), 2, 3, vec![]);
    let parked = match intake.receive(&v2, 0).await.unwrap() {
        IntakeOutcome::Parked(item) => item,
        other => panic!("expected a parked item, got {other:?}"),
    };
    assert_eq!(parked.reason_code, "quarantine.dependent_held");

    shared.replace(Arc::new(catalog(true)));
    let views = intake
        .retry_all(&system(), "after metadata upgrade", 5)
        .await
        .unwrap();
    assert_eq!(views.len(), 2);
    assert_eq!(views[0].result, OperationResult::Released);
    assert_eq!(views[0].item.version, 1, "v1 first");
    assert_eq!(views[1].result, OperationResult::Released);
    assert_eq!(
        views[1].item.version, 2,
        "then the version parked behind it"
    );
}

#[tokio::test]
async fn discard_is_audited_and_keeps_the_payload() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = pg_store(&db, "discard").await;
    let audit = Arc::new(RecordingAudit::new());
    let intake = intake_over(
        &store,
        fixed(false),
        RetryPolicy::sync_default(),
        audit.clone(),
    );
    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let item = quarantined(intake.receive(&received, 10).await.unwrap());

    let view = intake
        .discard(&system(), item.id, "duplicate of an import", 11)
        .await
        .unwrap();
    assert_eq!(view.result, OperationResult::Discarded);
    assert_eq!(view.item.state, State::Discarded);
    let entries = audit.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].action, AuditAction::Discarded);

    let stored = store.record(item.id).await.unwrap().expect("the payload");
    assert_eq!(
        stored, received,
        "the stored payload is exactly what arrived"
    );

    assert_eq!(
        intake
            .discard(&system(), item.id, "   ", 12)
            .await
            .unwrap_err(),
        QuarantineError::ReasonRequired { id: item.id }
    );
    assert!(matches!(
        intake
            .discard(&system(), item.id, "again", 12)
            .await
            .unwrap_err(),
        QuarantineError::NotOpen {
            state: State::Discarded,
            ..
        }
    ));
}

#[tokio::test]
async fn restart_keeps_the_queue() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    {
        let store = pg_store(&db, "before").await;
        let intake = intake_over(
            &store,
            fixed(false),
            RetryPolicy::sync_default(),
            Arc::new(RecordingAudit::new()),
        );
        for seq in 1..=3 {
            let received = record(ghost(RecordId::new_v7(), "x", 1), 1, seq, vec![]);
            assert!(matches!(
                intake.receive(&received, 0).await.unwrap(),
                IntakeOutcome::Quarantined(_)
            ));
        }
    }

    // A fresh pool, store and intake over the same database see the same queue, in order.
    let store = pg_store(&db, "after").await;
    let intake = intake_over(
        &store,
        fixed(false),
        RetryPolicy::sync_default(),
        Arc::new(RecordingAudit::new()),
    );
    let items = admin::list(
        &*store,
        &AllowAll,
        &system(),
        &ItemFilter::all(),
        &RetryPolicy::sync_default(),
    )
    .await
    .unwrap();
    assert_eq!(items.len(), 3, "the queue survived the restart");
    assert_eq!(items[0].id, ItemId(1));
    assert_eq!(items[2].id, ItemId(3));

    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 4, vec![]);
    let item = quarantined(intake.receive(&received, 0).await.unwrap());
    assert_eq!(item.id, ItemId(4));
}

#[tokio::test]
async fn automatic_retry_stops_at_the_cap_and_raises_a_condition() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = pg_store(&db, "cap").await;
    let intake = intake_over(
        &store,
        fixed(false),
        RetryPolicy::new(vec![0, 10, 20], 2),
        Arc::new(RecordingAudit::new()),
    );
    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let item = quarantined(intake.receive(&received, 1_000).await.unwrap());

    let first = intake.run_due(item.first_seen).await.unwrap();
    assert_eq!(first[0].result, OperationResult::StillHeld);
    assert_eq!(first[0].item.attempts, 1);
    assert!(first[0].condition.is_none());

    let second = intake.run_due(item.first_seen + 10).await.unwrap();
    assert_eq!(second[0].item.attempts, 2);
    let raised = second[0]
        .condition
        .clone()
        .expect("the cap raises a condition");
    assert_eq!(raised.code, "QUARANTINED");
    assert_eq!(raised.severity, Severity::Amber);

    let capped = intake.run_due(item.first_seen + 100).await.unwrap();
    assert_eq!(capped[0].item.attempts, 2, "no attempt past the cap");
    assert_eq!(capped[0].item.state, State::Held);
    // The attempts survive in the database, not only in the view.
    assert_eq!(store.item(item.id).await.unwrap().unwrap().attempts, 2);
}

#[tokio::test]
async fn a_quarantined_record_is_not_stored_twice_and_releases_once_fixed() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = pg_store(&db, "twice").await;
    let intake = intake_over(
        &store,
        fixed(false),
        RetryPolicy::sync_default(),
        Arc::new(RecordingAudit::new()),
    );
    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let first = quarantined(intake.receive(&received, 0).await.unwrap());
    assert_eq!(
        intake.receive(&received, 0).await.unwrap(),
        IntakeOutcome::AlreadyQuarantined
    );
    assert_eq!(store.list(&ItemFilter::all()).await.unwrap().len(), 1);

    let fixed_intake = intake_over(
        &store,
        fixed(true),
        RetryPolicy::sync_default(),
        Arc::new(RecordingAudit::new()),
    );
    let view = fixed_intake.release(&system(), first.id, 9).await.unwrap();
    assert_eq!(view.result, OperationResult::Released);
    assert_eq!(
        store.item(first.id).await.unwrap().unwrap().state,
        State::Released
    );
}
