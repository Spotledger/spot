//! The twelve race cases of ADR-087 section 6.3, one test each, on real PostgreSQL, plus the
//! rules around them (ownership, holder, range checks, audit of refusals, availability, sweeper
//! batches). Every test skips without `SPOT_TEST_PG_URL` and needs `btree_gist`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_core::ManualClock;
use spot_occupancy::{
    Action, Caller, ClaimRequest, HomeEdges, HubOwnsAll, Limits, OccupancyError, Outcome, Owner,
    Released, Span, State,
};
use spot_storage_contract::{
    ChangeFeed, ChangeOp, DocumentStore, ErrorKind, FeedPosition, RangePoint, RangeValue, RowLock,
    Transaction, Version, Wait,
};
use support::*;
use time::OffsetDateTime;

macro_rules! fx {
    () => {
        fx!(4)
    };
    ($units:expr) => {{
        let Some(fx) = fixture($units, Span::Dates, Limits::default()).await else {
            return;
        };
        fx
    }};
}

fn conflict(r: &Result<spot_occupancy::Hold, OccupancyError>) -> bool {
    matches!(r, Err(e @ OccupancyError::Conflict(_)) if e.storage_kind() == Some(ErrorKind::ExclusionViolation))
}

// --- case 1: two consumers hold the same unit and dates -------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_01_two_holds_of_the_same_dates_one_commits_one_conflicts() {
    let fx = fx!();
    let unit = fx.units[0];
    let (a, b) = {
        let (s1, s2) = (fx.svc.clone(), fx.svc.clone());
        let (r1, r2) = (claim(unit, 1, 3, "a"), claim(unit, 1, 3, "b"));
        let h1 = tokio::spawn(async move { s1.hold(&r1).await });
        let h2 = tokio::spawn(async move { s2.hold(&r2).await });
        (h1.await.unwrap(), h2.await.unwrap())
    };
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|x| **x).count(), 1);
    assert!(conflict(&a) || conflict(&b));
    // The refusal is audited with its code, though its transaction rolled back.
    let entries = fx.audit.entries();
    assert!(
        entries.iter().any(
            |e| e.action == Action::Hold && e.outcome == Outcome::Refused("occupancy.conflict")
        )
    );
    assert!(entries.iter().any(|e| e.outcome == Outcome::Done));
    // Exactly one record, one `held` event.
    assert_eq!(fx.count("occupancy_held").await, 1);
}

// --- case 2: a hold and a confirmed booking for the same dates ------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_02_a_hold_and_a_booking_of_the_same_dates_conflict_both_ways() {
    let fx = fx!();
    fx.svc
        .book_now(&claim(fx.units[0], 3, 5, "a"))
        .await
        .unwrap();
    assert!(conflict(&fx.svc.hold(&claim(fx.units[0], 3, 5, "b")).await));
    fx.svc.hold(&claim(fx.units[1], 3, 5, "a")).await.unwrap();
    assert!(conflict(
        &fx.svc.book_now(&claim(fx.units[1], 3, 5, "b")).await
    ));
}

// --- case 3: two takers of an expired hold --------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_03_two_takers_of_an_expired_hold_one_wins_and_the_loser_meets_the_winner() {
    let fx = fx!();
    let unit = fx.units[0];
    let old = fx.svc.hold(&claim(unit, 1, 3, "old")).await.unwrap();
    fx.clock.advance(secs(11 * 60));

    let (s1, s2) = (fx.svc.clone(), fx.svc.clone());
    let (r1, r2) = (claim(unit, 1, 3, "b"), claim(unit, 1, 3, "c"));
    let h1 = tokio::spawn(async move { s1.hold(&r1).await });
    let h2 = tokio::spawn(async move { s2.hold(&r2).await });
    let (a, b) = (h1.await.unwrap(), h2.await.unwrap());
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|x| **x).count(), 1);
    assert!(conflict(&a) || conflict(&b));

    // The old hold was deleted once, with one `expired` event, and the winner is the only row
    // on those dates.
    let expired: Vec<_> = fx
        .topics()
        .await
        .into_iter()
        .filter(|(t, _)| t == "occupancy_expired")
        .collect();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].1, old.id);
    assert!(fx.load(old.id).await.is_none());

    // The loser retries at once: the winner's hold is live, so the answer is a clean conflict
    // (not an error from the stale hold). After the winner's own expiry, the retry succeeds.
    let loser = if a.is_err() { "b" } else { "c" };
    assert!(conflict(&fx.svc.hold(&claim(unit, 1, 3, loser)).await));
    fx.clock.advance(secs(11 * 60));
    fx.svc.hold(&claim(unit, 1, 3, loser)).await.unwrap();
    assert_eq!(fx.count("occupancy_expired").await, 2);
}

