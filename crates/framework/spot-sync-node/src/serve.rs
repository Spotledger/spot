//! The serve-sequence prefix (ADR-065 section 1, WP-SYN-11.T2): the node acknowledges the highest
//! serve sequence for which it has applied everything the hub queued before it.

use std::collections::BTreeMap;

/// The serve sequence the hub's queue gave a bundle, as it travels with the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeStamp {
    /// The bundle's own serve sequence: per destination, assigned by the hub's queue, strictly
    /// increasing, without gaps (the first is 1).
    pub seq: u64,
    /// The serve sequence of the greatest bundle the hub's queue still held below this one when it
    /// served it, or 0. The bundle can be counted into the prefix only once everything up to this
    /// number is.
    pub after: u64,
}

/// Tracks which serve sequences are settled (applied, replayed or held for skew; never parked,
/// refused or on the repair list) and the prefix they form.
///
/// The prefix (`floor`) is the highest sequence such that every bundle the hub queued up to it is
/// settled. It moves from `floor` to a settled bundle's sequence only when that bundle's `after`
/// is at or below `floor`: nothing the hub queued in between can have been skipped, and a bundle
/// the node was never sent is below some served bundle's `after` and so blocks the way. A node
/// that was handed a high sequence first therefore acknowledges nothing until it has the lower
/// ones, whatever the lane order or the journal positions.
///
/// The tracker is volatile, like [`PrefixTracker`](crate::PrefixTracker): a restart starts from
/// 0 (or from the stored prefix, see [`ServeTracker::restored`]) and the hub re-serves what it still queues, each bundle naming what lies below it, so the
/// prefix is rebuilt from the stamps alone.
#[derive(Debug, Clone, Default)]
pub struct ServeTracker {
    floor: u64,
    /// Settled sequences above `floor`, with their `after`.
    settled: BTreeMap<u64, u64>,
}

impl ServeTracker {
    /// A tracker with nothing settled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A tracker restored from the stored prefix: everything up to `floor` is settled.
    #[must_use]
    pub fn restored(floor: u64) -> Self {
        Self {
            floor,
            settled: BTreeMap::new(),
        }
    }

    /// The bundle with this stamp is settled.
    pub fn settle(&mut self, stamp: ServeStamp) {
        if stamp.seq <= self.floor {
            return;
        }
        self.settled.insert(stamp.seq, stamp.after);
        loop {
            let next = self
                .settled
                .iter()
                .find(|(_, after)| **after <= self.floor)
                .map(|(seq, _)| *seq);
            let Some(seq) = next else { break };
            self.floor = seq;
            self.settled = self.settled.split_off(&(seq + 1));
        }
    }

    /// The acknowledged serve sequence, if any bundle counts yet.
    #[must_use]
    pub fn prefix(&self) -> Option<u64> {
        (self.floor > 0).then_some(self.floor)
    }

    /// Settled bundles waiting for a lower one.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.settled.len()
    }
}
