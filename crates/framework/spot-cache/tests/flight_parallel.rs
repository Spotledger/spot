//! Single-flight loading across keys: different keys never serialize on one load.

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod common;

use common::sid;
use spot_cache::{BoxLoader, Bytes, CacheStore, InProcessCache, InProcessConfig};
use tokio::sync::Notify;

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{what} failed: {error:?}"),
    }
}

fn value(text: &str) -> Bytes {
    Bytes::from(text.as_bytes().to_vec())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_keys_load_in_parallel() {
    let cache = Arc::new(InProcessCache::new(InProcessConfig::new(1 << 20, 1 << 16)));
    let active = Arc::new(AtomicUsize::new(0));
    let gate0 = Arc::new(Notify::new());
    let gate1 = Arc::new(Notify::new());
    let mut handles = Vec::new();
    for (gate, index) in [(gate0.clone(), 0usize), (gate1.clone(), 1usize)] {
        let cache = cache.clone();
        let site = sid(format!("s{index}"));
        let active = active.clone();
        handles.push(tokio::spawn(async move {
            let loader: BoxLoader = Box::pin(async move {
                active.fetch_add(1, Ordering::SeqCst);
                gate.notified().await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok::<Bytes, Box<dyn Error + Send + Sync>>(value("loaded"))
            });
            ok(
                cache.get_or_load(&site, "k", None, loader).await,
                "get_or_load",
            )
        }));
    }
    // Both loaders must be in flight at the same time: different keys never serialize.
    // A wall-clock deadline, not a spin count: a loaded CI runner (Windows) can take
    // many scheduler turns to start the second task.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while active.load(Ordering::SeqCst) < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "different keys did not load in parallel"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // `notify_one` stores a permit, so a loader that has bumped `active` but not yet
    // reached `notified()` is still released.
    gate0.notify_one();
    gate1.notify_one();
    for handle in handles {
        let loaded = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .unwrap_or_else(|_| panic!("a parallel load hung"))
            .unwrap_or_else(|error| panic!("a parallel load task failed: {error}"));
        assert_eq!(loaded, value("loaded"));
    }
}
