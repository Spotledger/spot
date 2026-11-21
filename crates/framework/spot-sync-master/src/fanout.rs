//! The fan-out planner: batches per destination, deterministic order, idempotent re-emit.
//!
//! The hub does not send one message per change per branch. It turns a set of published changes
//! into one batch per destination (ADR-063: routing is evaluated on the sender, per record
//! version), and it does so **deterministically** and **idempotently**:
//!
//! * the destinations of a batch are in node-id order, and the changes inside it are in
//!   `(name, version)` order, so the same input always produces the same plan;
//! * a `(node, name, version)` already planned is never planned again, so re-emitting a change
//!   (an at-least-once retry, a replay, ADR-062 G4) adds nothing.

use std::collections::BTreeMap;

use spot_sync_protocol::{NodeId, RecordKey};

use crate::change::MasterChange;
use crate::subscription::Router;

/// One destination's batch: the changes to send it, in `(name, version)` order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationBatch {
    node: NodeId,
    changes: Vec<MasterChange>,
}

impl DestinationBatch {
    /// The destination node.
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// The changes for this node, in `(name, version)` order.
    pub fn changes(&self) -> &[MasterChange] {
        &self.changes
    }
}

/// A plan: one batch per destination, destinations in node-id order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FanoutPlan {
    batches: Vec<DestinationBatch>,
}

impl FanoutPlan {
    /// The batches, in node-id order.
    pub fn batches(&self) -> &[DestinationBatch] {
        &self.batches
    }

    /// Whether the plan sends nothing.
    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    /// How many destinations the plan reaches.
    pub fn destinations(&self) -> usize {
        self.batches.len()
    }

    /// How many change deliveries the plan carries in total.
    pub fn deliveries(&self) -> usize {
        self.batches.iter().map(|batch| batch.changes.len()).sum()
    }
}

/// The planner: a routing table plus the record of what it has already planned.
///
/// Keep one planner per hub stream so re-emits are absorbed. A planner with no history behaves
/// like a pure `plan` over the router.
#[derive(Clone, Debug, Default)]
pub struct FanoutPlanner {
    emitted: BTreeMap<NodeId, Vec<RecordKey>>,
}

impl FanoutPlanner {
    /// A planner that has planned nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Plans `changes` to their destinations.
    ///
    /// Every `(node, name, version)` is planned at most once across the planner's life: a change
    /// already planned for a node is skipped, so calling `plan` again with the same input yields
    /// an empty plan. Changes in one call are deduplicated the same way, and the result is
    /// deterministic.
    pub fn plan(&mut self, router: &Router, changes: &[MasterChange]) -> FanoutPlan {
        let mut by_node: BTreeMap<NodeId, BTreeMap<RecordKey, MasterChange>> = BTreeMap::new();
        for change in changes {
            let key = change.key();
            for node in router.destinations(change) {
                let emitted = self.emitted.entry(node.clone()).or_default();
                if emitted.binary_search(&key).is_ok() {
                    continue;
                }
                let position = emitted.partition_point(|held| held < &key);
                emitted.insert(position, key.clone());
                by_node
                    .entry(node)
                    .or_default()
                    .entry(key.clone())
                    .or_insert_with(|| change.clone());
            }
        }
        let batches = by_node
            .into_iter()
            .map(|(node, changes)| DestinationBatch {
                node,
                changes: changes.into_values().collect(),
            })
            .collect();
        FanoutPlan { batches }
    }

    /// How many `(name, version)` deliveries have been planned for `node`.
    pub fn emitted(&self, node: &NodeId) -> usize {
        self.emitted.get(node).map_or(0, Vec::len)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_sync_protocol::RecordName;

    use super::*;
    use crate::scope::{Doctype, ScopeKey};
    use crate::subscription::Subscription;

    fn doctype() -> Doctype {
        Doctype::new("price_list").unwrap()
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn change(version: u64, value: &str) -> MasterChange {
        MasterChange::new(
            doctype(),
            RecordName::new("PL-1").unwrap(),
            version,
            [],
            value,
        )
        .unwrap()
    }

    fn router() -> Router {
        Router::from_subscriptions([
            Subscription::new(node("edge-b"), [], [doctype()]),
            Subscription::new(node("edge-a"), [], [doctype()]),
        ])
        .unwrap()
    }

    #[test]
    fn batches_are_per_destination_in_deterministic_order() {
        let mut planner = FanoutPlanner::new();
        let plan = planner.plan(&router(), &[change(2, "v2"), change(1, "v1")]);
        assert_eq!(plan.destinations(), 2);
        let nodes: Vec<_> = plan.batches().iter().map(|b| b.node().clone()).collect();
        assert_eq!(nodes, vec![node("edge-a"), node("edge-b")]);
        for batch in plan.batches() {
            let versions: Vec<_> = batch.changes().iter().map(MasterChange::version).collect();
            assert_eq!(versions, vec![1, 2], "changes are in (name, version) order");
        }
        assert_eq!(plan.deliveries(), 4);
    }

    #[test]
    fn re_emitting_the_same_change_adds_nothing() {
        let mut planner = FanoutPlanner::new();
        let first = planner.plan(&router(), &[change(1, "v1")]);
        assert_eq!(first.deliveries(), 2);
        let again = planner.plan(&router(), &[change(1, "v1")]);
        assert!(again.is_empty());
        assert_eq!(planner.emitted(&node("edge-a")), 1);
    }

    #[test]
    fn duplicates_within_one_call_are_planned_once() {
        let mut planner = FanoutPlanner::new();
        let plan = planner.plan(&router(), &[change(1, "v1"), change(1, "v1")]);
        assert_eq!(plan.deliveries(), 2);
    }

    #[test]
    fn a_new_version_is_planned_after_an_old_one() {
        let mut planner = FanoutPlanner::new();
        planner.plan(&router(), &[change(1, "v1")]);
        let plan = planner.plan(&router(), &[change(2, "v2")]);
        assert_eq!(plan.deliveries(), 2);
        for batch in plan.batches() {
            assert_eq!(batch.changes()[0].version(), 2);
        }
    }

    #[test]
    fn a_scope_key_change_reroutes() {
        let router = Router::from_subscriptions([
            Subscription::new(
                node("edge-c1"),
                [ScopeKey::new("company", "C1").unwrap()],
                [doctype()],
            ),
            Subscription::new(
                node("edge-c2"),
                [ScopeKey::new("company", "C2").unwrap()],
                [doctype()],
            ),
        ])
        .unwrap();
        let scoped = MasterChange::new(
            doctype(),
            RecordName::new("PL-1").unwrap(),
            1,
            [ScopeKey::new("company", "C2").unwrap()],
            "v1",
        )
        .unwrap();
        let mut planner = FanoutPlanner::new();
        let plan = planner.plan(&router, &[scoped]);
        assert_eq!(plan.destinations(), 1);
        assert_eq!(plan.batches()[0].node(), &node("edge-c2"));
    }
}
