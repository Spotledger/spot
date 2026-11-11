//! The bounded in-process [`CacheStore`] backend.
//!
//! # Layout and locking
//!
//! All entries, counters and accounted bytes live behind a single [`Mutex`]:
//! one map of [`SiteId`] to its per-site state, plus the global byte total. One mutex keeps
//! the byte accounting exact — sharded maps would split per-site and global totals across
//! locks — and the critical sections are short and never held across an `.await`.
//! Single-flight state sits behind its own mutex, never nested with the state mutex.
//!
//! # Eviction: sampled LRU in bounded time
//!
//! When a write pushes a site over its quota, the least recently used entries of **that site
//! only** are evicted; when the global total exceeds the configured maximum, eviction crosses
//! sites. The victim entry is the least recently used of [`EVICTION_SAMPLE`] entries drawn
//! uniformly from the **whole** pool — the same approach Redis uses in `dictGetSomeKeys` /
//! `evictionPoolPopulate`: random slots across the whole table, evicting the stalest of the
//! sample. A full LRU structure would need an intrusive per-entry list rewritten on every
//! access; sampled LRU keeps writes and evictions cheap, and the contract only requires
//! approximately-LRU. With at most [`EVICTION_SAMPLE`] live entries the sample covers the
//! whole pool, so small working sets behave exactly LRU.
//!
//! Sampling is bounded however large the pool: every entry lives in a per-site vector of
//! keys (`keys[i]` is the key whose entry records `i`), so an eviction draws
//! [`EVICTION_SAMPLE`] pseudo-random indices — a seeded, dependency-free splitmix64 — and
//! examines exactly that many entries whatever the pool size. Uniform draws never leave part
//! of the pool unsampled: a stale entry is a candidate the moment it lands in a sample, so
//! cold keys are reclaimed while a frequently touched working set — nearly always younger
//! than the stalest of eight random draws — survives churn. Eviction cost therefore does not
//! grow with the number of entries.
//!
//! # Fairness: global eviction takes from the most saturated site
//!
//! Per-site eviction evicts that site only, so a noisy site cannot evict a second site's
//! entries as long as every site stays within its own quota and the quotas fit inside the
//! global maximum. Under global pressure (`max_bytes`) the victim *site* is the most
//! saturated one:
//!
//! 1. a site strictly over its own quota donates first — a write always leaves its own site
//!    within its quota, so this tier is defensive; and
//! 2. otherwise the site whose accounted bytes are the largest fraction of its own quota —
//!    `bytes / quota`, compared exactly by cross-multiplication, never in floating point —
//!    donates. The writing site wins an exact tie, so eviction work lands where the
//!    pressure came from, and a site under its quota donates only as the last resort, when
//!    the site quotas oversubscribe `max_bytes` (the most saturated site must then donate).
//!
//! Keep the sum of site quotas at or below `max_bytes` if every site must stay fully immune
//! to global pressure. Within the chosen site the victim entry is still chosen by recency.
//! The donor scan is O(sites) per global eviction and the amortised sweep allocates one
//! `Vec` of the site names per trigger; both are bounded by the site count, not the entries.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use spot_core::{Clock, SiteId, SystemClock};
use tokio::sync::Notify;

use crate::error::{CacheError, CacheResult};
use crate::store::{BoxLoader, Bytes, CacheFuture, CacheStats, CacheStore, Ttl};

/// Fixed per-entry accounting overhead, in bytes: one map slot, the boxed value, the expiry
/// stamp and the LRU stamp. A plain entry costs `key.len() + value.len() + ENTRY_OVERHEAD_BYTES`.
///
/// A hash, list or set costs `key.len() + ENTRY_OVERHEAD_BYTES` plus, for every element, its
/// bytes (field name and value for a hash; the value for a list element or set member) plus
/// [`ELEMENT_OVERHEAD_BYTES`]. Quotas and eviction treat the whole structure as one entry.
pub const ENTRY_OVERHEAD_BYTES: u64 = 64;

/// Fixed accounting overhead of one hash field, list element or set member, in bytes: the
/// container slot and the shared-bytes header.
pub const ELEMENT_OVERHEAD_BYTES: u64 = 32;

/// Entries sampled per eviction step; the least recently used of the sample is evicted
/// (sampled LRU). Small pools (at most this many live entries) behave exactly LRU.
pub const EVICTION_SAMPLE: usize = 8;

/// Writes between amortised expired-entry sweeps.
pub const SWEEP_INTERVAL_WRITES: u64 = 64;

/// Entries examined per amortised expired-entry sweep: one site per sweep, chosen by
/// rotating through the sites, and at most this many consecutive positions of that site's
/// key vector, starting at the position the previous sweep of the site stopped at and
/// wrapping at the length. A sweep is bounded work, never a full pass over the cache; the
/// rotating window still reaches every entry over time.
pub const SWEEP_SCAN_BUDGET: usize = 64;

/// Memory bounds of [`InProcessCache`].
///
/// `per_site_max_bytes` holds per-site overrides; a site without an entry uses
/// `default_per_site_max_bytes`. The global total is additionally capped by `max_bytes`.
#[derive(Clone, Debug)]
pub struct InProcessConfig {
    /// Cap on the bytes accounted across all sites.
    pub max_bytes: u64,
    /// Per-site quota overrides, keyed by site.
    pub per_site_max_bytes: HashMap<SiteId, u64>,
    /// Quota for a site without an override.
    pub default_per_site_max_bytes: u64,
}

impl InProcessConfig {
    /// A config capping the global total at `max_bytes` and giving every site without an
    /// override a quota of `default_per_site_max_bytes`.
    pub fn new(max_bytes: u64, default_per_site_max_bytes: u64) -> Self {
        Self {
            max_bytes,
            per_site_max_bytes: HashMap::new(),
            default_per_site_max_bytes,
        }
    }
}

/// The default `CacheStore`: bounded in-process memory with TTL, per-site quotas and
/// single-flight loading (ADR-20). Lossy by contract; the per-site byte accounting is exact
/// and enforced on every write.
pub struct InProcessCache {
    config: InProcessConfig,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    flights: Mutex<HashMap<FlightKey, Arc<Flight>>>,
}

struct State {
    sites: HashMap<SiteId, SiteState>,
    total_bytes: u64,
    writes: u64,
    /// Rotates which site the next amortised expiry sweep visits.
    sweep_cursor: u64,
    /// SplitMix64 state feeding the eviction sampler; seeded deterministically so behaviour
    /// is reproducible. See `next_sample`.
    sample_rng: u64,
}

struct SiteState {
    entries: HashMap<String, Entry>,
    /// The site's keys in no particular order — the index both the evictor and the expired
    /// sweep sample uniformly from in constant time. Invariant: `keys[i]` is the key of the
    /// entry whose `slot` is `i`, so `entries.len() == keys.len()`.
    keys: Vec<String>,
    bytes: u64,
    quota: u64,
    /// Rotates the window the expiry sweep examines inside this site's key vector.
    sweep_cursor: u64,
    hits: u64,
    misses: u64,
    loads: u64,
    evictions: u64,
    expired: u64,
}

impl SiteState {
    fn new(quota: u64) -> Self {
        Self {
            entries: HashMap::new(),
            keys: Vec::new(),
            bytes: 0,
            quota,
            sweep_cursor: 0,
            hits: 0,
            misses: 0,
            loads: 0,
            evictions: 0,
            expired: 0,
        }
    }

    /// Inserts `entry` under `key`, appending the key to the site's key vector so eviction
    /// and sweep sampling can reach every entry in constant time.
    fn insert_key(&mut self, key: &str, mut entry: Entry) {
        entry.slot = self.keys.len();
        self.keys.push(key.to_owned());
        self.entries.insert(key.to_owned(), entry);
    }

