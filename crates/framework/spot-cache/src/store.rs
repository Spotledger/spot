//! The [`CacheStore`] interface: every command a cache answers, with Redis semantics.

use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::error::CacheResult;
use spot_core::SiteId;

/// A cached value. `Arc<[u8]>`: single-flight loading hands the same value to every
/// concurrent caller, so cheap clones are part of the shape. No `bytes` crate dependency.
pub type Bytes = Arc<[u8]>;

/// The future every [`CacheStore`] command returns. The trait must stay object-safe
/// (`Arc<dyn CacheStore>`), so async work is erased behind boxed futures.
pub type CacheFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The loader handed to [`CacheStore::get_or_load`]: a boxed future yielding the value or a
/// boxed error. It must be `'static` and it is owned by the loading flight (see the
/// [`in_process`](crate::in_process) module docs), so it is dropped unpolled when the value
/// is already cached.
pub type BoxLoader =
    Pin<Box<dyn Future<Output = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send>>;

/// Time-to-live of a live key, Redis-style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ttl {
    /// The key is absent or expired.
    Missing,
    /// The key is live and has no expiry.
    Persistent,
    /// The key is live and expires this long from now.
    Remaining(Duration),
}

/// Per-site counters and gauges, as returned by [`CacheStore::stats`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups (`get`, `get_or_load`) answered from a live entry.
    pub hits: u64,
    /// Lookups that found no live entry — including callers that then waited on a flight and
    /// received the loaded value.
    pub misses: u64,
    /// Completed loader runs whose value was stored.
    pub loads: u64,
    /// Entries evicted under quota pressure (per-site or global). Explicit deletes,
    /// invalidations and expirations are not evictions.
    pub evictions: u64,
    /// Entries removed because their TTL had passed, lazily on access or in a sweep.
    pub expired: u64,
    /// Live entries right now.
    pub entries: u64,
    /// Accounted bytes right now: key + value + [`crate::in_process::ENTRY_OVERHEAD_BYTES`] per entry.
    pub bytes: u64,
}

/// One tenant's cache. Every command takes the site first; semantics follow Redis where they
/// apply. Async commands return boxed futures, so the default backend and a later Redis
/// backend (WP-PLT-19) are interchangeable behind `Arc<dyn CacheStore>`.
///
/// The cache is lossy: any command may behave as if a key vanished (expiry, eviction).
/// Nothing that must survive a restart belongs here.
pub trait CacheStore: Send + Sync {
    /// The live value at `key`, or `None` when it is missing or expired. Fails with
    /// [`crate::error::CacheError::WrongType`] when the key holds a hash, list or set.
    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>>;

    /// Stores `value`, overwriting whatever was there (any shape in later tasks) and
    /// resetting the TTL: `None` makes the entry persistent. Fails with
    /// [`crate::error::CacheError::QuotaExceeded`] when the entry cannot fit the site's quota; the previous
    /// value is left untouched.
    fn set<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> CacheFuture<'a, CacheResult<()>>;

    /// Stores only when the key is absent — an expired key counts as absent — and reports
    /// whether it stored. The value and TTL of an existing entry are left untouched.
    fn set_nx<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> CacheFuture<'a, CacheResult<bool>>;

    /// Removes the key; `true` when a live entry was removed.
    fn delete<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, bool>;

    /// Whether a live entry exists.
    fn exists<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, bool>;

