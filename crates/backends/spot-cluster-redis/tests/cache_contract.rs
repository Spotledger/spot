//! The cache contract, against a real Redis/Valkey server.
//!
//! `spot-cache` has no reusable backend-independent suite (its tests are written against
//! `InProcessCache` directly), so this file runs the same command-by-command semantics the
//! in-process suite pins — missing keys, structures, `WrongType`, `incr_by` errors, `delete_prefix`
//! and `clear_site` — against the Redis backend. Eviction and exact byte accounting are the
//! in-process backend's own policy (the cache is lossy by contract), so this suite pins the
//! semantics and the accounting it can, not the eviction algorithm.
//!
//! Every test skips without `SPOT_TEST_REDIS_URL`.

#![cfg(feature = "redis")]
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use spot_cache::{
    Bytes, CacheError, CacheStore, ELEMENT_OVERHEAD_BYTES, ENTRY_OVERHEAD_BYTES, Ttl,
};
use spot_cluster_redis::{RedisCacheConfig, RedisCacheStore};
use spot_core::SiteId;

fn bytes(text: &str) -> Bytes {
    Bytes::from(text.as_bytes().to_vec())
}

fn all(texts: &[&str]) -> Vec<Bytes> {
    texts.iter().map(|text| bytes(text)).collect()
}

fn got<T>(reply: Result<T, CacheError>) -> T {
    match reply {
        Ok(value) => value,
        Err(error) => panic!("cache command failed: {error}"),
    }
}

/// A fresh store and a site, or `None` when no server is configured.
async fn cache() -> Option<(RedisCacheStore, SiteId)> {
    let connection = common::connect().await?;
    Some((
        RedisCacheStore::with_default(connection),
        common::sid("acme.example"),
    ))
}

#[tokio::test]
async fn set_get_delete_roundtrip() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    got(cache.set(&site, "alpha", bytes("hello"), None).await);
    assert_eq!(got(cache.get(&site, "alpha").await), Some(bytes("hello")));
    assert!(cache.exists(&site, "alpha").await);
    assert!(cache.delete(&site, "alpha").await);
    assert_eq!(got(cache.get(&site, "alpha").await), None);
    assert!(!cache.delete(&site, "alpha").await);
}

#[tokio::test]
async fn missing_key_answers_neutral() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    assert_eq!(got(cache.get(&site, "nope").await), None);
    assert!(!cache.exists(&site, "nope").await);
    assert!(!cache.delete(&site, "nope").await);
    assert_eq!(cache.ttl(&site, "nope").await, Ttl::Missing);
}

#[tokio::test]
async fn set_nx_sets_only_when_absent() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    assert!(got(cache.set_nx(&site, "k", bytes("first"), None).await));
    assert!(!got(cache.set_nx(&site, "k", bytes("second"), None).await));
    assert_eq!(got(cache.get(&site, "k").await), Some(bytes("first")));
    // A structure counts as occupied too.
    got(cache.sadd(&site, "s", &all(&["m"])).await);
    assert!(!got(cache.set_nx(&site, "s", bytes("v"), None).await));
}

#[tokio::test]
async fn set_overwrites_any_type() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    got(cache.hset(&site, "h", "f", bytes("1")).await);
    got(cache.set(&site, "h", bytes("plain"), None).await);
    assert_eq!(got(cache.get(&site, "h").await), Some(bytes("plain")));
    // And a structure command on a plain value is a wrong type.
    assert!(matches!(
        cache.hset(&site, "h", "f", bytes("1")).await,
        Err(CacheError::WrongType)
    ));
}

#[tokio::test]
async fn ttl_expire_and_expiry() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    got(cache.set(&site, "eternal", bytes("v"), None).await);
    assert_eq!(cache.ttl(&site, "eternal").await, Ttl::Persistent);
    got(cache
        .set(
            &site,
            "fleeting",
            bytes("v"),
            Some(Duration::from_millis(80)),
        )
        .await);
    assert!(matches!(
        cache.ttl(&site, "fleeting").await,
        Ttl::Remaining(_)
    ));
    assert!(
        cache
            .expire(&site, "eternal", Duration::from_millis(80))
            .await
    );
    assert!(
        !cache
            .expire(&site, "ghost", Duration::from_millis(80))
            .await
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(got(cache.get(&site, "fleeting").await), None);
    assert_eq!(got(cache.get(&site, "eternal").await), None);
    assert_eq!(cache.ttl(&site, "fleeting").await, Ttl::Missing);
}