    /// Removes `key` if present: drops the entry, swaps the key that sits at the removed
    /// key's vector position into its place (fixing that entry's `slot`), and accounts the
    /// freed bytes. Returns the removed entry.
    fn remove_key(&mut self, key: &str, total_bytes: &mut u64) -> Option<Entry> {
        let entry = self.entries.remove(key)?;
        // `swap_remove` moves the vector's last key into the freed position — unless the
        // removed key was itself the last, in which case nothing moves and no `slot` of
        // another entry needs fixing. (`swap_remove` returns the element at the removed
        // position — the removed key itself — so the moved key is read before the swap.)
        let moved_key = if entry.slot + 1 != self.keys.len() {
            self.keys.last().cloned()
        } else {
            None
        };
        self.keys.swap_remove(entry.slot);
        if let Some(moved_key) = moved_key
            && let Some(moved_entry) = self.entries.get_mut(&moved_key)
        {
            moved_entry.slot = entry.slot;
        }
        subtract_bytes(&mut self.bytes, entry.cost);
        subtract_bytes(total_bytes, entry.cost);
        Some(entry)
    }

    /// Walks at most `budget` consecutive positions of the site's key vector, starting at
    /// the position the previous sweep of this site stopped at and wrapping at the length,
    /// and removes the expired entries, accounting the freed bytes. Returns the number of
    /// positions examined; successive calls advance over the whole key vector, so every
    /// entry is reached over time however large the site.
    fn sweep_expired(&mut self, total_bytes: &mut u64, now: u64, budget: usize) -> usize {
        let len = self.keys.len();
        if len == 0 {
            return 0;
        }
        let start = (self.sweep_cursor % len as u64) as usize;
        let examined = budget.min(len);
        // Resolve positions to keys before removing anything: removals move the last key
        // into the freed position and would shift the window out from under the walk.
        let expired: Vec<String> = (start..start + examined)
            .map(|position| &self.keys[position % len])
            .filter(|key| {
                self.entries
                    .get(*key)
                    .is_some_and(|entry| !is_live(entry, now))
            })
            .cloned()
            .collect();
        for key in &expired {
            if self.remove_key(key, total_bytes).is_some() {
                self.expired += 1;
            }
        }
        // The next sweep of this site resumes after the window just examined.
        self.sweep_cursor = self.sweep_cursor.wrapping_add(examined as u64);
        examined
    }

    #[cfg(test)]
    fn assert_invariant(&self) {
        assert_eq!(self.entries.len(), self.keys.len(), "len divergence");
        let mut total = 0;
        for (key, entry) in &self.entries {
            assert_eq!(
                entry.cost,
                key.len() as u64 + ENTRY_OVERHEAD_BYTES + entry.value.payload(),
                "entry {key} cost drifted from its contents"
            );
            assert!(!entry.value.is_empty(), "entry {key} is an empty structure");
            total += entry.cost;
        }
        assert_eq!(self.bytes, total, "site bytes drifted from the entry costs");
        for (position, key) in self.keys.iter().enumerate() {
            let entry = self
                .entries
                .get(key)
                .unwrap_or_else(|| panic!("key {key} in keys but not in entries"));
            assert_eq!(
                entry.slot, position,
                "key {key} in keys at {position} but slot {}",
                entry.slot
            );
        }
    }
}

/// What a key holds. Structures never hold zero elements: the last removal deletes the key.
enum Value {
    Bytes(Bytes),
    Hash(HashMap<String, Bytes>),
    List(VecDeque<Bytes>),
    Set(HashSet<Bytes>),
}

/// The structure kinds, for type checks and for creating an empty value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Hash,
    List,
    Set,
}

impl Kind {
    fn empty(self) -> Value {
        match self {
            Self::Hash => Value::Hash(HashMap::new()),
            Self::List => Value::List(VecDeque::new()),
            Self::Set => Value::Set(HashSet::new()),
        }
    }
}

impl Value {
    fn is(&self, kind: Kind) -> bool {
        matches!(
            (self, kind),
            (Self::Hash(_), Kind::Hash) | (Self::List(_), Kind::List) | (Self::Set(_), Kind::Set)
        )
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Bytes(_) => false,
            Self::Hash(hash) => hash.is_empty(),
            Self::List(list) => list.is_empty(),
            Self::Set(set) => set.is_empty(),
        }
    }

    /// The accounted payload bytes, recomputed from scratch (tests only; the backend keeps
    /// `Entry::cost` up to date incrementally).
    #[cfg(test)]
    fn payload(&self) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::Hash(hash) => hash
                .iter()
                .map(|(field, value)| element_cost(field.len() + value.len()))
                .sum(),
            Self::List(list) => list.iter().map(|value| element_cost(value.len())).sum(),
            Self::Set(set) => set.iter().map(|value| element_cost(value.len())).sum(),
        }
    }
}

struct Entry {
    value: Value,
    /// Accounted bytes of this entry (key, payload and overheads), kept in lockstep with the
    /// site and global totals.
    cost: u64,
    expires_at: Option<u64>,
    last_access: u64,
    /// Position of this entry's key in the site's key vector, kept in lockstep by
    /// `insert_key`/`remove_key` so sampling is constant time.
    slot: usize,
}

type FlightKey = (SiteId, String);
type FlightOutcome = Result<Bytes, CacheError>;

/// One in-flight load: the first caller installed its loader, and every caller for the same
/// `(site, key)` cooperatively polls that one future, so it runs exactly once per flight.
struct Flight {
    inner: Mutex<FlightInner>,
    notify: Notify,
}

struct FlightInner {
    /// The loader while nobody polls it; `None` while some caller polls it or after it
    /// completed.
    loader: Option<BoxLoader>,
    /// The shared result, once a loader completed.
    outcome: Option<FlightOutcome>,
}

