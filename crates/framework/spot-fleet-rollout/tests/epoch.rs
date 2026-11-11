//! The epoch ledger never moves backwards, and the skew rule refuses exactly the unverified,
//! future and too-far-behind edges.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;

use proptest::prelude::*;
use spot_fleet_rollout::{
    EdgeEpoch, Epoch, EpochLedger, Moment, NodeId, SkewLimit, SkewRefusal, may_sync,
};
use support::node;

#[test]
fn an_edge_without_a_verified_epoch_is_refused() {
    let ledger = EpochLedger::new();
    assert_eq!(
        ledger.may_sync(&node("edge#1"), Epoch::new(4), SkewLimit::new(9)),
        Err(SkewRefusal::Unverified)
    );
    assert_eq!(
        may_sync(EdgeEpoch::unverified(), Epoch::new(4), SkewLimit::new(9)),
        Err(SkewRefusal::Unverified)
    );
}

#[test]
fn an_edge_ahead_of_the_hub_is_always_refused() {
    assert_eq!(
        may_sync(
            EdgeEpoch::verified(Epoch::new(6)),
            Epoch::new(5),
            SkewLimit::new(100)
        ),
        Err(SkewRefusal::EdgeAhead {
            edge: Epoch::new(6),
            hub: Epoch::new(5),
        })
    );
}

#[test]
fn the_skew_limit_is_inclusive_and_measured_behind_the_hub() {
    assert!(
        may_sync(
            EdgeEpoch::verified(Epoch::new(3)),
            Epoch::new(5),
            SkewLimit::new(2)
        )
        .is_ok()
    );
    assert_eq!(
        may_sync(
            EdgeEpoch::verified(Epoch::new(2)),
            Epoch::new(5),
            SkewLimit::new(2)
        ),
        Err(SkewRefusal::BeyondLimit {
            edge: Epoch::new(2),
            hub: Epoch::new(5),
            limit: SkewLimit::new(2),
        })
    );
    assert_eq!(
        may_sync(
            EdgeEpoch::verified(Epoch::new(5)),
            Epoch::new(5),
            SkewLimit::STRICT
        ),
        Ok(())
    );
}

#[test]
fn the_ledger_assigns_the_current_epoch_and_refuses_regressions() {
    let mut ledger = EpochLedger::new();
    assert_eq!(ledger.current(), Epoch::GENESIS);
    assert_eq!(ledger.advance().unwrap(), Epoch::new(1));
    assert_eq!(ledger.advance().unwrap(), Epoch::new(2));

    let edge = node("edge#1");
    assert_eq!(
        ledger
            .record_at_current(edge.clone(), Moment::from_secs(90))
            .unwrap(),
        Epoch::new(2)
    );
    let record = ledger.record(&edge).unwrap();
    assert_eq!(record.epoch, Epoch::new(2));
    assert_eq!(record.verified_at, Moment::from_secs(90));
    assert!(
        ledger
            .may_sync(&edge, Epoch::new(2), SkewLimit::STRICT)
            .is_ok()
    );

    assert_eq!(
        ledger.record_verified(edge.clone(), Epoch::new(1), Moment::from_secs(91)),
        Err(spot_fleet_rollout::Error::EpochRegression {
            node: "edge#1".to_owned(),
            previous: 2,
            requested: 1,
        })
    );
    assert_eq!(ledger.epoch_of(&edge), Some(Epoch::new(2)));
}

#[test]
fn a_node_cannot_be_recorded_past_the_network_epoch() {
    let mut ledger = EpochLedger::new();
    assert_eq!(
        ledger.record_verified(node("edge#1"), Epoch::new(1), Moment::ZERO),
        Err(spot_fleet_rollout::Error::EpochAhead {
            node: "edge#1".to_owned(),
            requested: 1,
            current: 0,
        })
    );
}

#[test]
fn the_overflow_boundary_is_refused() {
    // `Epoch::MAX` is not exposed; drive the type through its constructor and `next`.
    assert_eq!(
        Epoch::new(u64::MAX).next().unwrap_err().code(),
        "fleet_rollout.epoch_overflow"
    );
}

proptest! {
    /// No sequence of advances and records can lower the network epoch or a node's epoch, and a
    /// node's recorded epoch is never ahead of the network.
    #[test]
    fn ledger_monotonicity_holds(
        operations in prop::collection::vec((0_u8..3, 0_u8..10, 0_u8..12), 0..80)
    ) {
        let mut ledger = EpochLedger::new();
        let mut node_epochs: BTreeMap<NodeId, Epoch> = BTreeMap::new();
        let mut last_current = ledger.current();
        for (kind, node_index, epoch) in operations {
            let epoch = Epoch::new(u64::from(epoch));
            match kind {
                0 => {
                    let advanced = ledger.advance().unwrap();
                    prop_assert!(advanced >= last_current);
                    last_current = advanced;
                }
                1 => {
                    let node = NodeId::new(format!("edge#{node_index}")).unwrap();
                    let before = node_epochs.get(&node).copied();
                    let result = ledger.record_verified(node.clone(), epoch, Moment::ZERO);
                    if result.is_ok() {
                        prop_assert!(epoch >= before.unwrap_or(Epoch::GENESIS));
                        prop_assert_eq!(ledger.epoch_of(&node), Some(epoch));
                        node_epochs.insert(node, epoch);
                    } else {
                        prop_assert_eq!(ledger.epoch_of(&node), before);
                    }
                }
                _ => {
                    let node = NodeId::new(format!("edge#{node_index}")).unwrap();
                    let before = node_epochs.get(&node).copied();
                    let result = ledger.record_at_current(node.clone(), Moment::ZERO);
                    if result.is_ok() {
                        prop_assert!(ledger.current() >= before.unwrap_or(Epoch::GENESIS));
                        node_epochs.insert(node, ledger.current());
                    }
                }
            }
            prop_assert!(ledger.current() >= last_current);
            for record in ledger.records() {
                prop_assert!(record.epoch <= ledger.current());
            }
            for (node, epoch) in &node_epochs {
                prop_assert!(ledger.epoch_of(node) >= Some(*epoch));
            }
        }
    }
}
