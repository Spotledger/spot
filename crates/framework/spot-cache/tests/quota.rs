//! Quota accounting: oversize refusal, per-site eviction isolation, least-recently-used
//! victim choice, and global pressure evicting across sites.

mod common;

use common::{got, manual_clock, ms, sid};
use spot_cache::{
    BoxLoader, Bytes, CacheError, CacheStore, ENTRY_OVERHEAD_BYTES, InProcessCache, InProcessConfig,
};

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{what} failed: {error:?}"),
    }
}

fn bytes(text: &str) -> Bytes {
    Bytes::from(text.as_bytes().to_vec())
}

/// Entry cost of a `key.len()` key holding a 16-byte value.
fn cost(key: &str) -> u64 {
    key.len() as u64 + 16 + ENTRY_OVERHEAD_BYTES
}

#[tokio::test]
async fn oversize_value_is_refused_and_not_stored() {
    let cache = InProcessCache::new(InProcessConfig::new(1 << 20, 256));
    let site = sid("s1");
    let error = match cache
        .set(&site, "k", Bytes::from(vec![b'x'; 256]), None)
        .await
    {
        Ok(_) => panic!("a value over the site quota must be refused"),
        Err(error) => error,
    };
    assert!(matches!(error, CacheError::QuotaExceeded));
    assert_eq!(got(cache.get(&site, "k").await), None);
    let stats = cache.stats(&site).await;
    assert_eq!(stats.entries, 0);
    assert_eq!(stats.bytes, 0);
}

#[tokio::test]
async fn a_noisy_site_never_evicts_another_site() {
    let mut per_site = std::collections::HashMap::new();
    per_site.insert(sid("noisy"), 256);
    per_site.insert(sid("quiet"), 4096);
    let cache = InProcessCache::new(InProcessConfig {
        max_bytes: 8192,
        per_site_max_bytes: per_site,
        default_per_site_max_bytes: 4096,
    });
    let noisy = sid("noisy");
    let quiet = sid("quiet");
    for i in 0..3 {
        ok(
            cache
                .set(&quiet, &format!("q{i}"), bytes("0123456789abcdef"), None)
                .await,
            "set",
        );
    }
    let before = cache.stats(&quiet).await;
    assert_eq!(before.evictions, 0);
    for i in 0..50 {
        ok(
            cache
                .set(&noisy, &format!("n{i}"), Bytes::from(vec![b'n'; 160]), None)
                .await,
            "set",
        );
    }
    let noisy_stats = cache.stats(&noisy).await;
    assert!(
        noisy_stats.evictions > 0,
        "the noisy site must churn within its own quota"
    );
    let after = cache.stats(&quiet).await;
    assert_eq!(after.evictions, 0);
    assert_eq!(after.entries, before.entries);
    assert_eq!(after.bytes, before.bytes);
    for i in 0..3 {
        assert_eq!(
            got(cache.get(&quiet, &format!("q{i}")).await),
            Some(bytes("0123456789abcdef"))
        );
    }
}

#[tokio::test]
async fn eviction_picks_the_least_recently_used_entry() {
    let clock = manual_clock();
    // Three entries fit exactly; with at most EVICTION_SAMPLE live entries the sampled LRU
    // covers the whole pool, so eviction is exact LRU here.
    let quota = cost("k0") * 3;
    let cache = InProcessCache::with_clock(InProcessConfig::new(1 << 20, quota), clock.clone());
    let site = sid("s1");
    for key in ["k0", "k1", "k2"] {
        ok(
            cache.set(&site, key, bytes("0123456789abcdef"), None).await,
            "set",
        );
        clock.advance(ms(1));
    }
    assert_eq!(
        got(cache.get(&site, "k0").await),
        Some(bytes("0123456789abcdef"))
    );
    clock.advance(ms(1));
    ok(
        cache
            .set(&site, "k3", bytes("0123456789abcdef"), None)
            .await,
        "set",
    );
    // k1 was never touched since its insert: it is the LRU victim.
    assert_eq!(
        got(cache.get(&site, "k0").await),
        Some(bytes("0123456789abcdef"))
    );
    assert_eq!(got(cache.get(&site, "k1").await), None);
    assert_eq!(
        got(cache.get(&site, "k2").await),
        Some(bytes("0123456789abcdef"))
    );
    assert_eq!(
        got(cache.get(&site, "k3").await),
        Some(bytes("0123456789abcdef"))
    );
    let stats = cache.stats(&site).await;
    assert_eq!(stats.evictions, 1);
    assert_eq!(stats.bytes, cost("k0") * 3);
}