impl Flight {
    fn new(loader: BoxLoader) -> Self {
        Self {
            inner: Mutex::new(FlightInner {
                loader: Some(loader),
                outcome: None,
            }),
            notify: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, FlightInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Holds a loader taken out of its flight while it is being polled. On drop — a cancelled
/// caller — the loader returns to the flight and the waiting callers are woken so one of
/// them takes over.
struct PollGuard {
    flight: Arc<Flight>,
    loader: Option<BoxLoader>,
}

impl PollGuard {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<Bytes, Box<dyn Error + Send + Sync>>> {
        match self.loader.as_mut() {
            Some(loader) => {
                let polled = loader.as_mut().poll(cx);
                if polled.is_ready() {
                    self.loader = None;
                }
                polled
            }
            None => Poll::Pending,
        }
    }
}

impl Drop for PollGuard {
    fn drop(&mut self) {
        if let Some(loader) = self.loader.take() {
            let mut inner = self.flight.lock();
            if inner.outcome.is_none() {
                inner.loader = Some(loader);
            }
            drop(inner);
            self.flight.notify.notify_waiters();
        }
    }
}

fn entry_cost(key: &str, value: &[u8]) -> u64 {
    key.len() as u64 + value.len() as u64 + ENTRY_OVERHEAD_BYTES
}

/// Accounted cost of one hash field, list element or set member of `len` payload bytes.
fn element_cost(len: usize) -> u64 {
    len as u64 + ELEMENT_OVERHEAD_BYTES
}

/// Adds a signed change to an accounted byte counter.
fn adjust_bytes(counter: &mut u64, delta: i64) {
    if delta >= 0 {
        *counter += delta.unsigned_abs();
    } else {
        subtract_bytes(counter, delta.unsigned_abs());
    }
}

/// Resolves a Redis `start`/`stop` pair against a list of `len` elements into the inclusive
/// window `(first, last)`, or `None` when the window is empty.
fn window(len: usize, start: i64, stop: i64) -> Option<(usize, usize)> {
    let len = i64::try_from(len).unwrap_or(i64::MAX);
    let start = if start < 0 {
        (len + start).max(0)
    } else {
        start
    };
    let stop = if stop < 0 {
        len + stop
    } else {
        stop.min(len - 1)
    };
    if start > stop || start >= len {
        return None;
    }
    Some((usize::try_from(start).ok()?, usize::try_from(stop).ok()?))
}

fn duration_millis(ttl: Duration) -> u64 {
    u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX)
}

fn is_live(entry: &Entry, now: u64) -> bool {
    entry.expires_at.is_none_or(|expires_at| expires_at > now)
}

/// SplitMix64: a tiny, seeded, dependency-free PRNG for drawing eviction samples. The
/// stream is deterministic, which keeps eviction behaviour reproducible; only the sampled
/// victims' `last_access` values are compared, so the stream quality requirements are low.
fn next_sample(rng: &mut u64) -> u64 {
    *rng = rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *rng;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Decrements a byte counter; an underflow is an accounting bug and aborts loudly.
fn subtract_bytes(counter: &mut u64, cost: u64) {
    *counter = counter
        .checked_sub(cost)
        .unwrap_or_else(|| panic!("cache byte accounting underflow by {cost} bytes"));
}

fn parse_integer(value: &[u8]) -> CacheResult<i64> {
    let text = std::str::from_utf8(value).map_err(|_| CacheError::NotAnInteger)?;
    text.parse::<i64>().map_err(|_| CacheError::NotAnInteger)
}

impl InProcessCache {
    /// A cache on the real monotonic clock.
    pub fn new(config: InProcessConfig) -> Self {
        Self::with_clock(config, Arc::new(SystemClock::new()))
    }

    /// A cache on an injected clock; TTL tests use a [`spot_core::ManualClock`] and never sleep.
    pub fn with_clock(config: InProcessConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            config,
            clock,
            state: Mutex::new(State {
                sites: HashMap::new(),
                total_bytes: 0,
                writes: 0,
                sweep_cursor: 0,
                // Arbitrary non-zero seed; deterministic, so eviction behaviour is
                // reproducible run to run.
                sample_rng: 0x5EED_2024_11A7_A1B5,
            }),
            flights: Mutex::new(HashMap::new()),
        }
    }

    /// Monotonic milliseconds on the injected clock.
    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.monotonic().as_millis()).unwrap_or(u64::MAX)
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_flights(&self) -> MutexGuard<'_, HashMap<FlightKey, Arc<Flight>>> {
        self.flights.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn quota_for(&self, site: &SiteId) -> u64 {
        self.config
            .per_site_max_bytes
            .get(site)
            .copied()
            .unwrap_or(self.config.default_per_site_max_bytes)
    }

    // ---- core operations; all synchronous, one lock, never across an await ----

    fn get_sync(&self, site: &SiteId, key: &str) -> CacheResult<Option<Bytes>> {
        let mut state = self.lock_state();
        let now = self.now_ms();
        let State {
            sites, total_bytes, ..
        } = &mut *state;
        let Some(site_state) = sites.get_mut(site) else {
            return Ok(None);
        };
        let mut value = None;
        match site_state
            .entries
            .get(key)
            .is_some_and(|entry| is_live(entry, now))
        {
            true => {
                if let Some(entry) = site_state.entries.get_mut(key) {
                    match &entry.value {
                        Value::Bytes(bytes) => value = Some(bytes.clone()),
                        _ => return Err(CacheError::WrongType),
                    }
                    entry.last_access = now;
                }
                site_state.hits += 1;
            }
            false => {
                // Missing, or expired: an expired entry is removed on access.
                if site_state.remove_key(key, total_bytes).is_some() {
                    site_state.expired += 1;
                }
                site_state.misses += 1;
            }
        }
        Ok(value)
    }

    fn write_sync(
        &self,
        site: &SiteId,
        key: &str,
        value: Bytes,
        ttl: Option<Duration>,
        only_if_absent: bool,
        from_loader: bool,
    ) -> CacheResult<bool> {
        let mut state = self.lock_state();
        let now = self.now_ms();
        let State {
            sites,
            total_bytes,
            writes,
            sweep_cursor,
            sample_rng,
        } = &mut *state;
        *writes += 1;
        if *writes % SWEEP_INTERVAL_WRITES == 0 {
            sweep(sites, total_bytes, now, sweep_cursor);
        }
        let quota = self.quota_for(site);
        let site_state = sites
            .entry(site.clone())
            .or_insert_with(|| SiteState::new(quota));
        if only_if_absent {
            let occupied = site_state
                .entries
                .get(key)
                .is_some_and(|entry| is_live(entry, now));
            if occupied {
                return Ok(false);
            }
        }
        let cost = entry_cost(key, &value);
        if cost > site_state.quota || cost > self.config.max_bytes {
            return Err(CacheError::QuotaExceeded);
        }
        if let Some(old) = site_state.remove_key(key, total_bytes)
            && !is_live(&old, now)
        {
            site_state.expired += 1;
        }
        while site_state.bytes + cost > site_state.quota {
            if !evict_one_from_site(site_state, total_bytes, None, sample_rng) {
                break;
            }
        }
        site_state.insert_key(
            key,
            Entry {
                value: Value::Bytes(value),
                cost,
                expires_at: ttl.map(|ttl| now.saturating_add(duration_millis(ttl))),
                last_access: now,
                slot: 0,
            },
        );
        site_state.bytes += cost;
        *total_bytes += cost;
        if from_loader {
            site_state.loads += 1;
        }
        while *total_bytes > self.config.max_bytes {
            if !evict_one_global(sites, total_bytes, Some(site), sample_rng) {
                break;
            }
        }
        Ok(true)
    }

    fn delete_sync(&self, site: &SiteId, key: &str) -> bool {
        let mut state = self.lock_state();
        let now = self.now_ms();
        let State {
            sites, total_bytes, ..
        } = &mut *state;
        let Some(site_state) = sites.get_mut(site) else {
            return false;
        };
        match site_state.remove_key(key, total_bytes) {
            Some(entry) => {
                if is_live(&entry, now) {
                    true
                } else {
                    site_state.expired += 1;
                    false
                }
            }
            None => false,
        }
    }

    fn exists_sync(&self, site: &SiteId, key: &str) -> bool {
        let mut state = self.lock_state();
        let now = self.now_ms();
        let State {
            sites, total_bytes, ..
        } = &mut *state;
        let Some(site_state) = sites.get_mut(site) else {
            return false;
        };
        if site_state
            .entries
            .get(key)
            .is_some_and(|entry| is_live(entry, now))
        {
            if let Some(entry) = site_state.entries.get_mut(key) {
                entry.last_access = now;
            }
            true
        } else {
            if site_state.remove_key(key, total_bytes).is_some() {
                site_state.expired += 1;
            }
            false
        }
    }

    fn expire_sync(&self, site: &SiteId, key: &str, ttl: Duration) -> bool {
        let mut state = self.lock_state();
        let now = self.now_ms();
        let State {
            sites, total_bytes, ..
        } = &mut *state;
        let Some(site_state) = sites.get_mut(site) else {
            return false;
        };
        if !site_state
            .entries
            .get(key)
            .is_some_and(|entry| is_live(entry, now))
        {
            if site_state.remove_key(key, total_bytes).is_some() {
                site_state.expired += 1;
            }
            return false;
        }
        if let Some(entry) = site_state.entries.get_mut(key) {
            entry.expires_at = Some(now.saturating_add(duration_millis(ttl)));
        }
        true
    }

    fn ttl_sync(&self, site: &SiteId, key: &str) -> Ttl {
        let mut state = self.lock_state();
        let now = self.now_ms();
        let State {
            sites, total_bytes, ..
        } = &mut *state;
        let Some(site_state) = sites.get_mut(site) else {
            return Ttl::Missing;
        };
        if !site_state
            .entries
            .get(key)
            .is_some_and(|entry| is_live(entry, now))
        {
            if site_state.remove_key(key, total_bytes).is_some() {
                site_state.expired += 1;
            }
            return Ttl::Missing;
        }
        match site_state
            .entries
            .get(key)
            .and_then(|entry| entry.expires_at)
        {
            None => Ttl::Persistent,
            Some(expires_at) => Ttl::Remaining(Duration::from_millis(expires_at - now)),
        }
    }

    fn incr_sync(&self, site: &SiteId, key: &str, delta: i64) -> CacheResult<i64> {
        let mut state = self.lock_state();
        let now = self.now_ms();
        let State {
            sites,
            total_bytes,
            writes,
            sweep_cursor,
            sample_rng,
        } = &mut *state;
        *writes += 1;
        if *writes % SWEEP_INTERVAL_WRITES == 0 {
            sweep(sites, total_bytes, now, sweep_cursor);
        }
        let quota = self.quota_for(site);
        let site_state = sites
            .entry(site.clone())
            .or_insert_with(|| SiteState::new(quota));
        enum Current {
            Live(i64, Option<u64>, u64),
            Expired,
            Absent,
        }
        let current = match site_state.entries.get(key) {
            Some(entry) if is_live(entry, now) => {
                let Value::Bytes(bytes) = &entry.value else {
                    return Err(CacheError::WrongType);
                };
                Current::Live(parse_integer(bytes)?, entry.expires_at, entry.cost)
            }
            Some(_) => Current::Expired,
            None => Current::Absent,
        };
        let (base, old_cost, keep_expiry) = match current {
            Current::Live(parsed, expires_at, cost) => (parsed, cost, expires_at),
            Current::Expired => {
                if site_state.remove_key(key, total_bytes).is_some() {
                    site_state.expired += 1;
                }
                (0, 0, None)
            }
            Current::Absent => (0, 0, None),
        };
        let updated = base.checked_add(delta).ok_or(CacheError::Overflow)?;
        let text = updated.to_string();
        let cost = entry_cost(key, text.as_bytes());
        if cost > site_state.quota || cost > self.config.max_bytes {
            return Err(CacheError::QuotaExceeded);
        }
        while site_state.bytes.saturating_sub(old_cost) + cost > site_state.quota {
            if !evict_one_from_site(site_state, total_bytes, Some(key), sample_rng) {
                break;
            }
        }
        if old_cost > 0 {
            site_state.remove_key(key, total_bytes);
        }
        site_state.insert_key(
            key,
            Entry {
                value: Value::Bytes(Bytes::from(text.into_bytes())),
                cost,
                expires_at: keep_expiry,
                last_access: now,
                slot: 0,
            },
        );
        site_state.bytes += cost;
        *total_bytes += cost;
        while *total_bytes > self.config.max_bytes {
            if !evict_one_global(sites, total_bytes, Some(site), sample_rng) {
                break;
            }
        }
        Ok(updated)
    }

    // ---- structures: hashes, lists and sets --------------------------------------------

    /// Reads a live structure of `kind` through `read`; `Ok(None)` when the key is missing
    /// or expired (an expired key is removed), `WrongType` when it holds another type.
    /// Reads refresh the entry's recency but are not counted as hits or misses.
    fn read_structure<R>(
        &self,
        site: &SiteId,
        key: &str,
        kind: Kind,
        read: impl FnOnce(&Value) -> R,
    ) -> CacheResult<Option<R>> {
        let mut state = self.lock_state();
        let now = self.now_ms();
        let State {
            sites, total_bytes, ..
        } = &mut *state;
        let Some(site_state) = sites.get_mut(site) else {
            return Ok(None);
        };
        match site_state.entries.get_mut(key) {
            Some(entry) if is_live(entry, now) => {
                if !entry.value.is(kind) {
                    return Err(CacheError::WrongType);
                }
                entry.last_access = now;
                Ok(Some(read(&entry.value)))
            }
            Some(_) => {
                if site_state.remove_key(key, total_bytes).is_some() {
                    site_state.expired += 1;
                }
                Ok(None)
            }
            None => Ok(None),
        }
    }

    /// Changes a structure of `kind`.
    ///
    /// `growth` sees the current value (`None` when the key is absent) and returns an upper
    /// bound of the bytes the change adds — or the command's own error, such as `Overflow`;
    /// the write is refused with `QuotaExceeded` before anything changes when key plus
    /// growth cannot fit the site's quota or the global cap. `change` then mutates the value
    /// and reports its result and the exact signed payload-byte change. With `create` the
    /// key is made when absent; without it an absent key yields `Ok(None)` and nothing
    /// happens. A structure left empty is deleted, and the key's TTL is never touched.
    fn mutate_structure<R>(
        &self,
        site: &SiteId,
        key: &str,
        kind: Kind,
        create: bool,
        growth: impl FnOnce(Option<&Value>) -> CacheResult<u64>,
        change: impl FnOnce(&mut Value) -> (R, i64),
    ) -> CacheResult<Option<R>> {
        let mut state = self.lock_state();
        let now = self.now_ms();
        let State {
            sites,
            total_bytes,
            writes,
            sweep_cursor,
            sample_rng,
        } = &mut *state;
        *writes += 1;
        if *writes % SWEEP_INTERVAL_WRITES == 0 {
            sweep(sites, total_bytes, now, sweep_cursor);
        }
        let quota = self.quota_for(site);
        let site_state = sites
            .entry(site.clone())
            .or_insert_with(|| SiteState::new(quota));
        if site_state
            .entries
            .get(key)
            .is_some_and(|entry| !is_live(entry, now))
            && site_state.remove_key(key, total_bytes).is_some()
        {
            site_state.expired += 1;
        }
        let existing = site_state.entries.get(key);
        if existing.is_some_and(|entry| !entry.value.is(kind)) {
            return Err(CacheError::WrongType);
        }
        if existing.is_none() && !create {
            return Ok(None);
        }
        let grow = growth(existing.map(|entry| &entry.value))?;
        let old_cost = existing.map_or(0, |entry| entry.cost);
        let base_cost = existing.map_or(entry_cost(key, &[]), |entry| entry.cost);
        let projected = base_cost.saturating_add(grow);
        if projected > site_state.quota || projected > self.config.max_bytes {
            return Err(CacheError::QuotaExceeded);
        }
        while site_state.bytes.saturating_sub(old_cost) + projected > site_state.quota {
            if !evict_one_from_site(site_state, total_bytes, Some(key), sample_rng) {
                break;
            }
        }
        if !site_state.entries.contains_key(key) {
            let cost = entry_cost(key, &[]);
            site_state.insert_key(
                key,
                Entry {
                    value: kind.empty(),
                    cost,
                    expires_at: None,
                    last_access: now,
                    slot: 0,
                },
            );
            site_state.bytes += cost;
            *total_bytes += cost;
        }
        let Some(entry) = site_state.entries.get_mut(key) else {
            return Ok(None);
        };
        let (result, delta) = change(&mut entry.value);
        entry.last_access = now;
        adjust_bytes(&mut entry.cost, delta);
        let emptied = entry.value.is_empty();
        adjust_bytes(&mut site_state.bytes, delta);
        adjust_bytes(total_bytes, delta);
        if emptied {
            site_state.remove_key(key, total_bytes);
        }
        while *total_bytes > self.config.max_bytes {
            if !evict_one_global(sites, total_bytes, Some(site), sample_rng) {
                break;
            }
        }
        Ok(Some(result))
    }

    fn hset_sync(&self, site: &SiteId, key: &str, field: &str, value: Bytes) -> CacheResult<bool> {
        let new_len = value.len();
        let grow = |current: Option<&Value>| {
            let previous = match current {
                Some(Value::Hash(hash)) => hash.get(field).map(|old| old.len()),
                _ => None,
            };
            Ok(match previous {
                Some(old) => new_len.saturating_sub(old) as u64,
                None => element_cost(field.len() + new_len),
            })
        };
        let change = move |current: &mut Value| {
            let Value::Hash(hash) = current else {
                return (false, 0);
            };
            match hash.insert(field.to_owned(), value) {
                Some(old) => (false, new_len as i64 - old.len() as i64),
                None => (true, element_cost(field.len() + new_len) as i64),
            }
        };
        let created = self.mutate_structure(site, key, Kind::Hash, true, grow, change)?;
        Ok(created.unwrap_or(false))
    }

    fn hincr_sync(&self, site: &SiteId, key: &str, field: &str, delta: i64) -> CacheResult<i64> {
        // The sum is worked out under the write lock, so concurrent increments never lose
        // one. `growth` runs first and rejects a bad field or an overflow before anything
        // changes; `change` then repeats the same, now infallible, computation.
        let sum = |current: Option<&Value>| -> CacheResult<i64> {
            let base = match current {
                Some(Value::Hash(hash)) => match hash.get(field) {
                    Some(text) => parse_integer(text)?,
                    None => 0,
                },
                _ => 0,
            };
            base.checked_add(delta).ok_or(CacheError::Overflow)
        };
        let grow = |current: Option<&Value>| {
            let text_len = sum(current)?.to_string().len();
            Ok(element_cost(field.len() + text_len))
        };
        let change = |current: &mut Value| {
            let updated = sum(Some(current)).unwrap_or_default();
            let text = Bytes::from(updated.to_string().into_bytes());
            let text_len = text.len();
            let Value::Hash(hash) = current else {
                return (updated, 0);
            };
            let bytes = match hash.insert(field.to_owned(), text) {
                Some(old) => text_len as i64 - old.len() as i64,
                None => element_cost(field.len() + text_len) as i64,
            };
            (updated, bytes)
        };
        let updated = self.mutate_structure(site, key, Kind::Hash, true, grow, change)?;
        Ok(updated.unwrap_or_default())
    }

    fn push_sync(
        &self,
        site: &SiteId,
        key: &str,
        values: &[Bytes],
        at_head: bool,
    ) -> CacheResult<u64> {
        if values.is_empty() {
            let length = self.read_structure(site, key, Kind::List, |value| match value {
                Value::List(list) => list.len() as u64,
                _ => 0,
            })?;
            return Ok(length.unwrap_or(0));
        }
        let added: u64 = values.iter().map(|value| element_cost(value.len())).sum();
        let change = |current: &mut Value| {
            let Value::List(list) = current else {
                return (0, 0);
            };
            for value in values {
                if at_head {
                    list.push_front(value.clone());
                } else {
                    list.push_back(value.clone());
                }
            }
            (list.len() as u64, added as i64)
        };
        let length = self.mutate_structure(site, key, Kind::List, true, |_| Ok(added), change)?;
        Ok(length.unwrap_or(0))
    }

    fn pop_sync(&self, site: &SiteId, key: &str, from_head: bool) -> CacheResult<Option<Bytes>> {
        let change = |current: &mut Value| {
            let Value::List(list) = current else {
                return (None, 0);
            };
            let popped = if from_head {
                list.pop_front()
            } else {
                list.pop_back()
            };
            let freed = popped
                .as_ref()
                .map_or(0, |value| element_cost(value.len()) as i64);
            (popped, -freed)
        };
        let popped = self.mutate_structure(site, key, Kind::List, false, |_| Ok(0), change)?;
        Ok(popped.flatten())
    }

    fn lrange_sync(
        &self,
        site: &SiteId,
        key: &str,
        start: i64,
        stop: i64,
    ) -> CacheResult<Vec<Bytes>> {
        let range = self.read_structure(site, key, Kind::List, |value| match value {
            Value::List(list) => match window(list.len(), start, stop) {
                Some((first, last)) => list.range(first..=last).cloned().collect(),
                None => Vec::new(),
            },
            _ => Vec::new(),
        })?;
        Ok(range.unwrap_or_default())
    }

    fn ltrim_sync(&self, site: &SiteId, key: &str, start: i64, stop: i64) -> CacheResult<()> {
        let change = |current: &mut Value| {
            let Value::List(list) = current else {
                return ((), 0);
            };
            let before: u64 = list.iter().map(|value| element_cost(value.len())).sum();
            match window(list.len(), start, stop) {
                Some((first, last)) => {
                    list.truncate(last + 1);
                    list.drain(..first);
                }
                None => list.clear(),
            }
            let after: u64 = list.iter().map(|value| element_cost(value.len())).sum();
            ((), after as i64 - before as i64)
        };
        self.mutate_structure(site, key, Kind::List, false, |_| Ok(0), change)?;
        Ok(())
    }

    fn sadd_sync(&self, site: &SiteId, key: &str, members: &[Bytes]) -> CacheResult<u64> {
        // Members already present, or repeated in `members`, add no bytes.
        let grow = |current: Option<&Value>| {
            let mut seen: HashSet<&[u8]> = HashSet::new();
            let mut bytes = 0;
            for member in members {
                let present = match current {
                    Some(Value::Set(set)) => set.contains(&**member),
                    _ => false,
                };
                if !present && seen.insert(&**member) {
                    bytes += element_cost(member.len());
                }
            }
            Ok(bytes)
        };
        let change = |current: &mut Value| {
            let Value::Set(set) = current else {
                return (0, 0);
            };
            let mut added = 0u64;
            let mut bytes = 0i64;
            for member in members {
                if set.insert(member.clone()) {
                    added += 1;
                    bytes += element_cost(member.len()) as i64;
                }
            }
            (added, bytes)
        };
        let added = self.mutate_structure(site, key, Kind::Set, true, grow, change)?;
        Ok(added.unwrap_or(0))
    }

    fn srem_sync(&self, site: &SiteId, key: &str, members: &[Bytes]) -> CacheResult<u64> {
        let change = |current: &mut Value| {
            let Value::Set(set) = current else {
                return (0, 0);
            };
            let mut removed = 0u64;
            let mut freed = 0i64;
            for member in members {
                if set.remove(member) {
                    removed += 1;
                    freed += element_cost(member.len()) as i64;
                }
            }
            (removed, -freed)
        };
        let removed = self.mutate_structure(site, key, Kind::Set, false, |_| Ok(0), change)?;
        Ok(removed.unwrap_or(0))
    }

    fn hdel_sync(&self, site: &SiteId, key: &str, fields: &[&str]) -> CacheResult<u64> {
        let change = |current: &mut Value| {
            let Value::Hash(hash) = current else {
                return (0, 0);
            };
            let mut removed = 0u64;
            let mut freed = 0i64;
            for field in fields {
                if let Some(old) = hash.remove(*field) {
                    removed += 1;
                    freed += element_cost(field.len() + old.len()) as i64;
                }
            }
            (removed, -freed)
        };
        let removed = self.mutate_structure(site, key, Kind::Hash, false, |_| Ok(0), change)?;
        Ok(removed.unwrap_or(0))
    }

    fn delete_prefix_sync(&self, site: &SiteId, prefix: &str) -> u64 {
        let mut state = self.lock_state();
        let now = self.now_ms();
        let State {
            sites, total_bytes, ..
        } = &mut *state;
        let Some(site_state) = sites.get_mut(site) else {
            return 0;
        };
        let doomed: Vec<String> = site_state
            .entries
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect();
        let mut removed = 0;
        for key in doomed {
            if let Some(entry) = site_state.remove_key(&key, total_bytes) {
                if is_live(&entry, now) {
                    removed += 1;
                } else {
                    site_state.expired += 1;
                }
            }
        }
        removed
    }

    fn clear_site_sync(&self, site: &SiteId) {
        let mut state = self.lock_state();
        let State {
            sites, total_bytes, ..
        } = &mut *state;
        if let Some(site_state) = sites.get_mut(site) {
            subtract_bytes(total_bytes, site_state.bytes);
            site_state.bytes = 0;
            site_state.entries.clear();
            site_state.keys.clear();
        }
    }

    fn stats_sync(&self, site: &SiteId) -> CacheStats {
        let state = self.lock_state();
        match state.sites.get(site) {
            Some(site_state) => CacheStats {
                hits: site_state.hits,
                misses: site_state.misses,
                loads: site_state.loads,
                evictions: site_state.evictions,
                expired: site_state.expired,
                entries: site_state.entries.len() as u64,
                bytes: site_state.bytes,
            },
            None => CacheStats::default(),
        }
    }

    fn finish_flight(&self, key: &FlightKey, flight: &Arc<Flight>, outcome: FlightOutcome) {
        {
            let mut inner = flight.lock();
            inner.outcome = Some(outcome);
            inner.loader = None;
        }
        self.remove_flight(key, flight);
        flight.notify.notify_waiters();
    }

    fn remove_flight(&self, key: &FlightKey, flight: &Arc<Flight>) {
        let mut flights = self.lock_flights();
        if matches!(flights.get(key), Some(existing) if Arc::ptr_eq(existing, flight)) {
            flights.remove(key);
        }
    }
}

/// Removes expired entries from a bounded slice of the cache (amortised sweep): one site
/// per call — chosen by rotating through the sites — examined from a rotating window of at
/// most [`SWEEP_SCAN_BUDGET`] entries. Every call is bounded, however large the cache is;
/// taken together the calls still reach every entry of every site, and entries are
/// additionally removed lazily on access.
fn sweep(
    sites: &mut HashMap<SiteId, SiteState>,
    total_bytes: &mut u64,
    now: u64,
    sweep_cursor: &mut u64,
) {
    let site_count = sites.len();
    if site_count == 0 {
        return;
    }
    let names: Vec<SiteId> = sites.keys().cloned().collect();
    let index = (*sweep_cursor % site_count as u64) as usize;
    *sweep_cursor = sweep_cursor.wrapping_add(1);
    if let Some(site_state) = sites.get_mut(&names[index]) {
        site_state.sweep_expired(total_bytes, now, SWEEP_SCAN_BUDGET);
    }
}

/// Evicts one entry of `site_state` — sampled LRU, optionally excluding one key — and
/// reports whether an entry was evicted. The victim is the least recently used of
/// [`EVICTION_SAMPLE`] entries drawn uniformly from the whole pool (Redis
/// `dictGetSomeKeys`/`evictionPoolPopulate` style): the per-site key vector plus a
/// seeded PRNG (`next_sample`) keep every eviction bounded at [`EVICTION_SAMPLE`] draws
/// however large the pool, and uniform draws reach every part of the pool over time. A
/// frequently touched working set is young, and a young entry is the stalest of a uniform
/// sample only when every co-sampled entry is younger still, so hot keys survive heavy
/// churn while cold entries are reclaimed. Pools of at most [`EVICTION_SAMPLE`] entries
/// are sampled in full, keeping exact LRU.
///
/// When every draw lands on the excluded key (possible once the pool is larger than the
/// sample), the sample is redrawn once over the positions other than the excluded key's,
/// so a site holding any other entry always yields a victim and a write never leaves the
/// site over its quota for want of one.
fn evict_one_from_site(
    site_state: &mut SiteState,
    total_bytes: &mut u64,
    exclude: Option<&str>,
    rng: &mut u64,
) -> bool {
    evict_sampled(site_state, total_bytes, exclude, rng, EVICTION_SAMPLE)
}

/// [`evict_one_from_site`] with the sample size as a parameter, so tests can force the
/// all-draws-excluded redraw on a tiny pool.
fn evict_sampled(
    site_state: &mut SiteState,
    total_bytes: &mut u64,
    exclude: Option<&str>,
    rng: &mut u64,
    sample_size: usize,
) -> bool {
    let len = site_state.keys.len();
    if len == 0 {
        return false;
    }
    // The sample: whole pool when it is small enough (exact LRU); otherwise
    // `sample_size` uniform pseudo-random positions of the key vector. A duplicate draw
    // only forfeits one sample slot; it never skews the choice.
    let sample: Vec<usize> = if len <= sample_size {
        (0..len).collect()
    } else {
        (0..sample_size)
            .map(|_| (next_sample(rng) % len as u64) as usize)
            .collect()
    };
    let mut best = stalest_of(site_state, sample, exclude);
    if best.is_none()
        && len > sample_size
        && let Some(excluded_slot) = exclude
            .and_then(|excluded| site_state.entries.get(excluded))
            .map(|entry| entry.slot)
    {
        // Every draw hit the excluded key. Redraw once, uniformly over the other `len - 1`
        // positions (a draw at or past the excluded slot shifts up by one), so the redraw
        // cannot hit it again.
        let others = (len - 1) as u64;
        let redraw: Vec<usize> = (0..sample_size)
            .map(|_| {
                let position = (next_sample(rng) % others) as usize;
                if position >= excluded_slot {
                    position + 1
                } else {
                    position
                }
            })
            .collect();
        best = stalest_of(site_state, redraw, exclude);
    }
    let Some(slot) = best else {
        return false;
    };
    let key = site_state.keys[slot].clone();
    if site_state.remove_key(&key, total_bytes).is_some() {
        site_state.evictions += 1;
        true
    } else {
        false
    }
}

/// The slot of the least recently used entry among `sample`, skipping `exclude`.
fn stalest_of(
    site_state: &SiteState,
    sample: impl IntoIterator<Item = usize>,
    exclude: Option<&str>,
) -> Option<usize> {
    let mut best: Option<(usize, u64)> = None; // (slot, last_access): the stalest seen.
    for slot in sample {
        let Some(key) = site_state.keys.get(slot) else {
            continue;
        };
        if exclude.is_some_and(|excluded| excluded == key.as_str()) {
            continue;
        }
        // The key-vector invariant guarantees an entry exists; skip defensively if not.
        let Some(entry) = site_state.entries.get(key) else {
            continue;
        };
        if best.is_none_or(|(_, best_access)| entry.last_access < best_access) {
            best = Some((slot, entry.last_access));
        }
    }
    best.map(|(slot, _)| slot)
}

/// Compares two `bytes / quota` fractions exactly: `(a_bytes * b_quota)` against
/// `(b_bytes * a_quota)` in wide integers, never in floating point. A site with a zero
/// quota but accounted bytes ranks as infinitely saturated.
fn ratio_cmp(a_bytes: u64, a_quota: u64, b_bytes: u64, b_quota: u64) -> Ordering {
    (a_bytes as u128 * b_quota as u128).cmp(&(b_bytes as u128 * a_quota as u128))
}

/// Evicts one entry across all sites (global pressure) from the most saturated site:
///
/// 1. a site strictly over its own quota donates first, if one exists;
/// 2. otherwise the site whose accounted bytes are the largest fraction of its own quota
///    (`bytes / quota`, compared exactly by [`ratio_cmp`]) donates — a site under its quota
///    donates only when the site quotas oversubscribe `max_bytes`, and then because it is
///    the most saturated site left. The writing site wins an exact tie.
///
/// The donor scan is O(sites) per eviction — bounded by the site count, not the entries;
/// see [`InProcessConfig`] for the quota layout. Within the chosen site the victim entry
/// is sampled LRU ([`evict_one_from_site`]).
fn evict_one_global(
    sites: &mut HashMap<SiteId, SiteState>,
    total_bytes: &mut u64,
    writer: Option<&SiteId>,
    rng: &mut u64,
) -> bool {
    let mut donor: Option<(SiteId, bool, u64, u64)> = None;
    for (name, site_state) in sites.iter() {
        if site_state.keys.is_empty() {
            continue;
        }
        let over_quota = site_state.bytes > site_state.quota;
        let writer_here = writer.is_some_and(|writer| writer == name);
        let take = match donor {
            None => true,
            Some((_, best_over, best_bytes, best_quota)) => {
                if over_quota != best_over {
                    over_quota
                } else {
                    match ratio_cmp(site_state.bytes, site_state.quota, best_bytes, best_quota) {
                        Ordering::Greater => true,
                        Ordering::Equal => writer_here,
                        Ordering::Less => false,
                    }
                }
            }
        };
        if take {
            donor = Some((name.clone(), over_quota, site_state.bytes, site_state.quota));
        }
    }
    let Some((name, _, _, _)) = donor else {
        return false;
    };
    let Some(site_state) = sites.get_mut(&name) else {
        return false;
    };
    evict_one_from_site(site_state, total_bytes, None, rng)
}

impl CacheStore for InProcessCache {
    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        Box::pin(async move { self.get_sync(site, key) })
    }