// --- case 4: the sweeper deletes a hold while the holder confirms it ------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_04_a_hold_swept_before_the_confirm_fails_stale_and_never_becomes_a_booking() {
    let fx = fx!();
    let h = fx.svc.hold(&claim(fx.units[0], 1, 3, "a")).await.unwrap();
    fx.clock.advance(secs(11 * 60));
    let swept = fx.svc.sweep_once().await.unwrap();
    assert_eq!(swept.expired, 1);
    let err = fx
        .svc
        .confirm(h.id, &Caller::Holder(holder("a")))
        .await
        .unwrap_err();
    assert!(matches!(err, OccupancyError::Expired(_)));
    assert_eq!(err.storage_kind(), Some(ErrorKind::StaleVersion));
    assert!(fx.load(h.id).await.is_none());
    assert_eq!(fx.count("occupancy_confirmed").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_04_the_sweeper_skips_a_row_another_transaction_holds_locked() {
    let fx = fx!();
    let h = fx.svc.hold(&claim(fx.units[0], 1, 3, "a")).await.unwrap();
    fx.clock.advance(secs(11 * 60));
    // A confirm (or a takeover) holds the row: SKIP LOCKED leaves it for the next pass.
    let mut tx = fx.begin("test_lock").await;
    tx.load(&n("occupancy"), h.id, RowLock::Update(Wait::Block))
        .await
        .unwrap();
    assert_eq!(fx.svc.sweep_once().await.unwrap().expired, 0);
    assert!(fx.load(h.id).await.is_some());
    tx.rollback().await.unwrap();
    assert_eq!(fx.svc.sweep_once().await.unwrap().expired, 1);
    assert!(fx.load(h.id).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_04_a_confirm_racing_the_sweeper_after_expiry_never_yields_a_booking() {
    let fx = fx!();
    for round in 0..10 {
        let unit = fx.units[round % 4];
        let h = fx
            .svc
            .hold(&claim(
                unit,
                1 + (round as i64 % 2) * 4,
                3 + (round as i64 % 2) * 4,
                "a",
            ))
            .await
            .unwrap();
        fx.clock.advance(secs(11 * 60));
        let (s1, s2) = (fx.svc.clone(), fx.svc.clone());
        let id = h.id;
        let confirm =
            tokio::spawn(async move { s1.confirm(id, &Caller::Holder(holder("a"))).await });
        let sweep = tokio::spawn(async move { s2.sweep_once().await });
        let confirmed = confirm.await.unwrap();
        sweep.await.unwrap().unwrap();
        assert!(
            matches!(confirmed, Err(OccupancyError::Expired(_))),
            "{confirmed:?}"
        );
        if let Some(doc) = fx.load(id).await {
            // The sweeper skipped it while the confirm held the lock: still a plain hold.
            assert_eq!(
                doc.data.fields.get(&n("state")),
                Some(&spot_storage_contract::Value::Text("tentative".into()))
            );
            fx.svc.sweep_once().await.unwrap();
        }
        assert!(fx.load(id).await.is_none());
    }
    assert_eq!(fx.count("occupancy_confirmed").await, 0);
}

// --- case 5: the holder confirms after expires_at but before the sweep ----------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_05_a_late_confirm_is_stale_even_before_the_sweep_and_an_early_one_wins() {
    let fx = fx!();
    let h = fx.svc.hold(&claim(fx.units[0], 1, 3, "a")).await.unwrap();
    let expires = h.expires_at.unwrap();

    // One second before the deadline: confirms.
    fx.clock.advance(secs(10 * 60 - 1));
    let ok = fx
        .svc
        .confirm(h.id, &Caller::Holder(holder("a")))
        .await
        .unwrap();
    assert_eq!(ok.state, State::Confirmed);
    assert_eq!(ok.expires_at, None);
    assert!(ok.version > h.version);
    assert!(expires > fx.clock_now());

    // Exactly at the deadline, and after it: stale, the row stays a tentative hold.
    for (unit, extra) in [(fx.units[1], 0u64), (fx.units[2], 5)] {
        let h = fx.svc.hold(&claim(unit, 1, 3, "a")).await.unwrap();
        fx.clock.advance(secs(10 * 60 + extra));
        let err = fx
            .svc
            .confirm(h.id, &Caller::Holder(holder("a")))
            .await
            .unwrap_err();
        assert!(matches!(err, OccupancyError::Expired(_)), "{err:?}");
        assert_eq!(err.storage_kind(), Some(ErrorKind::StaleVersion));
        assert!(fx.load(h.id).await.is_some());
    }
}

// --- case 6: a hold extending into a confirmed booking --------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_06_a_hold_overlapping_a_booking_conflicts_and_an_adjacent_one_does_not() {
    let fx = fx!();
    let unit = fx.units[0];
    fx.svc.book_now(&claim(unit, 3, 5, "a")).await.unwrap();
    assert!(conflict(&fx.svc.hold(&claim(unit, 4, 6, "b")).await));
    assert!(conflict(&fx.svc.hold(&claim(unit, 2, 4, "b")).await));
    assert!(conflict(&fx.svc.hold(&claim(unit, 1, 9, "b")).await));
    fx.svc.hold(&claim(unit, 5, 7, "b")).await.unwrap();
    fx.svc.hold(&claim(unit, 1, 3, "b")).await.unwrap();
}

// --- case 7: a unit deleted while held ------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_07_a_unit_cannot_be_deleted_while_a_hold_points_at_it() {
    let fx = fx!();
    let unit = fx.units[0];
    fx.svc.hold(&claim(unit, 1, 3, "a")).await.unwrap();
    let mut tx = fx.begin("test_delete_unit").await;
    let result = match tx.delete(&n("room"), unit, Version::FIRST).await {
        Ok(()) => tx.commit().await,
        Err(e) => {
            tx.rollback().await.unwrap();
            Err(e)
        }
    };
    assert_eq!(result.unwrap_err().kind, ErrorKind::ReferenceViolation);
}

