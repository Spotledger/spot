//! One owning node per bookable resource (ADR-087 section 6.5).
//!
//! An exclusion constraint is atomic inside one database. A tenant's network is a hub plus edges,
//! each with its own database, so "no two bookings on the same dates" holds only if every
//! occupancy row of one unit is written by one node. Every unit has exactly one owner: the hub by
//! default, or one named home edge. A non-owner never holds or confirms; it submits a request
//! through the owner (an edge-side request flow needs WP-SYN-12 and is not shipped here).

use std::collections::BTreeMap;
use std::fmt;

use spot_storage_contract::RecordId;

/// A node of the tenant's network.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Owner {
    /// The hub: the default owner of every unit.
    Hub,
    /// A named home edge.
    Edge(String),
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Owner::Hub => f.write_str("the hub"),
            Owner::Edge(name) => write!(f, "edge `{name}`"),
        }
    }
}

/// Which node owns which unit. A platform default that a business process may override: the
/// override is the routing table the deployment supplies.
pub trait OwnerRoutes: Send + Sync {
    /// The one node that owns `unit`.
    fn owner_of(&self, unit: RecordId) -> Owner;
}

/// Every unit is owned by the hub (the default).
#[derive(Clone, Copy, Debug, Default)]
pub struct HubOwnsAll;

impl OwnerRoutes for HubOwnsAll {
    fn owner_of(&self, _unit: RecordId) -> Owner {
        Owner::Hub
    }
}

/// An explicit table of home edges; every unit not listed belongs to the hub.
#[derive(Clone, Debug, Default)]
pub struct HomeEdges {
    homes: BTreeMap<RecordId, String>,
}

impl HomeEdges {
    /// An empty table: the hub owns everything.
    pub fn new() -> Self {
        Self::default()
    }

    /// Names `edge` as the home of `unit`.
    pub fn with_home(mut self, unit: RecordId, edge: &str) -> Self {
        self.homes.insert(unit, edge.to_owned());
        self
    }
}

impl OwnerRoutes for HomeEdges {
    fn owner_of(&self, unit: RecordId) -> Owner {
        match self.homes.get(&unit) {
            Some(edge) => Owner::Edge(edge.clone()),
            None => Owner::Hub,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_hub_owns_what_no_edge_claims() {
        let a = RecordId::new_v7();
        let b = RecordId::new_v7();
        let routes = HomeEdges::new().with_home(a, "branch1");
        assert_eq!(routes.owner_of(a), Owner::Edge("branch1".into()));
        assert_eq!(routes.owner_of(b), Owner::Hub);
        assert_eq!(HubOwnsAll.owner_of(a), Owner::Hub);
    }
}
