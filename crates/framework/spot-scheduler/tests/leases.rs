//! Lease rules and fencing: tokens increase strictly, a stale holder cannot renew, release or
//! write, and expiry hands the name to the next acquirer.

mod common;

use std::time::Duration;

use common::ts;
use spot_scheduler::{
    FenceGuard, FencedWrite, FencingToken, InMemoryLeaseStore, LeaseStore, still_holds,
};

const TTL: Duration = Duration::from_secs(30);

fn code<T: std::fmt::Debug>(r: spot_scheduler::Result<T>) -> String {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.code.to_owned(),
    }
}

#[test]
fn first_token_is_one_and_a_live_lease_refuses_a_second_holder() {
    let store = InMemoryLeaseStore::new();
    let now = ts("2026-10-07T00:00:00Z");
    let a = store
        .acquire("scheduler:s", "a", TTL, now)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(a.token, FencingToken(1));
    assert_eq!(
        code(store.acquire("scheduler:s", "b", TTL, now)),
        "scheduler.lease_lost"
    );
    // A different name has its own counter.
    let other = store
        .acquire("scheduler:t", "b", TTL, now)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(other.token, FencingToken(1));
}

#[test]
fn tokens_increase_strictly_across_release_and_expiry() {
    let store = InMemoryLeaseStore::new();
    let mut now = ts("2026-10-07T00:00:00Z");
    let mut last = 0;
    for round in 0..6 {
        let lease = store
            .acquire("scheduler:s", "n", TTL, now)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            lease.token.0 > last,
            "round {round}: {} after {last}",
            lease.token.0
        );
        last = lease.token.0;
        if round % 2 == 0 {
            store.release(&lease).unwrap_or_else(|e| panic!("{e}"));
        } else {
            // Let it expire instead.
            now += jiff::Span::new().seconds(31);
        }
    }
}

#[test]
fn a_stale_holder_cannot_renew_release_or_write() {
    let store = InMemoryLeaseStore::new();
    let t0 = ts("2026-10-07T00:00:00Z");
    let old = store
        .acquire("scheduler:s", "a", TTL, t0)
        .unwrap_or_else(|e| panic!("{e}"));
    // `a` pauses past the ttl; `b` takes the name.
    let t1 = t0 + jiff::Span::new().seconds(45);
    let fresh = store
        .acquire("scheduler:s", "b", TTL, t1)
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(fresh.token > old.token);
    assert_eq!(code(store.renew(&old, TTL, t1)), "scheduler.lease_lost");
    assert_eq!(code(store.release(&old)), "scheduler.lease_lost");
    assert!(!still_holds(&store, &old));
    assert!(still_holds(&store, &fresh));
    // The old release did not disturb the new holder.
    assert_eq!(
        store.current("scheduler:s").map(|l| l.token),
        Some(fresh.token)
    );
    assert_eq!(
        code(FencedWrite::check(&store, &old)),
        "scheduler.lease_lost"
    );
    assert!(FencedWrite::check(&store, &fresh).is_ok());
}

#[test]
fn renew_extends_the_lease_and_keeps_the_token() {
    let store = InMemoryLeaseStore::new();
    let t0 = ts("2026-10-07T00:00:00Z");
    let lease = store
        .acquire("scheduler:s", "a", TTL, t0)
        .unwrap_or_else(|e| panic!("{e}"));
    let t1 = t0 + jiff::Span::new().seconds(20);
    let renewed = store
        .renew(&lease, TTL, t1)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(renewed.token, lease.token);
    assert!(renewed.expires_at > lease.expires_at);
    // 40 s after the start would have expired the original ttl, but not the renewed one.
    let t2 = t0 + jiff::Span::new().seconds(40);
    assert_eq!(
        code(store.acquire("scheduler:s", "b", TTL, t2)),
        "scheduler.lease_lost"
    );
}

#[test]
fn a_fenced_counter_refuses_the_old_holder() {
    let store = InMemoryLeaseStore::new();
    let guard = FenceGuard::new();
    let t0 = ts("2026-10-07T00:00:00Z");
    let old = store
        .acquire("scheduler:s", "a", TTL, t0)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        guard.bump(&store, &old).unwrap_or_else(|e| panic!("{e}")),
        1
    );
    let t1 = t0 + jiff::Span::new().seconds(45);
    let fresh = store
        .acquire("scheduler:s", "b", TTL, t1)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        guard.bump(&store, &fresh).unwrap_or_else(|e| panic!("{e}")),
        2
    );
    // The resurrected old holder is refused and the value stays put.
    assert_eq!(code(guard.bump(&store, &old)), "scheduler.lease_lost");
    assert_eq!(guard.get(), (2, Some(fresh.token)));
}