// --- case 8: two holds, adjacent ------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_08_adjacent_holds_both_commit() {
    let fx = fx!();
    let unit = fx.units[0];
    let (s1, s2) = (fx.svc.clone(), fx.svc.clone());
    let (r1, r2) = (claim(unit, 1, 3, "a"), claim(unit, 3, 5, "b"));
    let h1 = tokio::spawn(async move { s1.hold(&r1).await });
    let h2 = tokio::spawn(async move { s2.hold(&r2).await });
    h1.await.unwrap().unwrap();
    h2.await.unwrap().unwrap();
    assert_eq!(fx.count("occupancy_held").await, 2);
}

// --- case 9: a consumer holds many units (not a storage rule) -------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_09_one_holder_may_hold_many_units_a_per_user_cap_is_an_app_rule() {
    let fx = fx!(4);
    for unit in fx.units.clone() {
        fx.svc.hold(&claim(unit, 1, 3, "greedy")).await.unwrap();
    }
    assert_eq!(fx.count("occupancy_held").await, 4);
}

// --- case 10: clock skew between two app servers --------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_10_a_node_behind_by_the_skew_sees_a_hold_as_longer_never_shorter() {
    let fx = fx!();
    let unit = fx.units[0];
    // Node A writes the hold at T. Node B's clock is 60 s behind A's.
    let clock_b = Arc::new(ManualClock::new(start() - time::Duration::seconds(60)));
    let b = fx.service(
        clock_b.clone(),
        Owner::Hub,
        Arc::new(HubOwnsAll),
        Limits::default(),
    );
    fx.svc.hold(&claim(unit, 1, 3, "a")).await.unwrap();

    // Real time passes 10 minutes: A's clock says the hold just expired; B's says 60 s remain.
    fx.clock.advance(secs(10 * 60));
    clock_b.advance(secs(10 * 60));
    assert!(
        conflict(&b.hold(&claim(unit, 1, 3, "b")).await),
        "B must not shorten the hold"
    );
    // Once B's own clock passes the expiry, B takes it over.
    clock_b.advance(secs(61));
    b.hold(&claim(unit, 1, 3, "b")).await.unwrap();
}

