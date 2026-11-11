//! [`RedisCacheStore`]: the [`CacheStore`] over Redis/Valkey, with per-tenant namespacing.
//!
//! Every command follows Redis' own semantics — which is exactly what the interface specifies —
//! so the same `WrongType`, `NotAnInteger` and `Overflow` rules hold as for the in-process cache,
//! and a structure key of another shape is refused the same way. TTLs are Redis TTLs (real time;
//! unlike the coordinator, the cache is not driven by an injected clock, because it is lossy and
//! its expiry is the server's business).
//!
//! # Accounting
//!
//! The cache keeps a per-site size index — a hash mapping each key to its accounted bytes and a
//! counter of the site's total — and a small Lua script recomputes a key's size after every
//! mutation. [`stats`](CacheStore::stats) reconciles the whole site first, so `entries`, `bytes`
//! and `expired` reflect the live keys at the moment it is read. The hit/miss/load counters are
//! per node, as they are for any cache backend.
//!
//! # Quota
//!
//! A plain `set` (and the store step of [`get_or_load`](CacheStore::get_or_load)) refuses a value
//! that would not fit the configured per-site quota, leaving the key untouched, exactly like the
//! in-process cache. A structure write is bounded by Redis' own memory policy rather than by this
//! quota (see the crate report's open questions).
//!
//! # Failure
//!
//! The cache is lossy by contract, so a Redis failure degrades instead of surfacing an error the
//! trait cannot carry: a read is a miss, a write is swallowed, a delete reports `false`, a counter
//! reads as `0`. Nothing panics.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use redis::Script;
use spot_cache::{
    Bytes, CacheError, CacheFuture, CacheResult, CacheStats, CacheStore, ELEMENT_OVERHEAD_BYTES,
    ENTRY_OVERHEAD_BYTES, Ttl,
};
use spot_core::SiteId;

use crate::connection::RedisConnection;
use crate::scripts;

/// How a [`RedisCacheStore`] bounds one site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RedisCacheConfig {
    /// The most accounted bytes one site may hold; a write past it is refused.
    pub site_quota_bytes: u64,
}

impl Default for RedisCacheConfig {
    fn default() -> Self {
        // A roomy default; a profile sets its own.
        Self {
            site_quota_bytes: 1 << 30,
        }
    }
}

impl RedisCacheConfig {
    /// A config with `site_quota_bytes` per site.
    #[must_use]
    pub fn with_quota(site_quota_bytes: u64) -> Self {
        Self { site_quota_bytes }
    }
}

/// Per-node hit/miss/load/eviction/expiry counters, keyed by site.
#[derive(Clone, Copy, Debug, Default)]
struct Counters {
    hits: u64,
    misses: u64,
    loads: u64,
    evictions: u64,
    expired: u64,
}

/// Per-node in-flight loads, so one loader per `(site, key)` runs at a time.
type Inflight = Arc<Mutex<HashMap<(SiteId, String), Arc<tokio::sync::Mutex<()>>>>>;

/// One tenant's cache over Redis/Valkey.
#[derive(Clone)]
pub struct RedisCacheStore {
    connection: RedisConnection,
    config: RedisCacheConfig,
    counters: Arc<Mutex<HashMap<SiteId, Counters>>>,
    inflight: Inflight,
}