    /// Sets the expiry of a live key to `ttl` from now; `false` when it is missing.
    fn expire<'a>(&'a self, site: &'a SiteId, key: &'a str, ttl: Duration)
    -> CacheFuture<'a, bool>;

    /// Remaining TTL: [`Ttl::Missing`], [`Ttl::Persistent`] or [`Ttl::Remaining`].
    fn ttl<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, Ttl>;

    /// Adds `delta` to the ASCII decimal integer at `key` — a missing key counts as 0 — and
    /// returns the new value, stored as ASCII decimal with the entry's TTL kept. Fails with
    /// [`crate::error::CacheError::NotAnInteger`] when the stored value is not an integer,
    /// [`crate::error::CacheError::Overflow`] when the sum would not fit an `i64`, or
    /// [`crate::error::CacheError::WrongType`] when the key holds a hash, list or set.
    fn incr_by<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        delta: i64,
    ) -> CacheFuture<'a, CacheResult<i64>>;

    /// Removes every key with `prefix`, for metadata invalidation; returns how many live
    /// keys were removed (expired ones are cleaned up but not counted).
    fn delete_prefix<'a>(&'a self, site: &'a SiteId, prefix: &'a str) -> CacheFuture<'a, u64>;

    /// Removes every entry of the site, live or expired; its statistics survive.
    fn clear_site<'a>(&'a self, site: &'a SiteId) -> CacheFuture<'a, ()>;

    /// The live value, or loaded on a miss: at most one loader per `(site, key)` runs at a
    /// time, concurrent callers share its result, a loader error reaches every waiter and is
    /// never cached, and a cancelled caller never leaves the others hanging. The freshly
    /// loaded value is stored with `ttl`; a value too large for the site's quota fails with
    /// [`crate::error::CacheError::QuotaExceeded`] and is not stored.
    fn get_or_load<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        ttl: Option<Duration>,
        loader: BoxLoader,
    ) -> CacheFuture<'a, CacheResult<Bytes>>;

    /// Per-site counters and gauges.
    fn stats<'a>(&'a self, site: &'a SiteId) -> CacheFuture<'a, CacheStats>;

    // ---- hashes -------------------------------------------------------------------------
    //
    // Every structure command follows Redis: a key of another type fails with
    // `WrongType` (so does `get`/`incr_by` on a structure); removing the last field, member
    // or element deletes the key; the key's TTL belongs to the whole key and adding
    // elements keeps it. A write that would push the key past the site's quota fails with
    // `QuotaExceeded` and leaves the key unchanged.

    /// Sets `field` to `value`, creating the hash when missing; `true` when the field is new.
    fn hset<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
        value: Bytes,
    ) -> CacheFuture<'a, CacheResult<bool>>;

    /// The value of `field`, or `None` when the field or the hash is missing.
    fn hget<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>>;

    /// Removes `fields` (each counted once, however often listed); returns how many existed.
    fn hdel<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        fields: &'a [&'a str],
    ) -> CacheFuture<'a, CacheResult<u64>>;

    /// Every `(field, value)` pair, in unspecified order; empty for a missing hash.
    fn hgetall<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Vec<(String, Bytes)>>>;

    /// Whether `field` exists.
    fn hexists<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
    ) -> CacheFuture<'a, CacheResult<bool>>;

    /// The number of fields; 0 for a missing hash.
    fn hlen<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>>;

    /// Adds `delta` to the ASCII decimal integer in `field` — a missing field or hash counts
    /// as 0 — and returns the new value. Fails with `NotAnInteger` when the field holds
    /// something else and `Overflow` when the sum would not fit an `i64`.
    fn hincr_by<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
        delta: i64,
    ) -> CacheFuture<'a, CacheResult<i64>>;

    // ---- lists --------------------------------------------------------------------------

    /// Pushes `values` onto the head one after the other (so the last one ends up first, as in
    /// Redis) and returns the new length. An empty `values` changes nothing and returns the
    /// current length.
    fn lpush<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        values: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>>;

    /// Pushes `values` onto the tail in order and returns the new length.
    fn rpush<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        values: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>>;

    /// Removes and returns the head, or `None` for a missing list.
    fn lpop<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>>;

    /// Removes and returns the tail, or `None` for a missing list.
    fn rpop<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>>;

    /// The elements from `start` to `stop`, both inclusive, with Redis' rules: a negative
    /// index counts from the tail, `start` below the head clamps to 0, `stop` past the tail
    /// clamps to the last element, and an empty range yields an empty vector.
    fn lrange<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        start: i64,
        stop: i64,
    ) -> CacheFuture<'a, CacheResult<Vec<Bytes>>>;

    /// The length; 0 for a missing list.
    fn llen<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>>;

    /// Keeps only the elements in the `lrange` window; an empty window deletes the key.
    fn ltrim<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        start: i64,
        stop: i64,
    ) -> CacheFuture<'a, CacheResult<()>>;

    // ---- sets ---------------------------------------------------------------------------

    /// Adds `members`; returns how many were not yet in the set.
    fn sadd<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        members: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>>;

    /// Removes `members`; returns how many were in the set.
    fn srem<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        members: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>>;

    /// Every member, in unspecified order; empty for a missing set.
    fn smembers<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Vec<Bytes>>>;

    /// Whether `member` is in the set.
    fn sismember<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        member: &'a [u8],
    ) -> CacheFuture<'a, CacheResult<bool>>;

    /// The number of members; 0 for a missing set.
    fn scard<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>>;
}

/// Wraps a [`CacheStore`] and records every `get` as a hit or a miss in the current request's
/// cost ([`spot_telemetry::RequestCost`], through the task-local scope the HTTP layer and the
/// job worker establish).
///
/// The cache crate does not know which request a lookup belongs to; the cost accumulator does.
/// Recording outside a scope is a no-op, so the wrapper is safe to use everywhere. The wrapper
/// itself adds no work beyond one counter update: every other command is forwarded unchanged.
#[derive(Clone, Debug)]
pub struct RecordingCache<C> {
    inner: C,
}

impl<C> RecordingCache<C> {
    /// Wraps `inner`.
    pub fn new(inner: C) -> Self {
        Self { inner }
    }

    /// The wrapped store.
    pub fn inner(&self) -> &C {
        &self.inner
    }

    /// Unwraps the store.
    pub fn into_inner(self) -> C {
        self.inner
    }
}

