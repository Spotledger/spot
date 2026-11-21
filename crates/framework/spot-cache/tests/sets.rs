//! Set commands, one by one.

mod common;

use common::{all, bytes, cache, got, sid, texts};
use spot_cache::{CacheError, CacheStore};

#[tokio::test]
async fn sadd_counts_only_new_members() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    assert_eq!(
        got(cache.sadd(&site, "s", &all(&["a", "b", "a"])).await),
        2,
        "a repeat within one call counts once"
    );
    assert_eq!(got(cache.sadd(&site, "s", &all(&["b", "c"])).await), 1);
    assert_eq!(got(cache.scard(&site, "s").await), 3);
    assert_eq!(
        texts(&got(cache.smembers(&site, "s").await)),
        ["a", "b", "c"]
    );
}

#[tokio::test]
async fn sismember_scard_smembers_on_a_missing_key() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    assert!(!got(cache.sismember(&site, "nope", b"a").await));
    assert_eq!(got(cache.scard(&site, "nope").await), 0);
    assert!(got(cache.smembers(&site, "nope").await).is_empty());
    assert_eq!(got(cache.srem(&site, "nope", &all(&["a"])).await), 0);
    assert_eq!(got(cache.sadd(&site, "nope", &[]).await), 0);
    assert!(
        !cache.exists(&site, "nope").await,
        "no key is made without members"
    );
}

#[tokio::test]
async fn srem_counts_members_removed_and_the_last_one_deletes_the_key() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    got(cache.sadd(&site, "s", &all(&["a", "b", "c"])).await);
    assert_eq!(
        got(cache.srem(&site, "s", &all(&["a", "a", "zzz"])).await),
        1
    );
    assert!(got(cache.sismember(&site, "s", b"b").await));
    assert!(!got(cache.sismember(&site, "s", b"a").await));
    assert_eq!(got(cache.srem(&site, "s", &all(&["b", "c"])).await), 2);
    assert!(!cache.exists(&site, "s").await);
    assert_eq!(cache.stats(&site).await.bytes, 0);
}

#[tokio::test]
async fn members_are_arbitrary_bytes() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    let odd = spot_cache::Bytes::from(vec![0u8, 255, 10, 13]);
    assert_eq!(
        got(cache.sadd(&site, "s", &[odd.clone(), bytes("")]).await),
        2
    );
    assert!(got(cache.sismember(&site, "s", &odd).await));
    assert!(
        got(cache.sismember(&site, "s", b"").await),
        "the empty member is a member"
    );
}

#[tokio::test]
async fn every_set_command_rejects_other_types() {
    let (cache, _clock) = cache();
    let site = sid("s1");
    got(cache.set(&site, "plain", bytes("v"), None).await);
    got(cache.rpush(&site, "list", &all(&["x"])).await);
    for key in ["plain", "list"] {
        assert!(matches!(
            cache.sadd(&site, key, &all(&["x"])).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.srem(&site, key, &all(&["x"])).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.smembers(&site, key).await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.sismember(&site, key, b"x").await,
            Err(CacheError::WrongType)
        ));
        assert!(matches!(
            cache.scard(&site, key).await,
            Err(CacheError::WrongType)
        ));
    }
}
