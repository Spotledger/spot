//! Scoped subscriptions and the pure routing function (ADR-063 section 3).
//!
//! A [`Subscription`] is the hub's registry entry for one node: the scope keys the node holds and
//! the document types it receives. This is the **hub-side** record of what a branch is entitled
//! to, not a list the branch chooses for itself (ADR-063 rejects branch-chosen subscriptions as
//! the main mechanism: a branch cannot know what it lacks).
//!
//! [`Router::destinations`] is a **pure function**: a change goes to exactly the nodes whose
//! subscription covers it, with no state and no clock. A node covers a change when it subscribes
//! to the change's entity and holds every scope key the change requires. The platform never reads
//! a key type by name; routing compares opaque keys only.

use std::collections::BTreeMap;

use spot_sync_protocol::NodeId;

use crate::change::MasterChange;
use crate::error::{Error, Result};
use crate::scope::{Doctype, ScopeKey};

/// One node's hub-side subscription: the scope keys it holds and the entities it receives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscription {
    node: NodeId,
    scope_keys: Vec<ScopeKey>,
    doctypes: Vec<Doctype>,
}

impl Subscription {
    /// A subscription for `node`.
    ///
    /// The scope keys and document types are sorted and deduplicated, so two subscriptions built
    /// from the same sets compare equal whatever order they were given in.
    pub fn new(
        node: NodeId,
        scope_keys: impl IntoIterator<Item = ScopeKey>,
        doctypes: impl IntoIterator<Item = Doctype>,
    ) -> Self {
        let mut scope_keys: Vec<ScopeKey> = scope_keys.into_iter().collect();
        scope_keys.sort();
        scope_keys.dedup();
        let mut doctypes: Vec<Doctype> = doctypes.into_iter().collect();
        doctypes.sort();
        doctypes.dedup();
        Self {
            node,
            scope_keys,
            doctypes,
        }
    }

    /// The node this subscription is for.
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// The scope keys the node holds, sorted.
    pub fn scope_keys(&self) -> &[ScopeKey] {
        &self.scope_keys
    }

    /// The document types the node receives, sorted.
    pub fn doctypes(&self) -> &[Doctype] {
        &self.doctypes
    }

    /// Whether the node holds `key`.
    pub fn holds(&self, key: &ScopeKey) -> bool {
        self.scope_keys.binary_search(key).is_ok()
    }

    /// Whether the node subscribes to `doctype`.
    pub fn subscribes_to(&self, doctype: &Doctype) -> bool {
        self.doctypes.binary_search(doctype).is_ok()
    }

    /// Whether this subscription covers `change`: it subscribes to the change's entity and holds
    /// every scope key the change requires. An empty requirement set is covered by any
    /// subscriber of the entity (the `all` selector).
    pub fn covers(&self, change: &MasterChange) -> bool {
        self.subscribes_to(change.entity()) && change.scope_keys().iter().all(|key| self.holds(key))
    }
}

/// The routing table: every node's subscription, keyed by node.
#[derive(Clone, Debug, Default)]
pub struct Router {
    subscriptions: BTreeMap<NodeId, Subscription>,
}

impl Router {
    /// A router with no nodes.
    pub fn new() -> Self {
        Self::default()
    }

    /// A router from a set of subscriptions; fails when two name the same node.
    pub fn from_subscriptions(
        subscriptions: impl IntoIterator<Item = Subscription>,
    ) -> Result<Self> {
        let mut router = Self::new();
        for subscription in subscriptions {
            router.insert(subscription)?;
        }
        Ok(router)
    }

    /// Adds a node's subscription; fails when the node already has one.
    pub fn insert(&mut self, subscription: Subscription) -> Result<()> {
        if self.subscriptions.contains_key(subscription.node()) {
            return Err(Error::duplicate_subscription(subscription.node()));
        }
        self.subscriptions
            .insert(subscription.node().clone(), subscription);
        Ok(())
    }

    /// The nodes a change reaches, in node-id order. A pure function of the subscriptions and the
    /// change: no state, no clock, and the same change always yields the same destinations.
    pub fn destinations(&self, change: &MasterChange) -> Vec<NodeId> {
        self.subscriptions
            .values()
            .filter(|subscription| subscription.covers(change))
            .map(|subscription| subscription.node().clone())
            .collect()
    }

    /// The subscription of `node`, if any.
    pub fn subscription(&self, node: &NodeId) -> Option<&Subscription> {
        self.subscriptions.get(node)
    }

    /// Every node, in node-id order.
    pub fn nodes(&self) -> impl Iterator<Item = &NodeId> {
        self.subscriptions.keys()
    }

    /// How many nodes are registered.
    pub fn len(&self) -> usize {
        self.subscriptions.len()
    }

    /// Whether no node is registered.
    pub fn is_empty(&self) -> bool {
        self.subscriptions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_sync_protocol::RecordName;

    use super::*;

    fn doctype() -> Doctype {
        Doctype::new("price_list").unwrap()
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn change(keys: Vec<ScopeKey>) -> MasterChange {
        MasterChange::new(
            doctype(),
            RecordName::new("PL-1").unwrap(),
            1,
            keys,
            "100.00",
        )
        .unwrap()
    }

    fn key(key_type: &str, value: &str) -> ScopeKey {
        ScopeKey::new(key_type, value).unwrap()
    }

    #[test]
    fn a_change_reaches_only_nodes_that_hold_its_keys() {
        let router = Router::from_subscriptions([
            Subscription::new(node("edge-c1"), [key("company", "C1")], [doctype()]),
            Subscription::new(node("edge-c2"), [key("company", "C2")], [doctype()]),
        ])
        .unwrap();
        let destinations = router.destinations(&change(vec![key("company", "C1")]));
        assert_eq!(destinations, vec![node("edge-c1")]);
    }

    #[test]
    fn an_empty_requirement_reaches_every_subscriber_of_the_entity() {
        let router = Router::from_subscriptions([
            Subscription::new(node("edge-a"), [], [doctype()]),
            Subscription::new(node("edge-b"), [], [doctype()]),
            Subscription::new(node("edge-c"), [], [Doctype::new("other").unwrap()]),
        ])
        .unwrap();
        let destinations = router.destinations(&change(vec![]));
        assert_eq!(destinations, vec![node("edge-a"), node("edge-b")]);
    }

    #[test]
    fn a_node_missing_one_required_key_is_not_a_destination() {
        let router = Router::from_subscriptions([Subscription::new(
            node("edge-w7"),
            [key("company", "C1")],
            [doctype()],
        )])
        .unwrap();
        let destinations =
            router.destinations(&change(vec![key("company", "C1"), key("warehouse", "W7")]));
        assert!(destinations.is_empty());
    }

    #[test]
    fn a_duplicate_node_is_refused() {
        let subscription = Subscription::new(node("edge-a"), [], [doctype()]);
        let error = Router::from_subscriptions([subscription.clone(), subscription])
            .err()
            .unwrap();
        assert_eq!(error.code, crate::error::DUPLICATE_SUBSCRIPTION);
    }

    #[test]
    fn routing_is_deterministic_and_pure() {
        let router = Router::from_subscriptions([
            Subscription::new(node("edge-z"), [], [doctype()]),
            Subscription::new(node("edge-a"), [], [doctype()]),
        ])
        .unwrap();
        let change = change(vec![]);
        let first = router.destinations(&change);
        let second = router.destinations(&change);
        assert_eq!(first, second);
        assert_eq!(first, vec![node("edge-a"), node("edge-z")]);
    }
}