    fn set<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> CacheFuture<'a, CacheResult<()>> {
        Box::pin(async move {
            self.write_sync(site, key, value, ttl, false, false)
                .map(|_| ())
        })
    }

    fn set_nx<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        value: Bytes,
        ttl: Option<Duration>,
    ) -> CacheFuture<'a, CacheResult<bool>> {
        Box::pin(async move { self.write_sync(site, key, value, ttl, true, false) })
    }

    fn delete<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, bool> {
        Box::pin(async move { self.delete_sync(site, key) })
    }

    fn exists<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, bool> {
        Box::pin(async move { self.exists_sync(site, key) })
    }

    fn expire<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        ttl: Duration,
    ) -> CacheFuture<'a, bool> {
        Box::pin(async move { self.expire_sync(site, key, ttl) })
    }

    fn ttl<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, Ttl> {
        Box::pin(async move { self.ttl_sync(site, key) })
    }

    fn incr_by<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        delta: i64,
    ) -> CacheFuture<'a, CacheResult<i64>> {
        Box::pin(async move { self.incr_sync(site, key, delta) })
    }

    fn delete_prefix<'a>(&'a self, site: &'a SiteId, prefix: &'a str) -> CacheFuture<'a, u64> {
        Box::pin(async move { self.delete_prefix_sync(site, prefix) })
    }

    fn clear_site<'a>(&'a self, site: &'a SiteId) -> CacheFuture<'a, ()> {
        Box::pin(async move { self.clear_site_sync(site) })
    }

    fn get_or_load<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        ttl: Option<Duration>,
        loader: BoxLoader,
    ) -> CacheFuture<'a, CacheResult<Bytes>> {
        Box::pin(async move {
            // Join or create the flight BEFORE the cache lookup, so exactly one loader runs
            // per generation of the key no matter how the lookups interleave.
            let flight_key: FlightKey = (site.clone(), key.to_string());
            let (flight, created) = {
                let mut flights = self.lock_flights();
                match flights.get(&flight_key) {
                    Some(existing) => (existing.clone(), false),
                    None => {
                        let flight = Arc::new(Flight::new(loader));
                        flights.insert(flight_key.clone(), flight.clone());
                        (flight, true)
                    }
                }
            };
            let cached = match self.get_sync(site, key) {
                Ok(cached) => cached,
                Err(error) => {
                    if created {
                        self.finish_flight(&flight_key, &flight, Err(error.clone()));
                    }
                    return Err(error);
                }
            };
            if let Some(bytes) = cached {
                if created {
                    // The value was already cached: resolve the flight without running the
                    // loader (a joining caller may still hold it in a poll guard).
                    self.finish_flight(&flight_key, &flight, Ok(bytes.clone()));
                }
                return Ok(bytes);
            }
            loop {
                let notified = flight.notify.notified();
                let mut notified = std::pin::pin!(notified);
                notified.as_mut().enable();
                enum Step {
                    Done(CacheResult<Bytes>),
                    Poll(BoxLoader),
                    Wait,
                }
                let step = {
                    let mut inner = flight.lock();
                    match &inner.outcome {
                        Some(outcome) => Step::Done(outcome.clone()),
                        None => match inner.loader.take() {
                            Some(loader) => Step::Poll(loader),
                            None => Step::Wait,
                        },
                    }
                };
                match step {
                    Step::Done(outcome) => return outcome,
                    Step::Wait => notified.await,
                    Step::Poll(loader) => {
                        let mut guard = PollGuard {
                            flight: flight.clone(),
                            loader: Some(loader),
                        };
                        let loaded = std::future::poll_fn(|cx| guard.poll(cx)).await;
                        let outcome = match loaded {
                            Ok(bytes) => {
                                match self.write_sync(site, key, bytes.clone(), ttl, false, true) {
                                    Ok(_) => Ok(bytes),
                                    Err(error) => Err(error),
                                }
                            }
                            Err(error) => Err(CacheError::Loader(Arc::from(error))),
                        };
                        {
                            let mut inner = flight.lock();
                            inner.outcome = Some(outcome.clone());
                            inner.loader = None;
                        }
                        self.remove_flight(&flight_key, &flight);
                        flight.notify.notify_waiters();
                        return outcome;
                    }
                }
            }
        })
    }

    fn stats<'a>(&'a self, site: &'a SiteId) -> CacheFuture<'a, CacheStats> {
        Box::pin(async move { self.stats_sync(site) })
    }

    fn hset<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
        value: Bytes,
    ) -> CacheFuture<'a, CacheResult<bool>> {
        Box::pin(async move { self.hset_sync(site, key, field, value) })
    }

    fn hget<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        Box::pin(async move {
            let found = self.read_structure(site, key, Kind::Hash, |value| match value {
                Value::Hash(hash) => hash.get(field).cloned(),
                _ => None,
            })?;
            Ok(found.flatten())
        })
    }

    fn hdel<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        fields: &'a [&'a str],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move { self.hdel_sync(site, key, fields) })
    }

    fn hgetall<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Vec<(String, Bytes)>>> {
        Box::pin(async move {
            let all = self.read_structure(site, key, Kind::Hash, |value| match value {
                Value::Hash(hash) => hash
                    .iter()
                    .map(|(field, value)| (field.clone(), value.clone()))
                    .collect(),
                _ => Vec::new(),
            })?;
            Ok(all.unwrap_or_default())
        })
    }

    fn hexists<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
    ) -> CacheFuture<'a, CacheResult<bool>> {
        Box::pin(async move {
            let found = self.read_structure(
                site,
                key,
                Kind::Hash,
                |value| matches!(value, Value::Hash(hash) if hash.contains_key(field)),
            )?;
            Ok(found.unwrap_or(false))
        })
    }

    fn hlen<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move {
            let len = self.read_structure(site, key, Kind::Hash, |value| match value {
                Value::Hash(hash) => hash.len() as u64,
                _ => 0,
            })?;
            Ok(len.unwrap_or(0))
        })
    }

    fn hincr_by<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        field: &'a str,
        delta: i64,
    ) -> CacheFuture<'a, CacheResult<i64>> {
        Box::pin(async move { self.hincr_sync(site, key, field, delta) })
    }

    fn lpush<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        values: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move { self.push_sync(site, key, values, true) })
    }

    fn rpush<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        values: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move { self.push_sync(site, key, values, false) })
    }

    fn lpop<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        Box::pin(async move { self.pop_sync(site, key, true) })
    }

    fn rpop<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Option<Bytes>>> {
        Box::pin(async move { self.pop_sync(site, key, false) })
    }

    fn lrange<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        start: i64,
        stop: i64,
    ) -> CacheFuture<'a, CacheResult<Vec<Bytes>>> {
        Box::pin(async move { self.lrange_sync(site, key, start, stop) })
    }

    fn llen<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move {
            let len = self.read_structure(site, key, Kind::List, |value| match value {
                Value::List(list) => list.len() as u64,
                _ => 0,
            })?;
            Ok(len.unwrap_or(0))
        })
    }

    fn ltrim<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        start: i64,
        stop: i64,
    ) -> CacheFuture<'a, CacheResult<()>> {
        Box::pin(async move { self.ltrim_sync(site, key, start, stop) })
    }

    fn sadd<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        members: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move { self.sadd_sync(site, key, members) })
    }

    fn srem<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        members: &'a [Bytes],
    ) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move { self.srem_sync(site, key, members) })
    }

    fn smembers<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
    ) -> CacheFuture<'a, CacheResult<Vec<Bytes>>> {
        Box::pin(async move {
            let all = self.read_structure(site, key, Kind::Set, |value| match value {
                Value::Set(set) => set.iter().cloned().collect(),
                _ => Vec::new(),
            })?;
            Ok(all.unwrap_or_default())
        })
    }

    fn sismember<'a>(
        &'a self,
        site: &'a SiteId,
        key: &'a str,
        member: &'a [u8],
    ) -> CacheFuture<'a, CacheResult<bool>> {
        Box::pin(async move {
            let found = self.read_structure(
                site,
                key,
                Kind::Set,
                |value| matches!(value, Value::Set(set) if set.contains(member)),
            )?;
            Ok(found.unwrap_or(false))
        })
    }

    fn scard<'a>(&'a self, site: &'a SiteId, key: &'a str) -> CacheFuture<'a, CacheResult<u64>> {
        Box::pin(async move {
            let len = self.read_structure(site, key, Kind::Set, |value| match value {
                Value::Set(set) => set.len() as u64,
                _ => 0,
            })?;
            Ok(len.unwrap_or(0))
        })
    }
}

