//! Residency of the peer, through the [`RegionOracle`] port.
//!
//! ADR-058 rule 6 lets a grant require the peer to be in a given region. This crate never holds
//! a region map itself: an adapter answers for the peer, using `spot-data-policy`'s region
//! vocabulary and its [`Refusal`] (which names the policy clause that said no).

use std::collections::BTreeMap;
use std::sync::Mutex;

use spot_data_policy::{Refusal, Region};

use crate::model::Party;

/// Answers which region a peer sits in.
///
/// `Ok(None)` means the oracle does not know; the caller refuses rather than assumes. A
/// [`Refusal`] is the policy's own answer — its clause is kept verbatim in the link's error so a
/// user sees which residency rule refused the exchange.
pub trait RegionOracle: Send + Sync {
    /// The peer's region, if known.
    ///
    /// # Errors
    /// A [`Refusal`] naming the policy clause that makes the region unusable.
    fn peer_region(&self, peer: &Party) -> Result<Option<Region>, Refusal>;
}

/// A fixed peer-to-region map, for tests and for adapters that already hold the answer.
#[derive(Default)]
pub struct MemoryRegionOracle {
    regions: Mutex<BTreeMap<Party, Region>>,
}

impl MemoryRegionOracle {
    /// An oracle that knows no peer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the region of a peer.
    ///
    /// # Errors
    /// [`crate::Error::StoreFailed`] when the lock is poisoned.
    pub fn insert(&self, peer: Party, region: Region) -> crate::Result<()> {
        self.regions
            .lock()
            .map_err(|_| crate::Error::StoreFailed {
                reason: "the region oracle lock is poisoned",
            })?
            .insert(peer, region);
        Ok(())
    }
}

impl RegionOracle for MemoryRegionOracle {
    fn peer_region(&self, peer: &Party) -> Result<Option<Region>, Refusal> {
        Ok(self
            .regions
            .lock()
            .ok()
            .and_then(|map| map.get(peer).cloned()))
    }
}
