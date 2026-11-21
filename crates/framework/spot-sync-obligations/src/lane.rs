//! Lane order for the catch-up planner (ADR-065 section 3, "Priority").

use serde::{Deserialize, Serialize};

/// A delivery lane. The mandatory lane is served before bulk data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Lane {
    /// Obligation-bearing events: tax, price lists, anything with legal effect.
    Mandatory,
    /// Everything else.
    Bulk,
}

/// A pending delivery item with the lane and stream position it sorts by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending<T> {
    /// The lane of the item.
    pub lane: Lane,
    /// Position in the source stream.
    pub position: u64,
    /// The caller's payload.
    pub item: T,
}

/// Sort pending items: mandatory before bulk, position order within a lane. The sort is stable,
/// so items with an equal lane and position keep their input order.
#[must_use]
pub fn lane_order<T>(mut pending: Vec<Pending<T>>) -> Vec<Pending<T>> {
    pending.sort_by_key(|p| (p.lane, p.position));
    pending
}
