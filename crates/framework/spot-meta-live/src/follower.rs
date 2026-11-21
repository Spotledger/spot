//! [`Follower`]: one node that keeps its [`LiveMeta`] fresh from the bus and the version store.
//!
//! A node has three jobs. On a bus event it loads the announced version, builds it and swaps it
//! in ([`Follower::deliver_next`]). On a bus gap — or at start-up, or on its own schedule — it
//! reconciles from the version store ([`Follower::poll_once`]). And it **publishes** its own
//! changes ([`Follower::apply`]). It never goes backwards: an older version is ignored, and the
//! same version with a different hash is [`LiveError::Divergence`](crate::LiveError::Divergence).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_core::{Clock, SiteId};

use crate::bus::{InvalidationBus, InvalidationStream, next_event};
use crate::error::{LiveError, LiveResult, divergence};
use crate::invalidation::{BusEvent, Invalidation};
use crate::live::{Adopt, LiveMeta, MetaChange};
use crate::polling::{MemoryVersionStore, PollSchedule, VersionStore};
use crate::snapshot::{Definitions, MetaHash, Snapshot, Version};

/// The future a [`MetaLoader::load`] returns; boxed so the trait stays object-safe
/// (`Arc<dyn MetaLoader>`) for a database-backed loader.
pub type LoadFuture<'a, T> = Pin<Box<dyn Future<Output = LiveResult<T>> + Send + 'a>>;

/// Loads the definitions at a version — the persistence seam (WP-MET-01.T2) plus whatever reads
/// it back out.
pub trait MetaLoader: Send + Sync {
    /// The definitions recorded at `version`.
    ///
    /// # Errors
    ///
    /// [`LiveError::Store`] when the version has no definitions, or whatever a database reports.
    fn load<'a>(&'a self, version: Version) -> LoadFuture<'a, Definitions>;
}

impl MetaLoader for MemoryVersionStore {
    fn load<'a>(&'a self, version: Version) -> LoadFuture<'a, Definitions> {
        let definitions = self.definitions_at(version);
        Box::pin(async move {
            definitions.ok_or_else(|| LiveError::Store {
                message: format!("no definitions are recorded for metadata version {version}"),
            })
        })
    }
}

/// One node: a [`LiveMeta`], a bus subscription, a version store and a loader.
///
/// Both an applier and a follower: [`apply`](Follower::apply) changes the local snapshot and
/// announces it; [`deliver_next`](Follower::deliver_next) and
/// [`poll_once`](Follower::poll_once) bring changes in. The host owns the loop — this type
/// spawns nothing and holds no timer.
pub struct Follower {
    site: SiteId,
    meta: Arc<LiveMeta>,
    bus: Arc<dyn InvalidationBus>,
    store: Arc<dyn VersionStore>,
    loader: Arc<dyn MetaLoader>,
    schedule: PollSchedule,
    events: InvalidationStream,
}

impl Follower {
    /// A node over an existing [`LiveMeta`], subscribed to `site` on `bus`.
    #[must_use]
    pub fn new(
        site: SiteId,
        meta: Arc<LiveMeta>,
        bus: Arc<dyn InvalidationBus>,
        store: Arc<dyn VersionStore>,
        loader: Arc<dyn MetaLoader>,
        schedule: PollSchedule,
    ) -> Self {
        let events = bus.subscribe(&site);
        Self {
            site,
            meta,
            bus,
            store,
            loader,
            schedule,
            events,
        }
    }

    /// A node that boots at `version` with `initial` definitions and subscribes immediately.
    ///
    /// # Errors
    ///
    /// [`LiveError::Build`] when the initial definitions do not build.
    pub fn with_initial(
        site: SiteId,
        initial: Definitions,
        version: Version,
        bus: Arc<dyn InvalidationBus>,
        store: Arc<dyn VersionStore>,
        loader: Arc<dyn MetaLoader>,
        schedule: PollSchedule,
    ) -> LiveResult<Self> {
        let meta = Arc::new(LiveMeta::at_version(site.clone(), initial, version)?);
        Ok(Self::new(site, meta, bus, store, loader, schedule))
    }

