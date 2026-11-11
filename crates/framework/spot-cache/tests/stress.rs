//! Multi-threaded stress: the byte accounting stays within every site quota and the global
//! cap after every write returns, and concurrent `incr_by` never loses an increment. The
//! accounting itself aborts on any underflow (checked subtraction), so surviving the churn
//! is the check.

use std::collections::HashMap;
use std::sync::Arc;

mod common;

use common::got;

use common::sid;
use spot_cache::{Bytes, CacheStore, InProcessCache, InProcessConfig};

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{what} failed: {error:?}"),
    }
}

const SITES: usize = 8;
const QUOTA: u64 = 512;
const ITERATIONS: u64 = 400;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_traffic_keeps_accounting_within_bounds() {
    let cache = Arc::new(InProcessCache::new(InProcessConfig::new(
        SITES as u64 * QUOTA,
        QUOTA,
    )));
    let mut handles = Vec::new();
    for thread in 0..SITES {
        let cache = cache.clone();
        handles.push(tokio::spawn(async move {
            let site = sid(format!("site-{thread}"));
            let mut state = 0x9E37_79B9_7F4A_7C15 ^ ((thread as u64 + 1) << 32);
            for _ in 0..ITERATIONS {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let key = format!("k{}", state % 32);
                match state % 4 {
                    0 => {
                        let len = ((state >> 8) % 48) as usize + 8;
                        ok(
                            cache.set(&site, &key, Bytes::from(vec![b'x'; len]), None).await,
                            "set",
                        );
                        let stats = cache.stats(&site).await;
                        assert!(
                            stats.bytes <= QUOTA,
                            "site {} holds {} bytes after a set returned, over its {QUOTA} byte quota",
                            site,
                            stats.bytes,
                        );
                    }
                    1 => {
                        let _ = got(cache.get(&site, &key).await);
                    }
                    2 => {
                        let _ = cache.delete(&site, &key).await;
                    }
                    _ => {
                        if cache.incr_by(&site, &key, (state % 7) as i64 - 3).await.is_ok() {
                            let stats = cache.stats(&site).await;
                            assert!(
                                stats.bytes <= QUOTA,
                                "site {} holds {} bytes after an incr_by returned, over its {QUOTA} byte quota",
                                site,
                                stats.bytes,
                            );
                        }
                    }
                }
            }
            cache.stats(&site).await
        }));
    }
    let mut total = 0;
    for handle in handles {
        let stats = handle
            .await
            .unwrap_or_else(|error| panic!("a worker failed: {error}"));
        assert!(
            stats.bytes <= QUOTA,
            "final site bytes {} over quota {QUOTA}",
            stats.bytes
        );
        assert!(
            stats.evictions > 0,
            "the stress traffic must have evicted entries"
        );
        total += stats.bytes;
    }
    assert!(
        total <= SITES as u64 * QUOTA,
        "global total {total} over the cap"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_cap_holds_under_shared_key_churn() {
    // The per-site quotas sum far over the cap: global eviction must keep the total low.
    let mut per_site = HashMap::new();
    for i in 0..4 {
        per_site.insert(sid(format!("s{i}")), 4096);
    }
    let cache = Arc::new(InProcessCache::new(InProcessConfig {
        max_bytes: 1024,
        per_site_max_bytes: per_site,
        default_per_site_max_bytes: 4096,
    }));
    let mut handles = Vec::new();
    for thread in 0..4 {
        let cache = cache.clone();
        handles.push(tokio::spawn(async move {
            let site = sid(format!("s{thread}"));
            let mut state = 0xC0FF_EE12_3456_789A ^ ((thread as u64 + 3) << 16);
            for _ in 0..300 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let key = format!("shared-{}", state % 16);
                ok(
                    cache
                        .set(&site, &key, Bytes::from(vec![b'y'; 32]), None)
                        .await,
                    "set",
                );
                // Only this thread writes its own site, so this bound is race-free. The
                // global cap cannot be observed atomically through per-site stats while
                // other threads write; it is asserted after all workers join.
                let stats = cache.stats(&site).await;
                assert!(
                    stats.bytes <= 4096,
                    "site {site} holds {} bytes after a set returned, over its quota",
                    stats.bytes,
                );
            }
        }));
    }
    for handle in handles {
        handle
            .await
            .unwrap_or_else(|error| panic!("a worker failed: {error}"));
    }
    // With every writer parked, the four stats reads see one consistent state: the global
    // cap must hold, and the shared-key churn must have gone through global eviction.
    let mut total = 0;
    let mut evictions = 0;
    for i in 0..4 {
        let stats = cache.stats(&sid(format!("s{i}"))).await;
        total += stats.bytes;
        evictions += stats.evictions;
    }
    assert!(
        total <= 1024,
        "final global total {total} over the 1024 byte cap"
    );
    assert!(
        evictions > 0,
        "the churn must have triggered global evictions"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_incr_by_never_loses_an_increment() {
    let cache = Arc::new(InProcessCache::new(InProcessConfig::new(1 << 20, 1 << 16)));
    let site = sid("shared");
    let mut handles = Vec::new();
    for _ in 0..4 {
        let cache = cache.clone();
        let site = site.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..250 {
                ok(cache.incr_by(&site, "counter", 1).await, "incr_by");
            }
        }));
    }
    for handle in handles {
        handle
            .await
            .unwrap_or_else(|error| panic!("an incr worker failed: {error}"));
    }
    assert_eq!(
        got(cache.get(&site, "counter").await),
        Some(Bytes::from(b"1000".to_vec())),
    );
}