// --- case 11: restore to a point before the hold --------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_11_restored_holds_keep_their_expiry_and_the_sweeper_clears_them() {
    let fx = fx!();
    let past = start() - time::Duration::minutes(30);
    let restored = fx.raw(fx.units[0], nights(1, 3), "tentative", "user:a", Some(past));
    let mut tx = fx.begin("test_restore").await;
    tx.insert(&restored).await.unwrap();
    tx.commit().await.unwrap();
    // Intact as written.
    let doc = fx.load(restored.id).await.unwrap();
    assert_eq!(
        doc.data.fields.get(&n("expires_at")),
        Some(&spot_storage_contract::Value::DateTime(past))
    );
    let swept = fx.svc.sweep_once().await.unwrap();
    assert_eq!(swept.expired, 1);
    assert!(fx.load(restored.id).await.is_none());
    assert_eq!(fx.count("occupancy_expired").await, 1);
}

// --- case 12: a hold confirmed in the transaction that creates it ---------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_12_book_now_is_one_insert_reported_only_as_the_confirmed_version() {
    let fx = fx!();
    let h = fx
        .svc
        .book_now(&claim(fx.units[0], 1, 3, "a"))
        .await
        .unwrap();
    assert_eq!(h.state, State::Confirmed);
    assert_eq!(h.expires_at, None);
    assert_eq!(h.version, Version::FIRST);

    let topics: Vec<_> = fx
        .topics()
        .await
        .into_iter()
        .filter(|(_, id)| *id == h.id)
        .map(|(t, _)| t)
        .collect();
    assert_eq!(topics, ["occupancy_confirmed"]);

    let changes: Vec<_> = fx
        .backend
        .read_changes(FeedPosition::START, 100_000)
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.document.id == h.id)
        .collect();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].op, ChangeOp::Insert);
    assert_eq!(changes[0].version, Version::FIRST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn case_12_a_record_created_and_deleted_in_one_transaction_has_no_change_entry() {
    let fx = fx!();
    let doc = fx.raw(
        fx.units[0],
        nights(1, 3),
        "tentative",
        "user:a",
        Some(start()),
    );
    let mut tx = fx.begin("test_create_delete").await;
    tx.insert(&doc).await.unwrap();
    tx.delete(&n("occupancy"), doc.id, Version::FIRST)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let changes = fx
        .backend
        .read_changes(FeedPosition::START, 100_000)
        .await
        .unwrap();
    assert!(changes.iter().all(|c| c.document.id != doc.id));
}

// --- the rules around the cases -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_holder_confirms_or_releases_and_an_operator_may_do_either() {
    let fx = fx!();
    let h = fx.svc.hold(&claim(fx.units[0], 1, 3, "a")).await.unwrap();
    let other = Caller::Holder(holder("mallory"));
    assert_eq!(
        fx.svc.confirm(h.id, &other).await.unwrap_err(),
        OccupancyError::NotHolder
    );
    assert_eq!(
        fx.svc.release(h.id, &other).await.unwrap_err(),
        OccupancyError::NotHolder
    );
    assert!(fx.load(h.id).await.is_some());
    assert!(
        fx.audit
            .entries()
            .iter()
            .any(|e| e.outcome == Outcome::Refused("occupancy.not_holder"))
    );

    let confirmed = fx.svc.confirm(h.id, &Caller::Operator).await.unwrap();
    assert_eq!(confirmed.state, State::Confirmed);
    // A second confirm is the same booking, with no second event.
    let again = fx
        .svc
        .confirm(h.id, &Caller::Holder(holder("a")))
        .await
        .unwrap();
    assert_eq!(again.version, confirmed.version);
    assert_eq!(fx.count("occupancy_confirmed").await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_deletes_the_row_frees_the_dates_and_a_second_release_succeeds() {
    let fx = fx!();
    let unit = fx.units[0];
    let h = fx.svc.book_now(&claim(unit, 1, 3, "a")).await.unwrap();
    assert!(conflict(&fx.svc.hold(&claim(unit, 2, 4, "b")).await));
    let first = fx
        .svc
        .release(h.id, &Caller::Holder(holder("a")))
        .await
        .unwrap();
    assert!(matches!(first, Released::Released(_)));
    assert!(fx.load(h.id).await.is_none());
    assert_eq!(fx.count("occupancy_cancelled").await, 1);
    let second = fx
        .svc
        .release(h.id, &Caller::Holder(holder("a")))
        .await
        .unwrap();
    assert_eq!(second, Released::AlreadyGone);
    assert_eq!(fx.count("occupancy_cancelled").await, 1);
    fx.svc.hold(&claim(unit, 2, 4, "b")).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_requests_are_refused_before_storage_and_audited() {
    let fx = fx!();
    let unit = fx.units[0];
    // Empty range, an instant range on a date field, too long a range, ttl out of bounds.
    let instants = ClaimRequest {
        during: RangeValue::half_open(
            RangePoint::DateTime(start()),
            RangePoint::DateTime(start() + time::Duration::hours(1)),
        ),
        ..claim(unit, 1, 3, "a")
    };
    let long_ttl = ClaimRequest {
        ttl: Some(secs(24 * 3600)),
        ..claim(unit, 1, 3, "a")
    };
    let zero_ttl = ClaimRequest {
        ttl: Some(secs(0)),
        ..claim(unit, 1, 3, "a")
    };
    let bad = vec![
        claim(unit, 3, 3, "a"),
        instants,
        claim(unit, 1, 400, "a"),
        long_ttl,
        zero_ttl,
    ];
    for r in &bad {
        let e = fx.svc.hold(r).await.unwrap_err();
        assert!(matches!(e, OccupancyError::InvalidRange(_)), "{e:?}");
    }
    assert_eq!(fx.count("occupancy_held").await, 0);
    let refused = fx
        .audit
        .entries()
        .iter()
        .filter(|e| e.outcome == Outcome::Refused("occupancy.invalid_range"))
        .count();
    assert_eq!(refused, bad.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_does_not_own_the_unit_neither_holds_nor_confirms_nor_sweeps_it() {
    let fx = fx!();
    let (mine, theirs) = (fx.units[0], fx.units[1]);
    let routes = Arc::new(HomeEdges::new().with_home(theirs, "branch1"));
    let hub = fx.service(
        Arc::new(ManualClock::new(start())),
        Owner::Hub,
        routes.clone(),
        Limits::default(),
    );
    let edge = fx.service(
        Arc::new(ManualClock::new(start())),
        Owner::Edge("branch1".into()),
        routes,
        Limits::default(),
    );

    // The hub owns `mine`; the edge owns `theirs`.
    hub.hold(&claim(mine, 1, 3, "a")).await.unwrap();
    let e = hub.hold(&claim(theirs, 1, 3, "a")).await.unwrap_err();
    assert_eq!(
        e,
        OccupancyError::NotOwner {
            unit: theirs,
            owner: Owner::Edge("branch1".into())
        }
    );
    let e = edge.hold(&claim(mine, 5, 7, "a")).await.unwrap_err();
    assert!(matches!(
        e,
        OccupancyError::NotOwner {
            owner: Owner::Hub,
            ..
        }
    ));
    let held = edge.hold(&claim(theirs, 1, 3, "a")).await.unwrap();
    // The hub cannot confirm or release a record on the edge's unit either.
    assert!(matches!(
        hub.confirm(held.id, &Caller::Operator).await.unwrap_err(),
        OccupancyError::NotOwner { .. }
    ));
    assert!(matches!(
        hub.release(held.id, &Caller::Operator).await.unwrap_err(),
        OccupancyError::NotOwner { .. }
    ));
    // Nothing was written by the refused requests; the audit says so.
    assert_eq!(fx.count("occupancy_held").await, 2);
    assert!(
        fx.audit
            .entries()
            .iter()
            .any(|e| e.outcome == Outcome::Refused("partition.not_owner"))
    );

    // The hub's sweeper leaves the edge's expired hold alone and reports it.
    hub.sweep_once().await.unwrap();
    let late = Arc::new(ManualClock::new(start() + time::Duration::hours(1)));
    let late_hub = fx.service(
        late,
        Owner::Hub,
        Arc::new(HomeEdges::new().with_home(theirs, "branch1")),
        Limits::default(),
    );
    let swept = late_hub.sweep_once().await.unwrap();
    assert_eq!(swept.expired, 1, "only the hub's own hold");
    assert_eq!(swept.not_owned, 1, "the edge's hold is left for the edge");
    assert!(fx.load(held.id).await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn availability_lists_bookings_and_live_holds_without_the_holder_and_skips_expired() {
    let fx = fx!();
    let unit = fx.units[0];
    fx.svc.book_now(&claim(unit, 1, 3, "a")).await.unwrap();
    fx.svc.hold(&claim(unit, 5, 7, "b")).await.unwrap();
    let busy = fx.svc.availability(unit, &nights(1, 10)).await.unwrap();
    assert_eq!(busy.len(), 2);
    assert!(
        busy.iter()
            .any(|b| b.state == State::Confirmed && b.during == nights(1, 3))
    );
    assert!(
        busy.iter()
            .any(|b| b.state == State::Tentative && b.during == nights(5, 7))
    );
    // Touching ranges do not overlap.
    assert!(
        fx.svc
            .availability(unit, &nights(3, 5))
            .await
            .unwrap()
            .is_empty()
    );
    // Another unit is free.
    assert!(
        fx.svc
            .availability(fx.units[1], &nights(1, 10))
            .await
            .unwrap()
            .is_empty()
    );
    // An expired, unswept hold blocks nothing.
    fx.clock.advance(secs(11 * 60));
    let busy = fx.svc.availability(unit, &nights(1, 10)).await.unwrap();
    assert_eq!(busy.len(), 1);
    assert_eq!(busy[0].state, State::Confirmed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sweeper_works_in_bounded_batches_and_captures_one_event_per_hold() {
    let limits = Limits {
        sweep_batch: 2,
        ..Limits::default()
    };
    let Some(fx) = fixture(5, Span::Dates, limits).await else {
        return;
    };
    for unit in fx.units.clone() {
        fx.svc.hold(&claim(unit, 1, 3, "a")).await.unwrap();
    }
    // A live one is never touched.
    fx.clock.advance(secs(11 * 60));
    let live = fx.svc.hold(&claim(fx.units[0], 20, 22, "a")).await.unwrap();

    let first = fx.svc.sweep_once().await.unwrap();
    assert_eq!((first.expired, first.more), (2, true));
    let rest = fx.svc.sweep_all(10).await.unwrap();
    assert_eq!(rest.expired, 3);
    assert!(!rest.more);
    assert_eq!(fx.count("occupancy_expired").await, 5);
    assert!(fx.load(live.id).await.is_some());
    assert_eq!(fx.svc.sweep_once().await.unwrap().expired, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_takes_over_an_expired_hold_without_the_sweeper_and_emits_expired() {
    let fx = fx!();
    let unit = fx.units[0];
    let old = fx.svc.hold(&claim(unit, 1, 5, "old")).await.unwrap();
    fx.clock.advance(secs(11 * 60));
    // A different, overlapping range: the expired hold is deleted in the claim's transaction.
    let taken = fx.svc.book_now(&claim(unit, 3, 7, "new")).await.unwrap();
    assert_eq!(taken.state, State::Confirmed);
    assert!(fx.load(old.id).await.is_none());
    let expired: Vec<_> = fx
        .topics()
        .await
        .into_iter()
        .filter(|(t, _)| t == "occupancy_expired")
        .map(|(_, id)| id)
        .collect();
    assert_eq!(expired, [old.id]);
    // The takeover is scoped: an expired hold on another unit is untouched.
    let other = fx.svc.hold(&claim(fx.units[1], 1, 5, "x")).await.unwrap();
    fx.clock.advance(secs(11 * 60));
    fx.svc.book_now(&claim(unit, 20, 22, "new")).await.unwrap();
    assert!(fx.load(other.id).await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_service_refuses_a_catalog_without_the_exclusion() {
    let Some(fx) = fixture(1, Span::Dates, Limits::default()).await else {
        return;
    };
    // A catalog with no occupancy entity at all is refused at construction, loudly.
    let empty = Arc::new(
        spot_storage_contract::Catalog::new(
            1,
            spot_storage_contract::Collation::new("und").unwrap(),
            vec![],
        )
        .unwrap(),
    );
    let built = spot_occupancy::Occupancy::new(
        fx.backend.clone(),
        empty,
        fx.clock.clone() as Arc<dyn spot_core::Clock>,
        Owner::Hub,
        Arc::new(HubOwnsAll),
        fx.audit.clone(),
        Limits::default(),
    );
    assert_eq!(built.err().map(|e| e.kind), Some(ErrorKind::UnknownEntity));
}

impl Fx {
    fn clock_now(&self) -> OffsetDateTime {
        use spot_core::Clock;
        self.clock.now_utc()
    }
}
