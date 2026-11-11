//! Closed items are terminal in the database too (WP-EDG-06.T5):
//! closing is one method that records who and when, a raw client cannot rewrite a closed row, and
//! the size caps are table constraints as well as checks in the store.
//!
//! Skips cleanly without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{catalog, close, ghost, options, put, record, source, store};
use spot_postgres::testing::TestDb;
use spot_quarantine::{
    AllowAll, AuditAction, Closure, ClosureOutcome, FixedCatalog, Intake, IntakeOutcome, ItemId,
    MAX_DETAIL_BYTES, MAX_PAYLOAD_BYTES, NewItem, QuarantineError, QuarantineStore, RecordRef,
    RecordingAudit, State,
};
use spot_replica_apply::{MirrorAll, RecordingAudit as ReplicaRecording};
use spot_storage_contract::RecordId;
use spot_storage_fake::FakeBackend;

#[tokio::test]
async fn closing_records_who_and_when_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "close").await;
    let rec = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let item = put(&store, &rec, "r", 10).await;

    let closed = close(&store, &item, ClosureOutcome::Discarded, "ada", 77)
        .await
        .unwrap();
    assert_eq!(closed.state, State::Discarded);
    let closure = store.closure(item.id).await.unwrap().expect("a closure");
    assert_eq!((closure.by.as_str(), closure.at), ("ada", 77));

    let again = close(&store, &item, ClosureOutcome::Released, "eve", 99).await;
    assert_eq!(
        again.unwrap_err(),
        QuarantineError::ItemClosed { id: item.id }
    );
    let after = store.item(item.id).await.unwrap().unwrap();
    assert_eq!(after, closed, "who and when are immutable");
    assert_eq!(store.closure(item.id).await.unwrap(), Some(closure));
    assert_eq!(
        store.record(item.id).await.unwrap().expect("payload kept"),
        rec
    );

    let missing = close(
        &store,
        &spot_quarantine::QuarantineItem {
            id: ItemId(9_999),
            ..item.clone()
        },
        ClosureOutcome::Released,
        "ada",
        1,
    )
    .await;
    assert_eq!(
        missing.unwrap_err(),
        QuarantineError::NotFound { id: ItemId(9_999) }
    );
}

#[tokio::test]
async fn a_token_for_another_item_or_outcome_closes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "token").await;
    let a = put(
        &store,
        &record(ghost(RecordId::new_v7(), "a", 1), 1, 1, vec![]),
        "r",
        1,
    )
    .await;
    let b = put(
        &store,
        &record(ghost(RecordId::new_v7(), "b", 1), 1, 2, vec![]),
        "r",
        1,
    )
    .await;
    let token = spot_quarantine::authorize_close(
        &AllowAll,
        &spot_permissions::Principal::system(),
        spot_quarantine::Access::Release,
        &a,
        ClosureOutcome::Released,
    )
    .unwrap();
    let wrong = store
        .close(
            b.id,
            Closure {
                outcome: ClosureOutcome::Released,
                by: "x".to_owned(),
                at: 1,
            },
            token,
        )
        .await;
    assert!(matches!(wrong, Err(QuarantineError::Denied { .. })));
    assert_eq!(store.item(b.id).await.unwrap().unwrap().state, State::Held);
}

#[tokio::test]
async fn a_raw_client_cannot_rewrite_a_closed_row_or_close_one_without_a_closure() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "rawclosed").await;
    let open = put(
        &store,
        &record(ghost(RecordId::new_v7(), "open", 1), 1, 1, vec![]),
        "r",
        1,
    )
    .await;
    let shut = put(
        &store,
        &record(ghost(RecordId::new_v7(), "shut", 1), 1, 2, vec![]),
        "r",
        1,
    )
    .await;
    close(&store, &shut, ClosureOutcome::Discarded, "ada", 5)
        .await
        .unwrap();
    let open_id = i64::try_from(open.id.0).unwrap();
    let shut_id = i64::try_from(shut.id.0).unwrap();

    let raw = db.raw_client().await;
    // A closed row: nothing about it can change.
    for statement in [
        "UPDATE spot_quarantine_item SET detail = 'falsified' WHERE id = $1",
        "UPDATE spot_quarantine_item SET attempts = 99 WHERE id = $1",
        "UPDATE spot_quarantine_item SET state = 'held', closed_by = NULL, closed_at = NULL WHERE id = $1",
        "UPDATE spot_quarantine_item SET closed_by = 'mallory' WHERE id = $1",
        "UPDATE spot_quarantine_item SET closed_at = 0 WHERE id = $1",
    ] {
        let err = raw.execute(statement, &[&shut_id]).await.unwrap_err();
        assert!(err.as_db_error().is_some(), "{statement} must be refused");
    }
    // An open row: the identity is fixed, and a closed state needs who and when.
    for statement in [
        "UPDATE spot_quarantine_item SET seq = 99 WHERE id = $1",
        "UPDATE spot_quarantine_item SET first_seen = 99 WHERE id = $1",
        "UPDATE spot_quarantine_item SET source = 'other.site.app#hub' WHERE id = $1",
        "UPDATE spot_quarantine_item SET state = 'released' WHERE id = $1",
        "UPDATE spot_quarantine_item SET state = 'discarded', closed_by = 'x' WHERE id = $1",
        "UPDATE spot_quarantine_item SET closed_by = 'x', closed_at = 1 WHERE id = $1",
    ] {
        let err = raw.execute(statement, &[&open_id]).await.unwrap_err();
        assert!(err.as_db_error().is_some(), "{statement} must be refused");
    }
    assert_eq!(store.item(open.id).await.unwrap(), Some(open));
    let kept = store.item(shut.id).await.unwrap().unwrap();
    assert_eq!(kept.state, State::Discarded);
    assert_eq!(
        store
            .closure(shut.id)
            .await
            .unwrap()
            .map(|c| c.by)
            .as_deref(),
        Some("ada")
    );
}

