//! Per-record holds (ADR-066 sections 5 and 6): only the undecodable record is held.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::filter::Outgoing;
use crate::model::{Major, NodeReport, Topic};

/// A record by identity and version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordRef {
    /// The entity.
    pub entity: String,
    /// The record id.
    pub id: String,
    /// The record's version (a later version supersedes an earlier one).
    pub version: u64,
}

/// Why a record cannot be decoded yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HoldReason {
    /// The event's schema major is not one the node can decode.
    UnsupportedMajor {
        /// The topic.
        topic: Topic,
        /// The major.
        major: Major,
    },
    /// The record needs a metadata version the node lacks.
    MetadataTooLow {
        /// The metadata version required.
        required: u32,
    },
    /// An earlier version of the same record is held, so this later one is withheld too: the node
    /// must never apply version 3 and then receive version 2 (hub-side filter, `crate::filter`).
    BehindHeldVersion {
        /// The position of the earlier held entry this one waits behind.
        blocker: u64,
    },
    /// The major is retired for the whole fleet: the hold is permanent, the node can never decode
    /// it, and the hub raises `NODE_TOO_OLD` rather than hold forever.
    Retired {
        /// The topic.
        topic: Topic,
        /// The retired major.
        major: Major,
    },
}

impl HoldReason {
    /// Whether the node, as it now reports itself, can decode the record. A
    /// [`HoldReason::BehindHeldVersion`] depends on its blocker, not on the node alone, so it is
    /// never satisfied here: `release_for_upgrade` clears it once the blocker is released. A
    /// [`HoldReason::Retired`] is never satisfied.
    #[must_use]
    pub fn satisfied_by(&self, node: &NodeReport) -> bool {
        match self {
            Self::UnsupportedMajor { topic, major } => node.can_decode(topic, *major),
            Self::MetadataTooLow { required } => node.metadata_version >= *required,
            Self::BehindHeldVersion { .. } | Self::Retired { .. } => false,
        }
    }
}

/// One held record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    /// The record.
    pub record: RecordRef,
    /// Why.
    pub reason: HoldReason,
    /// After the node upgrades, the record is re-requested by id.
    pub rerequest_after_upgrade: bool,
    /// Always false: a hold never blocks the applied prefix or the records after it.
    pub blocks_applied_prefix: bool,
    /// Always false: the obligation for this record stays unsatisfied (output for WP-SYN-05.T1).
    pub obligation_satisfied: bool,
}

/// Hold only this record.
#[must_use]
pub fn on_undecodable(record: RecordRef, reason: HoldReason) -> Hold {
    Hold {
        record,
        reason,
        rerequest_after_upgrade: true,
        blocks_applied_prefix: false,
        obligation_satisfied: false,
    }
}

/// The outcome of re-checking holds after an upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// Holds the node can now decode: re-request these records by id and apply them.
    pub released: Vec<Hold>,
    /// Holds still undecodable.
    pub still_held: Vec<Hold>,
}

/// Split holds into those the node (as it reports itself now) can decode and those still held.
#[must_use]
pub fn release_holds(holds: &[Hold], node: &NodeReport) -> Release {
    let (released, still_held) = holds
        .iter()
        .cloned()
        .partition(|hold| hold.reason.satisfied_by(node));
    Release {
        released,
        still_held,
    }
}

/// The holds of one node, at most one per record identity: a later version supersedes an earlier
/// undecodable one (full images make this safe, ADR-066).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldSet {
    holds: BTreeMap<(String, String), Hold>,
    /// Hub-side queue per record key, ascending by position (see `crate::filter`).
    #[serde(default)]
    pending: BTreeMap<(String, String), Vec<Pending>>,
    /// Most entries the hub-side queue may hold for this node.
    #[serde(default = "default_limit")]
    limit: usize,
}

/// Default size limit of the hub-side queue per node (ADR-066 open-question starting value).
pub const DEFAULT_HOLD_LIMIT: usize = 100_000;

