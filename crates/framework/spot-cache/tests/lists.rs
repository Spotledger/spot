//! List commands, one by one, with Redis' index rules.

mod common;

use common::{all, bytes, cache, got, ordered, sid};
use spot_cache::{CacheError, CacheStore};

#[tokio::test]
async fn rpush_appends_in_order_and_returns_the_length() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    assert_eq!(got(cache.rpush(&site, "l", &all(&["a", "b"])).await), 2);
    assert_eq!(got(cache.rpush(&site, "l", &all(&["c"])).await), 3);
    assert_eq!(
        ordered(&got(cache.lrange(&site, "l", 0, -1).await)),
        ["a", "b", "c"]
    );
    assert_eq!(got(cache.llen(&site, "l").await), 3);
}

#[tokio::test]
async fn lpush_pushes_one_after_the_other_so_the_last_ends_first() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    assert_eq!(
        got(cache.lpush(&site, "l", &all(&["a", "b", "c"])).await),
        3
    );
    assert_eq!(
        ordered(&got(cache.lrange(&site, "l", 0, -1).await)),
        ["c", "b", "a"]
    );
}

#[tokio::test]
async fn pushing_nothing_changes_nothing() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    assert_eq!(got(cache.rpush(&site, "l", &[]).await), 0);
    assert!(
        !cache.exists(&site, "l").await,
        "no key is made for an empty push"
    );
    got(cache.rpush(&site, "l", &all(&["a"])).await);
    assert_eq!(got(cache.lpush(&site, "l", &[]).await), 1);
}

#[tokio::test]
async fn pops_take_from_each_end_and_the_last_one_deletes_the_key() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    got(cache.rpush(&site, "l", &all(&["a", "b", "c"])).await);
    assert_eq!(got(cache.lpop(&site, "l").await), Some(bytes("a")));
    assert_eq!(got(cache.rpop(&site, "l").await), Some(bytes("c")));
    assert_eq!(got(cache.lpop(&site, "l").await), Some(bytes("b")));
    assert!(!cache.exists(&site, "l").await);
    assert_eq!(got(cache.lpop(&site, "l").await), None);
    assert_eq!(got(cache.rpop(&site, "l").await), None);
    assert_eq!(cache.stats(&site).await.bytes, 0);
}

#[tokio::test]
async fn lrange_follows_redis_index_rules() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    got(cache
        .rpush(&site, "l", &all(&["a", "b", "c", "d", "e"]))
        .await);
    let range = |start: i64, stop: i64| {
        let cache = &cache;
        let site = &site;
        async move { ordered(&got(cache.lrange(site, "l", start, stop).await)) }
    };
    assert_eq!(range(0, 0).await, ["a"]);
    assert_eq!(range(1, 3).await, ["b", "c", "d"]);
    assert_eq!(range(-2, -1).await, ["d", "e"]);
    assert_eq!(
        range(-100, 1).await,
        ["a", "b"],
        "a start before the head clamps to 0"
    );
    assert_eq!(
        range(3, 100).await,
        ["d", "e"],
        "a stop past the tail clamps to the end"
    );
    assert_eq!(range(-100, 100).await, ["a", "b", "c", "d", "e"]);
    assert!(range(5, 10).await.is_empty(), "start past the tail");
    assert!(range(3, 1).await.is_empty(), "start after stop");
    assert!(range(0, -100).await.is_empty(), "stop before the head");
    assert!(range(-1, -2).await.is_empty());
    assert_eq!(
        range(i64::MIN, i64::MAX).await.len(),
        5,
        "extreme indices do not overflow"
    );
}

#[tokio::test]
async fn ltrim_keeps_the_window_and_an_empty_window_deletes_the_key() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    got(cache
        .rpush(&site, "l", &all(&["a", "b", "c", "d", "e"]))
        .await);
    got(cache.ltrim(&site, "l", 1, -2).await);
    assert_eq!(
        ordered(&got(cache.lrange(&site, "l", 0, -1).await)),
        ["b", "c", "d"]
    );
    got(cache.ltrim(&site, "l", 0, 100).await);
    assert_eq!(
        got(cache.llen(&site, "l").await),
        3,
        "a window wider than the list keeps it whole"
    );
    got(cache.ltrim(&site, "l", 5, 9).await);
    assert!(
        !cache.exists(&site, "l").await,
        "trimming down to empty deletes the key"
    );
    assert_eq!(cache.stats(&site).await.bytes, 0);
    got(cache.ltrim(&site, "l", 0, 1).await);
    assert!(
        !cache.exists(&site, "l").await,
        "trimming a missing list is a no-op"
    );
}

#[tokio::test]
async fn missing_list_reads() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    assert_eq!(got(cache.llen(&site, "nope").await), 0);
    assert!(got(cache.lrange(&site, "nope", 0, -1).await).is_empty());
}

#[tokio::test]
async fn every_list_command_rejects_other_types() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    got(cache.set(&site, "plain", bytes("v"), None).await);
    got(cache.hset(&site, "hash", "f", bytes("1")).await);
    for key in ["plain", "hash"] {
        assert!(matches!(
            cache.lpush(&site, key, &all(&["x"])).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.rpush(&site, key, &all(&["x"])).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.lpop(&site, key).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.rpop(&site, key).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.lrange(&site, key, 0, -1).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.llen(&site, key).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.ltrim(&site, key, 0, 1).await,
            Err(CacheError::WrongType)
        ));
    }
}