impl<C: CacheStore> CacheStore for RecordingCache<C> {
    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        let lookup = self.inner.get(site, key);
        Box::pin(async move {
            let result = lookup.await;
            // A failed lookup produced no value, so it counts as a miss like an absent key.
            spot_telemetry::cost::record_cache(matches!(&result, Ok(Some(_))));
            result
        })
    }

    fn set<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> CacheFuture<'a, CacheResult<()>> {
        self.inner.set(site, key, value, ttl)
    }

    fn set_nx<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> CacheFuture<'a, CacheResult<bool>> {
        self.inner.set_nx(site, key, value, ttl)
    }

    fn delete<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, bool> {
        self.inner.delete(site, key)
    }

    fn exists<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, bool> {
        self.inner.exists(site, key)
    }

    fn expire<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        ttl: Duration,
    ) -> CacheFuture<'a, bool> {
        self.inner.expire(site, key, ttl)
    }

    fn ttl<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, Ttl> {
        self.inner.ttl(site, key)
    }

    fn incr_by<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        delta: i64,
    ) -> CacheFuture<'a, CacheResult<i64>> {
        self.inner.incr_by(site, key, delta)
    }

    fn delete_prefix<'a>(&'a self, site: &'a SiteId, prefix: &'a str) -> CacheFuture<'a, u64> {
        self.inner.delete_prefix(site, prefix)
    }

    fn clear_site<'a>(&'a self, site: &'a SiteId) -> CacheFuture<'a, ()> {
        self.inner.clear_site(site)
    }

    fn get_or_load<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        ttl: Option<Duration>,
        loader: BoxLoader,
    ) -> CacheFuture<'a, CacheResult<Bytes>> {
        self.inner.get_or_load(site, key, ttl, loader)
    }

    fn stats<'a>(&'a self, site: &'a SiteId) -> CacheFuture<'a, CacheStats> {
        self.inner.stats(site)
    }

    fn hset<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
        value: Bytes,
    ) -> CacheFuture<'a, CacheResult<bool>> {
        self.inner.hset(site, key, field, value)
    }

    fn hget<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        self.inner.hget(site, key, field)
    }

    fn hdel<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        fields: &'a [&'a str],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        self.inner.hdel(site, key, fields)
    }

    fn hgetall<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Vec<(String, Bytes)>>> {
        self.inner.hgetall(site, key)
    }

    fn hexists<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
    ) -> CacheFuture<'a, CacheResult<bool>> {
        self.inner.hexists(site, key, field)
    }

    fn hlen<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>> {
        self.inner.hlen(site, key)
    }

    fn hincr_by<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
        delta: i64,
    ) -> CacheFuture<'a, CacheResult<i64>> {
        self.inner.hincr_by(site, key, field, delta)
    }

    fn lpush<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        values: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        self.inner.lpush(site, key, values)
    }

    fn rpush<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        values: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        self.inner.rpush(site, key, values)
    }

    fn lpop<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        self.inner.lpop(site, key)
    }

    fn rpop<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        self.inner.rpop(site, key)
    }

    fn lrange<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        start: i64,
        stop: i64,
    ) -> CacheFuture<'a, CacheResult<Vec<Bytes>>> {
        self.inner.lrange(site, key, start, stop)
    }

    fn llen<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>> {
        self.inner.llen(site, key)
    }

    fn ltrim<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        start: i64,
        stop: i64,
    ) -> CacheFuture<'a, CacheResult<()>> {
        self.inner.ltrim(site, key, start, stop)
    }

    fn sadd<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        members: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        self.inner.sadd(site, key, members)
    }

    fn srem<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        members: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        self.inner.srem(site, key, members)
    }

    fn smembers<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Vec<Bytes>>> {
        self.inner.smembers(site, key)
    }

    fn sismember<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        member: &'a [u8],
    ) -> CacheFuture<'a, CacheResult<bool>> {
        self.inner.sismember(site, key, member)
    }

    fn scard<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>> {
        self.inner.scard(site, key)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]
mod tests {
    use super::*;
    use crate::in_process::{InProcessCache, InProcessConfig};

    #[tokio::test]
    async fn get_records_a_hit_and_a_miss_in_the_cost_scope() {
        let cache =
            RecordingCache::new(InProcessCache::new(InProcessConfig::new(1 << 20, 1 << 20)));
        let site = SiteId::new("acme").unwrap();
        let ((), cost) = spot_telemetry::cost::scope(async {
            assert!(cache.get(&site, "missing").await.unwrap().is_none());
            cache
                .set(&site, "present", Arc::from(&b"value"[..]), None)
                .await
                .unwrap();
            assert!(cache.get(&site, "present").await.unwrap().is_some());
        })
        .await;
        assert_eq!(cost.cache_misses, 1);
        assert_eq!(cost.cache_hits, 1);
    }

    #[tokio::test]
    async fn recording_outside_a_scope_is_a_no_op() {
        let cache =
            RecordingCache::new(InProcessCache::new(InProcessConfig::new(1 << 20, 1 << 20)));
        let site = SiteId::new("acme").unwrap();
        assert!(cache.get(&site, "missing").await.unwrap().is_none());
        assert!(spot_telemetry::cost::current().is_none());
    }
}
