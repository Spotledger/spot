//! The acknowledged prefix (ADR-065 section 1): only what is applied is acknowledged.

use std::collections::{BTreeMap, BTreeSet};

use spot_sync_closure::Position;

/// Tracks settled bundles (applied, replayed or held for skew) and unsettled ones (parked, on
/// the repair list or refused). The prefix is the highest settled position below the first
/// position of every unsettled bundle.
#[derive(Debug, Clone, Default)]
pub struct PrefixTracker {
    /// The latest position of each settled bundle (compacted).
    settled: BTreeSet<Position>,
    /// First position of each unsettled bundle, with how many share it.
    pending: BTreeMap<Position, usize>,
}

impl PrefixTracker {
    /// A tracker with nothing settled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A bundle with this first position is unsettled until `release` removes it.
    pub fn hold(&mut self, first: Position) {
        *self.pending.entry(first).or_insert(0) += 1;
    }

    /// A bundle that held the prefix at `first` is settled (applied) and no longer holds it.
    pub fn release(&mut self, first: Position) {
        if let Some(n) = self.pending.get_mut(&first) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.pending.remove(&first);
            }
        }
        self.compact();
    }

    /// A bundle whose latest position is `last` is settled: applied, replayed or held for skew.
    pub fn settle(&mut self, last: Position) {
        self.settled.insert(last);
        self.compact();
    }

    /// The highest position such that every bundle up to it is settled, if any.
    #[must_use]
    pub fn prefix(&self) -> Option<Position> {
        match self.pending.keys().next() {
            Some(&floor) => self.settled.range(..floor).next_back().copied(),
            None => self.settled.iter().next_back().copied(),
        }
    }

    /// Bundles currently holding the prefix back.
    #[must_use]
    pub fn unsettled(&self) -> usize {
        self.pending.values().sum()
    }

    fn compact(&mut self) {
        let floor = self.pending.keys().next().copied();
        let below = match floor {
            Some(f) => self.settled.range(..f).next_back().copied(),
            None => self.settled.iter().next_back().copied(),
        };
        let mut keep: BTreeSet<Position> = match floor {
            Some(f) => self.settled.range(f..).copied().collect(),
            None => BTreeSet::new(),
        };
        if let Some(b) = below {
            keep.insert(b);
        }
        self.settled = keep;
    }
}
