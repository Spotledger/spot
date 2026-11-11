//! Eviction-quality regression tests.
//!
//! The sampled evictor (see the `in_process` module docs) must satisfy two properties beyond
//! the bounded-work guarantees of `eviction_cost.rs`:
//!
//! 1. **Hot keys survive churn.** The victim is the least recently used of
//!    `EVICTION_SAMPLE` entries drawn uniformly from the whole pool. A frequently touched
//!    working set is young, and a young entry is picked only when every co-sampled entry is
//!    younger still, so hot keys survive hundreds of evictions of cold entries. A windowed
//!    walk over arbitrary `HashMap` order (the reviewed behaviour) evicted hot keys instead.
//! 2. **Small pools keep exact LRU.** A pool of at most `EVICTION_SAMPLE` entries is
//!    sampled in full, so eviction order equals strict LRU.
//!
//! Plus the sweep-reachability regression (the reviewed behaviour): the background sweep's
//! per-site cursor must wrap at the site's key count, so a site that stops being written
//! still gets its expired entries physically deleted (memory does not idle at a tombstone).
//! Every test is deterministic: the sampler is seeded and the clock is manual, so a passing
//! run stays passing.
use std::sync::Arc;
use std::time::Duration;

mod common;

use common::{got, manual_clock, ms, sid};
use spot_cache::{
    Bytes, CacheStore, ENTRY_OVERHEAD_BYTES, InProcessCache, InProcessConfig, SWEEP_INTERVAL_WRITES,
};
use spot_core::SiteId;

/// Every entry below has a 5-byte key and a 16-byte value, so all costs are uniform.
const COST: u64 = 5 + 16 + ENTRY_OVERHEAD_BYTES;

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{what} failed: {error:?}"),
    }
}

async fn put_key(cache: &InProcessCache, site: &SiteId, key: &str, ttl: Option<Duration>) {
    ok(
        cache.set(site, key, Bytes::from(vec![b'x'; 16]), ttl).await,
        "set {key}",
    );
}

