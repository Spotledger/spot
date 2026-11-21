//! A node's side: what it knows, what it has spent, and what it still has to report.

use std::collections::BTreeMap;

use crate::hub::Grant;
use crate::ledger::{BalanceId, Entry, EntryId, Ledger, Millis, NodeId};
use crate::policy::Batch;
use crate::reply::HubReply;

/// The part of a [`Grant`] a node holds while offline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quota {
    /// What head office granted.
    pub amount: i64,
    /// What this node has spent from it.
    pub spent: i64,
    /// When the node must stop spending.
    pub expires_at: Millis,
}

#[derive(Clone, Debug, Default)]
struct LocalBalance {
    /// Head office's total as of the last sync, if the node has ever synced.
    known: Option<i64>,
    known_at: Millis,
    /// Every entry this node wrote for the balance.
    local: Ledger,
    /// Entries head office has not yet acknowledged, in the order they were written.
    pending: Vec<Entry>,
    quota: Option<Quota>,
}

/// One node's local record: its entry sequence, its last view of each balance, its quotas and
/// the entries it has not yet reported.
///
/// This is what a node persists across a restart; clone it to model one.
#[derive(Clone, Debug)]
pub struct NodeView {
    node: NodeId,
    next_seq: u64,
    balances: BTreeMap<BalanceId, LocalBalance>,
}

impl NodeView {
    /// An empty view for `node`.
    #[must_use]
    pub fn new(node: NodeId) -> NodeView {
        NodeView {
            node,
            next_seq: 1,
            balances: BTreeMap::new(),
        }
    }

    /// The node this view belongs to.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// The id the next entry will carry. It is only consumed by [`NodeView::record`].
    #[must_use]
    pub fn next_entry_id(&self) -> EntryId {
        EntryId {
            node: self.node.clone(),
            seq: self.next_seq,
        }
    }

    /// Notes head office's total for a balance, as of `at`.
    ///
    /// [`NodeView::apply_hub_reply`] does this for every total a reply carries, which is how a
    /// node keeps up with head office. Call it directly only to seed a view from an answer that
    /// is not a reply to a delivery (a live online-only confirmation, or a first sync).
    pub fn learn_balance(&mut self, balance: &BalanceId, total: i64, at: Millis) {
        let local = self.balances.entry(balance.clone()).or_default();
        local.known = Some(total);
        local.known_at = at;
    }

    /// Installs a freshly granted quota, replacing a settled one.
    pub fn install_grant(&mut self, grant: &Grant) {
        let local = self.balances.entry(grant.balance.clone()).or_default();
        local.quota = Some(Quota {
            amount: grant.amount,
            spent: 0,
            expires_at: grant.expires_at,
        });
    }

    /// Head office's total for the balance as of the last sync.
    #[must_use]
    pub fn known(&self, balance: &BalanceId) -> Option<i64> {
        self.balances.get(balance).and_then(|b| b.known)
    }

    /// When the node last learned the balance.
    #[must_use]
    pub fn known_at(&self, balance: &BalanceId) -> Option<Millis> {
        self.balances.get(balance).map(|b| b.known_at)
    }

    /// The node's quota on the balance, if it holds one.
    #[must_use]
    pub fn quota(&self, balance: &BalanceId) -> Option<&Quota> {
        self.balances.get(balance).and_then(|b| b.quota.as_ref())
    }

    /// What is left of the node's quota, if it holds one.
    #[must_use]
    pub fn quota_remaining(&self, balance: &BalanceId) -> Option<i64> {
        self.quota(balance)
            .map(|q| q.amount.saturating_sub(q.spent))
    }

    /// The total of this node's debits head office has not yet acknowledged.
    #[must_use]
    pub fn unsynced_spend(&self, balance: &BalanceId) -> i64 {
        self.balances.get(balance).map_or(0, |b| {
            b.pending
                .iter()
                .filter(|e| e.is_debit())
                .fold(0_i64, |sum, e| sum.saturating_add(e.amount))
        })
    }

    /// Every entry still to be reported, across all balances, in write order per balance.
    ///
    /// Send these again after a lost reply: an entry already applied is recognised by its id.
    #[must_use]
    pub fn pending(&self) -> Vec<Entry> {
        self.balances
            .values()
            .flat_map(|b| b.pending.iter().cloned())
            .collect()
    }

    /// A delivery of everything not yet acknowledged, keeping any quota: more spending may follow.
    #[must_use]
    pub fn progress_batch(&self) -> Batch {
        Batch {
            node: self.node.clone(),
            entries: self.pending(),
            complete: false,
        }
    }

    /// A final delivery. The node **stops spending from its quotas now** (it drops them
    /// locally), because head office will close them and return the unused value. Send the
    /// returned batch again if the reply is lost; a repeat is recognised.
    #[must_use]
    pub fn settlement_batch(&mut self) -> Batch {
        for local in self.balances.values_mut() {
            local.quota = None;
        }
        Batch {
            node: self.node.clone(),
            entries: self.pending(),
            complete: true,
        }
    }

    /// Every entry this node has written for the balance.
    #[must_use]
    pub fn local_ledger(&self, balance: &BalanceId) -> Option<&Ledger> {
        self.balances.get(balance).map(|b| &b.local)
    }

    /// Records a debit the node has just authorised. `acknowledged` is true when head office
    /// already holds it (the online-only policy), so nothing is left to report.
    pub(crate) fn record(&mut self, entry: Entry, acknowledged: bool) {
        self.next_seq = self.next_seq.saturating_add(1);
        let local = self.balances.entry(entry.balance.clone()).or_default();
        // The id is the node's own fresh sequence number, so the local append cannot conflict;
        // an overflow would only mean the local copy lags, never that the entry is lost.
        let _ = local.local.append(entry.clone());
        if let Some(quota) = local.quota.as_mut() {
            quota.spent = quota.spent.saturating_add(entry.amount);
        }
        if !acknowledged {
            local.pending.push(entry);
        }
    }

    /// Applies head office's reply to a delivery: acknowledges what it handled, **learns every
    /// total it reported**, and drops the quotas it closed.
    ///
    /// This is the only public way to apply a reply, and it always learns the balances: the two
    /// steps cannot be separated, so a node cannot acknowledge a delivery and keep spending
    /// against an old, higher view. The totals are head office's, stamped with head office's
    /// time when it answered. A consequence the caller must expect: after a reply that shows the
    /// balance has gone, an accept-and-reconcile node declines the next spend instead of
    /// overspending.
    ///
    /// ```compile_fail
    /// use spot_shared_balance::{EntryId, NodeId, NodeView};
    /// let mut view = NodeView::new(NodeId::new("branch-01"));
    /// let acknowledged: Vec<EntryId> = Vec::new();
    /// // Private: acknowledging is half a reply, so it cannot be applied on its own.
    /// view.acknowledge(&acknowledged);
    /// ```
    pub fn apply_hub_reply(&mut self, reply: &HubReply) {
        for (balance, total) in reply.totals() {
            self.learn_balance(balance, total.total(), total.at());
        }
        self.acknowledge(&reply.handled());
        for balance in &reply.closed_grants {
            if let Some(local) = self.balances.get_mut(balance) {
                local.quota = None;
            }
        }
    }

    /// Forgets entries head office has handled, so they are not sent again.
    ///
    /// Crate-private: acknowledging is half of applying a reply, and
    /// [`NodeView::apply_hub_reply`] does both so the halves cannot drift apart.
    pub(crate) fn acknowledge(&mut self, ids: &[EntryId]) {
        for local in self.balances.values_mut() {
            local.pending.retain(|e| !ids.contains(&e.id));
        }
    }
}