impl std::fmt::Debug for RedisCacheStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisCacheStore")
            .field("keyspace", &self.connection.keyspace().prefix())
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl RedisCacheStore {
    /// A cache over `connection` with `config`.
    #[must_use]
    pub fn new(connection: RedisConnection, config: RedisCacheConfig) -> Self {
        Self {
            connection,
            config,
            counters: Arc::new(Mutex::new(HashMap::new())),
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A cache over `connection` with the default quota.
    #[must_use]
    pub fn with_default(connection: RedisConnection) -> Self {
        Self::new(connection, RedisCacheConfig::default())
    }

    /// The key prefix in use.
    #[must_use]
    pub fn keyspace(&self) -> &crate::keys::KeySpace {
        self.connection.keyspace()
    }

    fn lock_counters(&self) -> MutexGuard<'_, HashMap<SiteId, Counters>> {
        self.counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn counters(&self, site: &SiteId) -> Counters {
        self.lock_counters().get(site).copied().unwrap_or_default()
    }

    fn bump(&self, site: &SiteId, change: impl FnOnce(&mut Counters)) {
        let mut all = self.lock_counters();
        change(all.entry(site.clone()).or_default());
    }

    fn ttl_millis(ttl: Option<Duration>) -> i64 {
        ttl.map_or(0, |ttl| {
            i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX).max(1)
        })
    }

    fn over_quota(&self, key: &str, value_len: usize) -> bool {
        let accounted = u64::try_from(value_len)
            .ok()
            .and_then(|value| value.checked_add(u64::try_from(key.len()).ok()?))
            .and_then(|total| total.checked_add(ENTRY_OVERHEAD_BYTES));
        match accounted {
            Some(total) => total > self.config.site_quota_bytes,
            None => true,
        }
    }

    /// Runs the single-key size reconciliation, best effort (a failure is ignored).
    async fn reconcile(&self, site: &SiteId, key: &str) {
        let keyspace = self.keyspace();
        let mut manager = self.connection.manager();
        let _: Result<i64, _> = Script::new(&scripts::reconcile_size())
            .key(keyspace.cache_sizes(site))
            .key(keyspace.cache_bytes(site))
            .arg(key)
            .arg(keyspace.cache_data(site, key))
            .arg(ELEMENT_OVERHEAD_BYTES)
            .arg(ENTRY_OVERHEAD_BYTES)
            .invoke_async(&mut manager)
            .await;
    }

    /// Reads every key matching `pattern` with `SCAN`, in bounded batches.
    async fn scan_keys(&self, pattern: &str) -> Vec<String> {
        let mut manager = self.connection.manager();
        let mut cursor = "0".to_owned();
        let mut keys = Vec::new();
        loop {
            let reply: Result<(String, Vec<String>), _> = redis::cmd("SCAN")
                .arg(&cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(500)
                .query_async(&mut manager)
                .await;
            match reply {
                Ok((next, batch)) => {
                    keys.extend(batch);
                    if next == "0" {
                        break;
                    }
                    cursor = next;
                }
                Err(_) => break,
            }
        }
        keys
    }

    /// Deletes a batch of `(user, data)` pairs, moving the site's accounting with them.
    async fn delete_pairs(&self, site: &SiteId, pairs: &[(String, String)]) -> u64 {
        let keyspace = self.keyspace();
        let mut manager = self.connection.manager();
        let source = scripts::cache_delete_many();
        let script = Script::new(&source);
        let mut removed = 0u64;
        for chunk in pairs.chunks(500) {
            let mut invocation = script.key(keyspace.cache_sizes(site));
            invocation.key(keyspace.cache_bytes(site));
            for (user, data) in chunk {
                invocation.arg(user).arg(data);
            }
            if let Ok(count) = invocation.invoke_async::<i64>(&mut manager).await {
                removed += u64::try_from(count).unwrap_or(0);
            }
        }
        removed
    }

    /// Bumps the site's namespace generation, the SCAN-free invalidation signal the bus carries.
    async fn bump_generation(&self, site: &SiteId) {
        let mut manager = self.connection.manager();
        let _: Result<i64, _> = redis::cmd("INCR")
            .arg(self.keyspace().cache_generation(site))
            .query_async(&mut manager)
            .await;
    }

    /// The site's namespace generation (0 when it has never been invalidated).
    ///
    /// # Errors
    ///
    /// `cluster_redis.unavailable` when the server does not answer.
    pub async fn generation(&self, site: &SiteId) -> Result<u64, crate::error::RedisError> {
        let mut manager = self.connection.manager();
        let value: Option<String> = redis::cmd("GET")
            .arg(self.keyspace().cache_generation(site))
            .query_async(&mut manager)
            .await
            .map_err(|error| {
                crate::error::RedisError::unavailable("reading a generation", error)
            })?;
        Ok(value.and_then(|text| text.parse::<u64>().ok()).unwrap_or(0))
    }

    /// Reconciles the whole site and returns `(entries, bytes, dropped)`.
    async fn reconcile_site(&self, site: &SiteId) -> (u64, u64, u64) {
        let keyspace = self.keyspace();
        let mut manager = self.connection.manager();
        let reply: Result<Vec<String>, _> = Script::new(&scripts::reconcile_site())
            .key(keyspace.cache_sizes(site))
            .key(keyspace.cache_bytes(site))
            .arg(format!("{}:cache:{}:data:", keyspace.prefix(), site))
            .arg(ELEMENT_OVERHEAD_BYTES)
            .arg(ENTRY_OVERHEAD_BYTES)
            .invoke_async(&mut manager)
            .await;
        match reply {
            Ok(values) => (
                values
                    .first()
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(0),
                values
                    .get(1)
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(0),
                values
                    .get(2)
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(0),
            ),
            Err(_) => (0, 0, 0),
        }
    }

    /// Reads a key without touching the hit/miss counters.
    async fn read(&self, site: &SiteId, key: &str) -> CacheResult<Option<Bytes>> {
        let mut manager = self.connection.manager();
        match redis::cmd("GET")
            .arg(self.keyspace().cache_data(site, key))
            .query_async::<Option<Vec<u8>>>(&mut manager)
            .await
        {
            Ok(Some(value)) => Ok(Some(Bytes::from(value))),
            Ok(None) => Ok(None),
            Err(error) if is_wrong_type(&error) => Err(CacheError::WrongType),
            Err(_) => Ok(None),
        }
    }
}

/// Whether a Redis error is `WRONGTYPE` (a command met a key of another shape).
fn is_wrong_type(error: &redis::RedisError) -> bool {
    error.code() == Some("WRONGTYPE") || error.to_string().contains("WRONGTYPE")
}

/// Maps a plain Redis command result, turning `WRONGTYPE` into [`CacheError::WrongType`] and
/// swallowing any other failure as `on_error` (the cache is lossy).
fn map<T>(result: redis::RedisResult<T>, on_error: T) -> CacheResult<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if is_wrong_type(&error) => Err(CacheError::WrongType),
        Err(_) => Ok(on_error),
    }
}

