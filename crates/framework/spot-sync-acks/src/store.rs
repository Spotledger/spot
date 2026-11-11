//! The state the ledger keeps, and the trait that holds it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_sync_obligations::{Crossing, Level, MandatoryEvent, NodeState};

/// What the hub knows about one node **for one tenant**, in hub time throughout. A node serving
/// several tenants has one record per tenant: each tenant has its own journal and its own
/// position space (ADR-061), so a prefix means nothing without its tenant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRecord {
    /// The node id.
    pub node: String,
    /// The tenant whose stream the prefix, holds and level are about.
    pub tenant: String,
    /// The applied prefix the node last reported for this tenant.
    pub applied_prefix: u64,
    /// Positions of mandatory records the node holds back (parked, held for a version,
    /// quarantined). A hold does not lower the prefix.
    pub holds: Vec<u64>,
    /// When the hub last heard from the node (an ack or a check-in), milliseconds.
    pub last_contact_millis: i64,
    /// The software version tag the node reported.
    pub version: String,
    /// The node's level at the last evaluation: the last notified level of the ladder.
    pub level: Level,
    /// Recorded prefix crossings, oldest first.
    pub crossings: Vec<Crossing>,
}

impl NodeRecord {
    /// The record in the shape the pure report functions take.
    #[must_use]
    pub fn to_state(&self) -> NodeState {
        NodeState {
            node: self.node.clone(),
            applied_prefix: self.applied_prefix,
            holds: self.holds.clone(),
            last_seen_millis: self.last_contact_millis,
            crossings: self.crossings.clone(),
        }
    }
}

/// Where the ledger keeps its state: the registered mandatory events of each tenant and one
/// record per `(node, tenant)`.
///
/// Every call is infallible, like the conditions crate's `AuditSink`: a durable implementation
/// owns its own retry queue. The ledger keeps no state of its own besides the conditions engine,
/// so a ledger built over a store holding the same records computes the same levels. The tenant
/// is part of every key and there is no call without it, so an implementation cannot answer one
/// tenant's question from another tenant's rows.
pub trait NodeStateStore {
    /// The record of `node` for `tenant`, if it is registered.
    fn load(&self, node: &str, tenant: &str) -> Option<NodeRecord>;
    /// Insert or replace the record keyed by the record's `(node, tenant)`.
    fn save(&mut self, record: NodeRecord);
    /// Every registered `(node, tenant)` pair as `(node, tenant)`, in (tenant, node) order.
    fn nodes(&self) -> Vec<(String, String)>;
    /// Every mandatory event registered for `tenant`, in position order.
    fn events(&self, tenant: &str) -> Vec<MandatoryEvent>;
    /// Register an event of `tenant` at its position (the ledger has already checked it is new
    /// for that tenant).
    fn save_event(&mut self, tenant: &str, event: MandatoryEvent);
}

/// An in-memory store, for tests and for a hub that persists by snapshot.
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    /// Keyed by (tenant, node).
    nodes: BTreeMap<(String, String), NodeRecord>,
    /// Keyed by (tenant, position).
    events: BTreeMap<(String, u64), MandatoryEvent>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl NodeStateStore for MemoryStore {
    fn load(&self, node: &str, tenant: &str) -> Option<NodeRecord> {
        self.nodes
            .get(&(tenant.to_owned(), node.to_owned()))
            .cloned()
    }

    fn save(&mut self, record: NodeRecord) {
        self.nodes
            .insert((record.tenant.clone(), record.node.clone()), record);
    }

    fn nodes(&self) -> Vec<(String, String)> {
        self.nodes
            .keys()
            .map(|(tenant, node)| (node.clone(), tenant.clone()))
            .collect()
    }

    fn events(&self, tenant: &str) -> Vec<MandatoryEvent> {
        self.events
            .range((tenant.to_owned(), 0)..=(tenant.to_owned(), u64::MAX))
            .map(|(_, e)| e.clone())
            .collect()
    }

    fn save_event(&mut self, tenant: &str, event: MandatoryEvent) {
        self.events
            .insert((tenant.to_owned(), event.position), event);
    }
}
