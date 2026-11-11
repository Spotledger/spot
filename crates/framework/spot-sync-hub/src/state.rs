//! The relay state a caller persists.

use crate::deliver::DestState;
use crate::model::StreamEntry;
use serde::{Deserialize, Serialize};
use spot_sync_closure::{ExtrasSet, RecordKey};
use spot_sync_obligations::Lane;
use spot_sync_skew::HoldSet;
use std::collections::{BTreeMap, BTreeSet};

/// What the relay must remember across a restart: the cursor, what it has seen of the stream's
/// contract (the transaction of the last entry it processed and the lane of each entity) and, per
/// destination, its holds, extras set, acknowledged prefix and the position it is caught up from.
/// It holds record references, never record content.
///
/// Serialisation note: the skew crate's `HoldSet` is keyed by `(entity, id)` tuples, so a text
/// format that needs string map keys (JSON) cannot carry it; use a format that can (for example a
/// binary one) or persist the snapshot through the storage layer's own encoder.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RelayState {
    /// The highest stream position processed.
    pub cursor: u64,
    /// The source transaction of the last entry processed. The next read must not continue it: a
    /// read that does split a transaction is refused (`sync_hub.stream_split_txn`).
    #[serde(default)]
    pub last_txn: Option<String>,
    /// The delivery lane each entity came in, so a change is refused
    /// (`sync_hub.lane_not_entity`) and an entity's versions never swap between lanes. Bounded by
    /// `RelayConfig::max_entities`.
    #[serde(default)]
    pub lane_of_entity: BTreeMap<String, Lane>,
    /// Per destination.
    pub destinations: BTreeMap<String, DestSnapshot>,
}

/// One destination's persisted state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestSnapshot {
    /// The version-skew holds.
    pub holds: HoldSet,
    /// The stream entries behind the holds, by position.
    pub held_entries: BTreeMap<u64, StreamEntry>,
    /// Entries withheld because a record their closure needs is held ("dependency held"), by
    /// position of the entry in `held_entries`: the held records that block each one.
    #[serde(default)]
    pub held_by_dependency: BTreeMap<u64, BTreeSet<RecordKey>>,
    /// Released entries not yet requeued.
    pub release_backlog: BTreeMap<u64, StreamEntry>,
    /// Out-of-scope records delivered as extras.
    pub extras: ExtrasSet,
    /// The acknowledged prefix.
    pub acked: Option<u64>,
    /// The position the destination is caught up from, when it is behind.
    pub catchup_from: Option<u64>,
}

impl DestSnapshot {
    pub(crate) fn of(d: &DestState) -> Self {
        Self {
            holds: d.holds.clone(),
            held_entries: d.held_entries.clone(),
            held_by_dependency: d.held_by_dependency.clone(),
            release_backlog: d.release_backlog.clone(),
            extras: d.extras.clone(),
            acked: d.acked,
            catchup_from: d.catchup_from,
        }
    }

    pub(crate) fn apply(self, d: &mut DestState) {
        d.holds = self.holds;
        d.held_entries = self.held_entries;
        d.held_by_dependency = self.held_by_dependency;
        d.release_backlog = self.release_backlog;
        d.extras = self.extras;
        d.acked = self.acked;
        d.catchup_from = self.catchup_from;
    }
}
