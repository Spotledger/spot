//! The database-polling backend: [`VersionStore`], an in-memory [`MemoryVersionStore`], and
//! [`PollingBus`].
//!
//! A multi-node deployment does not broadcast in memory — it writes the site's metadata version
//! and hash to storage, and every node reads them on its own schedule. [`VersionStore`] is the
//! small seam for that; [`MemoryVersionStore`] is the fake the tests and a single-process start
//! use, and [`crate::StorageVersionStore`] (feature `storage`) is the real document-backed one.
//!
//! No timer lives here: [`PollSchedule::due`] answers "should the host poll now?" from the
//! injected [`Clock`], and the host calls [`PollingBus::poll_once`] /
//! [`Follower::poll_once`](crate::Follower::poll_once) when it says yes.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use spot_core::{Clock, SiteId};

use crate::bus::{BusFuture, InProcessBus, InvalidationBus, InvalidationStream};
use crate::error::LiveResult;
use crate::invalidation::{Invalidation, InvalidationKind};
use crate::snapshot::{Definitions, MetaHash, Version};

/// The future every [`VersionStore`] command returns. The trait stays object-safe
/// (`Arc<dyn VersionStore>`), so a database backend's async work is erased behind a boxed
/// future.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = LiveResult<T>> + Send + 'a>>;

/// The site's metadata version and hash, as one small record a database can hold.
///
/// A `None` read means "nothing published yet". `write` replaces the head; it never goes
/// backwards in practice because [`crate::LiveMeta`] only mints rising versions, but a store is
/// free to accept whatever it is given — the follower is what refuses to go backwards.
pub trait VersionStore: Send + Sync {
    /// The current `(version, hash)`, or `None` when nothing has been published.
    fn read<'a>(&'a self) -> StoreFuture<'a, Option<(Version, MetaHash)>>;

    /// Records `version` and `hash` as the current head.
    fn write<'a>(&'a self, version: Version, hash: MetaHash) -> StoreFuture<'a, ()>;
}

/// An in-memory [`VersionStore`] and `MetaLoader`: the fake for tests and single-process starts.
///
/// It holds the head `(version, hash)` and, keyed by version, the definitions a
/// [`MetaLoader`](crate::MetaLoader) hands out. Real deployments keep the two in one database
/// record; this fake keeps them together too, so a test wires one `Arc<MemoryVersionStore>` into
/// both the bus and the follower.
#[derive(Debug, Default)]
pub struct MemoryVersionStore {
    head: Mutex<Option<(Version, MetaHash)>>,
    definitions: Mutex<BTreeMap<Version, Definitions>>,
}

impl MemoryVersionStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the head **and** the definitions at once — what a real apply-and-persist step
    /// does in one transaction.
    pub fn publish(&self, version: Version, hash: MetaHash, definitions: Definitions) {
        *self.lock_head() = Some((version, hash));
        self.lock_definitions().insert(version, definitions);
    }

    /// Records only the definitions for `version`, leaving the head untouched.
    pub fn put_definitions(&self, version: Version, definitions: Definitions) {
        self.lock_definitions().insert(version, definitions);
    }

    /// The current head, without a future.
    #[must_use]
    pub fn head(&self) -> Option<(Version, MetaHash)> {
        self.lock_head().clone()
    }

    /// The definitions recorded for `version`, if any.
    #[must_use]
    pub(crate) fn definitions_at(&self, version: Version) -> Option<Definitions> {
        self.lock_definitions().get(&version).cloned()
    }

    fn lock_head(&self) -> MutexGuard<'_, Option<(Version, MetaHash)>> {
        self.head
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_definitions(&self) -> MutexGuard<'_, BTreeMap<Version, Definitions>> {
        self.definitions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl VersionStore for MemoryVersionStore {
    fn read<'a>(&'a self) -> StoreFuture<'a, Option<(Version, MetaHash)>> {
        let head = self.head();
        Box::pin(async move { Ok(head) })
    }

    fn write<'a>(&'a self, version: Version, hash: MetaHash) -> StoreFuture<'a, ()> {
        *self.lock_head() = Some((version, hash));
        Box::pin(async { Ok(()) })
    }
}

/// The multi-node bus: it writes a metadata change to the version store and fans it out to
/// same-process subscribers.
///
/// Cross-node delivery is by polling, not by this stream: a node on another machine learns of
/// the change when its host calls [`Follower::poll_once`](crate::Follower::poll_once). The local
/// broadcast is still wired so a single process with several subscribers (or several nodes in
/// one test) gets push delivery too.
pub struct PollingBus {
    store: Arc<dyn VersionStore>,
    local: InProcessBus,
}

impl PollingBus {
    /// A polling bus over `store`, with the default local fan-out capacity.
    #[must_use]
    pub fn new(store: Arc<dyn VersionStore>) -> Self {
        Self::with_capacity(store, crate::bus::DEFAULT_BUS_CAPACITY)
    }

    /// As [`PollingBus::new`], with an explicit local fan-out queue size.
    #[must_use]
    pub fn with_capacity(store: Arc<dyn VersionStore>, capacity: usize) -> Self {
        Self {
            store,
            local: InProcessBus::new(capacity),
        }
    }

    /// The store behind this bus.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn VersionStore> {
        &self.store
    }

    /// Reads the store once. The host calls this at the interval [`PollSchedule`] derives from
    /// the injected clock; there is no timer inside the crate.
    ///
    /// # Errors
    ///
    /// Whatever the store reports, as [`crate::LiveError::Store`].
    pub async fn poll_once(&self) -> LiveResult<Option<(Version, MetaHash)>> {
        self.store.read().await
    }
}

impl InvalidationBus for PollingBus {
    fn publish<'a>(&'a self, site: &'a SiteId, invalidation: Invalidation) -> BusFuture<'a, ()> {
        Box::pin(async move {
            // Only a metadata change moves the stored metadata head; a cache-key message
            // (WP-PLT-19) is announced locally and, later, through its own store.
            if invalidation.kind == InvalidationKind::Meta {
                self.store
                    .write(invalidation.version, invalidation.hash.clone())
                    .await?;
            }
            self.local.send(site, invalidation);
            Ok(())
        })
    }

    fn subscribe(&self, site: &SiteId) -> InvalidationStream {
        self.local.subscribe(site)
    }
}

impl std::fmt::Debug for PollingBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollingBus").finish_non_exhaustive()
    }
}

/// Decides when the host should poll, from the injected clock — no timer, no sleeping.
///
/// The first [`due`](PollSchedule::due) returns `true`, so a restarted node reconciles at once;
/// every later call returns `true` once per interval of the clock's monotonic time. A
/// [`ManualClock`](spot_core::ManualClock) drives it in tests.
pub struct PollSchedule {
    interval: Duration,
    next: Mutex<Option<Duration>>,
}

impl PollSchedule {
    /// A schedule polling at most once per `interval`.
    #[must_use]
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: Mutex::new(None),
        }
    }

    /// The interval.
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Whether a poll is due at the clock's current monotonic time, advancing the schedule when
    /// it is. `true` the first time; then once the interval has elapsed since the last poll.
    #[must_use]
    pub fn due(&self, clock: &dyn Clock) -> bool {
        let now = clock.monotonic();
        let mut next = self
            .next
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *next {
            None => {
                *next = now.checked_add(self.interval);
                true
            }
            Some(at) if now >= at => {
                *next = now.checked_add(self.interval);
                true
            }
            Some(_) => false,
        }
    }
}

impl std::fmt::Debug for PollSchedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollSchedule")
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}
