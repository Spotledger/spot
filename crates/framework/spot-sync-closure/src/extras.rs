//! The per-destination extras set as pure data (ADR-064 section 5).

use crate::model::RecordKey;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The ids of out-of-scope records delivered to one destination as extras.
///
/// While a record is in the set, its later versions are routed to that destination like an
/// in-scope record, and the destination holds it as a mirror flagged `by_reference`. Records leave
/// the set only when the destination is decommissioned or a scope change brings them in scope
/// (so there is deliberately no `remove` here). The routing decision itself belongs to
/// `WP-SYN-02`; this type is the data it asks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtrasSet {
    keys: BTreeSet<RecordKey>,
}

/// The `EXTRAS_GROWTH` condition data (ADR-067): the set is over its threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtrasGrowth {
    /// Current size.
    pub size: usize,
    /// The threshold that was passed.
    pub threshold: usize,
}

impl ExtrasSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that `key` was delivered as an extra. Returns true when it was not already there.
    pub fn record_delivered_extra(&mut self, key: RecordKey) -> bool {
        self.keys.insert(key)
    }

    /// Whether `key` is in the set.
    #[must_use]
    pub fn contains(&self, key: &RecordKey) -> bool {
        self.keys.contains(key)
    }

    /// Number of records in the set.
    #[must_use]
    pub fn size(&self) -> usize {
        self.keys.len()
    }

    /// Whether a later version of `key` must be routed to this destination although it is out of
    /// scope: exactly when the record is in the set.
    #[must_use]
    pub fn routed_later_versions(&self, key: &RecordKey) -> bool {
        self.contains(key)
    }

    /// The `EXTRAS_GROWTH` check: `Some` when the set is larger than `threshold`.
    #[must_use]
    pub fn growth(&self, threshold: usize) -> Option<ExtrasGrowth> {
        (self.size() > threshold).then_some(ExtrasGrowth {
            size: self.size(),
            threshold,
        })
    }

    /// The members in key order.
    pub fn iter(&self) -> impl Iterator<Item = &RecordKey> {
        self.keys.iter()
    }
}
