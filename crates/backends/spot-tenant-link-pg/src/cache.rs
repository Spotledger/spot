//! Reading links through a cache, and making a revocation visible to the cache at once.
//!
//! # How a revocation reaches readers
//!
//! [`PgLinkStore`](crate::PgLinkStore) reads the database on every call, so a reader that uses
//! it directly sees a revocation on its very next read. A host that reads often can wrap the
//! store in a [`CachedLinkStore`]; the cache then relies on **push invalidation**, not on time:
//!
//! 1. every committed change calls [`LinkInvalidation::invalidate`] on the hook given to the
//!    store with [`PgLinkStore::with_invalidation`](crate::PgLinkStore::with_invalidation),
//!    before the change returns to its caller;
//! 2. on the same node the hook is the cache's own [`CacheHandle`], so the next read after a
//!    revocation misses the cache and reads the new state;
//! 3. on other nodes the host forwards the same notice over its invalidation bus (the pattern
//!    `spot-meta-live` uses for definitions) and calls [`CacheHandle::invalidate`] there. Until
//!    the notice arrives a node can still serve a cached active link; the host must call
//!    [`CacheHandle::invalidate_all`] when the bus reconnects after a gap.
//!
//! There is no time-to-live: this crate reads no clock (time is an argument everywhere). A host
//! that wants a hard ceiling on staleness calls `invalidate_all` on its own timer.
//!
//! A read that started before an invalidation never repopulates the cache with what it read: an
//! epoch counter, bumped by every invalidation, is compared before the entry is stored.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_tenant_link::{Error, Link, LinkId, LinkStore, Party, Result};

/// A listener told that a link changed.
pub trait LinkInvalidation: Send + Sync {
    /// The link changed (inserted, updated, revoked or expired); drop anything cached for it.
    fn invalidate(&self, link: &LinkId);
}

#[derive(Default)]
struct State {
    epoch: u64,
    links: BTreeMap<LinkId, Link>,
}

/// The invalidation side of a cache; give a clone to the store and keep one for the bus.
#[derive(Clone, Default)]
pub struct CacheHandle {
    state: Arc<Mutex<State>>,
}

impl CacheHandle {
    /// A fresh, empty handle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| Error::StoreFailed {
            reason: "the link cache lock is poisoned",
        })
    }

    /// Drops everything cached; use after a bus reconnect.
    pub fn invalidate_all(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.epoch = state.epoch.wrapping_add(1);
            state.links.clear();
        }
    }

    /// How many links are cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.links.len())
            .unwrap_or(0)
    }

    /// Whether nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl LinkInvalidation for CacheHandle {
    fn invalidate(&self, link: &LinkId) {
        if let Ok(mut state) = self.state.lock() {
            state.epoch = state.epoch.wrapping_add(1);
            state.links.remove(link);
        }
    }
}

/// A [`LinkStore`] that caches `get` and forwards everything else.
pub struct CachedLinkStore<S> {
    inner: S,
    handle: CacheHandle,
}

impl<S: LinkStore> CachedLinkStore<S> {
    /// A cache over `inner`, invalidated through `handle`.
    #[must_use]
    pub fn new(inner: S, handle: CacheHandle) -> Self {
        Self { inner, handle }
    }

    /// The wrapped store.
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// The invalidation handle.
    #[must_use]
    pub fn handle(&self) -> &CacheHandle {
        &self.handle
    }
}

impl<S: LinkStore> LinkStore for CachedLinkStore<S> {
    fn insert(&self, link: Link) -> Result<Link> {
        let id = link.id;
        let result = self.inner.insert(link);
        self.handle.invalidate(&id);
        result
    }

    fn get(&self, id: &LinkId) -> Result<Link> {
        let epoch = {
            let state = self.handle.lock()?;
            if let Some(link) = state.links.get(id) {
                return Ok(link.clone());
            }
            state.epoch
        };
        let link = self.inner.get(id)?;
        let mut state = self.handle.lock()?;
        if state.epoch == epoch {
            state.links.insert(*id, link.clone());
        }
        Ok(link)
    }

    fn update(&self, link: Link, expected_version: u64) -> Result<Link> {
        let id = link.id;
        let result = self.inner.update(link, expected_version);
        self.handle.invalidate(&id);
        result
    }

    fn find(&self, first: &Party, second: &Party) -> Result<Option<Link>> {
        self.inner.find(first, second)
    }

    fn list_for(&self, party: &Party) -> Result<Vec<Link>> {
        self.inner.list_for(party)
    }
}
