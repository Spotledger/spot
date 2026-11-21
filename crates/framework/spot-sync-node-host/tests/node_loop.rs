//! The loop against a scripted hub on the real wire: convergence, resume after a cut at every
//! protocol step, backoff, offline for days, the stop signal and the push side.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use spot_core::Clock as _;
use spot_sync_closure::RecordKey;
use support::*;

fn stored(rig: &Rig, entity: &str, id: &str) -> bool {
    rig.apply
        .0
        .lock()
        .unwrap()
        .stored
        .contains_key(&(TENANT.to_owned(), RecordKey::new(entity, id)))
}

#[tokio::test]
async fn changes_at_the_hub_appear_at_the_edge_and_the_ack_is_the_applied_prefix() {
    let rig = Rig::new();
    for n in 1..=10 {
        rig.hub
            .queue_bundle(&bundle("Customer", &format!("c{n}"), n, &[]));
    }
    let parts = rig.parts(Rig::settings(), false);
    rig.run_until(parts, || *rig.cursor.0.lock().unwrap() == 10)
        .await
        .unwrap();
    assert!(stored(&rig, "Customer", "c10"));
    // The pull cursor advanced only after each batch applied: the last pull asked for 11.
    let pulls = rig.hub.state.lock().unwrap().pulls.clone();
    assert_eq!(pulls.first(), Some(&1));
    assert!(pulls.contains(&11));
    assert_eq!(rig.status.snapshot().applied, 10);
}

#[tokio::test]
async fn a_cut_in_the_middle_of_a_pull_loses_nothing() {
    let rig = Rig::new();
    for n in 1..=6 {
        rig.hub
            .queue_bundle(&bundle("Customer", &format!("c{n}"), n, &[]));
    }
    rig.hub.fail_pulls.store(2, Ordering::SeqCst);
    let parts = rig.parts(Rig::settings(), false);
    rig.run_until(parts, || *rig.cursor.0.lock().unwrap() == 6)
        .await
        .unwrap();
    assert_eq!(rig.apply.0.lock().unwrap().stored.len(), 6);
    // Two failed sessions: two waits, each at least the minimum reconnect interval.
    let waits = rig.sleeper.waits();
    assert!(waits.len() >= 2);
    assert!(waits[0] >= Duration::from_secs(1));
}

#[tokio::test]
async fn a_crash_between_apply_and_cursor_makes_the_resend_a_counted_replay() {
    let rig = Rig::new();
    for n in 1..=4 {
        rig.hub
            .queue_bundle(&bundle("Customer", &format!("c{n}"), n, &[]));
    }
    let parts = rig.parts(Rig::settings(), false);
    rig.run_until(parts, || *rig.cursor.0.lock().unwrap() == 4)
        .await
        .unwrap();
    assert_eq!(rig.apply.0.lock().unwrap().calls, 4);
    // The bundles are durable but the cursor write never happened (a kill between the two): the
    // next start pulls from the beginning, and the intake recognises every record.
    *rig.cursor.0.lock().unwrap() = 0;
    let parts = rig.parts(Rig::settings(), false);
    rig.run_until(parts, || *rig.cursor.0.lock().unwrap() == 4)
        .await
        .unwrap();
    assert_eq!(
        rig.apply.0.lock().unwrap().calls,
        4,
        "nothing was applied twice"
    );
    assert_eq!(rig.apply.0.lock().unwrap().stored.len(), 4);
}

#[tokio::test]
async fn a_hub_that_refuses_the_handshake_is_an_offline_state_not_a_crash() {
    let rig = Rig::new();
    rig.hub.refuse_hello.store(true, Ordering::SeqCst);
    let parts = rig.parts(Rig::settings(), false);
    rig.run_until(parts, || rig.status.snapshot().last_error.is_some())
        .await
        .unwrap();
    assert_eq!(
        rig.status.snapshot().last_error,
        Some(spot_sync_node_host::error::HUB_REFUSED)
    );
}

