//! Helpers shared by the integration tests: valid site ids and a manual clock that reads in
//! milliseconds, so the tests stay readable on the shared `spot-core` clock.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use spot_core::{ManualClock, SiteId};
use time::OffsetDateTime;

/// A site id from a valid host-name literal.
pub fn sid(name: impl Into<String>) -> SiteId {
    match SiteId::new(name) {
        Ok(site) => site,
        Err(error) => panic!("test site id is not a valid host name: {error}"),
    }
}

/// A manual clock at monotonic time zero.
pub fn manual_clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH))
}

/// `millis` milliseconds.
pub fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

/// The value of a `get`/`hget`-style reply that must not fail.
pub fn got<T>(reply: Result<T, spot_cache::CacheError>) -> T {
    match reply {
        Ok(value) => value,
        Err(error) => panic!("cache command failed: {error}"),
    }
}

/// A shared-bytes value from text.
pub fn bytes(text: &str) -> spot_cache::Bytes {
    spot_cache::Bytes::from(text.as_bytes().to_vec())
}

/// Shared-bytes values from texts.
pub fn all(texts: &[&str]) -> Vec<spot_cache::Bytes> {
    texts.iter().map(|text| bytes(text)).collect()
}

/// A roomy cache on a manual clock.
pub fn cache() -> (spot_cache::InProcessCache, Arc<ManualClock>) {
    let clock = manual_clock();
    let cache = spot_cache::InProcessCache::with_clock(
        spot_cache::InProcessConfig::new(1 << 22, 1 << 20),
        clock.clone(),
    );
    (cache, clock)
}

/// Sorted texts of some values, for order-insensitive comparisons.
pub fn texts(values: &[spot_cache::Bytes]) -> Vec<String> {
    let mut out: Vec<String> = values
        .iter()
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .collect();
    out.sort();
    out
}

/// Texts of some values in their given order.
pub fn ordered(values: &[spot_cache::Bytes]) -> Vec<String> {
    values
        .iter()
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .collect()
}
