//! The cache part of the world (WP-AI-07.T6): the real [`CacheOps`] over the cache service's
//! [`CacheStore`] port.
//!
//! # Namespaces
//!
//! The cache store keeps flat keys per site, so a "namespace" is a key prefix the platform owns.
//! The one the platform writes is the **step** namespace, the `step:` prefix every step's
//! `ctx.cache()` entry lives under ([`STEP_PREFIX`]); it is the only namespace this part knows. A
//! flush of any other name is refused as unknown (the part never clears the whole site).
//!
//! - `namespaces` answers `step`.
//! - `entries` for `step` is the **exact** number of live `step:` keys of the site, from the
//!   store's `count_prefix` (an expired key is not counted, and the other keys of the site's cache
//!   are not). Every other name holds 0. A Redis-backed store counts with a bounded scan, so a
//!   site with an extreme number of `step:` keys may read a lower bound.
//! - `flush` removes the `step:` prefix with the store's `delete_prefix` (never `clear_site`) and
//!   returns the counts before and after, both read the same way as `entries`: a flush of 3 step
//!   keys beside 5 other keys reports `before 3, after 0` and leaves the 5.
//! - `undo_flush` records the reversal and restores nothing: the cache is derived data and
//!   refills from its source, exactly what the seed remedy declares. No cache value is ever read
//!   or stored by this part.
//! - `rehearsal` seeds the in-memory fixture with the step entry count; flushing the rehearsal
//!   never reaches the store.
//!
//! The ops are synchronous and block on the runtime handle captured when the site was built
//! (`Handle::block_on`, never `block_in_place`).

use std::sync::Arc;

use spot_cache::CacheStore;
use spot_core::SiteId;
use spot_remedies::fixture::FakeCache;
use spot_remedies::{CacheChange, CacheNs, CacheOps, OpError};
use tokio::runtime::Handle;

use super::parts::{CachePart, capture_runtime};
use crate::services::SiteCtx;
use crate::services::cache::STEP_PREFIX;
use crate::services::remedies::world::WorldError;

/// The namespace name of the step cache: [`STEP_PREFIX`] without its colon.
#[must_use]
pub fn step_namespace() -> &'static str {
    STEP_PREFIX.trim_end_matches(':')
}

/// The cache part over a store.
pub struct StoreCachePart {
    store: Arc<dyn CacheStore>,
    site: SiteId,
    handle: Handle,
}

impl StoreCachePart {
    /// A part over `store` for `site`; `handle` is the runtime the ops block on.
    #[must_use]
    pub fn new(store: Arc<dyn CacheStore>, site: SiteId, handle: Handle) -> Self {
        Self {
            store,
            site,
            handle,
        }
    }

    /// The live `step:` entries of the site's cache (exact: the store counts the prefix).
    fn step_entries(&self) -> usize {
        let count = self
            .handle
            .block_on(self.store.count_prefix(&self.site, STEP_PREFIX));
        usize::try_from(count).unwrap_or(usize::MAX)
    }
}

/// The part for a site, or `None` when there is no cache store, the site name is invalid or no
/// runtime is available to block on.
#[must_use]
pub fn from_parts(
    store: Option<Arc<dyn CacheStore>>,
    site: &str,
    handle: Option<Handle>,
) -> Option<Arc<dyn CachePart>> {
    let store = store?;
    let site = SiteId::new(site).ok()?;
    let handle = handle?;
    Some(Arc::new(StoreCachePart::new(store, site, handle)))
}

/// The cache part for this site, or `None` when no module provided a cache store.
pub(super) fn from_site(ctx: &SiteCtx<'_>) -> Option<Arc<dyn CachePart>> {
    let env = *ctx.env().ok()?;
    from_parts(
        ctx.port::<dyn CacheStore>(),
        &env.site.site,
        capture_runtime(),
    )
}

impl CachePart for StoreCachePart {
    fn live(&self) -> Box<dyn CacheOps + '_> {
        Box::new(StoreCacheOps { part: self })
    }

    fn rehearsal(&self) -> Result<Box<dyn CacheOps>, WorldError> {
        let mut fake = FakeCache::new();
        fake.seed(step_namespace(), self.step_entries());
        Ok(Box::new(fake))
    }
}

/// The live ops.
struct StoreCacheOps<'a> {
    part: &'a StoreCachePart,
}

impl StoreCacheOps<'_> {
    fn known(ns: &CacheNs) -> bool {
        ns.as_str() == step_namespace()
    }
}

impl CacheOps for StoreCacheOps<'_> {
    fn namespaces(&self) -> Vec<CacheNs> {
        vec![CacheNs::new(step_namespace())]
    }

    fn entries(&self, ns: &CacheNs) -> usize {
        if Self::known(ns) {
            self.part.step_entries()
        } else {
            0
        }
    }

    fn flush(&mut self, ns: &CacheNs) -> Result<CacheChange, OpError> {
        if !Self::known(ns) {
            return Err(OpError::new(
                "cache_flush",
                &format!("no namespace `{}`", ns.as_str()),
            ));
        }
        let before = self.part.step_entries();
        // Only the namespace's own prefix: never `clear_site`.
        let _removed = self
            .part
            .handle
            .block_on(self.part.store.delete_prefix(&self.part.site, STEP_PREFIX));
        let after = self.part.step_entries();
        Ok(CacheChange {
            ns: ns.clone(),
            before,
            after,
        })
    }

    fn undo_flush(&mut self, change: &CacheChange) -> Result<(), OpError> {
        if Self::known(&change.ns) {
            // A cache is derived data: the reversal records the intent without restoring entries.
            Ok(())
        } else {
            Err(OpError::new(
                "cache_undo_flush",
                &format!("no namespace `{}`", change.ns.as_str()),
            ))
        }
    }
}
