//! Single-flight cancellation: the loading caller's future being dropped must never leave
//! its co-waiters hanging; one of them takes over the loader.

use std::error::Error;
use std::future::pending;
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;

mod common;

use common::sid;
use spot_cache::{BoxLoader, Bytes, CacheStore, InProcessCache, InProcessConfig};

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
async fn cancelled_leader_hands_the_loader_to_a_waiter() {
    let cache = Arc::new(InProcessCache::new(InProcessConfig::new(1 << 20, 1 << 16)));
    let site = sid("s1");
    let polls = Arc::new(AtomicUsize::new(0));
    // The leader's loader: Pending on the first poll and it never registers a waker, so the
    // leader parks until the test cancels it; every later poll completes.
    let loader_polls = polls.clone();
    let leader_loader: BoxLoader = Box::pin(poll_fn(move |_cx| {
        if loader_polls.fetch_add(1, Ordering::SeqCst) == 0 {
            Poll::Pending
        } else {
            Poll::Ready(Ok::<Bytes, Box<dyn Error + Send + Sync>>(value("rescued")))
        }
    }));
    let leader_cache = cache.clone();
    let leader_site = site.clone();
    let leader = tokio::spawn(async move {
        leader_cache
            .get_or_load(&leader_site, "k", None, leader_loader)
            .await
    });
    // Wait until the leader has been polled once: it now owns the flight's loader.
    while polls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    // Two waiters join the flight while the leader holds the loader.
    let mut waiters = Vec::new();
    for _ in 0..2 {
        let cache = cache.clone();
        let site = site.clone();
        waiters.push(tokio::spawn(async move {
            let loader: BoxLoader =
                Box::pin(pending::<Result<Bytes, Box<dyn Error + Send + Sync>>>());
            ok(
                cache.get_or_load(&site, "k", None, loader).await,
                "get_or_load",
            )
        }));
    }
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    // Cancel the leader: its loader returns to the flight and a waiter takes over.
    leader.abort();
    let _ = leader.await;
    for waiter in waiters {
        let loaded = tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .unwrap_or_else(|_| panic!("a waiter hung after the leader was cancelled"))
            .unwrap_or_else(|error| panic!("a waiter task failed: {error}"));
        assert_eq!(loaded, value("rescued"));
    }
    assert_eq!(
        polls.load(Ordering::SeqCst),
        2,
        "the loader was polled once by the leader and once by its taker"
    );
    let stats = cache.stats(&site).await;
    assert_eq!(stats.loads, 1);
}