#[tokio::test]
async fn the_size_caps_are_table_constraints_too() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "caps").await;
    let raw = db.raw_client().await;
    let long = "d".repeat(MAX_DETAIL_BYTES + 1);
    let err = raw
        .execute(
            "INSERT INTO spot_quarantine_item \
             (source, seq, record_name, record_version, entity, record_id, reason_code, detail, \
              first_seen, state) VALUES ('a.b.c#hub', 1, 'n', 1, 'note', 'x', 'r', $1, 0, 'held')",
            &[&long],
        )
        .await
        .unwrap_err();
    assert!(
        err.as_db_error().is_some(),
        "a long detail is refused by the table"
    );
    let long_reason = "r".repeat(129);
    let err = raw
        .execute(
            "INSERT INTO spot_quarantine_item \
             (source, seq, record_name, record_version, entity, record_id, reason_code, detail, \
              first_seen, state) VALUES ('a.b.c#hub', 1, 'n', 1, 'note', 'x', $1, '', 0, 'held')",
            &[&long_reason],
        )
        .await
        .unwrap_err();
    assert!(
        err.as_db_error().is_some(),
        "a long reason code is refused by the table"
    );
    let huge = "p".repeat(MAX_PAYLOAD_BYTES + 1);
    // A payload row cannot exceed the cap either.
    let other = put(
        &store,
        &record(ghost(RecordId::new_v7(), "ok2", 1), 1, 2, vec![]),
        "r",
        1,
    )
    .await;
    let err = raw
        .execute(
            "INSERT INTO spot_quarantine_payload (item_id, payload) VALUES ($1, $2)",
            &[&i64::try_from(other.id.0).unwrap(), &huge],
        )
        .await
        .unwrap_err();
    assert!(err.as_db_error().is_some());
}

#[tokio::test]
async fn the_store_refuses_oversize_values_before_the_database() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "storecaps").await;
    let small = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let item = NewItem::held(
        source(),
        1,
        RecordRef::of(&small),
        "r",
        "d".repeat(MAX_DETAIL_BYTES + 1),
        0,
    );
    assert!(matches!(
        store.insert(item, small.clone()).await,
        Err(QuarantineError::TooLarge { what: "detail", .. })
    ));
    let big = record(
        ghost(RecordId::new_v7(), &"x".repeat(MAX_PAYLOAD_BYTES + 1), 1),
        1,
        2,
        vec![],
    );
    let big_item = NewItem::held(source(), 2, RecordRef::of(&big), "r", "d", 0);
    assert!(matches!(
        store.insert(big_item, big).await,
        Err(QuarantineError::TooLarge {
            what: "payload",
            ..
        })
    ));
    assert!(
        store
            .list(&spot_quarantine::ItemFilter::all())
            .await
            .unwrap()
            .is_empty(),
        "nothing was stored"
    );
}

#[tokio::test]
async fn an_oversize_record_leaves_a_marker_and_a_discard_names_the_actor() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store: Arc<dyn QuarantineStore> = Arc::new(store(&db, "marker").await);
    let audit = Arc::new(RecordingAudit::new());
    let intake = Intake::new(
        FakeBackend::new(),
        Arc::new(FixedCatalog::new(Arc::new(catalog(false)))),
        Arc::clone(&store),
        Arc::new(AllowAll),
        audit.clone(),
        Arc::new(MirrorAll),
        Arc::new(ReplicaRecording::new()),
        options(),
    );
    let big = record(
        ghost(RecordId::new_v7(), &"x".repeat(MAX_PAYLOAD_BYTES + 1), 1),
        1,
        1,
        vec![],
    );
    let marker = match intake.receive(&big, 10).await.unwrap() {
        IntakeOutcome::TooLarge(item) => item,
        other => panic!("expected a marker, got {other:?}"),
    };
    assert_eq!(
        marker.reason_code,
        spot_quarantine::codes::PAYLOAD_TOO_LARGE
    );
    assert!(marker.detail.contains("sha256 "));
    let stored = store.record(marker.id).await.unwrap().unwrap();
    assert!(stored.payload_json().unwrap().len() < 4096);

    let view = intake
        .discard(
            &spot_permissions::Principal::user("alice", vec![]),
            marker.id,
            "hostile peer",
            20,
        )
        .await
        .unwrap();
    assert_eq!(view.item.closed_by.as_deref(), Some("alice"));
    assert_eq!(view.item.closed_at, Some(20));
    let entries = audit.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].action, AuditAction::Discarded);
    assert_eq!(entries[0].actor, "alice");
}