#[tokio::test]
async fn offline_for_three_days_then_catch_up_without_losing_anything() {
    let rig = Rig::new();
    rig.hub.down.store(true, Ordering::SeqCst);
    for n in 1..=8 {
        rig.hub
            .queue_bundle(&bundle("Customer", &format!("c{n}"), n, &[]));
    }
    let parts = rig.parts(Rig::settings(), false);
    let three_days = Duration::from_secs(3 * 24 * 3_600);
    let started = rig.clock.monotonic();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(
        spot_sync_node_host::NodeLoop::new(parts)
            .unwrap()
            .run(stop_rx),
    );
    // The manual sleeper advances the clock by every backoff wait: let virtual time pass.
    for _ in 0..200_000 {
        if rig.clock.monotonic().saturating_sub(started) >= three_days {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(rig.clock.monotonic().saturating_sub(started) >= three_days);
    let offline = rig.status.snapshot();
    assert_eq!(offline.state, spot_sync_node_host::LinkState::Offline);
    let line = offline.readiness(rig.clock.now_utc().unix_timestamp());
    assert!(line.starts_with("sync_node: offline 3d"), "{line}");
    // The longest wait is the cap: five minutes, never more.
    assert!(
        rig.sleeper
            .waits()
            .into_iter()
            .all(|w| w <= Duration::from_secs(300))
    );
    // The hub comes back: the edge catches up.
    rig.hub.down.store(false, Ordering::SeqCst);
    for _ in 0..50_000 {
        if *rig.cursor.0.lock().unwrap() == 8 {
            break;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let _ = stop_tx.send(true);
    handle.await.unwrap().unwrap();
    assert_eq!(*rig.cursor.0.lock().unwrap(), 8);
    assert_eq!(rig.status.snapshot().offline_since, None);
}

#[tokio::test]
async fn a_stop_ends_the_loop_cleanly_even_while_offline() {
    let rig = Rig::new();
    rig.hub.down.store(true, Ordering::SeqCst);
    let parts = rig.parts(Rig::settings(), false);
    rig.run_until(parts, || rig.status.snapshot().failures >= 1)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_edge_pushes_its_own_outbox_with_dense_sequences_over_gaps() {
    let rig = Rig::new();
    // Outbox positions skip (events the hub does not forward): the push sequence is dense.
    for (position, name) in [
        (3, "Customer/a"),
        (7, "Customer/b"),
        (9, "Invoice/c"),
        (20, "Invoice/d"),
    ] {
        rig.outbox.push(position, name, true);
    }
    let parts = rig.parts(Rig::settings(), true);
    rig.run_until(parts, || rig.hub.pushed_through() == 4)
        .await
        .unwrap();
    let state = rig.hub.state.lock().unwrap();
    assert_eq!(
        state.pushed.keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    drop(state);
    let cursor = *rig.push_cursor.0.lock().unwrap();
    assert_eq!((cursor.seq, cursor.position), (4, 20));
    assert_eq!(rig.status.snapshot().pushed, 4);
}

#[tokio::test]
async fn a_cut_after_a_push_before_its_ack_resends_and_the_hub_absorbs_it() {
    let rig = Rig::new();
    for position in 1..=3 {
        rig.outbox
            .push(position, &format!("Customer/c{position}"), true);
    }
    rig.hub.cut_push_reply.store(1, Ordering::SeqCst);
    let parts = rig.parts(Rig::settings(), true);
    rig.run_until(parts, || rig.push_cursor.0.lock().unwrap().seq == 3)
        .await
        .unwrap();
    let state = rig.hub.state.lock().unwrap();
    assert_eq!(state.pushed.len(), 3);
    // The batch arrived twice (the first reply was cut) and was applied once.
    assert!(state.push_arrivals.values().any(|n| *n >= 2));
}

#[tokio::test]
async fn a_cut_in_the_middle_of_a_push_resumes_from_the_stored_cursor() {
    let rig = Rig::new();
    for position in 1..=6 {
        rig.outbox
            .push(position, &format!("Customer/c{position}"), true);
    }
    rig.hub.fail_pushes.store(2, Ordering::SeqCst);
    let parts = rig.parts(Rig::settings(), true);
    rig.run_until(parts, || rig.hub.pushed_through() == 6)
        .await
        .unwrap();
    assert_eq!(rig.push_cursor.0.lock().unwrap().seq, 6);
}

#[tokio::test]
async fn a_record_the_node_does_not_own_is_never_pushed() {
    let rig = Rig::new();
    rig.outbox.push(1, "Customer/mine", true);
    rig.outbox.push(2, "Customer/theirs", false);
    let parts = rig.parts(Rig::settings(), true);
    rig.run_until(parts, || {
        rig.status.snapshot().last_error == Some(spot_sync_node_host::error::NOT_OWNED)
    })
    .await
    .unwrap();
    // The offer failed as a whole: nothing of it reached the hub, the loud failure is the signal.
    assert!(rig.hub.state.lock().unwrap().pushed.is_empty());
}

#[tokio::test]
async fn the_backoff_doubles_to_the_cap_and_never_passes_it() {
    let rig = Rig::new();
    rig.hub.down.store(true, Ordering::SeqCst);
    let mut settings = Rig::settings();
    settings.min_reconnect_interval = Duration::ZERO;
    let parts = rig.parts(settings, false);
    rig.run_until(parts, || rig.sleeper.waits().len() >= 12)
        .await
        .unwrap();
    let seconds: Vec<u64> = rig
        .sleeper
        .waits()
        .iter()
        .take(12)
        .map(Duration::as_secs)
        .collect();
    assert_eq!(
        seconds,
        vec![1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300]
    );
}

#[tokio::test]
async fn the_breaker_stops_dialling_a_dead_hub() {
    let mut rig = Rig::new();
    rig.breaker =
        spot_sync_http::BreakerConfig::new(2, Duration::from_secs(10_000_000), 1).unwrap();
    rig.hub.down.store(true, Ordering::SeqCst);
    let parts = rig.parts(Rig::settings(), false);
    rig.run_until(parts, || {
        rig.status.snapshot().last_error == Some(spot_sync_http::error::CIRCUIT_OPEN)
    })
    .await
    .unwrap();
    // Two real attempts opened it; the later sessions never reached the transport.
    assert_eq!(rig.hub.state.lock().unwrap().refused, 2);
}
