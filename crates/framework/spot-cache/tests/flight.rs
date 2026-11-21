//! Single-flight loading: one loader run for many concurrent callers, error broadcast,
//! already-cached short-circuit, and the requested TTL of a loaded value.

use std::error::Error;
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod common;

use common::{got, manual_clock, ms, sid};
use spot_cache::{BoxLoader, Bytes, CacheError, CacheStore, InProcessCache, InProcessConfig};

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{what} failed: {error:?}"),
    }
}

fn value(text: &str) -> Bytes {
    Bytes::from(text.as_bytes().to_vec())
}

fn cache() -> Arc<InProcessCache> {
    Arc::new(InProcessCache::new(InProcessConfig::new(1 << 20, 1 << 16)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_hundred_concurrent_callers_share_one_loader_run() {
    let cache = cache();
    let site = sid("s1");
    let runs = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..100 {
        let cache = cache.clone();
        let site = site.clone();
        let runs = runs.clone();
        handles.push(tokio::spawn(async move {
            let loader: BoxLoader = Box::pin(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                for _ in 0..32 {
                    tokio::task::yield_now().await;
                }
                Ok::<Bytes, Box<dyn Error + Send + Sync>>(value("shared"))
            });
            let loaded = ok(
                cache.get_or_load(&site, "hot", None, loader).await,
                "get_or_load",
            );
            assert_eq!(loaded, value("shared"));
        }));
    }
    for handle in handles {
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .unwrap_or_else(|_| panic!("a caller hung waiting on the flight"))
            .unwrap_or_else(|error| panic!("a caller task failed: {error}"));
    }
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the loader must run exactly once"
    );
    let stats = cache.stats(&site).await;
    assert_eq!(stats.loads, 1);
    assert_eq!(stats.entries, 1);
    // A later caller finds the loaded value; the never-polled pending loader is not run.
    let again = ok(
        cache
            .get_or_load(
                &site,
                "hot",
                None,
                Box::pin(pending::<Result<Bytes, Box<dyn Error + Send + Sync>>>()),
            )
            .await,
        "get_or_load",
    );
    assert_eq!(again, value("shared"));
}

#[tokio::test]
async fn loader_errors_reach_every_waiter_and_are_not_cached() {
    let cache = cache();
    let site = sid("s1");
    let mut handles = Vec::new();
    for _ in 0..50 {
        let cache = cache.clone();
        let site = site.clone();
        handles.push(tokio::spawn(async move {
            let loader: BoxLoader = Box::pin(async {
                Err::<Bytes, Box<dyn Error + Send + Sync>>("boom".to_string().into())
            });
            cache.get_or_load(&site, "k", None, loader).await
        }));
    }
    for handle in handles {
        let result = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .unwrap_or_else(|_| panic!("a caller hung waiting on the failed flight"))
            .unwrap_or_else(|error| panic!("a caller task failed: {error}"));
        match result {
            Err(CacheError::Loader(_)) => {}
            other => panic!("every caller must see the loader error, got {other:?}"),
        }
    }
    let stats = cache.stats(&site).await;
    assert_eq!(stats.loads, 0, "a failed load must not be cached");
    assert_eq!(stats.entries, 0);
    // The failure is not cached: the next call loads again and succeeds.
    let runs = Arc::new(AtomicUsize::new(0));
    let loader_runs = runs.clone();
    let loader: BoxLoader = Box::pin(async move {
        loader_runs.fetch_add(1, Ordering::SeqCst);
        Ok::<Bytes, Box<dyn Error + Send + Sync>>(value("second"))
    });
    let loaded = ok(
        cache.get_or_load(&site, "k", None, loader).await,
        "get_or_load",
    );
    assert_eq!(loaded, value("second"));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cached_value_skips_the_loader() {
    let (cache, _clock) = {
        let clock = manual_clock();
        (
            InProcessCache::with_clock(InProcessConfig::new(1 << 20, 1 << 16), clock.clone()),
            clock,
        )
    };
    let site = sid("s1");
    ok(cache.set(&site, "k", value("cached"), None).await, "set");
    let runs = Arc::new(AtomicUsize::new(0));
    let loader_runs = runs.clone();
    let loader: BoxLoader = Box::pin(async move {
        loader_runs.fetch_add(1, Ordering::SeqCst);
        Ok::<Bytes, Box<dyn Error + Send + Sync>>(value("loaded"))
    });
    let loaded = ok(
        cache.get_or_load(&site, "k", None, loader).await,
        "get_or_load",
    );
    assert_eq!(loaded, value("cached"));
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "a cached value must not run the loader"
    );
    assert_eq!(cache.stats(&site).await.loads, 0);
}

#[tokio::test]
async fn loaded_value_gets_the_requested_ttl() {
    let clock = manual_clock();
    let cache = InProcessCache::with_clock(InProcessConfig::new(1 << 20, 1 << 16), clock.clone());
    let site = sid("s1");
    let loader: BoxLoader =
        Box::pin(async { Ok::<Bytes, Box<dyn Error + Send + Sync>>(value("loaded")) });
    let loaded = ok(
        cache
            .get_or_load(&site, "k", Some(Duration::from_millis(500)), loader)
            .await,
        "get_or_load",
    );
    assert_eq!(loaded, value("loaded"));
    assert_eq!(got(cache.get(&site, "k").await), Some(value("loaded")));
    clock.advance(ms(500));
    assert_eq!(got(cache.get(&site, "k").await), None);
}