/// The 20 hot keys are seeded once at a clock jump, then kept warm by the workload: each
/// churn write re-reads one hot key again (round-robin), which is what a hot working set
/// means. A hot key is the stalest of an `EVICTION_SAMPLE` sample only when nothing colder
/// is co-sampled with it; with ~48 keys resident that are younger than any hot key's last
/// refresh that is astronomically unlikely, and no churn round gets there. The reviewed
/// windowed walk lost hot keys within the first few dozen evictions.
#[tokio::test]
async fn sampled_eviction_keeps_a_hot_working_set_under_churn() {
    const POOL: u64 = 2_400;
    const HOT: u64 = 20;
    const CHURN: u64 = 4_800;
    let clock = manual_clock();
    let cache = InProcessCache::with_clock(
        InProcessConfig::new(4 * POOL * COST, POOL * COST),
        clock.clone() as Arc<dyn spot_core::Clock>,
    );
    let site = sid("s");

    for i in 0..POOL {
        put_key(&cache, &site, &format!("k{i:04}"), None).await;
    }
    // The hot set is inserted (touched) strictly after every cold pool entry — the manual
    // clock is still at zero during the bulk fill — and the churn writes then go strictly
    // after the hot set, so ages are real and a hot key is the stalest of a sample only
    // when every co-sampled entry is another hot key.
    clock.advance(ms(1_000_000));
    for i in 0..HOT {
        put_key(&cache, &site, &format!("hot{i:02}"), None).await;
    }
    // Churn: 4,800 distinct cold writes while the workload keeps re-reading the hot set
    // one key per write (round-robin), which is what a hot working set means. The read
    // refreshes `last_access`, so a hot key is stale in a sample only when the sample
    // contains nothing but hot and very recently written keys.
    for n in 0..CHURN {
        let hit = got(cache.get(&site, &format!("hot{:02}", n % HOT)).await);
        assert!(hit.is_some(), "hot key hot{:02} read failed", n % HOT);
        put_key(&cache, &site, &format!("k{:04}", POOL + n), None).await;
        clock.advance(ms(1));
    }

    let stats = cache.stats(&site).await;
    assert!(
        stats.evictions >= HOT + CHURN,
        "the churn must have evicted: evictions={}",
        stats.evictions
    );
    assert!(
        stats.entries <= POOL,
        "entries={} over the quota's pool",
        stats.entries
    );
    for i in 0..HOT {
        assert!(
            CacheStore::exists(&cache, &site, &format!("hot{i:02}")).await,
            "hot key hot{i:02} was evicted by the cold churn"
        );
    }
    // And the evictions really reaped the cold tail: the oldest cold keys are long gone.
    for key in ["k0000", "k0500"] {
        assert!(
            got(cache.get(&site, key).await).is_none(),
            "cold key {key} survived the churn, so the test never exercised eviction"
        );
    }
}
/// A pool no larger than `EVICTION_SAMPLE` is sampled in full, so eviction follows exact
/// LRU: with `k0..k4` resident and `k0`/`k1` re-touched, inserting `k5` must evict `k2`
/// (the stalest), then `k6` must evict `k3`.
#[tokio::test]
async fn eviction_with_small_pool_is_exact_lru() {
    let clock = manual_clock();
    let cache = InProcessCache::with_clock(
        InProcessConfig::new(4 * 8 * COST, 5 * COST),
        clock.clone() as Arc<dyn spot_core::Clock>,
    );
    let site = sid("s");
    // One tick per insert, so every resident entry has a distinct `last_access`.
    for i in 0..5 {
        put_key(&cache, &site, &format!("k{i:04}"), None).await;
        clock.advance(ms(1));
    }
    put_key(&cache, &site, "k0000", None).await;
    clock.advance(ms(1));
    put_key(&cache, &site, "k0001", None).await;
    clock.advance(ms(1));

    put_key(&cache, &site, "k0005", None).await; // evicts k0002
    put_key(&cache, &site, "k0006", None).await; // evicts k0003

    let stats = cache.stats(&site).await;
    assert_eq!(stats.evictions, 2, "evictions={}", stats.evictions);
    for (key, live) in [
        ("k0000", true),
        ("k0001", true),
        ("k0002", false),
        ("k0003", false),
        ("k0004", true),
        ("k0005", true),
        ("k0006", true),
    ] {
        assert_eq!(
            CacheStore::exists(&cache, &site, key).await,
            live,
            "key {key} should be {}",
            if live { "live" } else { "evicted" }
        );
    }
}
/// Sweep reachability (the reviewed behaviour): a site whose writes stop still gets
/// physically emptied — expired entries are dropped by the rotation even when the budget
/// only allows `SWEEP_SCAN_BUDGET` positions of the key vector per sweep, because the
/// site's cursor wraps at the key count. The reviewed cursor advanced past the pool and
/// never wrapped, so nothing was ever removed: 300 expired entries would have sat there
/// until the loop below gave up.
#[tokio::test]
async fn sweep_reaches_expired_entries_on_an_idle_site() {
    const EXPIRED_POOL: u64 = 300;
    let clock = manual_clock();
    let cache = InProcessCache::with_clock(
        InProcessConfig::new(4 * EXPIRED_POOL * COST, (EXPIRED_POOL + 32) * COST),
        clock.clone() as Arc<dyn spot_core::Clock>,
    );
    let pool_site = sid("a");
    let trigger_site = sid("b");

    for i in 0..EXPIRED_POOL {
        put_key(
            &cache,
            &pool_site,
            &format!("e{i:04}"),
            Some(Duration::from_millis(400)),
        )
        .await;
    }
    clock.advance(ms(1_000)); // Every `e…` entry is expired now; nothing else ever expires.

    // Each round of `SWEEP_INTERVAL_WRITES` overwrites triggers one sweep; the sweep visits
    // one site (budget-bounded) per trigger, so it takes a handful of rounds to drain 300
    // entries at `SWEEP_SCAN_BUDGET` positions per visit to the pool site.
    for round in 0..25 {
        for _ in 0..SWEEP_INTERVAL_WRITES {
            put_key(&cache, &trigger_site, "trigger", None).await;
        }
        if cache.stats(&pool_site).await.entries == 0 {
            let drained_in_rounds = round + 1;
            assert!(
                drained_in_rounds >= 2,
                "the pool drained too fast to prove the rotation: rounds={drained_in_rounds}"
            );
            break;
        }
        assert!(
            round < 24,
            "the sweep rotation never reached the expired pool"
        );
    }
    let stats = cache.stats(&pool_site).await;
    assert_eq!(stats.entries, 0, "expired entries were never swept");
    assert_eq!(stats.bytes, 0, "expired bytes were never reclaimed");
    assert_eq!(stats.expired, EXPIRED_POOL, "expired={}", stats.expired);
}
