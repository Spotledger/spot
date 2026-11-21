//! Rules shared by hashes, lists and sets: types, TTL, byte accounting, quotas, eviction.

mod common;

use std::sync::Arc;

use common::{all, bytes, cache, got, manual_clock, ms, sid};
use spot_cache::{
    CacheError, CacheStore, ELEMENT_OVERHEAD_BYTES, ENTRY_OVERHEAD_BYTES, InProcessCache,
    InProcessConfig, Ttl,
};

/// Fills one key of each structure kind.
async fn fill(cache: &InProcessCache, site: &spot_core::SiteId) {
    got(cache.hset(site, "h", "f", bytes("1")).await);
    got(cache.rpush(site, "l", &all(&["a", "b"])).await);
    got(cache.sadd(site, "s", &all(&["x", "y"])).await);
}

#[tokio::test]
async fn get_incr_by_and_set_nx_see_structures_as_wrong_type_or_occupied() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    fill(&cache, &site).await;
    for key in ["h", "l", "s"] {
        assert!(matches!(
            cache.get(&site, key).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.incr_by(&site, key, 1).await,
            Err(CacheError::WrongType)
        ));
        assert!(
            !got(cache.set_nx(&site, key, bytes("v"), None).await),
            "{key} is occupied"
        );
        let flight = cache
            .get_or_load(&site, key, None, Box::pin(async { Ok(bytes("loaded")) }))
            .await;
        assert!(matches!(flight, Err(CacheError::WrongType)), "{key}");
    }
}

#[tokio::test]
async fn set_overwrites_any_type_and_structures_overwrite_nothing() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    fill(&cache, &site).await;
    for key in ["h", "l", "s"] {
        got(cache.set(&site, key, bytes("plain"), None).await);
        assert_eq!(got(cache.get(&site, key).await), Some(bytes("plain")));
    }
    assert!(matches!(
        cache.hset(&site, "l", "f", bytes("1")).await,
        Err(CacheError::WrongType)
    ));
    assert_eq!(got(cache.get(&site, "l").await), Some(bytes("plain")));
    // A structure command on a key of another *structure* type fails the same way.
    got(cache.sadd(&site, "set", &all(&["m"])).await);
    assert!(matches!(
        cache.rpush(&site, "set", &all(&["m"])).await,
        Err(CacheError::WrongType)
    ));
    assert!(matches!(
        cache.hlen(&site, "set").await,
        Err(CacheError::WrongType)
    ));
}

#[tokio::test]
async fn expire_ttl_delete_exists_work_on_every_type() {
    let (cache, clock) = cache();
    let site = sid("s1");
    fill(&cache, &site).await;
    for key in ["h", "l", "s"] {
        assert!(cache.exists(&site, key).await);
        assert_eq!(cache.ttl(&site, key).await, Ttl::Persistent);
        assert!(cache.expire(&site, key, ms(1_000)).await);
        assert_eq!(cache.ttl(&site, key).await, Ttl::Remaining(ms(1_000)));
    }
    clock.advance(ms(400));
    assert_eq!(cache.ttl(&site, "l").await, Ttl::Remaining(ms(600)));
    clock.advance(ms(600));
    for key in ["h", "l", "s"] {
        assert!(!cache.exists(&site, key).await, "{key} expired");
        assert_eq!(cache.ttl(&site, key).await, Ttl::Missing);
    }
    assert_eq!(
        cache.stats(&site).await.bytes,
        0,
        "expired structures release their bytes"
    );
    fill(&cache, &site).await;
    for key in ["h", "l", "s"] {
        assert!(cache.delete(&site, key).await);
        assert!(!cache.delete(&site, key).await);
    }
}

#[tokio::test]
async fn expired_structure_reads_as_missing_and_the_key_can_change_type() {
    let (cache, clock) = cache();
    let site = sid("s1");
    got(cache.sadd(&site, "k", &all(&["a"])).await);
    cache.expire(&site, "k", ms(100)).await;
    clock.advance(ms(100));
    assert_eq!(got(cache.scard(&site, "k").await), 0);
    assert_eq!(
        got(cache.rpush(&site, "k", &all(&["x"])).await),
        1,
        "the expired set is gone, not a type clash"
    );
    assert_eq!(
        cache.ttl(&site, "k").await,
        Ttl::Persistent,
        "the new key has no TTL"
    );
}