#[tokio::test]
async fn incr_by_counts_from_zero_and_reports_errors() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    assert_eq!(got(cache.incr_by(&site, "hits", 5).await), 5);
    assert_eq!(got(cache.incr_by(&site, "hits", 3).await), 8);
    assert_eq!(got(cache.get(&site, "hits").await), Some(bytes("8")));
    assert_eq!(got(cache.incr_by(&site, "debts", -4).await), -4);
    got(cache.set(&site, "text", bytes("hello"), None).await);
    assert!(matches!(
        cache.incr_by(&site, "text", 1).await,
        Err(CacheError::NotAnInteger)
    ));
    got(cache
        .set(&site, "max", bytes("9223372036854775807"), None)
        .await);
    assert!(matches!(
        cache.incr_by(&site, "max", 1).await,
        Err(CacheError::Overflow)
    ));
    got(cache.hset(&site, "h", "f", bytes("1")).await);
    assert!(matches!(
        cache.incr_by(&site, "h", 1).await,
        Err(CacheError::WrongType)
    ));
}

#[tokio::test]
async fn hashes_behave() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    assert!(got(cache.hset(&site, "h", "f", bytes("1")).await));
    assert!(!got(cache.hset(&site, "h", "f", bytes("2")).await));
    got(cache.hset(&site, "h", "g", bytes("3")).await);
    assert_eq!(got(cache.hget(&site, "h", "f").await), Some(bytes("2")));
    assert_eq!(got(cache.hget(&site, "h", "missing").await), None);
    assert!(got(cache.hexists(&site, "h", "f").await));
    assert_eq!(got(cache.hlen(&site, "h").await), 2);
    assert_eq!(got(cache.hgetall(&site, "h").await).len(), 2);
    assert_eq!(got(cache.hincr_by(&site, "h", "n", 12).await), 12);
    assert_eq!(got(cache.hdel(&site, "h", &["f", "g"]).await), 2);
    assert_eq!(got(cache.hlen(&site, "h").await), 1);
    assert_eq!(got(cache.hdel(&site, "h", &["n"]).await), 1);
    assert!(!cache.exists(&site, "h").await, "the empty hash is gone");
}

#[tokio::test]
async fn hash_on_another_shape_is_a_wrong_type() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    got(cache.set(&site, "plain", bytes("v"), None).await);
    got(cache.sadd(&site, "set", &all(&["m"])).await);
    for key in ["plain", "set"] {
        assert!(matches!(
            cache.hget(&site, key, "f").await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.hset(&site, key, "f", bytes("1")).await,
            Err(CacheError::WrongType)
        ));
    }
    assert_eq!(got(cache.get(&site, "plain").await), Some(bytes("v")));
    assert!(matches!(
        cache.get(&site, "set").await,
        Err(CacheError::WrongType)
    ));
}

#[tokio::test]
async fn lists_behave() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    assert_eq!(
        got(cache.rpush(&site, "l", &all(&["a", "b", "c"])).await),
        3
    );
    assert_eq!(got(cache.lpush(&site, "l", &all(&["z"])).await), 4);
    assert_eq!(got(cache.llen(&site, "l").await), 4);
    assert_eq!(
        got(cache.lrange(&site, "l", 0, -1).await),
        all(&["z", "a", "b", "c"])
    );
    assert_eq!(got(cache.lpop(&site, "l").await), Some(bytes("z")));
    assert_eq!(got(cache.rpop(&site, "l").await), Some(bytes("c")));
    got(cache.ltrim(&site, "l", 0, 0).await);
    assert_eq!(got(cache.lrange(&site, "l", 0, -1).await), all(&["a"]));
    got(cache.set(&site, "plain", bytes("v"), None).await);
    assert!(matches!(
        cache.rpush(&site, "plain", &all(&["x"])).await,
        Err(CacheError::WrongType)
    ));
}

