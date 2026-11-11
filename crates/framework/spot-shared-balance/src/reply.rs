//! Head office's reply to one delivery: what it did, and every balance it touched as
//! it stands after settling.
//!
//! The overspend bound of the accept-and-reconcile policy holds only while a node keeps
//! learning head office's totals: a node that acknowledges a delivery and keeps an old, higher
//! view can spend against value that is already gone. The old shape made that a caller's
//! responsibility — apply the reconciliation, then remember to call
//! [`NodeView::learn_balance`](crate::NodeView::learn_balance) — so a caller who forgot
//! overspent. This type closes it by construction: a reply always carries the hub's totals, its
//! fields are private, its only constructor is in this crate, so the one way to apply a delivery
//! is [`NodeView::apply_hub_reply`](crate::NodeView::apply_hub_reply), which learns them.
//!
//! ```compile_fail
//! use spot_shared_balance::HubTotal;
//! // A node cannot mint a total: only head office's reply carries one.
//! let _ = HubTotal::new(1_000, spot_shared_balance::Millis(0));
//! ```

use std::collections::BTreeMap;
use std::ops::Deref;

use crate::hub::Hub;
use crate::ledger::{BalanceId, Millis};
use crate::policy::{Batch, Reconciliation};

/// One balance total as head office reports it: the value and the hub time it was read at.
///
/// The two always travel together, so a node cannot learn a value from one moment and stamp it
/// with another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubTotal {
    total: i64,
    at: Millis,
}

impl HubTotal {
    /// Built by head office's reply only.
    pub(crate) fn new(total: i64, at: Millis) -> HubTotal {
        HubTotal { total, at }
    }

    /// The balance's total in minor units, as head office sees it.
    #[must_use]
    pub fn total(self) -> i64 {
        self.total
    }

    /// The hub time the total was read at.
    #[must_use]
    pub fn at(self) -> Millis {
        self.at
    }
}

/// Head office's answer to one delivery: the [`Reconciliation`] **and** the totals of every
/// balance the delivery named.
///
/// Only [`BalancePolicy::settle`](crate::BalancePolicy::settle) builds one. There is no public
/// constructor, so a caller cannot hand a node a reply without the hub's totals, and
/// [`NodeView::apply_hub_reply`](crate::NodeView::apply_hub_reply) is the only way to consume
/// one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubReply {
    reconciliation: Reconciliation,
    totals: BTreeMap<BalanceId, HubTotal>,
}

impl HubReply {
    /// Built by a policy's `settle` from the hub it just wrote to.
    pub(crate) fn new(
        reconciliation: Reconciliation,
        totals: BTreeMap<BalanceId, HubTotal>,
    ) -> Self {
        HubReply {
            reconciliation,
            totals,
        }
    }

    /// What head office did with the delivery.
    #[must_use]
    pub fn reconciliation(&self) -> &Reconciliation {
        &self.reconciliation
    }

    /// Head office's total for `balance`, when the delivery named it.
    #[must_use]
    pub fn total(&self, balance: &BalanceId) -> Option<HubTotal> {
        self.totals.get(balance).copied()
    }

    /// Every balance the delivery named, with head office's total for it.
    pub fn totals(&self) -> impl Iterator<Item = (&BalanceId, HubTotal)> {
        self.totals.iter().map(|(balance, total)| (balance, *total))
    }

    /// Consumes the reply and returns what head office did.
    #[must_use]
    pub fn into_reconciliation(self) -> Reconciliation {
        self.reconciliation
    }
}

/// A reply reads as its reconciliation: `reply.applied` and `reply.handled()` mean what they
/// mean on a [`Reconciliation`], and the totals are reached through [`HubReply::total`].
impl Deref for HubReply {
    type Target = Reconciliation;

    fn deref(&self) -> &Self::Target {
        &self.reconciliation
    }
}

/// Head office's reply to `batch`, read after settling it at `now`.
///
/// The totals are the hub's own, for every balance the batch named that head office holds, so a
/// node that applies the reply learns exactly what head office held when it answered — including
/// the balances of entries it refused.
pub(crate) fn reply_for(
    hub: &Hub,
    batch: &Batch,
    reconciliation: Reconciliation,
    now: Millis,
) -> HubReply {
    let mut totals: BTreeMap<BalanceId, HubTotal> = BTreeMap::new();
    for entry in &batch.entries {
        if let Some(total) = hub.balance(&entry.balance) {
            totals
                .entry(entry.balance.clone())
                .or_insert_with(|| HubTotal::new(total, now));
        }
    }
    HubReply::new(reconciliation, totals)
}
