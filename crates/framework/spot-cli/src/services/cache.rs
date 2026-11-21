//! The site's cache, hosted (WP-PLT-09.T3): `spot-cache` is the [`CacheStore`] behind the steps'
//! `ctx.cache()`.
//!
//! Before this module the engine served `ctx.cache()` from its private `MemoryCache`, an
//! unbounded map that never swept an expired entry, so one app's step could fill the process
//! memory, and a value set on one node was a miss on the next even with the Redis plugin on.
//! Now:
//!
//! - [`StepCache`] is the engine's [`AppCache`]. It is created by the host before the engine and
//!   filled by this module's `register` once the store exists (the late-fill pattern of the files
//!   service's `AttachSlot`). It is lossy: until it is filled, and on **any** cache error, `get`
//!   answers `None` and `set` and `delete` do nothing, and each such failure is counted in
//!   `spot_cache_step_dropped_total{reason}`. A step must always be able to reload.
//! - Entries live under the `step:` key prefix of the site, so the store can host other users.
//! - The store is the `CacheStore` port when one was provided first (the Redis plugin, whose
//!   module has a smaller order), else an in-process, bounded one built here and provided as the
//!   port. The module is always on and has no `enabled` key.
//!
//! Operator guide: `docs/running-spot/cache.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use spot_app_api::BoxFuture;
use spot_app_api::context::AppCache;
use spot_cache::{CacheError, CacheStats, CacheStore, InProcessCache, InProcessConfig};
use spot_core::{Clock, SiteId};
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after `cluster_redis` (1000), so a store it provides is found first.
pub const ORDER: u32 = 1001;

/// The module and service name, and the `[services.<name>]` table it reads.
pub const NAME: &str = "cache";

/// The prefix every step key is stored under, so `delete_prefix("step:")` clears only steps.
pub const STEP_PREFIX: &str = "step:";

/// The longest key a step may use, in bytes.
pub const MAX_KEY_BYTES: usize = 512;

/// How often the module's loop reads the store's statistics into the metrics.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Metric names.
pub mod metric {
    /// Counter: lookups answered from a live entry.
    pub const HITS: &str = "spot_cache_hits_total";
    /// Counter: lookups that found nothing.
    pub const MISSES: &str = "spot_cache_misses_total";
    /// Counter: entries evicted under quota pressure.
    pub const EVICTIONS: &str = "spot_cache_evictions_total";
    /// Counter: entries removed because their time to live passed.
    pub const EXPIRED: &str = "spot_cache_expired_total";
    /// Gauge: live entries.
    pub const ENTRIES: &str = "spot_cache_entries";
    /// Gauge: accounted bytes.
    pub const BYTES: &str = "spot_cache_bytes";
    /// Counter, label `reason`: step cache calls that did nothing.
    pub const STEP_DROPPED: &str = "spot_cache_step_dropped_total";
}

/// Why a step's cache call did nothing.
pub mod reason {
    /// The slot was not filled yet.
    pub const UNFILLED: &str = "unfilled";
    /// The key is over [`super::MAX_KEY_BYTES`].
    pub const KEY_SIZE: &str = "key_size";
    /// The value is over `max_value_bytes`.
    pub const VALUE_SIZE: &str = "value_size";
    /// The store refused it: over the site's quota.
    pub const QUOTA: &str = "quota";
    /// Any other store error (a lost Redis, a wrong type).
    pub const ERROR: &str = "error";
}

const MIB: u64 = 1024 * 1024;

/// `[services.cache]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The in-process cache's budget for the whole process, in bytes. Ignored when the Redis
    /// plugin provides the store. Default 64 MiB.
    pub max_bytes: u64,
    /// One site's share of it, in bytes. Default 32 MiB.
    pub site_max_bytes: u64,
    /// The largest value a step may cache, in bytes. Default 1 MiB.
    pub max_value_bytes: u64,
    /// The time to live of an entry a step stored without one, in seconds. Default 3600: nothing
    /// lives forever by accident.
    pub default_ttl_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_bytes: 64 * MIB,
            site_max_bytes: 32 * MIB,
            max_value_bytes: MIB,
            default_ttl_seconds: 3600,
        }
    }
}

fn refuse(key: &str, why: &str) -> Error {
    Error(format!("services.{NAME}.{key}: {why}"))
}

impl Settings {
    /// Checks the settings, naming the first key that cannot work.
    ///
    /// # Errors
    ///
    /// A zero budget, a zero value limit or ttl, or a site quota above the process budget.
    pub fn validate(&self) -> Result<()> {
        if self.max_bytes == 0 {
            return Err(refuse("max_bytes", "must be positive"));
        }
        if self.site_max_bytes == 0 {
            return Err(refuse("site_max_bytes", "must be positive"));
        }
        if self.site_max_bytes > self.max_bytes {
            return Err(refuse(
                "site_max_bytes",
                "must not be above max_bytes (one site cannot hold more than the process)",
            ));
        }
        if self.max_value_bytes == 0 {
            return Err(refuse("max_value_bytes", "must be positive"));
        }
        if self.default_ttl_seconds == 0 {
            return Err(refuse("default_ttl_seconds", "must be positive"));
        }
        Ok(())
    }

