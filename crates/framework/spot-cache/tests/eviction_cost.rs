//! Bounded eviction-work regression tests.
//!
//! The eviction samplers in the in-process cache must never walk the whole entry pool to
//! pick a victim: per-site eviction draws exactly `EVICTION_SAMPLE` uniform samples of the
//! site's key vector however large the pool, and global eviction ranks the sites by
//! saturation (O(sites)) instead of scoring entries. The pools below hold tens of thousands
//! of entries while the tests insert hundreds of thousands of entries, so at these sizes a
//! sampler that walks a pool-proportional slice per eviction (the reviewed behavior:
//! `skip(cursor % len)`) is tens of times slower per insert than the same cache with a
//! small pool, while the bounded samplers stay within a small constant of it.
//!
//! The check is relative, not an absolute wall-clock budget: CI runs the whole workspace in
//! parallel, so a fixed time limit fails whenever the machine is loaded. Each test measures
//! the per-insert cost on a small pool (best of two runs, on the same thread, just before)
//! and requires the large-pool per-insert cost to stay under a fixed multiple of it. Measured
//! on the reference machine, the bounded sampled walk costs about 1.5 µs per eviction whether
//! the pool holds 3k, 12k or 48k entries (about 0.1 µs in release), so the ratio is about
//! 1 to 1.5; an unbounded walk would multiply the large-pool cost by the pool size.

use std::time::{Duration, Instant};

mod common;

use common::sid;
use spot_cache::{Bytes, CacheStore, InProcessCache, InProcessConfig};
use spot_core::SiteId;

/// Cheapest possible churn entry below: 2-byte key + 16-byte value + per-entry overhead.
const MIN_ENTRY_COST: u64 = 2 + 16 + spot_cache::ENTRY_OVERHEAD_BYTES;

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{what} failed: {error:?}"),
    }
}

/// Per-insert wall time of `inserts` 16-byte-value writes spread over `sites` sites, into a
/// cache whose shared pool is `max_bytes` and whose per-site quota is `site_quota`.
async fn per_insert_cost(
    max_bytes: u64,
    site_quota: u64,
    sites: usize,
    inserts: usize,
) -> (Duration, InProcessCache, Vec<SiteId>) {
    let cache = InProcessCache::new(InProcessConfig::new(max_bytes, site_quota));
    let sites: Vec<SiteId> = (0..sites).map(|i| sid(format!("s{i}"))).collect();
    let started = Instant::now();
    for i in 0..inserts {
        ok(
            cache
                .set(
                    &sites[i % sites.len()],
                    &format!("k{i}"),
                    Bytes::from(vec![b'x'; 16]),
                    None,
                )
                .await,
            "set",
        );
    }
    (started.elapsed() / inserts as u32, cache, sites)
}

/// The eviction cost must not scale with the pool. Instead of an absolute wall-clock budget
/// (which fails whenever the machine is loaded: CI runs the whole workspace in parallel), the
/// large-pool per-insert cost is compared with a small-pool baseline measured on the same
/// thread moments earlier, so load slows both alike. A sampler that walks a pool-proportional
/// slice per eviction is tens of times slower on the large pool; the bounded one is within a
/// small constant (cache misses on the bigger map) of the baseline.
const MAX_SLOWDOWN_VS_SMALL_POOL: u32 = 12;

async fn small_pool_baseline(sites: usize) -> Duration {
    // About 700 live entries across the sites: tiny pool, same eviction path.
    let quota = 64 * 1024u64;
    let first = per_insert_cost(2 * quota, quota, sites, 40_000).await.0;
    let second = per_insert_cost(2 * quota, quota, sites, 40_000).await.0;
    first.min(second).max(Duration::from_nanos(1))
}

#[tokio::test]
async fn per_site_eviction_stays_bounded_at_large_pool_sizes() {
    let inserts = 150_000usize;
    let baseline = small_pool_baseline(1).await;
    let (per_insert, cache, sites) = per_insert_cost(5_000_000, 5_000_000, 1, inserts).await;
    let site = sites[0].clone();

    let stats = cache.stats(&site).await;
    // The site quota equals `max_bytes`, so no global-pressure eviction ever happens and
    // the site's own counter covers everything.
    assert!(
        stats.evictions > inserts as u64 / 2,
        "evictions={}",
        stats.evictions
    );
    assert!(
        stats.entries <= 5_000_000 / MIN_ENTRY_COST,
        "entries={} exceeds the byte quota",
        stats.entries
    );
    assert!(
        per_insert < baseline * MAX_SLOWDOWN_VS_SMALL_POOL,
        "per-site eviction must not scale with the pool: {per_insert:?} per insert against \
         {baseline:?} on a small pool"
    );
}

#[tokio::test]
async fn global_eviction_stays_bounded_across_sites() {
    let quota = 2 * 1024 * 1024u64;
    let inserts = 300_000usize;
    let baseline = small_pool_baseline(4).await;
    let (per_insert, cache, sites) = per_insert_cost(2 * quota, quota, 4, inserts).await;

    let mut evictions = 0u64;
    let mut entries = 0u64;
    let mut bytes = 0u64;
    for site in &sites {
        let stats = cache.stats(site).await;
        evictions += stats.evictions;
        entries += stats.entries;
        bytes += stats.bytes;
    }
    // Roughly one eviction per write once the shared pool is full; every site's own quota
    // leaves it far below `max_bytes`, so these are global-pressure evictions.
    assert!(evictions > inserts as u64 / 2, "evictions={evictions}");
    assert!(
        entries <= 2 * quota / MIN_ENTRY_COST,
        "entries={entries} exceeds the shared cap"
    );
    assert!(bytes <= 2 * quota, "bytes={bytes} over the shared cap");
    assert!(
        per_insert < baseline * MAX_SLOWDOWN_VS_SMALL_POOL,
        "global eviction must not scale with the pool: {per_insert:?} per insert against \
         {baseline:?} on a small pool"
    );
}
