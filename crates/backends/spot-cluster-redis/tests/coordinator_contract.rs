//! The coordinator contract suite, against a real Redis/Valkey server.
//!
//! [`spot_coordinator::contract::run_all`] drives time through a `ManualClock`, and this backend
//! reads time only from the clock it is built on, so the suite runs **unchanged**: every case
//! gets a fresh connection under a fresh key prefix and a fresh `ManualClock` at the Unix epoch.
//! Skips without `SPOT_TEST_REDIS_URL`.

#![cfg(feature = "redis")]
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use spot_cluster_redis::{KeySpace, RedisConfig, RedisConnection, RedisCoordinator};
use spot_coordinator::Coordinator;
use spot_coordinator::contract::{Harness, run_all};
use spot_core::ManualClock;
use time::OffsetDateTime;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redis_passes_the_coordinator_contract() {
    let Some(url) = common::url() else {
        return common::skip();
    };
    let report = run_all(|| {
        let url = url.clone();
        async move {
            let config =
                RedisConfig::new(url).with_keyspace(KeySpace::with_prefix(common::unique_prefix()));
            let connection = RedisConnection::connect(&config)
                .await
                .expect("connect to Redis");
            let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
            let coordinator: Arc<dyn Coordinator> =
                Arc::new(RedisCoordinator::new(connection, clock.clone()));
            Harness { coordinator, clock }
        }
    })
    .await;
    report.assert_all_passed();
}

/// A coordinator on its own connection and key prefix, with a `ManualClock` at the epoch.
async fn fresh(url: &str) -> RedisCoordinator {
    let config =
        RedisConfig::new(url).with_keyspace(KeySpace::with_prefix(common::unique_prefix()));
    let connection = RedisConnection::connect(&config)
        .await
        .expect("connect to Redis");
    RedisCoordinator::new(
        connection,
        Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_acquire_never_blocks_the_next_acquire_of_the_same_holder() {
    let Some(url) = common::url() else {
        return common::skip();
    };
    let coordinator = fresh(&url).await;
    let ttl = Duration::from_secs(10);
    for micros in [0u64, 1, 20, 200] {
        for round in 0..50 {
            let attempt = tokio::time::timeout(
                Duration::from_micros(micros),
                coordinator.acquire("cx", "a", ttl),
            )
            .await;
            if let Ok(done) = attempt {
                let lease = done.expect("an acquire that beat its timeout");
                coordinator.release(&lease).await.expect("release");
            }
            // Whether the first attempt finished or was abandoned, the same holder can
            // take the name at once and nobody else holds it meanwhile.
            let lease = coordinator
                .acquire("cx", "a", ttl)
                .await
                .unwrap_or_else(|error| panic!("{micros}us round {round}: {error:?}"));
            coordinator.release(&lease).await.expect("release");
            assert!(coordinator.current("cx").await.expect("current").is_none());
        }
    }
}

/// A TCP forwarder to `target` that, once `blackhole` is set, keeps connections open but
/// forwards nothing (a server that stopped answering).
fn forwarder(target: String, blackhole: Arc<AtomicBool>) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let Ok(server) = std::net::TcpStream::connect(&target) else {
                continue;
            };
            for (mut from, mut to) in [
                (
                    client.try_clone().expect("clone"),
                    server.try_clone().expect("clone"),
                ),
                (server, client),
            ] {
                let blackhole = Arc::clone(&blackhole);
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 4096];
                    while let Ok(read) = from.read(&mut buffer) {
                        if read == 0 {
                            break;
                        }
                        if !blackhole.load(Ordering::SeqCst)
                            && to.write_all(&buffer[..read]).is_err()
                        {
                            break;
                        }
                    }
                });
            }
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_for_an_abandoned_acquire_is_bounded_when_the_server_stops_answering() {
    let Some(url) = common::url() else {
        return common::skip();
    };
    let target = url
        .strip_prefix("redis://")
        .unwrap_or(&url)
        .trim_end_matches('/')
        .to_owned();
    let blackhole = Arc::new(AtomicBool::new(false));
    let addr = forwarder(target, Arc::clone(&blackhole));
    let config = RedisConfig::new(format!("redis://{addr}"))
        .with_keyspace(KeySpace::with_prefix(common::unique_prefix()));
    let connection = RedisConnection::connect(&config)
        .await
        .expect("connect through the forwarder");
    let coordinator = RedisCoordinator::new(
        connection,
        Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    )
    .with_abandoned_wait(Duration::from_millis(300));
    blackhole.store(true, Ordering::SeqCst);
    let ttl = Duration::from_secs(10);
    let abandoned = tokio::time::timeout(
        Duration::from_millis(1),
        coordinator.acquire("cx", "a", ttl),
    )
    .await;
    assert!(
        abandoned.is_err(),
        "the acquire cannot finish without a server"
    );
    let started = Instant::now();
    let refused = coordinator.acquire("cx", "a", ttl).await;
    let error = refused.expect_err("a dead server must not hand out a lease");
    assert_eq!(error.code, "coordinator.unavailable");
    let current = coordinator.current("cx").await;
    assert_eq!(
        current.expect_err("current waits too").code,
        "coordinator.unavailable"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the wait must be bounded"
    );
}