/// Maps an integer command, distinguishing a non-integer and an overflow from a connection loss.
fn map_int(result: redis::RedisResult<i64>) -> CacheResult<i64> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if is_wrong_type(&error) => Err(CacheError::WrongType),
        Err(error) => {
            let text = error.to_string();
            if text.contains("not an integer") {
                Err(CacheError::NotAnInteger)
            } else if text.contains("overflow") {
                Err(CacheError::Overflow)
            } else {
                Ok(0)
            }
        }
    }
}

impl CacheStore for RedisCacheStore {
    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        Box::pin(async move {
            let value = self.read(site, key).await?;
            self.bump(site, |counters| {
                if value.is_some() {
                    counters.hits += 1;
                } else {
                    counters.misses += 1;
                }
            });
            Ok(value)
        })
    }

    fn set<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> CacheFuture<'a, CacheResult<()>> {
        Box::pin(async move {
            if self.over_quota(key, value.len()) {
                return Err(CacheError::QuotaExceeded);
            }
            let mut manager = self.connection.manager();
            let stored = Script::new(&scripts::cache_set())
                .key(self.keyspace().cache_data(site, key))
                .arg(value.as_ref())
                .arg(Self::ttl_millis(ttl))
                .invoke_async::<i64>(&mut manager)
                .await;
            match stored {
                Ok(_) => self.reconcile(site, key).await,
                Err(error) if is_wrong_type(&error) => return Err(CacheError::WrongType),
                Err(_) => {}
            }
            Ok(())
        })
    }

    fn set_nx<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> CacheFuture<'a, CacheResult<bool>> {
        Box::pin(async move {
            if self.over_quota(key, value.len()) {
                return Err(CacheError::QuotaExceeded);
            }
            let mut manager = self.connection.manager();
            let stored = Script::new(&scripts::cache_set_nx())
                .key(self.keyspace().cache_data(site, key))
                .arg(value.as_ref())
                .arg(Self::ttl_millis(ttl))
                .invoke_async::<i64>(&mut manager)
                .await;
            match stored {
                Ok(1) => {
                    self.reconcile(site, key).await;
                    Ok(true)
                }
                Ok(_) => Ok(false),
                Err(_) => Ok(false),
            }
        })
    }

    fn delete<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, bool> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let removed: Result<i64, _> = redis::cmd("DEL")
                .arg(self.keyspace().cache_data(site, key))
                .query_async(&mut manager)
                .await;
            if matches!(removed, Ok(count) if count > 0) {
                self.reconcile(site, key).await;
                true
            } else {
                false
            }
        })
    }

    fn exists<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, bool> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            redis::cmd("EXISTS")
                .arg(self.keyspace().cache_data(site, key))
                .query_async::<i64>(&mut manager)
                .await
                .map(|count| count > 0)
                .unwrap_or(false)
        })
    }

    fn expire<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        ttl: Duration,
    ) -> CacheFuture<'a, bool> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            redis::cmd("PEXPIRE")
                .arg(self.keyspace().cache_data(site, key))
                .arg(Self::ttl_millis(Some(ttl)))
                .query_async::<i64>(&mut manager)
                .await
                .map(|set| set > 0)
                .unwrap_or(false)
        })
    }

    fn ttl<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, Ttl> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let millis: Result<i64, _> = redis::cmd("PTTL")
                .arg(self.keyspace().cache_data(site, key))
                .query_async(&mut manager)
                .await;
            match millis {
                Ok(-2) => Ttl::Missing,
                Ok(-1) => Ttl::Persistent,
                Ok(value) if value >= 0 => Ttl::Remaining(Duration::from_millis(value as u64)),
                _ => Ttl::Missing,
            }
        })
    }

    fn incr_by<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        delta: i64,
    ) -> CacheFuture<'a, CacheResult<i64>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("INCRBY")
                .arg(self.keyspace().cache_data(site, key))
                .arg(delta)
                .query_async::<i64>(&mut manager)
                .await;
            let value = map_int(result)?;
            self.reconcile(site, key).await;
            Ok(value)
        })
    }

    fn delete_prefix<'a>(&'a self, site: &'a SiteId, prefix: &'a str) -> CacheFuture<'a, u64> {
        Box::pin(async move {
            let keyspace = self.keyspace();
            let data_prefix = format!("{}:cache:{}:data:", keyspace.prefix(), site);
            let data_keys = self.scan_keys(&keyspace.cache_pattern(site, prefix)).await;
            let pairs: Vec<(String, String)> = data_keys
                .into_iter()
                .filter_map(|data| {
                    data.strip_prefix(&data_prefix)
                        .map(|user| (user.to_owned(), data.clone()))
                })
                .collect();
            let removed = self.delete_pairs(site, &pairs).await;
            self.bump_generation(site).await;
            removed
        })
    }

    fn clear_site<'a>(&'a self, site: &'a SiteId) -> CacheFuture<'a, ()> {
        Box::pin(async move {
            let keyspace = self.keyspace();
            let data_prefix = format!("{}:cache:{}:data:", keyspace.prefix(), site);
            let data_keys = self.scan_keys(&keyspace.cache_all_pattern(site)).await;
            let pairs: Vec<(String, String)> = data_keys
                .into_iter()
                .filter_map(|data| {
                    data.strip_prefix(&data_prefix)
                        .map(|user| (user.to_owned(), data.clone()))
                })
                .collect();
            self.delete_pairs(site, &pairs).await;
            let mut manager = self.connection.manager();
            let _: Result<i64, _> = redis::cmd("DEL")
                .arg(keyspace.cache_sizes(site))
                .query_async(&mut manager)
                .await;
            let _: Result<i64, _> = redis::cmd("SET")
                .arg(keyspace.cache_bytes(site))
                .arg(0)
                .query_async(&mut manager)
                .await;
            self.bump_generation(site).await;
        })
    }

    fn get_or_load<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        ttl: Option<Duration>,
        loader: spot_cache::BoxLoader,
    ) -> CacheFuture<'a, CacheResult<Bytes>> {
        Box::pin(async move {
            if let Some(value) = self.read(site, key).await? {
                self.bump(site, |counters| counters.hits += 1);
                return Ok(value);
            }
            // Single-flight within this process: one loader per (site, key) at a time; a waiter
            // re-reads the cache after the first loader stored its value.
            let gate = {
                let mut inflight = self
                    .inflight
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                Arc::clone(inflight.entry((site.clone(), key.to_owned())).or_default())
            };
            let _guard = gate.lock().await;
            if let Some(value) = self.read(site, key).await? {
                self.bump(site, |counters| counters.hits += 1);
                return Ok(value);
            }
            self.bump(site, |counters| counters.misses += 1);
            let loaded = loader
                .await
                .map_err(|error| CacheError::Loader(Arc::from(error)))?;
            self.set(site, key, loaded.clone(), ttl).await?;
            self.bump(site, |counters| counters.loads += 1);
            Ok(loaded)
        })
    }

    fn stats<'a>(&'a self, site: &'a SiteId) -> CacheFuture<'a, CacheStats> {
        Box::pin(async move {
            let counters = self.counters(site);
            let (entries, bytes, dropped) = self.reconcile_site(site).await;
            CacheStats {
                hits: counters.hits,
                misses: counters.misses,
                loads: counters.loads,
                evictions: counters.evictions,
                expired: counters.expired.saturating_add(dropped),
                entries,
                bytes,
            }
        })
    }

    // ---- hashes -------------------------------------------------------------------------

    fn hset<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
        value: Bytes,
    ) -> CacheFuture<'a, CacheResult<bool>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("HSET")
                .arg(self.keyspace().cache_data(site, key))
                .arg(field)
                .arg(value.as_ref())
                .query_async::<i64>(&mut manager)
                .await;
            let added = map(result, 0)?;
            self.reconcile(site, key).await;
            Ok(added > 0)
        })
    }

    fn hget<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("HGET")
                .arg(self.keyspace().cache_data(site, key))
                .arg(field)
                .query_async::<Option<Vec<u8>>>(&mut manager)
                .await;
            Ok(map(result, None)?.map(Bytes::from))
        })
    }

    fn hdel<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        fields: &'a [&'a str],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let mut command = redis::cmd("HDEL");
            command.arg(self.keyspace().cache_data(site, key));
            for field in fields {
                command.arg(*field);
            }
            let result = command.query_async::<i64>(&mut manager).await;
            let removed = map(result, 0)?;
            self.reconcile(site, key).await;
            Ok(u64::try_from(removed).unwrap_or(0))
        })
    }

    fn hgetall<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Vec<(String, Bytes)>>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("HGETALL")
                .arg(self.keyspace().cache_data(site, key))
                .query_async::<HashMap<String, Vec<u8>>>(&mut manager)
                .await;
            let fields = map(result, HashMap::new())?;
            Ok(fields
                .into_iter()
                .map(|(field, value)| (field, Bytes::from(value)))
                .collect())
        })
    }

    fn hexists<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
    ) -> CacheFuture<'a, CacheResult<bool>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("HEXISTS")
                .arg(self.keyspace().cache_data(site, key))
                .arg(field)
                .query_async::<i64>(&mut manager)
                .await;
            Ok(map(result, 0)? > 0)
        })
    }

    fn hlen<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("HLEN")
                .arg(self.keyspace().cache_data(site, key))
                .query_async::<i64>(&mut manager)
                .await;
            Ok(u64::try_from(map(result, 0)?).unwrap_or(0))
        })
    }

    fn hincr_by<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
        delta: i64,
    ) -> CacheFuture<'a, CacheResult<i64>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("HINCRBY")
                .arg(self.keyspace().cache_data(site, key))
                .arg(field)
                .arg(delta)
                .query_async::<i64>(&mut manager)
                .await;
            let value = map_int(result)?;
            self.reconcile(site, key).await;
            Ok(value)
        })
    }

    // ---- lists --------------------------------------------------------------------------

    fn lpush<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        values: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let mut command = redis::cmd("LPUSH");
            command.arg(self.keyspace().cache_data(site, key));
            for value in values {
                command.arg(value.as_ref());
            }
            let result = command.query_async::<i64>(&mut manager).await;
            let length = map(result, 0)?;
            self.reconcile(site, key).await;
            Ok(u64::try_from(length).unwrap_or(0))
        })
    }

    fn rpush<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        values: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let mut command = redis::cmd("RPUSH");
            command.arg(self.keyspace().cache_data(site, key));
            for value in values {
                command.arg(value.as_ref());
            }
            let result = command.query_async::<i64>(&mut manager).await;
            let length = map(result, 0)?;
            self.reconcile(site, key).await;
            Ok(u64::try_from(length).unwrap_or(0))
        })
    }

    fn lpop<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("LPOP")
                .arg(self.keyspace().cache_data(site, key))
                .query_async::<Option<Vec<u8>>>(&mut manager)
                .await;
            let value = map(result, None)?.map(Bytes::from);
            self.reconcile(site, key).await;
            Ok(value)
        })
    }

    fn rpop<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("RPOP")
                .arg(self.keyspace().cache_data(site, key))
                .query_async::<Option<Vec<u8>>>(&mut manager)
                .await;
            let value = map(result, None)?.map(Bytes::from);
            self.reconcile(site, key).await;
            Ok(value)
        })
    }

    fn lrange<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        start: i64,
        stop: i64,
    ) -> CacheFuture<'a, CacheResult<Vec<Bytes>>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("LRANGE")
                .arg(self.keyspace().cache_data(site, key))
                .arg(start)
                .arg(stop)
                .query_async::<Vec<Vec<u8>>>(&mut manager)
                .await;
            Ok(map(result, Vec::new())?
                .into_iter()
                .map(Bytes::from)
                .collect())
        })
    }

    fn llen<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("LLEN")
                .arg(self.keyspace().cache_data(site, key))
                .query_async::<i64>(&mut manager)
                .await;
            Ok(u64::try_from(map(result, 0)?).unwrap_or(0))
        })
    }

    fn ltrim<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        start: i64,
        stop: i64,
    ) -> CacheFuture<'a, CacheResult<()>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("LTRIM")
                .arg(self.keyspace().cache_data(site, key))
                .arg(start)
                .arg(stop)
                .query_async::<String>(&mut manager)
                .await;
            map(result, String::new())?;
            self.reconcile(site, key).await;
            Ok(())
        })
    }

    // ---- sets ---------------------------------------------------------------------------

    fn sadd<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        members: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let mut command = redis::cmd("SADD");
            command.arg(self.keyspace().cache_data(site, key));
            for member in members {
                command.arg(member.as_ref());
            }
            let result = command.query_async::<i64>(&mut manager).await;
            let added = map(result, 0)?;
            self.reconcile(site, key).await;
            Ok(u64::try_from(added).unwrap_or(0))
        })
    }

    fn srem<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        members: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let mut command = redis::cmd("SREM");
            command.arg(self.keyspace().cache_data(site, key));
            for member in members {
                command.arg(member.as_ref());
            }
            let result = command.query_async::<i64>(&mut manager).await;
            let removed = map(result, 0)?;
            self.reconcile(site, key).await;
            Ok(u64::try_from(removed).unwrap_or(0))
        })
    }

    fn smembers<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Vec<Bytes>>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("SMEMBERS")
                .arg(self.keyspace().cache_data(site, key))
                .query_async::<Vec<Vec<u8>>>(&mut manager)
                .await;
            Ok(map(result, Vec::new())?
                .into_iter()
                .map(Bytes::from)
                .collect())
        })
    }

    fn sismember<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        member: &'a [u8],
    ) -> CacheFuture<'a, CacheResult<bool>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("SISMEMBER")
                .arg(self.keyspace().cache_data(site, key))
                .arg(member)
                .query_async::<i64>(&mut manager)
                .await;
            Ok(map(result, 0)? > 0)
        })
    }

    fn scard<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let result = redis::cmd("SCARD")
                .arg(self.keyspace().cache_data(site, key))
                .query_async::<i64>(&mut manager)
                .await;
            Ok(u64::try_from(map(result, 0)?).unwrap_or(0))
        })
    }
}