#[tokio::test]
async fn adding_elements_keeps_the_ttl_of_the_key() {
    let (cache, clock) = cache();
    let site = sid("s1");
    got(cache.rpush(&site, "l", &all(&["a"])).await);
    cache.expire(&site, "l", ms(1_000)).await;
    clock.advance(ms(300));
    got(cache.rpush(&site, "l", &all(&["b"])).await);
    got(cache.lpop(&site, "l").await);
    assert_eq!(cache.ttl(&site, "l").await, Ttl::Remaining(ms(700)));
    got(cache.hset(&site, "h", "f", bytes("1")).await);
    cache.expire(&site, "h", ms(500)).await;
    got(cache.hincr_by(&site, "h", "n", 1).await);
    got(cache.hset(&site, "h", "f", bytes("2")).await);
    assert_eq!(cache.ttl(&site, "h").await, Ttl::Remaining(ms(500)));
}

#[tokio::test]
async fn delete_prefix_and_clear_site_remove_structures() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    let other = sid("s2");
    got(cache.hset(&site, "meta:h", "f", bytes("1")).await);
    got(cache.rpush(&site, "meta:l", &all(&["a"])).await);
    got(cache.sadd(&site, "keep:s", &all(&["a"])).await);
    got(cache.sadd(&other, "meta:s", &all(&["a"])).await);
    assert_eq!(cache.delete_prefix(&site, "meta:").await, 2);
    assert!(!cache.exists(&site, "meta:h").await && cache.exists(&site, "keep:s").await);
    cache.clear_site(&site).await;
    let stats = cache.stats(&site).await;
    assert_eq!((stats.entries, stats.bytes), (0, 0));
    assert_eq!(
        got(cache.scard(&other, "meta:s").await),
        1,
        "other sites are untouched"
    );
}

#[tokio::test]
async fn bytes_cover_key_fields_values_and_a_documented_overhead_per_element() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    let element = ELEMENT_OVERHEAD_BYTES;
    let entry = ENTRY_OVERHEAD_BYTES;

    got(cache.hset(&site, "hh", "ff", bytes("vvv")).await);
    assert_eq!(
        cache.stats(&site).await.bytes,
        entry + 2 + (2 + 3 + element)
    );
    got(cache.hset(&site, "hh", "ff", bytes("v")).await);
    assert_eq!(
        cache.stats(&site).await.bytes,
        entry + 2 + (2 + 1 + element),
        "a shorter value frees bytes"
    );
    got(cache.hincr_by(&site, "hh", "n", 12).await);
    assert_eq!(
        cache.stats(&site).await.bytes,
        entry + 2 + (2 + 1 + element) + (1 + 2 + element)
    );
    got(cache.hdel(&site, "hh", &["ff", "n"]).await);
    assert_eq!(cache.stats(&site).await.bytes, 0);

    got(cache.rpush(&site, "l", &all(&["abc", "de"])).await);
    assert_eq!(
        cache.stats(&site).await.bytes,
        entry + 1 + (3 + element) + (2 + element)
    );
    got(cache.ltrim(&site, "l", 1, 1).await);
    assert_eq!(cache.stats(&site).await.bytes, entry + 1 + (2 + element));
    got(cache.lpop(&site, "l").await);

    got(cache.sadd(&site, "s", &all(&["m", "m", "nn"])).await);
    assert_eq!(
        cache.stats(&site).await.bytes,
        entry + 1 + (1 + element) + (2 + element)
    );
    got(cache.srem(&site, "s", &all(&["m"])).await);
    assert_eq!(cache.stats(&site).await.bytes, entry + 1 + (2 + element));
    assert_eq!(cache.stats(&site).await.entries, 1);
}

/// A cache whose site quota is `quota` bytes.
fn small(quota: u64) -> (InProcessCache, Arc<spot_core::ManualClock>) {
    let clock = manual_clock();
    (
        InProcessCache::with_clock(InProcessConfig::new(1 << 20, quota), clock.clone()),
        clock,
    )
}

