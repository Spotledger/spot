//! Policy 3: accept and reconcile. Spend offline within a risk cap; settle the difference later.

use crate::error::BalanceError;
use crate::hub::Hub;
use crate::ledger::{BalanceId, Entry, EntryKind, Millis};
use crate::node::NodeView;
use crate::online::apply;
use crate::policy::{
    BalancePolicy, Batch, Decision, DeclineReason, Gate, Limits, PolicyKind, Reconciliation, gate,
    ordered, refuse, refuse_foreign,
};
use crate::quarantine::QuarantineSink;
use crate::reply::{HubReply, reply_for};

/// Spend offline within a per-node risk cap, and reconcile at head office afterwards.
///
/// A node accepts a spend when its last view of the balance was above zero and the debits it
/// has not yet reported, with this one, stay within `risk_cap`. At head office every such debit
/// is applied (the goods are already gone). If that takes a balance below zero, the shortfall is
/// written back as an upward adjustment and held as a [`Receivable`](crate::Receivable).
///
/// **Guarantee:** the total value spent beyond a balance never exceeds `risk_cap x max_nodes`
/// ([`BalancePolicy::limits`]). Once a node has reported, it sees the new, lower balance and
/// stops accepting, and each node can only hold a cap's worth from a view that is out of date.
/// **Cost:** a real, bounded loss is possible, and head office has to chase it.
#[derive(Clone, Copy, Debug)]
pub struct AcceptAndReconcile {
    risk_cap: i64,
    max_nodes: u32,
}

impl AcceptAndReconcile {
    /// A policy with a per-node cap in minor units and the most nodes that may be spending.
    ///
    /// # Errors
    /// [`BalanceError::Config`] if the cap or the node count is zero.
    pub fn new(risk_cap: i64, max_nodes: u32) -> Result<Self, BalanceError> {
        if risk_cap <= 0 {
            return Err(BalanceError::Config {
                why: "the risk cap must be above zero",
            });
        }
        if max_nodes == 0 {
            return Err(BalanceError::Config {
                why: "the node count must be above zero",
            });
        }
        Ok(AcceptAndReconcile {
            risk_cap,
            max_nodes,
        })
    }
}

impl BalancePolicy for AcceptAndReconcile {
    fn kind(&self) -> PolicyKind {
        PolicyKind::AcceptAndReconcile {
            risk_cap: self.risk_cap,
            max_nodes: self.max_nodes,
        }
    }

    fn authorise(
        &self,
        view: &mut NodeView,
        balance: &BalanceId,
        amount: i64,
        now: Millis,
    ) -> Decision {
        let Some(known) = view.known(balance) else {
            return Decision::Declined(DeclineReason::UnknownBalance);
        };
        let entry = match Entry::new(
            view.next_entry_id(),
            balance.clone(),
            EntryKind::Debit,
            amount,
            now,
        ) {
            Ok(entry) => entry,
            Err(_) => return Decision::Declined(DeclineReason::InvalidAmount),
        };
        if known <= 0 {
            return Decision::Declined(DeclineReason::NoFunds);
        }
        if view.unsynced_spend(balance).saturating_add(amount) > self.risk_cap {
            return Decision::Declined(DeclineReason::OverRiskCap);
        }
        view.record(entry.clone(), false);
        Decision::Approved(entry)
    }

    fn settle(
        &self,
        hub: &mut Hub,
        batch: &Batch,
        now: Millis,
        quarantine: &mut dyn QuarantineSink,
    ) -> Result<HubReply, BalanceError> {
        let mut outcome = Reconciliation::default();
        for entry in ordered(&batch.entries) {
            match gate(hub, &batch.node, entry, now) {
                Gate::Foreign => refuse_foreign(entry, quarantine, &mut outcome),
                Gate::Repeat => outcome.duplicates.push(entry.id.clone()),
                Gate::Queued(reason) | Gate::Refuse(reason) => {
                    refuse(hub, entry, reason, quarantine, &mut outcome);
                }
                Gate::Proceed => {
                    apply(hub, entry, &mut outcome)?;
                    let total = hub.balance(&entry.balance).unwrap_or(0);
                    if total < 0 {
                        let shortfall = total.checked_neg().ok_or(BalanceError::Overflow)?;
                        // Head office writes the adjustment, so it carries head office's time,
                        // not the node's word for when the spend happened.
                        hub.absorb_shortfall(
                            &entry.balance,
                            &entry.id.node,
                            shortfall,
                            &entry.id,
                            now,
                        )?;
                    }
                }
            }
        }
        outcome.receivables = hub
            .receivables()
            .iter()
            .filter(|r| outcome.applied.contains(&r.caused_by))
            .cloned()
            .collect();
        Ok(reply_for(hub, batch, outcome, now))
    }

    fn limits(&self) -> Limits {
        Limits {
            max_overspend: self.risk_cap.saturating_mul(i64::from(self.max_nodes)),
            offline_spend: true,
            may_strand_value: false,
        }
    }
}
