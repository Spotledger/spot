//! Virtual time: one counter for the hub, a skew per node, and jumps.

use std::collections::BTreeMap;

/// The world's clock.
///
/// The hub's time is `epoch_ms + elapsed_ms`; a node's wall clock is that plus its skew. Nothing
/// here reads an operating-system clock: `elapsed_ms` only moves when the World advances it, so a
/// run replays exactly. Monotonic instants (what a node's freshness tracker reads) are
/// `elapsed_ms`, which a wall-clock jump never moves — the property ADR-065 section 4 relies on.
#[derive(Clone, Debug)]
pub struct Clock {
    epoch_ms: i64,
    elapsed_ms: u64,
    tick_ms: u64,
    skews: BTreeMap<usize, i64>,
}

impl Clock {
    /// A clock at `epoch_ms` that advances `tick_ms` per round.
    #[must_use]
    pub fn new(epoch_ms: i64, tick_ms: u64) -> Self {
        Self {
            epoch_ms,
            elapsed_ms: 0,
            tick_ms,
            skews: BTreeMap::new(),
        }
    }

    /// Advance one round.
    pub fn advance(&mut self) {
        self.elapsed_ms = self.elapsed_ms.saturating_add(self.tick_ms.max(1));
    }

    /// Advance by `ms` (the scale scenario steps to the next admission retry).
    pub fn advance_by(&mut self, ms: u64) {
        self.elapsed_ms = self.elapsed_ms.saturating_add(ms);
    }

    /// The monotonic instant: no wall-clock change can move it.
    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        self.elapsed_ms
    }

    /// The hub's wall clock, milliseconds since the epoch.
    #[must_use]
    pub fn hub_wall_ms(&self) -> i64 {
        self.epoch_ms
            .saturating_add(i64::try_from(self.elapsed_ms).unwrap_or(i64::MAX))
    }

    /// A node's wall clock: the hub's, plus its skew, plus any jump it has taken.
    #[must_use]
    pub fn node_wall_ms(&self, node: usize) -> i64 {
        self.hub_wall_ms().saturating_add(self.skew(node))
    }

    /// A node's skew against the hub, milliseconds (positive: the node is ahead).
    #[must_use]
    pub fn skew(&self, node: usize) -> i64 {
        self.skews.get(&node).copied().unwrap_or(0)
    }

    /// Set a node's skew against the hub.
    pub fn set_skew(&mut self, node: usize, skew_ms: i64) {
        self.skews.insert(node, skew_ms);
    }

    /// Move a node's clock by `delta_ms` without the hub moving: a jump, not a skew.
    pub fn jump(&mut self, node: usize, delta_ms: i64) {
        let now = self.skew(node);
        self.skews.insert(node, now.saturating_add(delta_ms));
    }
}