#[cfg(test)]
mod invariant_probes {
    use super::*;

    fn fresh_entry(now: u64) -> Entry {
        Entry {
            value: Value::Bytes(Bytes::from(vec![b'x'; 16])),
            cost: 0,
            expires_at: None,
            last_access: now,
            slot: 0,
        }
    }

    #[test]
    fn keys_vector_stays_consistent_under_eviction_churn() {
        let mut site = SiteState::new(1_000);
        let mut total = 0u64;
        let mut rng = 0x5EED_2024u64;
        for i in 0..200u64 {
            // Keys repeat, so rewrites go through `remove_key`, which accounts the freed
            // bytes itself.
            let key = format!("k{:04}", i % 40);
            let cost = entry_cost(&key, &[b'x'; 16]);
            site.remove_key(&key, &mut total);
            while site.bytes + cost > site.quota {
                assert!(
                    evict_one_from_site(&mut site, &mut total, None, &mut rng),
                    "no evictable victim even though the site is at quota"
                );
            }
            let mut entry = fresh_entry(i);
            entry.cost = cost;
            site.insert_key(&key, entry);
            site.bytes += cost;
            total += cost;
            site.assert_invariant();
            assert_eq!(site.bytes, total, "site and total byte counters diverged");
            let live: u64 = site.entries.values().map(|entry| entry.cost).sum();
            assert_eq!(site.bytes, live, "site bytes drifted from live entries");
        }
    }