#[tokio::test]
async fn global_pressure_evicts_across_sites() {
    let clock = manual_clock();
    // Per-site quotas are huge; only the 300-byte global cap binds.
    let cache = InProcessCache::with_clock(InProcessConfig::new(300, 4096), clock.clone());
    let a = sid("a");
    let b = sid("b");
    for i in 0..3 {
        ok(
            cache
                .set(&a, &format!("a{i}"), bytes("0123456789abcdef"), None)
                .await,
            "set",
        );
        clock.advance(ms(1));
    }
    ok(
        cache.set(&b, "b0", bytes("0123456789abcdef"), None).await,
        "set",
    );
    // Global eviction must cross sites. Fairness decides the victim *site*: `a` held three
    // entries (three quarters of its own huge quota), `b` one, so the most saturated site
    // — `a`, nearest its own quota — donates, and its least recently used entry, `a0`, is
    // the victim.
    assert_eq!(got(cache.get(&a, "a0").await), None);
    assert_eq!(
        got(cache.get(&a, "a1").await),
        Some(bytes("0123456789abcdef"))
    );
    assert_eq!(
        got(cache.get(&b, "b0").await),
        Some(bytes("0123456789abcdef"))
    );
    assert_eq!(cache.stats(&a).await.evictions, 1);
    // Every further write keeps the cache within the global cap. Both sites then hold the
    // same share of their quotas; the exact tie is won by the writing site, so the churn
    // lands on `b` and the quieter `a` is not evicted from again. The clock advances with
    // every write, so within `b` the true LRU is always the previous key and the freshest
    // key survives the churn.
    for i in 0..10 {
        ok(
            cache
                .set(&b, &format!("b{i}"), bytes("0123456789abcdef"), None)
                .await,
            "set",
        );
        clock.advance(ms(1));
        let total = cache.stats(&a).await.bytes + cache.stats(&b).await.bytes;
        assert!(total <= 300, "global total {total} over the 300 byte cap");
    }
    assert_eq!(
        cache.stats(&a).await.evictions,
        1,
        "a site at a lower share of its quota must never donate while a more saturated site has entries"
    );
    assert!(cache.stats(&b).await.evictions >= 1);
    assert_eq!(
        got(cache.get(&b, "b9").await),
        Some(bytes("0123456789abcdef")),
        "the newest key of the writing site survives its own churn"
    );
}

#[tokio::test]
async fn loader_result_over_quota_is_refused_for_everyone() {
    let cache = InProcessCache::new(InProcessConfig::new(1 << 20, 256));
    let site = sid("s1");
    let loader: BoxLoader = Box::pin(async { Ok(Bytes::from(vec![b'x'; 512])) });
    let error = match cache.get_or_load(&site, "k", None, loader).await {
        Ok(loaded) => panic!(
            "an oversize load must be refused, got {} bytes",
            loaded.len()
        ),
        Err(error) => error,
    };
    assert!(matches!(error, CacheError::QuotaExceeded));
    assert_eq!(got(cache.get(&site, "k").await), None);
    let stats = cache.stats(&site).await;
    assert_eq!(stats.loads, 0);
    assert_eq!(stats.entries, 0);
}

#[tokio::test]
async fn a_flooded_site_leaves_a_quiet_sites_working_set_intact() {
    // Reviewer regression: with these settings the site quotas (2 × 800) oversubscribe the
    // 1000-byte global cap, and every site sits far over a fair share of the cap. A quiet
    // site holds three 100-byte values; then a noisy site floods 40-byte values with fresh
    // keys. Global eviction takes from the most saturated site — `noisy`, the one nearest
    // its own quota — so the quiet working set must stay untouched the whole time.
    let cache = InProcessCache::new(InProcessConfig::new(1000, 800));
    let quiet = sid("quiet");
    let noisy = sid("noisy");
    for i in 0..3 {
        ok(
            cache
                .set(&quiet, &format!("q{i}"), Bytes::from(vec![b'v'; 100]), None)
                .await,
            "set",
        );
    }
    let quiet_bytes = 3 * (2 + 100 + ENTRY_OVERHEAD_BYTES);
    assert_eq!(cache.stats(&quiet).await.bytes, quiet_bytes);
    for i in 0..50 {
        ok(
            cache
                .set(&noisy, &format!("n{i}"), Bytes::from(vec![b'n'; 40]), None)
                .await,
            "set",
        );
        let quiet_stats = cache.stats(&quiet).await;
        let noisy_stats = cache.stats(&noisy).await;
        assert_eq!(
            quiet_stats.bytes, quiet_bytes,
            "quiet bytes after write {i}"
        );
        assert_eq!(quiet_stats.entries, 3, "quiet entries after write {i}");
        assert_eq!(quiet_stats.evictions, 0, "quiet evictions after write {i}");
        assert!(
            quiet_stats.bytes + noisy_stats.bytes <= 1000,
            "global cap broken after write {i}"
        );
    }
    assert!(
        cache.stats(&noisy).await.evictions > 0,
        "the flood must churn the noisy site, not the quiet one"
    );
    for i in 0..3 {
        assert_eq!(
            got(cache.get(&quiet, &format!("q{i}")).await),
            Some(Bytes::from(vec![b'v'; 100])),
            "quiet key q{i} must survive the noisy site's flood"
        );
    }
}