#[tokio::test]
async fn sets_behave() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    assert_eq!(
        got(cache.sadd(&site, "s", &all(&["m", "m", "nn"])).await),
        2
    );
    assert_eq!(got(cache.scard(&site, "s").await), 2);
    assert!(got(cache.sismember(&site, "s", b"m").await));
    assert_eq!(got(cache.smembers(&site, "s").await).len(), 2);
    assert_eq!(got(cache.srem(&site, "s", &all(&["m"])).await), 1);
    assert_eq!(got(cache.scard(&site, "s").await), 1);
}

#[tokio::test]
async fn delete_prefix_and_clear_site_are_site_scoped() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    let other = common::sid("other.example");
    got(cache.set(&site, "meta:1", bytes("v"), None).await);
    got(cache.set(&site, "meta:2", bytes("v"), None).await);
    got(cache.set(&site, "other", bytes("v"), None).await);
    got(cache.set(&other, "meta:1", bytes("v"), None).await);
    assert_eq!(cache.delete_prefix(&site, "meta:").await, 2);
    assert_eq!(got(cache.get(&site, "meta:1").await), None);
    assert_eq!(got(cache.get(&site, "other").await), Some(bytes("v")));
    assert_eq!(
        got(cache.get(&other, "meta:1").await),
        Some(bytes("v")),
        "another site is untouched"
    );
    let before = cache.generation(&site).await.expect("generation");
    cache.clear_site(&site).await;
    assert_eq!(got(cache.get(&site, "other").await), None);
    assert!(cache.generation(&site).await.expect("generation") > before);
}

#[tokio::test]
async fn stats_count_hits_misses_and_bytes() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    let accounted = u64::try_from("k".len() + "value".len()).expect("len") + ENTRY_OVERHEAD_BYTES;
    got(cache.set(&site, "k", bytes("value"), None).await);
    assert_eq!(got(cache.get(&site, "k").await), Some(bytes("value")));
    assert_eq!(got(cache.get(&site, "gone").await), None);
    let stats = cache.stats(&site).await;
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.loads, 0);
    assert_eq!(stats.entries, 1);
    assert_eq!(stats.bytes, accounted);
}

#[tokio::test]
async fn get_or_load_stores_and_serves() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    let loaded = got(cache
        .get_or_load(
            &site,
            "flight",
            None,
            Box::pin(async { Ok(bytes("loaded")) }),
        )
        .await);
    assert_eq!(loaded, bytes("loaded"));
    assert_eq!(got(cache.get(&site, "flight").await), Some(bytes("loaded")));
    // A loader that fails is never cached.
    let failed = cache
        .get_or_load(&site, "bad", None, Box::pin(async { Err("boom".into()) }))
        .await;
    assert!(matches!(failed, Err(CacheError::Loader(_))));
    assert_eq!(got(cache.get(&site, "bad").await), None);
}

#[tokio::test]
async fn a_value_past_the_quota_is_refused() {
    let Some(connection) = common::connect().await else {
        return common::skip();
    };
    let site = common::sid("acme.example");
    let cache = RedisCacheStore::new(
        connection,
        RedisCacheConfig::with_quota(ENTRY_OVERHEAD_BYTES + 8),
    );
    got(cache.set(&site, "k", bytes("small"), None).await);
    assert!(matches!(
        cache.set(&site, "big", bytes(&"x".repeat(100)), None).await,
        Err(CacheError::QuotaExceeded)
    ));
    assert_eq!(got(cache.get(&site, "big").await), None);
    assert_eq!(got(cache.get(&site, "k").await), Some(bytes("small")));
}

#[tokio::test]
async fn structures_account_bytes_too() {
    let Some((cache, site)) = cache().await else {
        return common::skip();
    };
    let element = ELEMENT_OVERHEAD_BYTES;
    got(cache.hset(&site, "hh", "ff", bytes("vvv")).await);
    let stats = cache.stats(&site).await;
    assert_eq!(stats.bytes, ENTRY_OVERHEAD_BYTES + 2 + (2 + 3 + element));
    got(cache.hdel(&site, "hh", &["ff"]).await);
    assert_eq!(cache.stats(&site).await.bytes, 0);
    assert_eq!(cache.stats(&site).await.entries, 0);
}
