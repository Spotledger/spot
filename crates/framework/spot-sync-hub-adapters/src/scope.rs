//! The master-data scope of one destination.

use crate::keys::{ScopeKeysOf, decode_scope_key};
use spot_sync_closure::RecordKey;
use spot_sync_hub::{DestinationScope, StreamEntry};
use spot_sync_master::{Doctype, MasterChange, ScopeKey, Subscription};
use spot_sync_protocol::RecordName;

/// A destination's entitlement to master data: its hub-side [`Subscription`] plus the hub's
/// knowledge of which scope keys each record requires.
#[derive(Debug, Clone, Copy)]
pub struct MasterScope<'a, K: ScopeKeysOf> {
    subscription: &'a Subscription,
    keys: K,
}

impl<'a, K: ScopeKeysOf> MasterScope<'a, K> {
    /// A scope for the destination whose registry subscription is `subscription`.
    #[must_use]
    pub fn new(subscription: &'a Subscription, keys: K) -> Self {
        Self { subscription, keys }
    }

    /// The decision for `key` at `version`. Anything unknown or invalid is not covered.
    fn decide(&self, key: &RecordKey, version: u64) -> bool {
        let Ok(entity) = Doctype::new(&key.entity) else {
            return false;
        };
        let Ok(name) = RecordName::new(&key.id) else {
            return false;
        };
        let Some(texts) = self.keys.scope_keys(key) else {
            return false;
        };
        let mut scope_keys: Vec<ScopeKey> = Vec::with_capacity(texts.len());
        for text in &texts {
            match decode_scope_key(text) {
                Some(k) => scope_keys.push(k),
                None => return false,
            }
        }
        match MasterChange::new(entity, name, version, scope_keys, "") {
            Ok(change) => self.subscription.covers(&change),
            Err(_) => false,
        }
    }
}

impl<K: ScopeKeysOf> DestinationScope for MasterScope<'_, K> {
    fn covers(&self, entry: &StreamEntry) -> bool {
        self.decide(&entry.key, entry.version)
    }

    fn covers_key(&self, key: &RecordKey) -> bool {
        // Version 1 stands in: the version does not affect coverage.
        self.decide(key, 1)
    }

    fn owns(&self, _key: &RecordKey) -> bool {
        false
    }
}
