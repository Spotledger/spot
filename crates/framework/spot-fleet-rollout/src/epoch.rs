//! The migration epoch of one tenant network and the version-skew rule edges sync against.
//!
//! A tenant network is its hub plus its edges (ADR-062). Each migration release has one
//! **epoch**: a monotonically increasing number. A node enters the ledger with the epoch it was
//! **verified** at; a node that never finished verify has no record at all. An edge may sync only
//! when it has a verified epoch and that epoch is not newer than the hub's and not further behind
//! than the network's [`SkewLimit`]. This is the data ADR-066's version negotiation will consume;
//! the wire negotiation itself is not built here.
//!
//! The rule is a pure function, [`may_sync`], so `spot-sync-protocol::Hello` handling can call it
//! with the edge's claimed `metadata_epoch`, the hub's epoch and the limit. The ledger variant,
//! [`EpochLedger::may_sync`], first turns "no record" into [`EdgeEpoch::Unverified`].

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::ids::{Moment, NodeId};

/// A monotonically increasing migration epoch of one tenant network.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Epoch(u64);

impl Epoch {
    /// The epoch before the first release.
    pub const GENESIS: Self = Self(0);

    /// An epoch from its integer value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The integer value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next epoch.
    ///
    /// # Errors
    ///
    /// [`Error::EpochOverflow`] at `u64::MAX`.
    pub fn next(self) -> Result<Self, Error> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(Error::EpochOverflow { current: self.0 })
    }
}

impl fmt::Display for Epoch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// How many epochs behind the hub an edge may be and still sync. Zero accepts only the hub's
/// epoch.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SkewLimit(u32);

impl SkewLimit {
    /// Only the hub's epoch is accepted.
    pub const STRICT: Self = Self(0);

    /// A limit in epochs.
    #[must_use]
    pub const fn new(limit: u32) -> Self {
        Self(limit)
    }

    /// The limit in epochs.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// What an edge claims about its epoch when it opens a session.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EdgeEpoch {
    /// The network has no record that this edge finished verify.
    Unverified,
    /// The epoch the edge was verified at.
    Verified(Epoch),
}

impl EdgeEpoch {
    /// The unverified claim.
    #[must_use]
    pub const fn unverified() -> Self {
        Self::Unverified
    }

    /// A verified epoch.
    #[must_use]
    pub const fn verified(epoch: Epoch) -> Self {
        Self::Verified(epoch)
    }
}

/// Why an edge may not sync against the hub's epoch.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkewRefusal {
    /// The edge finished no verified migration, so the hub cannot know what it holds.
    Unverified,
    /// The edge claims an epoch the hub has not reached; the hub cannot read its records.
    EdgeAhead {
        /// The edge's claimed epoch.
        edge: Epoch,
        /// The hub's epoch.
        hub: Epoch,
    },
    /// The edge is further behind the hub than the network allows.
    BeyondLimit {
        /// The edge's verified epoch.
        edge: Epoch,
        /// The hub's epoch.
        hub: Epoch,
        /// The allowed limit.
        limit: SkewLimit,
    },
}

impl fmt::Display for SkewRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unverified => formatter.write_str("the edge has no verified epoch"),
            Self::EdgeAhead { edge, hub } => {
                write!(
                    formatter,
                    "the edge is at epoch {edge}, ahead of the hub at {hub}"
                )
            }
            Self::BeyondLimit { edge, hub, limit } => write!(
                formatter,
                "the edge is at epoch {edge}, more than {limit} behind the hub at {hub}",
                limit = limit.get()
            ),
        }
    }
}

impl std::error::Error for SkewRefusal {}

/// Whether an edge at `edge_epoch` may sync against a hub at `hub_epoch`.
///
/// The rule is fail-closed and pure:
///
/// * an unverified edge is refused;
/// * an edge newer than the hub is refused (the hub cannot interpret a future epoch);
/// * an edge more than `skew_limit` epochs behind the hub is refused;
/// * otherwise the edge may sync.
///
/// # Errors
///
/// The [`SkewRefusal`] naming which rule refused the edge.
pub fn may_sync(
    edge_epoch: EdgeEpoch,
    hub_epoch: Epoch,
    skew_limit: SkewLimit,
) -> Result<(), SkewRefusal> {
    let edge = match edge_epoch {
        EdgeEpoch::Unverified => return Err(SkewRefusal::Unverified),
        EdgeEpoch::Verified(epoch) => epoch,
    };
    if edge > hub_epoch {
        return Err(SkewRefusal::EdgeAhead {
            edge,
            hub: hub_epoch,
        });
    }
    if hub_epoch.get() - edge.get() > u64::from(skew_limit.get()) {
        return Err(SkewRefusal::BeyondLimit {
            edge,
            hub: hub_epoch,
            limit: skew_limit,
        });
    }
    Ok(())
}

/// The ledger record of one node: the epoch it finished verify at, and when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochRecord {
    /// The node.
    pub node: NodeId,
    /// The epoch it was verified at.
    pub epoch: Epoch,
    /// The monotonic reading at which verify passed.
    pub verified_at: Moment,
}

/// The epoch ledger of one tenant network: its current epoch and the verified epoch of every
/// node that finished verify.
///
/// `current` only ever increases. A node's epoch only ever increases; recording an older epoch
/// for the same node is refused. A node is never recorded ahead of `current`: verify happened
/// against a release the network already knows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochLedger {
    current: Epoch,
    records: BTreeMap<NodeId, EpochRecord>,
}

