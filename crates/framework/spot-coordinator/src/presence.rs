//! Second-node detection: how the in-process coordinator learns it is not alone.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::coordinator::BoxFuture;
use crate::error::Result;
use crate::types::NodeId;

/// A registry of the nodes that run against the same database. The database-backed presence is
/// WP-PLT-18.T2; [`InMemoryPresence`] is the fake tests use.
pub trait NodePresence: Send + Sync {
    /// Records that `node` is running, until the returned [`Registration`] is dropped.
    fn register<'a>(&'a self, node: &'a NodeId) -> BoxFuture<'a, Result<Registration>>;

    /// The nodes other than `me` that are running now.
    fn others<'a>(&'a self, me: &'a NodeId) -> BoxFuture<'a, Result<Vec<NodeId>>>;
}

/// Proof that a node is registered; dropping it withdraws the registration.
pub struct Registration {
    _guard: Box<dyn Send + Sync>,
}

impl Registration {
    /// A registration that withdraws itself when `guard` is dropped.
    pub fn new(guard: impl Send + Sync + 'static) -> Self {
        Self {
            _guard: Box::new(guard),
        }
    }
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Registration")
    }
}

/// A [`NodePresence`] in memory, for tests: clones share one set of nodes, so a test registers
/// "another node" with [`InMemoryPresence::add_node`].
#[derive(Clone, Debug, Default)]
pub struct InMemoryPresence {
    /// Node id to how many registrations hold it.
    nodes: Arc<Mutex<BTreeMap<NodeId, usize>>>,
}

impl InMemoryPresence {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a node as if another process had registered; it stays until
    /// [`InMemoryPresence::remove_node`].
    pub fn add_node(&self, node: &NodeId) {
        *self.lock().entry(node.clone()).or_insert(0) += 1;
    }

    /// Removes one registration of `node`.
    pub fn remove_node(&self, node: &NodeId) {
        Self::withdraw(&self.nodes, node);
    }

    fn withdraw(nodes: &Mutex<BTreeMap<NodeId, usize>>, node: &NodeId) {
        let mut nodes = nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = nodes.get_mut(node) {
            *count -= 1;
            if *count == 0 {
                nodes.remove(node);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<NodeId, usize>> {
        // A map of counts; a panic elsewhere cannot leave it inconsistent.
        self.nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Withdraws one registration when dropped.
struct Withdraw {
    nodes: Arc<Mutex<BTreeMap<NodeId, usize>>>,
    node: NodeId,
}

impl Drop for Withdraw {
    fn drop(&mut self) {
        InMemoryPresence::withdraw(&self.nodes, &self.node);
    }
}

impl NodePresence for InMemoryPresence {
    fn register<'a>(&'a self, node: &'a NodeId) -> BoxFuture<'a, Result<Registration>> {
        Box::pin(async move {
            self.add_node(node);
            Ok(Registration::new(Withdraw {
                nodes: Arc::clone(&self.nodes),
                node: node.clone(),
            }))
        })
    }

    fn others<'a>(&'a self, me: &'a NodeId) -> BoxFuture<'a, Result<Vec<NodeId>>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .keys()
                .filter(|node| *node != me)
                .cloned()
                .collect())
        })
    }
}
