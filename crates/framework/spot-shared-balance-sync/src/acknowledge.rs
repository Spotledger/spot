//! [`Acknowledgeable`]: the proof that a delivery may be acknowledged.
//!
//! A delivery is acknowledged to its sender only when every entry head office refused is held in
//! the quarantine queue. Acknowledging earlier and then crashing loses the refused entries: the
//! sender believes they are handled, the ledger does not have them and the queue never received
//! them. The value in this module makes the order a matter of types. Its fields are private and
//! its only constructor is private to the crate, so the one way to obtain it is
//! [`receive_and_flush`](crate::receive_and_flush), which returns it **after** the flush
//! succeeded. A sync layer whose acknowledgement function takes an `Acknowledgeable` cannot be
//! written so that it acknowledges first.
//!
//! ```compile_fail
//! use spot_shared_balance::{NodeId, Reconciliation};
//! // The fields are private: a caller cannot build the proof itself.
//! let _ = spot_shared_balance_sync::Acknowledgeable {
//!     node: NodeId::new("branch-01"),
//!     reconciliation: Reconciliation::default(),
//! };
//! ```

use spot_shared_balance::{HubReply, NodeId, Reconciliation};

/// A settled delivery whose refused entries are all held in the quarantine queue, so it may be
/// acknowledged to its sender.
///
/// Only [`receive_and_flush`](crate::receive_and_flush) builds one. It is neither `Clone` nor
/// constructible by hand, and it is `#[must_use]`: dropping it unread is the sign of a sync layer
/// that forgot to acknowledge.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a delivery that was settled and flushed must be acknowledged to its sender"]
pub struct Acknowledgeable {
    node: NodeId,
    reply: HubReply,
}

impl Acknowledgeable {
    /// Built by [`receive_and_flush`](crate::receive_and_flush) after the flush succeeded.
    pub(crate) fn new(node: NodeId, reply: HubReply) -> Self {
        Self { node, reply }
    }

    /// The node the acknowledgement is for: the node the transport authenticated.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// Head office's reply, whole: what it did and the balances as it sees them. The sender
    /// applies it with
    /// [`NodeView::apply_hub_reply`](spot_shared_balance::NodeView::apply_hub_reply), which also
    /// learns those balances, so the acknowledgement and the learned totals cannot part.
    #[must_use]
    pub fn reply(&self) -> &HubReply {
        &self.reply
    }

    /// What head office did with the delivery.
    #[must_use]
    pub fn reconciliation(&self) -> &Reconciliation {
        self.reply.reconciliation()
    }

    /// Consumes the proof and returns head office's reply, once the acknowledgement is sent.
    #[must_use]
    pub fn into_reply(self) -> HubReply {
        self.reply
    }

    /// Consumes the proof and returns what head office did, once the acknowledgement is sent.
    #[must_use]
    pub fn into_reconciliation(self) -> Reconciliation {
        self.reply.into_reconciliation()
    }
}
