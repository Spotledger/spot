//! The node identity the transport authenticated, which is the only authority a batch has.

use spot_shared_balance::NodeId;

/// The node the connection authenticated as: the mutual-TLS certificate the hub's node
/// directory resolved (ADR-062 section 3).
///
/// This is deliberately **not** a field a sender can put on the wire. The sync layer builds it
/// from the transport's answer — the node directory's `AuthenticatedNode` — and hands it to
/// [`receive_batch`](crate::receive_batch). Everything a batch says about itself (its `node`,
/// and each entry's node) must agree with this value; nothing else is trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeIdentity {
    node: NodeId,
}

impl NodeIdentity {
    /// The identity of the node the transport authenticated.
    ///
    /// Call this with the node the certificate resolved to, never with a value read from a
    /// request payload.
    #[must_use]
    pub fn authenticated(node: NodeId) -> Self {
        Self { node }
    }

    /// The authenticated node.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }
}
