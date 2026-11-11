//! A closed item is terminal, closing is one audited method that needs an authority token, and
//! the queue's size caps refuse (and mark) oversize input (WP-EDG-06.T5,
//! Recorded security finding).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{backend, catalog, editor, ghost, note, options, record, source, system};
use spot_quarantine::{
    Access, AllowAll, AuditAction, CatalogSource, Closure, ClosureOutcome, DenyAll, FixedCatalog,
    InMemoryStore, Intake, IntakeOutcome, ItemFilter, MAX_DETAIL_BYTES, MAX_PAYLOAD_BYTES,
    MAX_REASON_CODE_BYTES, MAX_SOURCE_BYTES, NewItem, OperationResult, QuarantineError,
    QuarantineGuard, QuarantineItem, QuarantineStore, RecordRef, RecordingAudit, RetryPolicy,
    SharedCatalog, State, authorize_close,
};
use spot_replica_apply::{MirrorAll, RecordingAudit as ReplicaRecording};
use spot_storage_contract::RecordId;
use spot_storage_fake::FakeBackend;
use spot_sync_protocol::SourceId;

struct Harness {
    intake: Intake<FakeBackend>,
    store: Arc<dyn QuarantineStore>,
    audit: Arc<RecordingAudit>,
}

fn harness(guard: Arc<dyn QuarantineGuard>) -> Harness {
    let catalog: Arc<dyn CatalogSource> = Arc::new(FixedCatalog::new(Arc::new(catalog(false))));
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
    .with_retry(RetryPolicy::sync_default());
    Harness {
        intake,
        store,
        audit,
    }
}

fn held(outcome: IntakeOutcome) -> QuarantineItem {
    match outcome {
        IntakeOutcome::Quarantined(item) => item,
        other => panic!("expected a quarantined item, got {other:?}"),
    }
}

async fn one_held(h: &Harness) -> QuarantineItem {
    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    held(h.intake.receive(&received, 10).await.unwrap())
}

#[test]
fn the_transition_table_is_closed_over_released_and_discarded() {
    use State::{Discarded, Held, Released, Retrying};
    assert!(Held.can_become(Retrying));
    assert!(Retrying.can_become(Held));
    assert!(Held.can_become(Released) && Retrying.can_become(Released));
    assert!(Held.can_become(Discarded) && Retrying.can_become(Discarded));
    for closed in [Released, Discarded] {
        for next in [Held, Retrying, Released, Discarded] {
            assert!(!closed.can_become(next), "{closed} must be terminal");
        }
    }
}

#[tokio::test]
async fn a_closed_item_cannot_have_its_metadata_rewritten() {
    let h = harness(Arc::new(AllowAll));
    let item = one_held(&h).await;
    h.intake
        .discard(&system(), item.id, "not wanted", 20)
        .await
        .unwrap();
    let closed = h.store.item(item.id).await.unwrap().unwrap();
    assert_eq!(closed.state, State::Discarded);

    let mut detail = closed.clone();
    detail.detail = "falsified".to_owned();
    let mut attempts = closed.clone();
    attempts.attempts = 99;
    let mut reopened = closed.clone();
    reopened.state = State::Held;
    for change in [detail, attempts, reopened, closed.clone()] {
        assert_eq!(
            h.store.update(&change).await.unwrap_err(),
            QuarantineError::ItemClosed { id: item.id }
        );
    }
    assert_eq!(
        h.store.item(item.id).await.unwrap().unwrap(),
        closed,
        "nothing changed"
    );
}

#[tokio::test]
async fn update_still_works_for_open_items_but_cannot_close_one() {
    let h = harness(Arc::new(AllowAll));
    let item = one_held(&h).await;

    let mut edited = item.clone();
    edited.detail = "a clearer explanation".to_owned();
    edited.attempts = 2;
    edited.state = State::Retrying;
    h.store.update(&edited).await.unwrap();
    let mut back = edited.clone();
    back.state = State::Held;
    h.store.update(&back).await.unwrap();

    for closed in [State::Released, State::Discarded] {
        let mut close = back.clone();
        close.state = closed;
        assert_eq!(
            h.store.update(&close).await.unwrap_err(),
            QuarantineError::ItemClosed { id: item.id },
            "a plain edit cannot make an item {closed}"
        );
    }
    let mut renamed = back.clone();
    renamed.first_seen += 1;
    assert!(h.store.update(&renamed).await.is_err(), "identity is fixed");
}

