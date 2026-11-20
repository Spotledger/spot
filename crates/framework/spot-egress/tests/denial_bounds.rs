//! `RateLimitedSink` keeps a bounded map: varying hosts or ports cannot grow memory, the
//! per-key limit still holds, the overflow bucket is limited too, and the window follows the
//! injected clock.
#![allow(missing_docs, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_core::ManualClock;
use spot_egress::{AddrClass, DenialRecord, DenialSink, InMemorySink, Purpose, RateLimitedSink};
use time::OffsetDateTime;

fn record(host: &str, port: u16) -> DenialRecord {
    DenialRecord {
        tenant: "t1".to_string(),
        purpose: Purpose::Webhook,
        actor: None,
        scheme: "https".to_string(),
        host: host.to_string(),
        port,
        resolved: vec![],
        class: AddrClass::Public,
        rule: "policy".to_string(),
        at: OffsetDateTime::UNIX_EPOCH,
    }
}

fn sink(
    max_per_minute: u32,
    max_keys: usize,
) -> (Arc<InMemorySink>, Arc<ManualClock>, RateLimitedSink) {
    let inner = Arc::new(InMemorySink::new());
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let limited =
        RateLimitedSink::with_limits(inner.clone(), max_per_minute, max_keys, clock.clone());
    (inner, clock, limited)
}

#[test]
fn ten_thousand_distinct_hosts_never_exceed_the_cap() {
    let (inner, _clock, limited) = sink(3, 100);
    for i in 0..10_000 {
        limited.denied(record(&format!("host-{i}.example"), 443));
        assert!(limited.tracked_keys() <= 100);
    }
    assert_eq!(limited.tracked_keys(), 100);
    // 100 keys with one denial each, plus the overflow bucket's budget of three.
    assert_eq!(inner.records().len(), 103);
    assert_eq!(limited.suppressed(), 10_000 - 103);
}

#[test]
fn varying_the_port_is_bounded_too() {
    let (_inner, _clock, limited) = sink(1, 50);
    for port in 1..=60_000u16 {
        limited.denied(record("same.example", port));
    }
    assert!(limited.tracked_keys() <= 50);
}

#[test]
fn per_key_limit_still_holds() {
    let (inner, _clock, limited) = sink(2, 10);
    for _ in 0..7 {
        limited.denied(record("a.example", 443));
    }
    assert_eq!(inner.records().len(), 2);
    assert_eq!(limited.suppressed(), 5);
    limited.denied(record("b.example", 443));
    assert_eq!(inner.records().len(), 3);
}

#[test]
fn overflow_bucket_is_rate_limited() {
    let (inner, _clock, limited) = sink(2, 1);
    limited.denied(record("first.example", 443));
    for i in 0..20 {
        limited.denied(record(&format!("other-{i}.example"), 443));
    }
    // One for the tracked key, two for the shared overflow bucket.
    assert_eq!(inner.records().len(), 3);
    assert_eq!(limited.tracked_keys(), 1);
    assert_eq!(limited.suppressed(), 18);
}

#[test]
fn window_rolls_with_the_fake_clock() {
    let (inner, clock, limited) = sink(1, 10);
    limited.denied(record("a.example", 443));
    limited.denied(record("a.example", 443));
    assert_eq!(inner.records().len(), 1);
    clock.advance(Duration::from_secs(59));
    limited.denied(record("a.example", 443));
    assert_eq!(inner.records().len(), 1);
    clock.advance(Duration::from_secs(1));
    limited.denied(record("a.example", 443));
    assert_eq!(inner.records().len(), 2);
}

#[test]
fn expired_keys_are_dropped_before_overflow_is_used() {
    let (inner, clock, limited) = sink(1, 3);
    for host in ["a", "b", "c"] {
        limited.denied(record(host, 443));
    }
    assert_eq!(limited.tracked_keys(), 3);
    clock.advance(Duration::from_secs(61));
    limited.denied(record("d", 443));
    // The expired keys made room: "d" is tracked and forwarded, not sent to overflow.
    assert_eq!(limited.tracked_keys(), 1);
    assert_eq!(inner.records().len(), 4);
    assert_eq!(limited.suppressed(), 0);
}

#[test]
fn new_keeps_the_two_argument_constructor_working() {
    let inner = Arc::new(InMemorySink::new());
    let limited = RateLimitedSink::new(inner.clone(), 1);
    limited.denied(record("a.example", 443));
    limited.denied(record("a.example", 443));
    assert_eq!(inner.records().len(), 1);
    assert_eq!(limited.records().len(), 1);
}