impl EpochLedger {
    /// An empty ledger at [`Epoch::GENESIS`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The network's current epoch.
    #[must_use]
    pub fn current(&self) -> Epoch {
        self.current
    }

    /// Opens the next epoch and returns it. Call it once per release, before any node verifies.
    ///
    /// # Errors
    ///
    /// [`Error::EpochOverflow`] at `u64::MAX`.
    pub fn advance(&mut self) -> Result<Epoch, Error> {
        self.current = self.current.next()?;
        Ok(self.current)
    }

    /// Records that `node` finished verify at `epoch`.
    ///
    /// # Errors
    ///
    /// [`Error::EpochAhead`] when `epoch` is newer than the network's current epoch, and
    /// [`Error::EpochRegression`] when it is older than this node's previous record.
    pub fn record_verified(
        &mut self,
        node: NodeId,
        epoch: Epoch,
        verified_at: Moment,
    ) -> Result<(), Error> {
        if epoch > self.current {
            return Err(Error::EpochAhead {
                node: node.to_string(),
                requested: epoch.get(),
                current: self.current.get(),
            });
        }
        if let Some(previous) = self.records.get(&node)
            && epoch < previous.epoch
        {
            return Err(Error::EpochRegression {
                node: node.to_string(),
                previous: previous.epoch.get(),
                requested: epoch.get(),
            });
        }
        self.records.insert(
            node.clone(),
            EpochRecord {
                node,
                epoch,
                verified_at,
            },
        );
        Ok(())
    }

    /// Records `node` at the network's current epoch.
    ///
    /// # Errors
    ///
    /// [`Error::EpochRegression`] when the node already has a newer record.
    pub fn record_at_current(&mut self, node: NodeId, verified_at: Moment) -> Result<Epoch, Error> {
        let epoch = self.current;
        self.record_verified(node, epoch, verified_at)?;
        Ok(epoch)
    }

    /// The node's record, when it finished verify.
    #[must_use]
    pub fn record(&self, node: &NodeId) -> Option<&EpochRecord> {
        self.records.get(node)
    }

    /// The node's verified epoch, when it finished verify.
    #[must_use]
    pub fn epoch_of(&self, node: &NodeId) -> Option<Epoch> {
        self.records.get(node).map(|record| record.epoch)
    }

    /// Every node's record, in node-id order.
    pub fn records(&self) -> impl Iterator<Item = &EpochRecord> {
        self.records.values()
    }

    /// Whether `node` finished verify.
    #[must_use]
    pub fn is_verified(&self, node: &NodeId) -> bool {
        self.records.contains_key(node)
    }

    /// The skew rule for a node known to the ledger: a node with no record is unverified.
    ///
    /// # Errors
    ///
    /// The [`SkewRefusal`] that applies.
    pub fn may_sync(
        &self,
        node: &NodeId,
        hub_epoch: Epoch,
        skew_limit: SkewLimit,
    ) -> Result<(), SkewRefusal> {
        let claim = match self.records.get(node) {
            Some(record) => EdgeEpoch::Verified(record.epoch),
            None => EdgeEpoch::Unverified,
        };
        may_sync(claim, hub_epoch, skew_limit)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn node(name: &str) -> NodeId {
        NodeId::new(name).expect("a valid test node")
    }

    #[test]
    fn unverified_edge_is_refused() {
        let ledger = EpochLedger::new();
        assert_eq!(
            ledger.may_sync(&node("edge#1"), Epoch::new(3), SkewLimit::new(1)),
            Err(SkewRefusal::Unverified)
        );
    }

    #[test]
    fn an_edge_ahead_of_the_hub_is_refused() {
        assert_eq!(
            may_sync(
                EdgeEpoch::Verified(Epoch::new(4)),
                Epoch::new(3),
                SkewLimit::new(9)
            ),
            Err(SkewRefusal::EdgeAhead {
                edge: Epoch::new(4),
                hub: Epoch::new(3)
            })
        );
    }

    #[test]
    fn skew_boundary_is_inclusive() {
        assert!(
            may_sync(
                EdgeEpoch::Verified(Epoch::new(2)),
                Epoch::new(4),
                SkewLimit::new(2)
            )
            .is_ok()
        );
        assert_eq!(
            may_sync(
                EdgeEpoch::Verified(Epoch::new(1)),
                Epoch::new(4),
                SkewLimit::new(2)
            ),
            Err(SkewRefusal::BeyondLimit {
                edge: Epoch::new(1),
                hub: Epoch::new(4),
                limit: SkewLimit::new(2)
            })
        );
    }

    #[test]
    fn ledger_records_and_refuses_regressions() {
        let mut ledger = EpochLedger::new();
        ledger.advance().expect("advance");
        ledger.advance().expect("advance");
        let edge = node("edge#1");
        ledger
            .record_verified(edge.clone(), Epoch::new(2), Moment::from_secs(1))
            .expect("record");
        assert_eq!(ledger.epoch_of(&edge), Some(Epoch::new(2)));
        assert!(ledger.is_verified(&edge));
        assert_eq!(
            ledger.record_verified(edge.clone(), Epoch::new(1), Moment::from_secs(2)),
            Err(Error::EpochRegression {
                node: "edge#1".to_owned(),
                previous: 2,
                requested: 1
            })
        );
    }

    #[test]
    fn a_future_epoch_is_refused() {
        let mut ledger = EpochLedger::new();
        assert_eq!(
            ledger.record_verified(node("edge#1"), Epoch::new(1), Moment::ZERO),
            Err(Error::EpochAhead {
                node: "edge#1".to_owned(),
                requested: 1,
                current: 0
            })
        );
    }
}