#[tokio::test]
async fn closing_records_who_and_when_and_twice_is_refused() {
    let h = harness(Arc::new(AllowAll));
    let item = one_held(&h).await;
    let second_open = held(
        h.intake
            .receive(&record(ghost(RecordId::new_v7(), "y", 1), 1, 2, vec![]), 10)
            .await
            .unwrap(),
    );
    let authority = |outcome| {
        authorize_close(
            &AllowAll,
            &system(),
            if outcome == ClosureOutcome::Released {
                Access::Release
            } else {
                Access::Discard
            },
            &item,
            outcome,
        )
        .unwrap()
    };
    let closed = h
        .store
        .close(
            item.id,
            Closure {
                outcome: ClosureOutcome::Discarded,
                by: "ada".to_owned(),
                at: 77,
            },
            authority(ClosureOutcome::Discarded),
        )
        .await
        .unwrap();
    assert_eq!(closed.state, State::Discarded);
    let closure = h.store.closure(item.id).await.unwrap().expect("a closure");
    assert_eq!((closure.by.as_str(), closure.at), ("ada", 77));
    assert_eq!(h.store.closure(second_open.id).await.unwrap(), None);

    assert_eq!(
        h.store
            .close(
                item.id,
                Closure {
                    outcome: ClosureOutcome::Released,
                    by: "eve".to_owned(),
                    at: 99,
                },
                authority(ClosureOutcome::Released),
            )
            .await
            .unwrap_err(),
        QuarantineError::ItemClosed { id: item.id }
    );
    let after = h.store.item(item.id).await.unwrap().unwrap();
    assert_eq!(after.state, State::Discarded);
    assert_eq!(
        h.store.closure(item.id).await.unwrap(),
        Some(closure),
        "who and when are set once"
    );
}

#[tokio::test]
async fn a_closure_token_is_for_one_item_and_one_outcome() {
    let h = harness(Arc::new(AllowAll));
    let first = one_held(&h).await;
    let second_record = record(ghost(RecordId::new_v7(), "y", 1), 1, 2, vec![]);
    let second = held(h.intake.receive(&second_record, 10).await.unwrap());

    let for_first_release = authorize_close(
        &AllowAll,
        &system(),
        Access::Release,
        &first,
        ClosureOutcome::Released,
    )
    .unwrap();
    // The token for `first` does not close `second` ...
    let wrong_item = h
        .store
        .close(
            second.id,
            Closure {
                outcome: ClosureOutcome::Released,
                by: "x".to_owned(),
                at: 1,
            },
            for_first_release,
        )
        .await;
    assert!(matches!(wrong_item, Err(QuarantineError::Denied { .. })));
    // ... and a release token does not discard.
    let for_release = authorize_close(
        &AllowAll,
        &system(),
        Access::Release,
        &first,
        ClosureOutcome::Released,
    )
    .unwrap();
    let wrong_outcome = h
        .store
        .close(
            first.id,
            Closure {
                outcome: ClosureOutcome::Discarded,
                by: "x".to_owned(),
                at: 1,
            },
            for_release,
        )
        .await;
    assert!(matches!(wrong_outcome, Err(QuarantineError::Denied { .. })));
    assert_eq!(
        h.store.item(first.id).await.unwrap().unwrap().state,
        State::Held
    );
}

#[tokio::test]
async fn no_token_without_the_permission_guard_saying_yes() {
    let h = harness(Arc::new(AllowAll));
    let item = one_held(&h).await;
    assert!(matches!(
        authorize_close(
            &DenyAll,
            &system(),
            Access::Discard,
            &item,
            ClosureOutcome::Discarded
        ),
        Err(QuarantineError::Denied { .. })
    ));
    // A view or retry is not a discard; the pairing must fit the outcome.
    assert!(matches!(
        authorize_close(
            &AllowAll,
            &system(),
            Access::View,
            &item,
            ClosureOutcome::Discarded
        ),
        Err(QuarantineError::Denied { .. })
    ));
    assert!(matches!(
        authorize_close(
            &AllowAll,
            &system(),
            Access::Discard,
            &item,
            ClosureOutcome::Released
        ),
        Err(QuarantineError::Denied { .. })
    ));
}

