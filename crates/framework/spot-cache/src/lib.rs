//! The Spotledger cache layer: the [`CacheStore`] interface and its default bounded
//! in-process backend (WP-PLT-09.T1, ADR-20).
//!
//! Redis did five jobs in Frappe; queues and pub/sub moved to the job engine and the event
//! bus, and the remaining two — caching and coordination — are separate interfaces because a
//! cache may lose data and a lock may not. This crate holds the first of them:
//!
//! - [`CacheStore`] is the pluggable interface. It is object-safe (`Arc<dyn CacheStore>`) so
//!   the Redis/Valkey backend of WP-PLT-19 can implement the very same trait; async commands
//!   return boxed futures for that reason.
//! - [`InProcessCache`] is the default backend: one process' memory, bounded, with TTL,
//!   per-site quotas and single-flight loading. There is no Redis in the default product.
//!
//! # The cache is lossy by contract
//!
//! Any entry may disappear at any time: it expires, it is evicted under memory pressure, or
//! the process restarts. Callers must always be able to reload from real storage; nothing
//! that must survive a restart belongs here.
//!
//! # Memory bounds
//!
//! Every entry is accounted as its key length plus its value length plus
//! [`ENTRY_OVERHEAD_BYTES`]. A value too large for a site's quota — or for the global
//! cache cap — is refused with
//! [`CacheError::QuotaExceeded`] and not stored. When a site exceeds its quota, eviction
//! takes entries from that site only; when the global total exceeds the configured maximum,
//! the victim site is the most saturated one — a site donates while under its own quota
//! only as the last resort, when it already holds the largest share of its own quota of
//! any site. Eviction is **sampled LRU in bounded time** — see the [`in_process`] module
//! docs for the choices and their reasons.
//!
//! # Single-flight loading
//!
//! [`CacheStore::get_or_load`] runs at most one loader per `(site, key)` at a time. The
//! loader future is owned by the loading *flight*, not by the caller that supplied it: every
//! caller for the same key cooperatively polls that one future, so concurrent callers share
//! the result, a loader error reaches every waiter and is never cached, and dropping any
//! caller (even the one that started the load) never leaves the others hanging — a remaining
//! caller drives the loader to completion.
//!
//! # Scope
//!
//! Values are plain bytes under string keys, or one of three Redis-style structures — hashes,
//! lists and sets — with Redis' return values and [`CacheError::WrongType`] rules (WP-PLT-09.T2).
//! Sorted sets, streams, pub/sub, scripting and blocking pops are not part of the interface.
//! [`SiteId`](spot_core::SiteId) and [`Clock`](spot_core::Clock) come from `spot-core`.

pub mod error;
pub mod in_process;
pub mod store;

pub use error::{CacheError, CacheResult};
pub use in_process::{
    ELEMENT_OVERHEAD_BYTES, ENTRY_OVERHEAD_BYTES, EVICTION_SAMPLE, InProcessCache, InProcessConfig,
    SWEEP_INTERVAL_WRITES, SWEEP_SCAN_BUDGET,
};
pub use store::{BoxLoader, Bytes, CacheFuture, CacheStats, CacheStore, Ttl};
