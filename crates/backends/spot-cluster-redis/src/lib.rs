//! `spot-cluster-redis` — the optional Redis/Valkey plugin (WP-PLT-19, ADR-20, ADR-072).
//!
//! The default product needs no Redis: the database-backed coordinator (WP-PLT-18.T2) coordinates
//! a cluster, [`InProcessCache`](spot_cache::InProcessCache) caches one process and the in-process
//! bus fans metadata changes out within it. This crate is the **opt-in** alternative for a
//! deployment that already runs Redis or Valkey, behind the non-default `redis` feature and the
//! `cluster-redis` profile. Nothing here is compiled unless that feature is on, so the workspace
//! build never pulls the `redis` crate.
//!
//! It implements the very same interfaces as the built-in backends, so it passes the same
//! contract suites:
//!
//! - [`RedisCacheStore`](cache::RedisCacheStore) — [`CacheStore`](spot_cache::CacheStore), with
//!   hashes, lists and sets, per-tenant key namespacing, TTLs and size accounting.
//! - [`RedisCoordinator`](coordinator::RedisCoordinator) —
//!   [`Coordinator`](spot_coordinator::Coordinator): fenced leases, counters to `u64::MAX` and
//!   GCRA rate limits, all in Lua so each call is one atomic round trip. It runs
//!   [`spot_coordinator::contract::run_all`] unchanged because it reads time only from the
//!   [`Clock`](spot_core::Clock) it was built on.
//! - [`RedisBus`](bus::RedisBus) — [`InvalidationBus`](spot_meta_live::InvalidationBus) over
//!   per-tenant pub/sub channels, with a [`Gap`](spot_meta_live::BusEvent::Gap) on a reconnect so
//!   a subscriber reconciles instead of trusting a stream that lost messages.
//! - [`RedisVersionStore`](store::RedisVersionStore) — [`VersionStore`](spot_meta_live::VersionStore).
//! - [`RedisPresence`](presence::RedisPresence) — [`NodePresence`](spot_coordinator::NodePresence)
//!   with TTL keys, for the in-process coordinator's second-node detection.
//!
//! # Failure behaviour
//!
//! Redis being down degrades each component the way its own contract says: the cache answers a
//! miss (it is lossy by contract) and swallows a failed write; the coordinator returns
//! `coordinator.unavailable` so its caller falls back or fails closed; the bus returns
//! `metalive.store`. Nothing panics, the connection manager reconnects with backoff, and
//! [`health`] probes the server for `spot-fleet-health`.
//!
//! # Time is an argument
//!
//! No script and no Rust path reads the wall clock: the coordinator and presence take a
//! [`Clock`](spot_core::Clock), so a lease's expiry, a counter and a rate limit are all decided
//! from the injected time. Two nodes must share a clock source (NTP) for leases to agree; even
//! then a paused holder is not fenced by the lock — the **fencing token** is what protects a
//! writer, and [`RedisCoordinator`](coordinator::RedisCoordinator) documents exactly why.
//!
//! # Running against a server
//!
//! The integration suites run only when `SPOT_TEST_REDIS_URL` is set (for example
//! `redis://127.0.0.1:6379`); otherwise they print `SKIPPED`. See `docs/redis-profile.md` in this
//! crate for the compose snippet and the threshold at which the profile is worth running.

pub mod error;
pub mod keys;
pub mod scripts;

#[cfg(feature = "redis")]
pub mod bus;
#[cfg(feature = "redis")]
pub mod cache;
#[cfg(feature = "redis")]
pub mod connection;
#[cfg(feature = "redis")]
pub mod coordinator;
#[cfg(feature = "redis")]
pub mod health;
#[cfg(feature = "redis")]
pub mod presence;
#[cfg(feature = "redis")]
pub mod store;

pub use error::{PROTOCOL, RedisError, RedisResult, UNAVAILABLE};
pub use keys::{DEFAULT_NAMESPACE, KeySpace};

#[cfg(feature = "redis")]
pub use bus::{BusMessage, RedisBus, SUBSCRIBER_CAPACITY};
#[cfg(feature = "redis")]
pub use cache::{RedisCacheConfig, RedisCacheStore};
#[cfg(feature = "redis")]
pub use connection::{RedisConfig, RedisConnection};
#[cfg(feature = "redis")]
pub use coordinator::RedisCoordinator;
#[cfg(feature = "redis")]
pub use health::{HealthReport, probe};
#[cfg(feature = "redis")]
pub use presence::RedisPresence;
#[cfg(feature = "redis")]
pub use store::RedisVersionStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-cluster-redis";