#[tokio::test]
async fn a_discard_and_a_release_name_the_actor_in_the_audit_and_on_the_item() {
    let h = harness(Arc::new(AllowAll));
    let item = one_held(&h).await;
    let view = h
        .intake
        .discard(&editor(), item.id, "duplicate", 30)
        .await
        .unwrap();
    assert_eq!(view.result, OperationResult::Discarded);
    assert_eq!(view.item.closed_by.as_deref(), Some("alice"));
    assert_eq!(view.item.closed_at, Some(30));
    let entries = h.audit.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].action, AuditAction::Discarded);
    assert_eq!(entries[0].actor, "alice");

    // A release through a metadata upgrade closes as the operator who asked for it.
    let shared = Arc::new(SharedCatalog::new(Arc::new(catalog(false))));
    let store: Arc<dyn QuarantineStore> = Arc::new(InMemoryStore::new());
    let audit = Arc::new(RecordingAudit::new());
    let intake = Intake::new(
        backend(),
        shared.clone(),
        Arc::clone(&store),
        Arc::new(AllowAll),
        audit.clone(),
        Arc::new(MirrorAll),
        Arc::new(ReplicaRecording::new()),
        options(),
    );
    let received = record(ghost(RecordId::new_v7(), "z", 1), 1, 1, vec![]);
    let waiting = held(intake.receive(&received, 10).await.unwrap());
    shared.replace(Arc::new(catalog(true)));
    let released = intake.release(&editor(), waiting.id, 40).await.unwrap();
    assert_eq!(released.result, OperationResult::Released);
    assert_eq!(released.item.closed_by.as_deref(), Some("alice"));
    assert_eq!(released.item.closed_at, Some(40));
    assert_eq!(audit.entries()[0].actor, "alice");

    // The automatic retry closes as `system`.
    let received = record(ghost(RecordId::new_v7(), "w", 1), 1, 2, vec![]);
    shared.replace(Arc::new(catalog(false)));
    let auto = held(intake.receive(&received, 50).await.unwrap());
    shared.replace(Arc::new(catalog(true)));
    let ran = intake.run_due(1_000_000_000).await.unwrap();
    let done = ran.iter().find(|v| v.item.id == auto.id).unwrap();
    assert_eq!(done.result, OperationResult::Released);
    assert_eq!(done.item.closed_by.as_deref(), Some("system"));
}

fn oversize_title() -> String {
    "x".repeat(MAX_PAYLOAD_BYTES + 1)
}

#[tokio::test]
async fn an_oversize_payload_is_marked_not_stored_or_truncated() {
    let h = harness(Arc::new(AllowAll));
    let big = record(
        ghost(RecordId::new_v7(), &oversize_title(), 1),
        1,
        1,
        vec![],
    );
    let marker = match h.intake.receive(&big, 10).await.unwrap() {
        IntakeOutcome::TooLarge(item) => item,
        other => panic!("expected a marker, got {other:?}"),
    };
    assert_eq!(
        marker.reason_code,
        spot_quarantine::codes::PAYLOAD_TOO_LARGE
    );
    assert_eq!(marker.state, State::Held);
    assert!(
        marker.detail.contains(source().as_str()),
        "names the sender"
    );
    assert!(
        marker.detail.contains("sha256 "),
        "carries a hash of the image"
    );
    assert!(marker.detail.len() <= MAX_DETAIL_BYTES);
    assert_eq!(marker.source, source());
    assert_eq!(marker.record_ref.version, 1);

    // What the store holds for the marker is small: the oversize image is nowhere.
    let stored = h.store.record(marker.id).await.unwrap().unwrap();
    assert!(stored.payload_json().unwrap().len() < 4096);
    // The stream moved on.
    assert_eq!(h.intake.checkpoint(&source()).unwrap().last_applied_seq, 1);

    // The marker holds no image to apply: retry and release refuse, discard works.
    assert_eq!(
        h.intake.retry(&system(), marker.id, 11).await.unwrap_err(),
        QuarantineError::MarkerOnly { id: marker.id }
    );
    assert_eq!(
        h.intake
            .release(&system(), marker.id, 11)
            .await
            .unwrap_err(),
        QuarantineError::MarkerOnly { id: marker.id }
    );
    assert!(
        h.intake
            .retry_all(&system(), "upgrade", 12)
            .await
            .unwrap()
            .is_empty()
    );
    let ran = h.intake.run_due(i64::MAX / 2).await.unwrap();
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].result, OperationResult::StillHeld);
    assert!(ran[0].condition.is_some(), "a person is needed");
    assert_eq!(ran[0].item.attempts, 0, "nothing was attempted");
    let discarded = h
        .intake
        .discard(&system(), marker.id, "hostile peer", 13)
        .await
        .unwrap();
    assert_eq!(discarded.result, OperationResult::Discarded);
}