    #[test]
    fn eviction_redraws_when_every_draw_hits_the_excluded_key() {
        // A tiny pool of two keys sampled one draw at a time: find a seed whose first draw
        // lands on the excluded key, so the first sample holds nothing evictable.
        let mut site = SiteState::new(1_000);
        let mut total = 0u64;
        for (i, key) in ["hot", "cold"].into_iter().enumerate() {
            let cost = entry_cost(key, &[b'x'; 16]);
            let mut entry = fresh_entry(i as u64);
            entry.cost = cost;
            site.insert_key(key, entry);
            site.bytes += cost;
            total += cost;
        }
        let excluded_slot = site.entries["hot"].slot as u64;
        let mut rng = (0u64..)
            .find(|seed| next_sample(&mut seed.clone()) % 2 == excluded_slot)
            .unwrap_or_else(|| panic!("no seed draws the excluded slot"));
        assert!(
            evict_sampled(&mut site, &mut total, Some("hot"), &mut rng, 1),
            "gave up although another key was evictable"
        );
        assert!(site.entries.contains_key("hot"), "evicted the excluded key");
        assert!(!site.entries.contains_key("cold"), "the other key survived");
        assert_eq!(site.evictions, 1);
        site.assert_invariant();
        // Only the excluded key is left: nothing is evictable, and the redraw must not spin.
        assert!(!evict_sampled(
            &mut site,
            &mut total,
            Some("hot"),
            &mut rng,
            1
        ));
    }
}

