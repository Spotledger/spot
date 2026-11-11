//! Whether a node is entitled to a range.
//!
//! Two implementations, and the difference matters:
//!
//! * [`LiveRegistryStanding`] holds a **shared handle** on the hub's real
//!   [`NodeRegistry`](spot_offline_identity::NodeRegistry). A suspension or decommission made
//!   through the hub's registry is seen by the very next request, because there is one registry
//!   and not a copy of it. **The hub wires this one.**
//! * [`RegistryStanding`] holds a **snapshot** of a registry, for tests and for an in-memory
//!   hub. A change made on some other copy of the registry does not reach it, so wiring it to a
//!   live registry silently keeps granting ranges to a node that was suspended on the hub.
//!
//! A port that cannot answer is
//! [`StandingUnavailable`](crate::StandingUnavailable) and the service refuses: failing closed is
//! the only safe answer when nobody can say whether the node is still allowed to name documents.

use std::sync::{Arc, Mutex};

use spot_offline_identity::{NodeRegistry, NodeState};
use spot_sync_protocol::NodeId;

/// A node's standing on the hub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Taking work. The only standing that is granted a range.
    Active,
    /// Suspended: refused.
    Suspended,
    /// Decommissioned: refused for good.
    Decommissioned,
    /// The node reported an older state than the hub gave it, and an operator has not yet
    /// re-enrolled it: refused (see [`ReenrolmentStanding`](crate::ReenrolmentStanding)).
    ReenrolmentRequired,
    /// The hub does not know the node: refused.
    Unknown,
}

/// The standing source could not answer. The service fails closed on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StandingUnavailable;

/// Reads a node's standing.
pub trait StandingPort {
    /// The standing of `node`.
    ///
    /// # Errors
    ///
    /// [`StandingUnavailable`] when the answer cannot be read; the service then refuses.
    fn standing(&self, node: &NodeId) -> Result<Standing, StandingUnavailable>;
}

/// A [`StandingPort`] over a **shared** [`NodeRegistry`]: the live view the hub wires.
///
/// The handle is `Arc<Mutex<..>>` because the registry is the hub's own value: the hub suspends
/// a node on it, and the grant service reads the same registry on the next request. A poisoned
/// lock is [`StandingUnavailable`], so a panic elsewhere never turns into a grant.
#[derive(Clone, Debug)]
pub struct LiveRegistryStanding {
    registry: Arc<Mutex<NodeRegistry>>,
}

impl LiveRegistryStanding {
    /// A port over the hub's live `registry`.
    #[must_use]
    pub fn new(registry: Arc<Mutex<NodeRegistry>>) -> Self {
        Self { registry }
    }

    /// The shared registry, so the hub can suspend, decommission or reinstate a node.
    #[must_use]
    pub fn handle(&self) -> Arc<Mutex<NodeRegistry>> {
        Arc::clone(&self.registry)
    }
}

impl StandingPort for LiveRegistryStanding {
    fn standing(&self, node: &NodeId) -> Result<Standing, StandingUnavailable> {
        let registry = self.registry.lock().map_err(|_| StandingUnavailable)?;
        Ok(standing_of(&registry, node))
    }
}

/// A [`StandingPort`] over a **snapshot** of a [`NodeRegistry`] (tests, and an in-memory hub).
///
/// Prefer [`LiveRegistryStanding`] wherever the hub holds the registry itself: this one sees
/// only the copy it was given.
#[derive(Clone, Debug)]
pub struct RegistryStanding {
    registry: NodeRegistry,
}

impl RegistryStanding {
    /// A port over (a snapshot of) `registry`.
    #[must_use]
    pub fn new(registry: NodeRegistry) -> Self {
        Self { registry }
    }

    /// The registry, for the hub to suspend or decommission a node and see it take effect.
    pub fn registry_mut(&mut self) -> &mut NodeRegistry {
        &mut self.registry
    }
}

impl StandingPort for RegistryStanding {
    fn standing(&self, node: &NodeId) -> Result<Standing, StandingUnavailable> {
        Ok(standing_of(&self.registry, node))
    }
}

/// The standing a registry reports for `node`.
///
/// The registry has its own node id type (a plain validated string). A node id it refuses is a
/// node it cannot know.
fn standing_of(registry: &NodeRegistry, node: &NodeId) -> Standing {
    let Ok(id) = spot_offline_identity::NodeId::new(node.as_str()) else {
        return Standing::Unknown;
    };
    match registry.state(&id) {
        Some(NodeState::Active) => Standing::Active,
        Some(NodeState::Suspended) => Standing::Suspended,
        Some(NodeState::Decommissioned) => Standing::Decommissioned,
        None => Standing::Unknown,
    }
}
