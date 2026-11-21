//! Hash commands, one by one: the normal case, a missing key, a key of another type and the
//! boundary cases.

mod common;

use common::{bytes, cache, got, sid};
use spot_cache::{CacheError, CacheStore};

#[tokio::test]
async fn hset_reports_whether_the_field_is_new_and_hget_reads_it() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    assert!(got(cache.hset(&site, "h", "f", bytes("1")).await));
    assert!(!got(cache.hset(&site, "h", "f", bytes("2")).await));
    assert!(got(cache.hset(&site, "h", "g", bytes("3")).await));
    assert_eq!(got(cache.hget(&site, "h", "f").await), Some(bytes("2")));
    assert_eq!(got(cache.hget(&site, "h", "missing").await), None);
}

#[tokio::test]
async fn hget_hexists_hlen_hgetall_on_a_missing_key() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    assert_eq!(got(cache.hget(&site, "nope", "f").await), None);
    assert!(!got(cache.hexists(&site, "nope", "f").await));
    assert_eq!(got(cache.hlen(&site, "nope").await), 0);
    assert!(got(cache.hgetall(&site, "nope").await).is_empty());
    assert_eq!(got(cache.hdel(&site, "nope", &["f"]).await), 0);
    assert!(
        !cache.exists(&site, "nope").await,
        "reads never create the key"
    );
}

#[tokio::test]
async fn hdel_counts_existing_fields_once_and_deletes_the_key_with_the_last_one() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    got(cache.hset(&site, "h", "a", bytes("1")).await);
    got(cache.hset(&site, "h", "b", bytes("2")).await);
    got(cache.hset(&site, "h", "c", bytes("3")).await);
    assert_eq!(got(cache.hdel(&site, "h", &["a", "a", "zzz"]).await), 1);
    assert_eq!(got(cache.hlen(&site, "h").await), 2);
    assert_eq!(got(cache.hdel(&site, "h", &[]).await), 0);
    assert_eq!(got(cache.hdel(&site, "h", &["b", "c"]).await), 2);
    assert!(
        !cache.exists(&site, "h").await,
        "the last field takes the key with it"
    );
    assert_eq!(cache.stats(&site).await.bytes, 0);
}

#[tokio::test]
async fn hgetall_returns_every_pair() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    got(cache.hset(&site, "h", "b", bytes("2")).await);
    got(cache.hset(&site, "h", "a", bytes("1")).await);
    let mut all = got(cache.hgetall(&site, "h").await);
    all.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(
        all,
        vec![("a".to_owned(), bytes("1")), ("b".to_owned(), bytes("2"))]
    );
    assert!(got(cache.hexists(&site, "h", "a").await));
    assert!(!got(cache.hexists(&site, "h", "c").await));
}

#[tokio::test]
async fn hincr_by_creates_adds_and_rejects_bad_input() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    assert_eq!(
        got(cache.hincr_by(&site, "h", "n", 5).await),
        5,
        "missing hash and field count as 0"
    );
    assert_eq!(got(cache.hincr_by(&site, "h", "n", -8).await), -3);
    assert_eq!(got(cache.hget(&site, "h", "n").await), Some(bytes("-3")));
    got(cache.hset(&site, "h", "text", bytes("abc")).await);
    assert!(matches!(
        cache.hincr_by(&site, "h", "text", 1).await,
        Err(CacheError::NotAnInteger)
    ));
    assert_eq!(
        got(cache.hget(&site, "h", "text").await),
        Some(bytes("abc"))
    );
}

#[tokio::test]
async fn hincr_by_overflow_leaves_the_field_alone() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    got(cache
        .hset(&site, "h", "n", bytes(&i64::MAX.to_string()))
        .await);
    assert!(matches!(
        cache.hincr_by(&site, "h", "n", 1).await,
        Err(CacheError::Overflow)
    ));
    assert_eq!(
        got(cache.hget(&site, "h", "n").await),
        Some(bytes(&i64::MAX.to_string()))
    );
    got(cache
        .hset(&site, "h", "m", bytes(&i64::MIN.to_string()))
        .await);
    assert!(matches!(
        cache.hincr_by(&site, "h", "m", -1).await,
        Err(CacheError::Overflow)
    ));
    assert_eq!(got(cache.hincr_by(&site, "h", "m", 1).await), i64::MIN + 1);
}

#[tokio::test]
async fn hincr_by_failing_on_a_new_hash_does_not_create_it() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    assert!(matches!(
        cache.hincr_by(&site, "h", "n", i64::MAX).await,
        Ok(i64::MAX)
    ));
    assert!(matches!(
        cache.hincr_by(&site, "h", "n", 1).await,
        Err(CacheError::Overflow)
    ));
    assert_eq!(got(cache.hlen(&site, "h").await), 1);
    assert!(!cache.exists(&site, "other").await);
}

#[tokio::test]
async fn every_hash_command_rejects_other_types() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    got(cache.set(&site, "plain", bytes("v"), None).await);
    got(cache.rpush(&site, "list", &[bytes("x")]).await);
    for key in ["plain", "list"] {
        assert!(matches!(
            cache.hset(&site, key, "f", bytes("1")).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.hget(&site, key, "f").await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.hdel(&site, key, &["f"]).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.hgetall(&site, key).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.hexists(&site, key, "f").await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.hlen(&site, key).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.hincr_by(&site, key, "f", 1).await,
            Err(CacheError::WrongType)
        ));
    }
    assert_eq!(
        got(cache.get(&site, "plain").await),
        Some(bytes("v")),
        "the failed commands changed nothing"
    );
}