#[cfg(test)]
mod structure_accounting {
    use super::*;
    use spot_core::ManualClock;
    use time::OffsetDateTime;

    fn check(cache: &InProcessCache) {
        let state = cache.lock_state();
        let mut total = 0;
        for site_state in state.sites.values() {
            site_state.assert_invariant();
            assert!(site_state.bytes <= site_state.quota, "site over quota");
            total += site_state.bytes;
        }
        assert_eq!(
            state.total_bytes, total,
            "global bytes drifted from the sites"
        );
        assert!(state.total_bytes <= cache.config.max_bytes);
    }

    /// A long pseudo-random command mix under a tight quota, so evictions, refused writes,
    /// expiry and type changes all happen; after every command the byte accounting must equal
    /// what the contents say.
    #[tokio::test]
    async fn accounting_stays_exact_under_a_random_command_mix() {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let cache = InProcessCache::with_clock(InProcessConfig::new(3_000, 1_500), clock.clone());
        let sites = [
            SiteId::new("a.test").unwrap_or_else(|_| unreachable!()),
            SiteId::new("b.test").unwrap_or_else(|_| unreachable!()),
        ];
        let mut rng = 0xC0FF_EE11_u64;
        let mut pick = |n: u64| next_sample(&mut rng) % n;
        for _ in 0..4_000 {
            let site = &sites[pick(2) as usize];
            let key = format!("k{}", pick(6));
            let member = |n: u64| Bytes::from(vec![b'm'; (n % 9) as usize]);
            let value = member(pick(20));
            match pick(20) {
                0 => drop(
                    cache
                        .hset(site, &key, &format!("f{}", pick(4)), value)
                        .await,
                ),
                1 => drop(
                    cache
                        .hincr_by(site, &key, &format!("n{}", pick(3)), pick(9) as i64 - 4)
                        .await,
                ),
                2 => drop(cache.hdel(site, &key, &["f0", "f1", "n0"]).await),
                3 | 4 => drop(cache.rpush(site, &key, &[value.clone(), member(3)]).await),
                5 => drop(cache.lpush(site, &key, &[value]).await),
                6 => drop(cache.lpop(site, &key).await),
                7 => drop(cache.rpop(site, &key).await),
                8 => drop(
                    cache
                        .ltrim(site, &key, pick(3) as i64, pick(5) as i64 - 2)
                        .await,
                ),
                9 | 10 => drop(cache.sadd(site, &key, &[value, member(2), member(5)]).await),
                11 => drop(cache.srem(site, &key, &[member(2), member(5)]).await),
                12 => drop(cache.set(site, &key, value, None).await),
                13 => drop(cache.incr_by(site, &key, 3).await),
                14 => drop(
                    cache
                        .expire(site, &key, Duration::from_millis(pick(30)))
                        .await,
                ),
                15 => drop(cache.delete(site, &key).await),
                16 => clock.advance(Duration::from_millis(pick(15))),
                17 => drop(cache.get(site, &key).await),
                18 => drop(cache.delete_prefix(site, "k1").await),
                _ => drop(cache.hgetall(site, &key).await),
            }
            check(&cache);
        }
    }
}
