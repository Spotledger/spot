//! `CacheStore::count_prefix`: the live keys under a prefix, per site, read-only.
//!
//! The in-process store and the recording wrapper are covered here; the Redis store runs the same
//! cases in `spot-cluster-redis/tests/cache_contract.rs` (gated on `SPOT_TEST_REDIS_URL`).

use std::sync::Arc;

mod common;

use common::{bytes, got, manual_clock, ms, sid};
use spot_cache::store::RecordingCache;
use spot_cache::{CacheStore, ENTRY_OVERHEAD_BYTES, InProcessCache, InProcessConfig};
use spot_core::Clock;

/// A roomy cache on a manual clock.
fn roomy() -> (InProcessCache, Arc<spot_core::ManualClock>) {
    common::cache()
}

#[tokio::test]
async fn it_counts_only_the_keys_under_the_prefix() {
    let (cache, _clock) = roomy();
    let site = sid("s1");
    for key in ["step:a", "step:b", "step:c", "other:x", "stepper"] {
        got(cache.set(&site, key, bytes("v"), None).await);
    }
    assert_eq!(cache.count_prefix(&site, "step:").await, 3);
    assert_eq!(cache.count_prefix(&site, "other:").await, 1);
    // A prefix is a prefix, not a word: "step" also matches "stepper".
    assert_eq!(cache.count_prefix(&site, "step").await, 4);
    // The count is what `delete_prefix` removes.
    assert_eq!(cache.delete_prefix(&site, "step:").await, 3);
    assert_eq!(cache.count_prefix(&site, "step:").await, 0);
    assert_eq!(cache.count_prefix(&site, "other:").await, 1);
}

#[tokio::test]
async fn a_prefix_with_no_keys_and_an_unknown_site_are_zero() {
    let (cache, _clock) = roomy();
    let site = sid("s1");
    got(cache.set(&site, "k", bytes("v"), None).await);
    assert_eq!(cache.count_prefix(&site, "nothing:").await, 0);
    assert_eq!(cache.count_prefix(&sid("never-seen"), "k").await, 0);
}

#[tokio::test]
async fn the_empty_prefix_is_every_live_key_of_the_site_as_delete_prefix_removes() {
    let (cache, _clock) = roomy();
    let (one, two) = (sid("s1"), sid("s2"));
    for key in ["a", "b", "c"] {
        got(cache.set(&one, key, bytes("v"), None).await);
    }
    got(cache.set(&two, "a", bytes("v"), None).await);
    assert_eq!(cache.count_prefix(&one, "").await, 3);
    assert_eq!(cache.count_prefix(&two, "").await, 1);
    assert_eq!(cache.delete_prefix(&one, "").await, 3);
}

#[tokio::test]
async fn sites_are_isolated() {
    let (cache, _clock) = roomy();
    let (one, two) = (sid("s1"), sid("s2"));
    got(cache.set(&one, "step:a", bytes("v"), None).await);
    got(cache.set(&one, "step:b", bytes("v"), None).await);
    got(cache.set(&two, "step:a", bytes("v"), None).await);
    assert_eq!(cache.count_prefix(&one, "step:").await, 2);
    assert_eq!(cache.count_prefix(&two, "step:").await, 1);
}

#[tokio::test]
async fn an_expired_key_is_not_counted_even_before_it_is_swept() {
    let (cache, clock) = roomy();
    let site = sid("s1");
    got(cache.set(&site, "step:eternal", bytes("v"), None).await);
    got(cache
        .set(&site, "step:fleeting", bytes("v"), Some(ms(1_000)))
        .await);
    assert_eq!(cache.count_prefix(&site, "step:").await, 2);
    clock.advance(ms(1_000));
    assert_eq!(cache.count_prefix(&site, "step:").await, 1);
    // Reading did not remove the expired entry: the site still holds both until a write or a
    // read of that key cleans it up.
    assert_eq!(cache.stats(&site).await.entries, 2);
}

#[tokio::test]
async fn every_value_type_counts_once() {
    let (cache, _clock) = roomy();
    let site = sid("s1");
    got(cache.set(&site, "step:string", bytes("v"), None).await);
    got(cache.hset(&site, "step:hash", "f", bytes("v")).await);
    got(cache
        .rpush(&site, "step:list", &[bytes("a"), bytes("b")])
        .await);
    got(cache.sadd(&site, "step:set", &[bytes("m")]).await);
    assert_eq!(cache.count_prefix(&site, "step:").await, 4);
}

#[tokio::test]
async fn counting_changes_no_statistics() {
    let (cache, _clock) = roomy();
    let site = sid("s1");
    got(cache.set(&site, "step:a", bytes("v"), None).await);
    got(cache.set(&site, "step:b", bytes("v"), None).await);
    let _ = got(cache.get(&site, "step:a").await);
    let _ = got(cache.get(&site, "missing").await);
    let before = cache.stats(&site).await;
    for _ in 0..10 {
        assert_eq!(cache.count_prefix(&site, "step:").await, 2);
        assert_eq!(cache.count_prefix(&site, "missing:").await, 0);
    }
    assert_eq!(
        cache.stats(&site).await,
        before,
        "hits, misses and bytes did not move"
    );
}

#[tokio::test]
async fn counting_does_not_refresh_recency_or_evict() {
    const COST: u64 = 5 + 1 + ENTRY_OVERHEAD_BYTES;
    let clock = manual_clock();
    // Room for exactly three entries of this site; a pool this small evicts in strict LRU order.
    let cache = InProcessCache::with_clock(
        InProcessConfig::new(100 * COST, 3 * COST),
        clock.clone() as Arc<dyn Clock>,
    );
    let site = sid("s1");
    for key in ["k0001", "k0002", "k0003"] {
        got(cache.set(&site, key, bytes("v"), None).await);
        clock.advance(ms(10));
    }
    // Counting the oldest key many times must not make it young.
    for _ in 0..20 {
        assert_eq!(cache.count_prefix(&site, "k0001").await, 1);
    }
    assert_eq!(
        cache.stats(&site).await.evictions,
        0,
        "counting evicts nothing"
    );
    got(cache.set(&site, "k0004", bytes("v"), None).await);
    assert!(
        !cache.exists(&site, "k0001").await,
        "the oldest entry is the one evicted"
    );
    assert!(cache.exists(&site, "k0002").await);
}

#[tokio::test]
async fn the_recording_wrapper_counts_through_to_the_store() {
    let (inner, _clock) = roomy();
    let cache = RecordingCache::new(inner);
    let site = sid("s1");
    got(cache.set(&site, "step:a", bytes("v"), None).await);
    got(cache.set(&site, "other", bytes("v"), None).await);
    assert_eq!(cache.count_prefix(&site, "step:").await, 1);
    assert_eq!(cache.count_prefix(&site, "").await, 2);
}
