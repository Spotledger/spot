//! The stored payload bytes behind a held effect's reference (WP-MIG-08.T7).
//!
//! The ledger keeps only a [`PayloadRef`](crate::PayloadRef): an opaque handle, a hash and a size.
//! The bytes live with the owner (a blob store, an outbox row). Purging the reference alone would
//! leave the purged data on disk, so a purge also deletes the bytes, through this port.
//!
//! # Order, and why a crash is safe
//!
//! The ledger and the byte store are two stores, so no single transaction covers both. The order
//! is chosen so that **nothing ever references bytes that are gone**:
//!
//! 1. One ledger transaction clears the references **and** queues their handles for deletion
//!    ([`LedgerStore::purge_payloads`](crate::LedgerStore::purge_payloads)). From here no effect
//!    refers to the bytes, and the intent to delete them is durable.
//! 2. [`drain_payload_deletes`] deletes the bytes of each queued handle that **no effect still
//!    references** (a payload shared by two holds is kept until the last reference goes), then
//!    removes the queue entry.
//!
//! A crash between the two leaves orphan bytes with a queue entry; the next drain deletes them. A
//! crash can never leave a reference to missing bytes. The remaining race (a new effect adopting a
//! handle between the check and the delete) is closed by the owner: a handle is minted per payload
//! and is never reused for a different effect.

use std::collections::BTreeMap;
use std::sync::Mutex;

use futures::future::BoxFuture;
use spot_core::SiteId;

use crate::error::{Error, Result};
use crate::store::LedgerStore;

/// Deletes stored payload bytes by handle. Implemented by whoever owns the bytes.
pub trait PayloadStore: Send + Sync {
    /// Deletes the bytes behind `handle`. A handle that is already gone is success (the call is
    /// idempotent: a drain that crashed after the delete repeats it).
    ///
    /// # Errors
    /// [`Error::LedgerUnavailable`] (or any error of the owner mapped to this crate's) when the
    /// bytes could not be deleted; the handle stays queued and the next drain retries.
    fn delete<'a>(&'a self, handle: &'a str) -> BoxFuture<'a, Result<()>>;
}

/// Deletes the bytes of every queued handle of `site` that no effect references any more, at most
/// `limit` handles, and removes their queue entries. Returns how many were deleted.
///
/// A handle that is still referenced stays queued (it is deleted by the drain after the last
/// reference is purged). The first byte-store failure stops the drain and is returned; the entries
/// not yet completed stay queued, so nothing is lost and nothing is deleted twice in a harmful way.
///
/// # Errors
/// The ledger's error, or the byte store's.
pub async fn drain_payload_deletes(
    ledger: &dyn LedgerStore,
    payloads: &dyn PayloadStore,
    site: &SiteId,
    limit: usize,
) -> Result<u64> {
    let mut deleted = 0_u64;
    for handle in ledger.pending_payload_deletes(site, limit).await? {
        payloads.delete(&handle).await?;
        ledger.complete_payload_delete(site, &handle).await?;
        deleted += 1;
    }
    Ok(deleted)
}

/// A byte store in memory, for tests and for hosts that keep payloads in process.
#[derive(Default)]
pub struct InMemoryPayloadStore {
    bytes: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl InMemoryPayloadStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `bytes` under `handle`, replacing what was there.
    pub fn put(&self, handle: impl Into<String>, bytes: Vec<u8>) {
        if let Ok(mut map) = self.bytes.lock() {
            map.insert(handle.into(), bytes);
        }
    }

    /// Whether bytes are stored under `handle`.
    #[must_use]
    pub fn contains(&self, handle: &str) -> bool {
        self.bytes.lock().is_ok_and(|map| map.contains_key(handle))
    }

    /// How many payloads are stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.lock().map_or(0, |map| map.len())
    }

    /// Whether nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for InMemoryPayloadStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryPayloadStore")
            .finish_non_exhaustive()
    }
}

impl PayloadStore for InMemoryPayloadStore {
    fn delete<'a>(&'a self, handle: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut map = self.bytes.lock().map_err(|_| Error::LedgerUnavailable {
                reason: "the in-memory payload store lock is poisoned".to_owned(),
            })?;
            map.remove(handle);
            Ok(())
        })
    }
}
