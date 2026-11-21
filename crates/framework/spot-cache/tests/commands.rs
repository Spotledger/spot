//! Command-by-command semantics: `get`, `set`, `set_nx`, `delete`, `exists` and `expire`,
//! including the missing-key and expired-key cases of the brief's command table.

use std::sync::Arc;
use std::time::Duration;

mod common;

use common::{got, manual_clock, ms, sid};
use spot_cache::{Bytes, CacheStore, InProcessCache, InProcessConfig, Ttl};
use spot_core::ManualClock;

/// Unwraps a result the panic-style way: workspace lints deny `unwrap` and `expect`, also
/// in tests.
fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{what} failed: {error:?}"),
    }
}

fn bytes(text: &str) -> Bytes {
    Bytes::from(text.as_bytes().to_vec())
}

/// A cache on a manual clock: TTL behaviour is tested by advancing the clock, never sleeping.
fn cache() -> (InProcessCache, Arc<ManualClock>) {
    let clock = manual_clock();
    (
        InProcessCache::with_clock(InProcessConfig::new(1 << 20, 1 << 16), clock.clone()),
        clock,
    )
}

#[tokio::test]
async fn set_then_get_roundtrip() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    ok(cache.set(&site, "alpha", bytes("hello"), None).await, "set");
    assert_eq!(got(cache.get(&site, "alpha").await), Some(bytes("hello")));
    assert!(cache.delete(&site, "alpha").await);
    assert_eq!(got(cache.get(&site, "alpha").await), None);
}

#[tokio::test]
async fn missing_key_answers_neutral() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    assert_eq!(got(cache.get(&site, "nope").await), None);
    assert!(!cache.exists(&site, "nope").await);
    assert!(!cache.delete(&site, "nope").await);
    assert_eq!(cache.ttl(&site, "nope").await, Ttl::Missing);
}

#[tokio::test]
async fn expired_key_reads_as_missing() {
    let (cache, clock) = cache();
    let site = sid("s1");
    ok(
        cache
            .set(&site, "k", bytes("v"), Some(Duration::from_millis(100)))
            .await,
        "set",
    );
    assert_eq!(got(cache.get(&site, "k").await), Some(bytes("v")));
    clock.advance(ms(150));
    assert_eq!(got(cache.get(&site, "k").await), None);
    let stats = cache.stats(&site).await;
    assert_eq!(stats.expired, 1);
    assert_eq!(stats.entries, 0);
}

#[tokio::test]
async fn set_overwrites_and_resets_ttl() {
    let (cache, clock) = cache();
    let site = sid("s1");
    ok(
        cache
            .set(&site, "k", bytes("v1"), Some(Duration::from_millis(100)))
            .await,
        "set",
    );
    clock.advance(ms(50));
    ok(cache.set(&site, "k", bytes("v2"), None).await, "set");
    clock.advance(ms(500));
    assert_eq!(got(cache.get(&site, "k").await), Some(bytes("v2")));
    assert_eq!(cache.ttl(&site, "k").await, Ttl::Persistent);
}

#[tokio::test]
async fn set_nx_sets_only_when_absent() {
    let (cache, clock) = cache();
    let site = sid("s1");
    assert!(
        ok(
            cache
                .set_nx(&site, "k", bytes("first"), Some(Duration::from_millis(100)))
                .await,
            "set_nx",
        ),
        "set_nx on an absent key must store",
    );
    assert!(
        !ok(
            cache.set_nx(&site, "k", bytes("second"), None).await,
            "set_nx"
        ),
        "set_nx on a live key must not store",
    );
    assert_eq!(got(cache.get(&site, "k").await), Some(bytes("first")));
    // The refused set_nx touched neither value nor TTL.
    clock.advance(ms(150));
    assert_eq!(got(cache.get(&site, "k").await), None);
    assert!(
        ok(
            cache.set_nx(&site, "k", bytes("again"), None).await,
            "set_nx"
        ),
        "an expired key counts as absent",
    );
    assert_eq!(got(cache.get(&site, "k").await), Some(bytes("again")));
}

#[tokio::test]
async fn delete_reports_live_keys_only() {
    let (cache, clock) = cache();
    let site = sid("s1");
    ok(cache.set(&site, "live", bytes("v"), None).await, "set");
    ok(
        cache
            .set(&site, "dead", bytes("v"), Some(Duration::from_millis(100)))
            .await,
        "set",
    );
    clock.advance(ms(150));
    assert!(
        !cache.delete(&site, "dead").await,
        "an expired key is not a live removal"
    );
    assert!(cache.delete(&site, "live").await);
    assert!(!cache.delete(&site, "live").await);
    let stats = cache.stats(&site).await;
    assert_eq!(stats.expired, 1);
    assert_eq!(stats.entries, 0);
}

#[tokio::test]
async fn exists_follows_liveness() {
    let (cache, clock) = cache();
    let site = sid("s1");
    ok(
        cache
            .set(&site, "k", bytes("v"), Some(Duration::from_millis(100)))
            .await,
        "set",
    );
    assert!(cache.exists(&site, "k").await);
    clock.advance(ms(150));
    assert!(!cache.exists(&site, "k").await);
}

#[tokio::test]
async fn expire_sets_expiry_and_fails_on_missing() {
    let (cache, clock) = cache();
    let site = sid("s1");
    assert!(
        !cache
            .expire(&site, "ghost", Duration::from_millis(100))
            .await
    );
    ok(cache.set(&site, "k", bytes("v"), None).await, "set");
    assert!(cache.expire(&site, "k", Duration::from_millis(100)).await);
    clock.advance(ms(50));
    assert_eq!(got(cache.get(&site, "k").await), Some(bytes("v")));
    clock.advance(ms(500));
    assert_eq!(got(cache.get(&site, "k").await), None);
}
