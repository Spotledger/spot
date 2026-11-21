//! The edge's side of the shared balances: what the node learns from head office, what it may
//! spend while offline, and the delivery it owes.
//!
//! The library ([`spot_shared_balance::NodeView`]) keeps the node's record; the hub's side
//! (`receive_batch` with the authenticated node, quarantine of refusals) lives in the hub host.
//! This wrapper adds only the **policy gate** in front of a spend, using the crate's own
//! [`check_spend`]: online, a spend always proceeds to the hub's decision; offline it needs the
//! balance's policy to allow it. The default is the provisional `OnlineOnly` (the architecture owner's business
//! decision is still open; nothing here picks another default), and an unknown policy text is
//! `OnlineOnly` too.
//!
//! * [`EdgeBalances::learn`] notes head office's total (an online confirmation, a first sync).
//! * [`EdgeBalances::apply_reply`] applies head office's reply to a delivery whole: it
//!   acknowledges what was handled, learns every total and drops the quotas the hub closed.
//! * [`EdgeBalances::delivery`] is what the node owes head office (the unacknowledged entries);
//!   resending after a lost reply is safe (the hub recognises an entry id).

use std::collections::BTreeMap;

use spot_shared_balance::{BalanceId, Batch, HubReply, Millis, NodeId, NodeView};

use crate::balance::{BalancePolicy, OnlineOnly, check_spend};

/// The edge's balances.
#[derive(Clone, Debug)]
pub struct EdgeBalances {
    view: NodeView,
    default: BalancePolicy,
    overrides: BTreeMap<String, BalancePolicy>,
}

impl EdgeBalances {
    /// A view for `node` with the site's default policy and per-balance overrides.
    pub fn new(
        node: NodeId,
        default: BalancePolicy,
        overrides: BTreeMap<String, BalancePolicy>,
    ) -> Self {
        Self {
            view: NodeView::new(node),
            default,
            overrides,
        }
    }

    /// As [`EdgeBalances::new`] for a node named by text.
    pub fn for_node(
        node: &str,
        default: BalancePolicy,
        overrides: BTreeMap<String, BalancePolicy>,
    ) -> Self {
        Self::new(NodeId::new(node), default, overrides)
    }

    /// Restores a persisted view.
    pub fn with_view(mut self, view: NodeView) -> Self {
        self.view = view;
        self
    }

    /// The node's record (persist it across a restart).
    pub fn view(&self) -> &NodeView {
        &self.view
    }

    /// The policy of `balance`: its override, else the default.
    pub fn policy_for(&self, balance: &BalanceId) -> BalancePolicy {
        self.overrides
            .get(&balance.0)
            .copied()
            .unwrap_or(self.default)
    }

    /// Notes head office's total for `balance` as of `at`.
    pub fn learn(&mut self, balance: &BalanceId, total: i64, at: Millis) {
        self.view.learn_balance(balance, total, at);
    }

    /// Applies head office's reply to a delivery.
    pub fn apply_reply(&mut self, reply: &HubReply) {
        self.view.apply_hub_reply(reply);
    }

    /// Whether the node may spend from `balance` now. Online it always may (the hub decides);
    /// offline the policy decides, and an allocated quota must have something left.
    ///
    /// # Errors
    ///
    /// [`OnlineOnly`] (`edge_offline.balance_online_only`) when the spend is refused.
    pub fn check(&self, balance: &BalanceId, offline: bool) -> Result<(), OnlineOnly> {
        let quota_left = self
            .view
            .quota_remaining(balance)
            .is_some_and(|left| left > 0);
        check_spend(self.policy_for(balance), offline, quota_left)
    }

    /// What the node owes head office: every unacknowledged entry, quotas kept.
    pub fn delivery(&self) -> Batch {
        self.view.progress_batch()
    }

    /// How many entries wait for an acknowledgement.
    pub fn pending(&self) -> usize {
        self.view.pending().len()
    }
}