fn default_limit() -> usize {
    DEFAULT_HOLD_LIMIT
}

impl Default for HoldSet {
    fn default() -> Self {
        Self {
            holds: BTreeMap::new(),
            pending: BTreeMap::new(),
            limit: DEFAULT_HOLD_LIMIT,
        }
    }
}

/// One withheld outgoing entry with its hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pending {
    pub(crate) entry: Outgoing,
    pub(crate) hold: Hold,
}

impl HoldSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a hold. An older version than the one already held is ignored.
    pub fn insert(&mut self, hold: Hold) {
        let key = (hold.record.entity.clone(), hold.record.id.clone());
        match self.holds.get(&key) {
            Some(existing) if existing.record.version >= hold.record.version => {}
            _ => {
                self.holds.insert(key, hold);
            }
        }
    }

    /// A version of this record was applied: drop a hold on it that is not newer.
    pub fn applied(&mut self, record: &RecordRef) {
        let key = (record.entity.clone(), record.id.clone());
        if self
            .holds
            .get(&key)
            .is_some_and(|hold| hold.record.version <= record.version)
        {
            self.holds.remove(&key);
        }
    }

    /// The holds, ordered by entity then id.
    #[must_use]
    pub fn holds(&self) -> Vec<Hold> {
        self.holds.values().cloned().collect()
    }

    /// A set whose hub-side queue holds at most `limit` entries (a setting, default
    /// [`DEFAULT_HOLD_LIMIT`]).
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    /// The hub-side queue limit.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Number of held records: the receiver-side holds plus the hub-side queued entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.holds.len() + self.pending_len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.holds.is_empty() && self.pending.is_empty()
    }

    /// Entries withheld by the hub-side filter.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.values().map(Vec::len).sum()
    }

    /// The holds of the hub-side queue, ascending by position.
    #[must_use]
    pub fn pending_holds(&self) -> Vec<Hold> {
        let mut all: Vec<&Pending> = self.pending.values().flatten().collect();
        all.sort_by_key(|p| p.entry.position);
        all.into_iter().map(|p| p.hold.clone()).collect()
    }

    pub(crate) fn queue(&self, key: &(String, String)) -> Option<&Vec<Pending>> {
        self.pending.get(key)
    }

    pub(crate) fn queue_mut(&mut self, key: (String, String)) -> &mut Vec<Pending> {
        self.pending.entry(key).or_default()
    }

    pub(crate) fn pending_mut(&mut self) -> &mut BTreeMap<(String, String), Vec<Pending>> {
        &mut self.pending
    }
}

/// One record arriving in the stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incoming {
    /// The record.
    pub record: RecordRef,
    /// Its event topic.
    pub topic: Topic,
    /// Its schema major.
    pub major: Major,
    /// The metadata version it needs, if any.
    pub requires_metadata: Option<u32>,
}

/// The result of classifying a batch: each record is judged on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Triage {
    /// Records to apply, in arrival order.
    pub apply: Vec<RecordRef>,
    /// Records held; none of them stops the ones after it.
    pub held: Vec<Hold>,
}

/// Classify a batch against what the node can decode.
#[must_use]
pub fn triage(batch: &[Incoming], node: &NodeReport) -> Triage {
    let mut apply = Vec::new();
    let mut held = Vec::new();
    for item in batch {
        let reason = if !node.can_decode(&item.topic, item.major) {
            Some(HoldReason::UnsupportedMajor {
                topic: item.topic.clone(),
                major: item.major,
            })
        } else {
            item.requires_metadata
                .filter(|required| node.metadata_version < *required)
                .map(|required| HoldReason::MetadataTooLow { required })
        };
        match reason {
            Some(reason) => held.push(on_undecodable(item.record.clone(), reason)),
            None => apply.push(item.record.clone()),
        }
    }
    Triage { apply, held }
}