    /// The site this node serves.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The live metadata, for callers that want the handle itself.
    #[must_use]
    pub fn meta(&self) -> &Arc<LiveMeta> {
        &self.meta
    }

    /// The current snapshot, lock-free.
    #[must_use]
    pub fn current(&self) -> Arc<Snapshot> {
        self.meta.current()
    }

    /// Applies a change locally and publishes the new `(version, hash)` on the bus. Returns the
    /// new version.
    ///
    /// # Errors
    ///
    /// Whatever [`LiveMeta::apply`] reports, or whatever the bus reports.
    pub async fn apply(&self, change: MetaChange) -> LiveResult<Version> {
        let snapshot = self.meta.apply_inner(change)?;
        let invalidation = Invalidation::meta(snapshot.version(), snapshot.hash().clone());
        self.bus.publish(&self.site, invalidation).await?;
        Ok(snapshot.version())
    }

    /// Handles the next bus event, returning the version when a newer snapshot was adopted.
    ///
    /// A [`BusEvent::Gap`] reconciles from the version store; a non-metadata change is
    /// ignored; a stream end (the bus is gone) yields `None`.
    ///
    /// # Errors
    ///
    /// [`LiveError::Divergence`] on a version/hash clash, [`LiveError::Build`] or
    /// [`LiveError::Store`] while loading, and whatever the store reports.
    pub async fn deliver_next(&mut self) -> LiveResult<Option<Version>> {
        match next_event(&mut self.events).await {
            None => Ok(None),
            Some(BusEvent::Gap) => self.poll_once().await,
            Some(BusEvent::Change(invalidation)) => self.receive(invalidation).await,
        }
    }

    /// Reads the version store once; if it is ahead, loads the announced version, builds it and
    /// swaps it in. Returns the adopted version, or `None` when the store adds nothing.
    ///
    /// This is the reconciliation path: the host calls it at the interval
    /// [`PollSchedule`] derives from the injected clock, and on a bus gap. It never goes
    /// backwards.
    ///
    /// # Errors
    ///
    /// [`LiveError::Divergence`], [`LiveError::Build`] or [`LiveError::Store`].
    pub async fn poll_once(&self) -> LiveResult<Option<Version>> {
        match self.store.read().await? {
            None => Ok(None),
            Some((version, hash)) => self.load_and_adopt(version, &hash).await,
        }
    }

    /// Whether [`PollSchedule`] says a poll is due at the clock's current monotonic time.
    #[must_use]
    pub fn poll_due(&self, clock: &dyn Clock) -> bool {
        self.schedule.due(clock)
    }

    /// The store behind this node's polling.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn VersionStore> {
        &self.store
    }

    async fn receive(&self, invalidation: Invalidation) -> LiveResult<Option<Version>> {
        if !invalidation.is_meta() {
            return Ok(None);
        }
        self.load_and_adopt(invalidation.version, &invalidation.hash)
            .await
    }

    /// The shared tail of the push path and the polling path: check the version, load, adopt.
    async fn load_and_adopt(
        &self,
        version: Version,
        hash: &MetaHash,
    ) -> LiveResult<Option<Version>> {
        let current = self.meta.current();
        if version < current.version() {
            return Ok(None);
        }
        if version == current.version() {
            return if hash == current.hash() {
                Ok(None)
            } else {
                Err(divergence(&self.site, version, current.hash(), hash))
            };
        }
        let definitions = self.loader.load(version).await?;
        match self.meta.adopt(version, hash, definitions)? {
            Adopt::Applied(applied) => Ok(Some(applied)),
            Adopt::Stale | Adopt::Unchanged => Ok(None),
        }
    }
}

impl std::fmt::Debug for Follower {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Follower")
            .field("site", &self.site)
            .field("version", &self.meta.current().version())
            .finish_non_exhaustive()
    }
}
