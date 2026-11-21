//! The public rate limiter's table (WP-PRD-01.T53): a bounded table that evicts its oldest
//! window instead of refusing a new key, with no scan under a lock. Driven through the public
//! `RateLimiter` with an injected manual clock.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spot_core::ManualClock;
use spot_http::api::router::routes::RateClass;
use spot_http::api::router::{RATE_TABLE_CAPACITY, RateLimiter, RateLimits};
use time::OffsetDateTime;

fn addr(n: u32) -> Option<IpAddr> {
    Some(IpAddr::V4(Ipv4Addr::from(0xc000_0200 + n)))
}

fn limiter(limit: u32, shards: usize, capacity: usize) -> (Arc<RateLimiter>, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let limits = RateLimits {
        auth_limit: limit,
        auth_window: Duration::from_secs(60),
    };
    (
        Arc::new(RateLimiter::with_capacity(
            clock.clone(),
            limits,
            shards,
            capacity,
        )),
        clock,
    )
}

fn check(l: &RateLimiter, n: u32) -> Result<(), u64> {
    l.check(RateClass::Auth, addr(n))
}

#[test]
fn the_defaults_are_the_numbers_the_limiter_always_had() {
    let d = RateLimits::default();
    assert_eq!(d.auth_limit, 20);
    assert_eq!(d.auth_window, Duration::from_secs(60));
    assert_eq!(RATE_TABLE_CAPACITY, 65_536);
}

#[test]
fn a_full_table_of_live_windows_admits_a_new_address_counted_from_one() {
    let (l, clock) = limiter(2, 1, 8);
    for n in 0..8 {
        clock.advance(Duration::from_secs(1));
        assert!(check(&l, n).is_ok());
    }
    assert_eq!(l.len(), 8, "the table is full of live windows");
    // (a) a request from a new address is admitted, not refused, and counted from 1.
    assert!(check(&l, 100).is_ok());
    assert!(
        check(&l, 100).is_ok(),
        "the second request of a limit of two"
    );
    assert!(
        check(&l, 100).is_err(),
        "counted from one: the third is limited"
    );
    assert_eq!(l.len(), 8, "never above capacity");
}

#[test]
fn the_oldest_address_starts_afresh_and_a_client_inside_its_limit_is_not_locked_out() {
    let (l, clock) = limiter(2, 1, 4);
    for n in 0..4 {
        clock.advance(Duration::from_secs(1));
        assert!(check(&l, n).is_ok());
    }
    // (b) a flood of new addresses evicts the oldest windows first (0, then 1) ...
    assert!(check(&l, 50).is_ok());
    assert!(check(&l, 51).is_ok());
    // ... the evicted address starts a fresh window, and counted from one again,
    assert!(check(&l, 0).is_ok());
    assert!(check(&l, 0).is_ok());
    assert!(check(&l, 0).is_err());
    // while a client that is still inside its limit (address 3 has used one of two) was never
    // locked out by the flood.
    assert!(check(&l, 3).is_ok() || check(&l, 3).is_ok());
}

#[test]
fn a_client_that_hit_its_limit_stays_limited_while_its_entry_survives() {
    let (l, _) = limiter(2, 1, 16);
    assert!(check(&l, 1).is_ok());
    assert!(check(&l, 1).is_ok());
    assert!(check(&l, 1).is_err());
    for n in 10..14 {
        assert!(check(&l, n).is_ok());
    }
    // (c) still limited: its entry survived because the table is not full.
    assert!(check(&l, 1).is_err());
    // The residual behaviour: more addresses than the table holds inside one window push the
    // entry out and reset it. This is why the capacity is a sized, documented constant.
    for n in 100..140 {
        let _ = check(&l, n);
    }
    assert!(
        check(&l, 1).is_ok(),
        "evicted by a flood larger than the table"
    );
}

#[test]
fn many_distinct_addresses_are_fast_and_never_exceed_capacity() {
    // (d) 200,000 distinct addresses through the standard table.
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let l = RateLimiter::new(clock);
    let started = std::time::Instant::now();
    for n in 0..200_000u32 {
        assert!(check(&l, n).is_ok(), "{n}");
        if n % 2_000 == 0 {
            assert!(l.len() <= RATE_TABLE_CAPACITY, "{n}");
        }
    }
    assert!(l.len() <= RATE_TABLE_CAPACITY);
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(20), "took {elapsed:?}");
}

#[test]
fn concurrent_checks_never_exceed_the_limit_of_one_key_in_a_window() {
    // (e) several threads on the same key and on different keys.
    let (l, _) = limiter(40, 4, 2048);
    let admitted = Arc::new(AtomicUsize::new(0));
    let other_admitted = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..8u32)
        .map(|t| {
            let (l, admitted, other) = (l.clone(), admitted.clone(), other_admitted.clone());
            std::thread::spawn(move || {
                for i in 0..120u32 {
                    if check(&l, 1).is_ok() {
                        admitted.fetch_add(1, Ordering::SeqCst);
                    }
                    // Each thread also has a key of its own, limited to 40 on its own.
                    if check(&l, 10 + t).is_ok() {
                        other.fetch_add(1, Ordering::SeqCst);
                    }
                    let _ = i;
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(admitted.load(Ordering::SeqCst), 40);
    assert_eq!(other_admitted.load(Ordering::SeqCst), 8 * 40);
}

#[test]
fn a_window_that_ends_frees_the_client() {
    let (l, clock) = limiter(1, 1, 4);
    assert!(check(&l, 1).is_ok());
    assert!(check(&l, 1).is_err());
    clock.advance(Duration::from_secs(61));
    assert!(check(&l, 1).is_ok());
}