#[tokio::test]
async fn a_write_past_the_quota_fails_and_leaves_the_key_unchanged() {
    let element = 8 + ELEMENT_OVERHEAD_BYTES;
    // Room for the key and exactly two 8-byte elements.
    let quota = ENTRY_OVERHEAD_BYTES + 1 + 2 * element;
    let (cache, _clock) = small(quota);
    let site = sid("s1");
    assert_eq!(
        got(cache
            .rpush(&site, "l", &all(&["12345678", "abcdefgh"]))
            .await),
        2
    );
    let before = cache.stats(&site).await;
    assert!(matches!(
        cache.rpush(&site, "l", &all(&["x"])).await,
        Err(CacheError::QuotaExceeded)
    ));
    assert!(matches!(
        cache.lpush(&site, "l", &all(&["x"])).await,
        Err(CacheError::QuotaExceeded)
    ));
    assert_eq!(cache.stats(&site).await, before);
    assert_eq!(got(cache.llen(&site, "l").await), 2);
    // Removing then adding again fits.
    got(cache.lpop(&site, "l").await);
    assert_eq!(got(cache.rpush(&site, "l", &all(&["87654321"])).await), 2);
}

#[tokio::test]
async fn a_failed_first_write_does_not_create_the_key() {
    let (cache, _clock) = small(ENTRY_OVERHEAD_BYTES + 40);
    let site = sid("s1");
    assert!(matches!(
        cache.sadd(&site, "s", &all(&["a", "b", "c"])).await,
        Err(CacheError::QuotaExceeded)
    ));
    assert!(matches!(
        cache.hset(&site, "h", "f", bytes(&"v".repeat(100))).await,
        Err(CacheError::QuotaExceeded)
    ));
    assert!(!cache.exists(&site, "s").await && !cache.exists(&site, "h").await);
    assert_eq!(cache.stats(&site).await.bytes, 0);
}

#[tokio::test]
async fn an_existing_member_or_a_repeated_field_never_trips_the_quota() {
    let quota = ENTRY_OVERHEAD_BYTES + 1 + 4 + ELEMENT_OVERHEAD_BYTES;
    let (cache, _clock) = small(quota);
    let site = sid("s1");
    assert_eq!(got(cache.sadd(&site, "s", &all(&["abcd"])).await), 1);
    assert_eq!(
        got(cache.sadd(&site, "s", &all(&["abcd", "abcd"])).await),
        0
    );
    got(cache.hset(&site, "h", "f", bytes("vv")).await);
    assert!(!got(cache.hset(&site, "h", "f", bytes("vv")).await));
}

#[tokio::test]
async fn eviction_drops_a_whole_structure_as_one_entry() {
    let element = 4 + ELEMENT_OVERHEAD_BYTES;
    let structure = ENTRY_OVERHEAD_BYTES + 2 + 3 * element;
    // Room for one three-element list, or that list's bytes in other entries, not both.
    let (cache, clock) = small(structure + 30);
    let site = sid("s1");
    got(cache
        .rpush(&site, "l1", &all(&["aaaa", "bbbb", "cccc"]))
        .await);
    clock.advance(ms(1));
    got(cache.set(&site, "p", bytes("v"), None).await);
    let stats = cache.stats(&site).await;
    assert_eq!(stats.evictions, 1, "the list went as one entry");
    assert_eq!(stats.entries, 1);
    assert_eq!(
        got(cache.llen(&site, "l1").await),
        0,
        "no half-evicted list"
    );
    assert!(!cache.exists(&site, "l1").await);
}

#[tokio::test]
async fn growing_a_structure_evicts_other_entries_not_itself() {
    let (cache, clock) = small(400);
    let site = sid("s1");
    for i in 0..3 {
        got(cache
            .set(&site, &format!("old{i}"), bytes("0123456789"), None)
            .await);
        clock.advance(ms(1));
    }
    got(cache.rpush(&site, "l", &all(&["x"])).await);
    for _ in 0..4 {
        clock.advance(ms(1));
        got(cache.rpush(&site, "l", &all(&["y"])).await);
    }
    assert_eq!(
        got(cache.llen(&site, "l").await),
        5,
        "the growing list survives"
    );
    let stats = cache.stats(&site).await;
    assert!(stats.evictions >= 1 && stats.bytes <= 400, "{stats:?}");
}

#[tokio::test]
async fn concurrent_hincr_by_never_loses_an_increment() {
    let (cache, _clock) = cache();
    let cache = Arc::new(cache);
    let site = sid("s1");
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let cache = cache.clone();
        let site = site.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..200 {
                got(cache.hincr_by(&site, "h", "n", 1).await);
            }
        }));
    }
    for task in tasks {
        assert!(task.await.is_ok());
    }
    assert_eq!(got(cache.hget(&site, "h", "n").await), Some(bytes("1600")));
}