    fn limits(&self) -> Limits {
        Limits {
            max_value_bytes: usize::try_from(self.max_value_bytes).unwrap_or(usize::MAX),
            default_ttl: Duration::from_secs(self.default_ttl_seconds),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Limits {
    max_value_bytes: usize,
    default_ttl: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Settings::default().limits()
    }
}

struct Inner {
    store: Option<Arc<dyn CacheStore>>,
    limits: Limits,
}

/// The engine's [`AppCache`], over a late-filled [`CacheStore`] and the site id.
///
/// Semantics, all deliberate: a cache is lossy by contract, so until the slot is filled and on
/// any [`CacheError`] (`QuotaExceeded`, a lost Redis) `get` answers `None` and `set` and
/// `delete` do nothing, never an error to the step; each such failure counts in
/// `spot_cache_step_dropped_total{reason}`. A key over 512 bytes or a value over
/// `max_value_bytes` is refused the same way. A `set` with no ttl gets the configured default
/// ttl, and a zero ttl stores nothing. Keys are stored under `step:` of the site, so a step reaches
/// nothing but its own site's `step:` entries. No key or value is ever logged or labelled.
pub struct StepCache {
    site: SiteId,
    inner: Mutex<Inner>,
    dropped: Mutex<HashMap<&'static str, u64>>,
}

static SLOTS: OnceLock<Mutex<HashMap<String, Arc<StepCache>>>> = OnceLock::new();

impl StepCache {
    /// An empty slot for `site`, with the default limits.
    #[must_use]
    pub fn new(site: SiteId) -> Arc<Self> {
        Arc::new(Self {
            site,
            inner: Mutex::new(Inner {
                store: None,
                limits: Limits::default(),
            }),
            dropped: Mutex::new(HashMap::new()),
        })
    }

    /// The slot of `site` for the life of the process: the host asks for it on every build, so a
    /// reload, which builds a new engine but not new services, keeps the store this module filled
    /// it with.
    #[must_use]
    pub fn for_site(site: &SiteId) -> Arc<Self> {
        let slots = SLOTS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut slots = slots.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(
            slots
                .entry(site.to_string())
                .or_insert_with(|| Self::new(site.clone())),
        )
    }

    /// Fills the slot (the latest store wins) and applies the limits.
    pub fn fill(&self, store: Arc<dyn CacheStore>, settings: &Settings) {
        let mut inner = self.lock();
        inner.store = Some(store);
        inner.limits = settings.limits();
    }

    /// How many calls did nothing for `reason` since the process started (see [`reason`]).
    #[must_use]
    pub fn dropped(&self, reason: &str) -> u64 {
        self.dropped
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(reason)
            .copied()
            .unwrap_or(0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn snapshot(&self) -> (Option<Arc<dyn CacheStore>>, Limits) {
        let inner = self.lock();
        (inner.store.clone(), inner.limits)
    }

    fn drop_call(&self, why: &'static str) {
        *self
            .dropped
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(why)
            .or_insert(0) += 1;
        metrics::counter!(metric::STEP_DROPPED, "reason" => why).increment(1);
    }

    fn drop_for(&self, error: &CacheError) {
        self.drop_call(match error {
            CacheError::QuotaExceeded => reason::QUOTA,
            _ => reason::ERROR,
        });
    }
}

fn stored_key(key: &str) -> String {
    format!("{STEP_PREFIX}{key}")
}

impl AppCache for StepCache {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let (store, _) = self.snapshot();
            let Some(store) = store else {
                self.drop_call(reason::UNFILLED);
                return None;
            };
            if key.len() > MAX_KEY_BYTES {
                self.drop_call(reason::KEY_SIZE);
                return None;
            }
            match store.get(&self.site, &stored_key(key)).await {
                Ok(value) => value.map(|bytes| bytes.to_vec()),
                Err(e) => {
                    self.drop_for(&e);
                    None
                }
            }
        })
    }

    fn set<'a>(&'a self, key: &'a str, value: Vec<u8>, ttl: Option<Duration>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let (store, limits) = self.snapshot();
            let Some(store) = store else {
                self.drop_call(reason::UNFILLED);
                return;
            };
            if key.len() > MAX_KEY_BYTES {
                self.drop_call(reason::KEY_SIZE);
                return;
            }
            if value.len() > limits.max_value_bytes {
                self.drop_call(reason::VALUE_SIZE);
                return;
            }
            let ttl = ttl.unwrap_or(limits.default_ttl);
            if ttl.is_zero() {
                return;
            }
            let bytes: Arc<[u8]> = Arc::from(value);
            if let Err(e) = store
                .set(&self.site, &stored_key(key), bytes, Some(ttl))
                .await
            {
                self.drop_for(&e);
            }
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let (store, _) = self.snapshot();
            let Some(store) = store else {
                self.drop_call(reason::UNFILLED);
                return;
            };
            if key.len() > MAX_KEY_BYTES {
                self.drop_call(reason::KEY_SIZE);
                return;
            }
            // The answer (whether an entry was removed) is of no use to a step.
            let _ = store.delete(&self.site, &stored_key(key)).await;
        })
    }
}

