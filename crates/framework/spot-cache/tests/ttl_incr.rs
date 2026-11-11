//! Command-by-command semantics: `ttl`, `incr_by`, `delete_prefix`, `clear_site` and
//! `stats`, with the error and expiry cases from the brief's command table.

use std::sync::Arc;
use std::time::Duration;

mod common;

use common::{got, manual_clock, ms, sid};
use spot_cache::{
    Bytes, CacheError, CacheStore, ENTRY_OVERHEAD_BYTES, InProcessCache, InProcessConfig, Ttl,
};
use spot_core::ManualClock;

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{what} failed: {error:?}"),
    }
}

fn bytes(text: &str) -> Bytes {
    Bytes::from(text.as_bytes().to_vec())
}

fn cache() -> (InProcessCache, Arc<ManualClock>) {
    let clock = manual_clock();
    (
        InProcessCache::with_clock(InProcessConfig::new(1 << 20, 1 << 16), clock.clone()),
        clock,
    )
}

#[tokio::test]
async fn ttl_reports_the_three_states() {
    let (cache, clock) = cache();
    let site = sid("s1");
    ok(cache.set(&site, "eternal", bytes("v"), None).await, "set");
    ok(
        cache
            .set(
                &site,
                "fleeting",
                bytes("v"),
                Some(Duration::from_millis(1_000)),
            )
            .await,
        "set",
    );
    assert_eq!(cache.ttl(&site, "eternal").await, Ttl::Persistent);
    assert_eq!(
        cache.ttl(&site, "fleeting").await,
        Ttl::Remaining(Duration::from_millis(1_000)),
    );
    clock.advance(ms(1_000));
    assert_eq!(cache.ttl(&site, "fleeting").await, Ttl::Missing);
    assert_eq!(cache.ttl(&site, "ghost").await, Ttl::Missing);
}

#[tokio::test]
async fn incr_by_counts_from_zero_and_keeps_ttl() {
    let (cache, clock) = cache();
    let site = sid("s1");
    assert_eq!(ok(cache.incr_by(&site, "hits", 5).await, "incr_by"), 5);
    assert_eq!(ok(cache.incr_by(&site, "hits", 3).await, "incr_by"), 8);
    assert_eq!(got(cache.get(&site, "hits").await), Some(bytes("8")));
    ok(
        cache
            .set(
                &site,
                "counted",
                bytes("1"),
                Some(Duration::from_millis(1_000)),
            )
            .await,
        "set",
    );
    assert_eq!(ok(cache.incr_by(&site, "counted", 4).await, "incr_by"), 5);
    clock.advance(ms(1_000));
    assert_eq!(
        got(cache.get(&site, "counted").await),
        None,
        "incr_by keeps the existing TTL"
    );
    assert_eq!(ok(cache.incr_by(&site, "debts", -4).await, "incr_by"), -4);
    assert_eq!(got(cache.get(&site, "debts").await), Some(bytes("-4")));
}

#[tokio::test]
async fn incr_by_rejects_non_integers_and_overflow() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    ok(cache.set(&site, "text", bytes("hello"), None).await, "set");
    let error = match cache.incr_by(&site, "text", 1).await {
        Ok(value) => panic!("incr_by on text must fail, got {value}"),
        Err(error) => error,
    };
    assert!(matches!(error, CacheError::NotAnInteger));
    assert_eq!(got(cache.get(&site, "text").await), Some(bytes("hello")));
    ok(
        cache
            .set(&site, "max", bytes("9223372036854775807"), None)
            .await,
        "set",
    );
    let error = match cache.incr_by(&site, "max", 1).await {
        Ok(value) => panic!("incr_by past i64::MAX must fail, got {value}"),
        Err(error) => error,
    };
    assert!(matches!(error, CacheError::Overflow));
    ok(
        cache
            .set(&site, "min", bytes("-9223372036854775808"), None)
            .await,
        "set",
    );
    let error = match cache.incr_by(&site, "min", -1).await {
        Ok(value) => panic!("incr_by past i64::MIN must fail, got {value}"),
        Err(error) => error,
    };
    assert!(matches!(error, CacheError::Overflow));
}

#[tokio::test]
async fn delete_prefix_removes_matching_live_keys_only() {
    let (cache, clock) = cache();
    let site = sid("s1");
    for key in ["meta:1", "meta:2", "other"] {
        ok(cache.set(&site, key, bytes("v"), None).await, "set");
    }
    ok(
        cache
            .set(
                &site,
                "meta:dead",
                bytes("v"),
                Some(Duration::from_millis(50)),
            )
            .await,
        "set",
    );
    clock.advance(ms(100));
    let removed = cache.delete_prefix(&site, "meta:").await;
    assert_eq!(removed, 2, "expired entries are cleaned but not counted");
    assert_eq!(got(cache.get(&site, "other").await), Some(bytes("v")));
    assert_eq!(got(cache.get(&site, "meta:1").await), None);
    assert_eq!(got(cache.get(&site, "meta:dead").await), None);
    assert_eq!(cache.stats(&site).await.expired, 1);
}

#[tokio::test]
async fn clear_site_empties_one_site_only() {
    let (cache, _clock) = cache();
    let a = sid("a");
    let b = sid("b");
    ok(cache.set(&a, "x", bytes("1"), None).await, "set");
    ok(cache.set(&b, "x", bytes("2"), None).await, "set");
    assert_eq!(got(cache.get(&a, "x").await), Some(bytes("1")));
    cache.clear_site(&a).await;
    assert_eq!(got(cache.get(&a, "x").await), None);
    assert_eq!(got(cache.get(&b, "x").await), Some(bytes("2")));
    let stats = cache.stats(&a).await;
    assert_eq!(stats.entries, 0);
    assert_eq!(stats.bytes, 0);
    assert_eq!(stats.hits, 1, "statistics survive the clear");
}

#[tokio::test]
async fn stats_counts_hits_misses_and_accounted_bytes() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    let accounted = "k".len() as u64 + "value".len() as u64 + ENTRY_OVERHEAD_BYTES;
    ok(cache.set(&site, "k", bytes("value"), None).await, "set");
    assert_eq!(got(cache.get(&site, "k").await), Some(bytes("value")));
    assert_eq!(got(cache.get(&site, "gone").await), None);
    let stats = cache.stats(&site).await;
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.loads, 0);
    assert_eq!(stats.evictions, 0);
    assert_eq!(stats.expired, 0);
    assert_eq!(stats.entries, 1);
    assert_eq!(stats.bytes, accounted);
}