#[tokio::test]
async fn a_payload_just_under_the_limit_is_stored_normally() {
    let h = harness(Arc::new(AllowAll));
    let title = "x".repeat(MAX_PAYLOAD_BYTES - 4096);
    let big = record(ghost(RecordId::new_v7(), &title, 1), 1, 1, vec![]);
    assert!(matches!(
        h.intake.receive(&big, 10).await.unwrap(),
        IntakeOutcome::Quarantined(_)
    ));
}

#[tokio::test]
async fn the_store_itself_refuses_oversize_values() {
    let store = InMemoryStore::new();
    let received = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let item = |reason: String, detail: String| {
        NewItem::held(source(), 1, RecordRef::of(&received), reason, detail, 0)
    };
    assert!(matches!(
        store
            .insert(
                item("r".repeat(MAX_REASON_CODE_BYTES + 1), String::new()),
                received.clone()
            )
            .await,
        Err(QuarantineError::TooLarge {
            what: "reason code",
            ..
        })
    ));
    assert!(matches!(
        store
            .insert(
                item("r".to_owned(), "d".repeat(MAX_DETAIL_BYTES + 1)),
                received.clone()
            )
            .await,
        Err(QuarantineError::TooLarge { what: "detail", .. })
    ));
    let big = record(
        ghost(RecordId::new_v7(), &oversize_title(), 1),
        1,
        1,
        vec![],
    );
    let big_item = NewItem::held(source(), 1, RecordRef::of(&big), "r", "d", 0);
    assert!(matches!(
        store.insert(big_item, big).await,
        Err(QuarantineError::TooLarge {
            what: "payload",
            ..
        })
    ));
    assert!(store.list(&ItemFilter::all()).await.unwrap().is_empty());
    // At the limits it is stored.
    store
        .insert(
            item(
                "r".repeat(MAX_REASON_CODE_BYTES),
                "d".repeat(MAX_DETAIL_BYTES),
            ),
            received,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn an_ordinary_valid_record_still_applies() {
    let h = harness(Arc::new(AllowAll));
    let good = record(note(RecordId::new_v7(), "fine", 1), 1, 1, vec![]);
    assert_eq!(
        h.intake.receive(&good, 0).await.unwrap(),
        IntakeOutcome::Applied
    );
}

fn long_sender() -> SourceId {
    let label = "a".repeat(60);
    SourceId::new(&format!("{label}.{label}.{label}.{label}")).unwrap()
}

#[tokio::test]
async fn an_overlong_sender_name_becomes_a_marker_and_the_stream_moves_on() {
    let h = harness(Arc::new(AllowAll));
    let from = long_sender();
    assert!(from.as_str().len() > MAX_SOURCE_BYTES);
    let mut bad = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    bad.source = from.clone();

    let marker = match h.intake.receive(&bad, 10).await.unwrap() {
        IntakeOutcome::TooLarge(item) => item,
        other => panic!("expected a marker, got {other:?}"),
    };
    assert_eq!(
        marker.reason_code,
        spot_quarantine::codes::PAYLOAD_TOO_LARGE
    );
    assert!(marker.source.as_str().len() <= MAX_SOURCE_BYTES);
    assert!(
        marker.detail.contains(&from.as_str().len().to_string()),
        "the size is named: {}",
        marker.detail
    );
    assert!(
        !marker.detail.contains(from.as_str()) && !marker.detail.contains(&"a".repeat(60)),
        "the text of the name is never kept: {}",
        marker.detail
    );
    let stored = h.store.record(marker.id).await.unwrap().unwrap();
    assert!(!stored.payload_json().unwrap().contains(&"a".repeat(60)));
    assert_eq!(
        h.intake.checkpoint(&from).unwrap().last_applied_seq,
        1,
        "the checkpoint moved past the marker"
    );

    // The same record again is recognised, not queued twice, and the stream still moves.
    assert_eq!(
        h.intake.receive(&bad, 11).await.unwrap(),
        IntakeOutcome::AlreadyQuarantined
    );
    assert_eq!(h.store.list(&ItemFilter::all()).await.unwrap().len(), 1);

    // A valid record from the same sender is applied after it.
    let mut good = record(note(RecordId::new_v7(), "ok", 1), 1, 2, vec![]);
    good.source = from.clone();
    assert_eq!(
        h.intake.receive(&good, 12).await.unwrap(),
        IntakeOutcome::Applied
    );
    assert_eq!(h.intake.checkpoint(&from).unwrap().last_applied_seq, 2);

    // The marker can only be discarded.
    assert_eq!(
        h.intake.retry(&system(), marker.id, 13).await.unwrap_err(),
        QuarantineError::MarkerOnly { id: marker.id }
    );
}