/// What [`build`] decided.
pub struct Built {
    /// The store the steps use.
    pub store: Arc<dyn CacheStore>,
    /// `in-process`, or `redis` when a plugin's store was used.
    pub backend: &'static str,
    /// Whether the caller must provide `store` as the `CacheStore` port (an in-process store we
    /// built; a plugin's store is provided already).
    pub provide: bool,
    /// The service that polls the statistics.
    pub service: Arc<CacheService>,
}

/// Validates `settings`, picks the store (`existing` when a plugin provided one, else a bounded
/// in-process cache on `clock`), fills `slot` and builds the service. Pure: no site context.
///
/// # Errors
///
/// A setting that [`Settings::validate`] refuses.
pub fn build(
    settings: &Settings,
    site: &SiteId,
    clock: Arc<dyn Clock>,
    existing: Option<Arc<dyn CacheStore>>,
    slot: &Arc<StepCache>,
) -> Result<Built> {
    settings.validate()?;
    let (store, backend, provide): (Arc<dyn CacheStore>, &'static str, bool) = match existing {
        Some(store) => (store, "redis", false),
        None => {
            let mut config = InProcessConfig::new(settings.max_bytes, settings.site_max_bytes);
            config
                .per_site_max_bytes
                .insert(site.clone(), settings.site_max_bytes);
            (
                Arc::new(InProcessCache::with_clock(config, clock)),
                "in-process",
                true,
            )
        }
    };
    slot.fill(Arc::clone(&store), settings);
    let service = Arc::new(CacheService {
        site: site.clone(),
        store: Arc::clone(&store),
        backend,
        max_bytes: settings.max_bytes,
        last: Mutex::new(CacheStats::default()),
    });
    Ok(Built {
        store,
        backend,
        provide,
        service,
    })
}

/// Registers the module: the store (the port when a plugin provided one, else an in-process one
/// that is provided here), the filled [`StepCache`] slot and the statistics service.
///
/// # Errors
///
/// A setting that does not decode or validate.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    // A bad setting is refused even where no site is attached.
    settings.validate()?;
    // A reload builds a new engine but keeps the services: the slot the host asked for keeps the
    // store filled at the first build.
    if !ctx.assembling() {
        return Ok(());
    }
    let site = SiteId::new(&ctx.env()?.site.site).map_err(|e| Error(e.to_string()))?;
    let slot = StepCache::for_site(&site);
    let existing = ctx.port::<dyn CacheStore>();
    let built = build(&settings, &site, Arc::clone(ctx.clock()), existing, &slot)?;
    if built.provide {
        ctx.provide::<dyn CacheStore>(Arc::clone(&built.store))?;
    }
    ctx.add(built.service);
    Ok(())
}

/// The statistics service: polls the store's counters into `/metrics`.
pub struct CacheService {
    site: SiteId,
    store: Arc<dyn CacheStore>,
    backend: &'static str,
    max_bytes: u64,
    last: Mutex<CacheStats>,
}

impl CacheService {
    /// The backend in use: `in-process` or `redis`.
    #[must_use]
    pub fn backend(&self) -> &'static str {
        self.backend
    }

    /// Reads the store's statistics once: counters move by the delta since the last read (the
    /// statistics only grow), gauges are set. No key, value or app name is a label.
    pub async fn poll_once(&self) -> CacheStats {
        let stats = self.store.stats(&self.site).await;
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        metrics::counter!(metric::HITS).increment(stats.hits.saturating_sub(last.hits));
        metrics::counter!(metric::MISSES).increment(stats.misses.saturating_sub(last.misses));
        metrics::counter!(metric::EVICTIONS)
            .increment(stats.evictions.saturating_sub(last.evictions));
        metrics::counter!(metric::EXPIRED).increment(stats.expired.saturating_sub(last.expired));
        #[allow(clippy::cast_precision_loss)]
        {
            metrics::gauge!(metric::ENTRIES).set(stats.entries as f64);
            metrics::gauge!(metric::BYTES).set(stats.bytes as f64);
        }
        *last = stats.clone();
        stats
    }
}

impl Service for CacheService {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        // A cache is never part of correctness.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            // Numbers and the backend only, never a key.
            let stats = self.store.stats(&self.site).await;
            tracing::debug!(
                backend = self.backend,
                bytes = stats.bytes,
                max_bytes = self.max_bytes,
                "step cache ready"
            );
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(POLL_INTERVAL) => {
                        self.poll_once().await;
                    }
                }
            }
        })
    }
}

/// The count of calls a step made that did nothing, for a [`StepCache`] (see [`reason`]), summed.
#[must_use]
pub fn total_dropped(slot: &StepCache) -> u64 {
    [
        reason::UNFILLED,
        reason::KEY_SIZE,
        reason::VALUE_SIZE,
        reason::QUOTA,
        reason::ERROR,
    ]
    .iter()
    .map(|r| slot.dropped(r))
    .sum()
}
