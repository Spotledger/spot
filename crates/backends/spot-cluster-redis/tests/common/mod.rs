//! Shared helpers for the Redis-backed integration suites.
//!
//! Every suite skips (printing `SKIPPED`) unless `SPOT_TEST_REDIS_URL` names a reachable server,
//! and each connection uses a **unique key prefix**, so the suites and a developer's own data can
//! share one server without colliding.

#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use spot_cluster_redis::{KeySpace, RedisConfig, RedisConnection};
use spot_core::SiteId;

/// The server URL, when one is configured.
pub fn url() -> Option<String> {
    std::env::var("SPOT_TEST_REDIS_URL")
        .ok()
        .filter(|url| !url.is_empty())
}

/// A fresh, unique key prefix for one connection.
///
/// The run nonce (a start timestamp) keeps two runs of the suite from reusing each other's keys
/// on a long-lived server, and the per-connection counter keeps the tests within one run apart.
pub fn unique_prefix() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    static RUN: OnceLock<u64> = OnceLock::new();
    let run = *RUN.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| u64::try_from(since.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    });
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    format!("spot:test:redis:{run}:{}:{n}", std::process::id())
}

/// A config for the configured server with a fresh prefix.
pub fn config() -> Option<RedisConfig> {
    let url = url()?;
    Some(RedisConfig::new(url).with_keyspace(KeySpace::with_prefix(unique_prefix())))
}

/// Connects to the configured server, or `None` when none is configured or it is unreachable.
pub async fn connect() -> Option<RedisConnection> {
    let config = config()?;
    match RedisConnection::connect(&config).await {
        Ok(connection) => Some(connection),
        Err(error) => {
            eprintln!("SKIPPED: SPOT_TEST_REDIS_URL is set but the server is unreachable: {error}");
            None
        }
    }
}

/// A valid site id from a host-name literal.
pub fn sid(name: &str) -> SiteId {
    SiteId::new(name).unwrap_or_else(|error| panic!("test site id: {error}"))
}

/// Prints the standard skip line for a suite without a server.
pub fn skip() {
    println!("SKIPPED: SPOT_TEST_REDIS_URL is not set");
}
