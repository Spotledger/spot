//! Policy 1: online-only. Spend only with a live head-office confirmation.

use crate::error::BalanceError;
use crate::hub::Hub;
use crate::ledger::{Appended, BalanceId, Entry, EntryKind, Millis};
use crate::node::NodeView;
use crate::policy::{
    BalancePolicy, Batch, Decision, DeclineReason, Gate, Limits, PolicyKind, Reconciliation, gate,
    ordered, refuse, refuse_foreign,
};
use crate::quarantine::{QuarantineSink, RejectReason};
use crate::reply::{HubReply, reply_for};

/// What head office answered to a spend it was asked to confirm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirmation {
    /// Head office debited the balance. It holds the entry now.
    Confirmed,
    /// Head office is reachable and the balance does not cover it.
    Insufficient,
    /// Head office is reachable and holds no such balance.
    UnknownBalance,
    /// Head office could not be reached.
    Unreachable,
}

/// The live link to head office. Implementations must apply the debit atomically on head
/// office's ledger, and answer a repeated entry id the same way as the first time.
pub trait Confirmer {
    /// Asks head office to debit `entry` now, if the balance covers it.
    fn confirm(&self, entry: &Entry, now: Millis) -> Confirmation;
}

/// Spend only while head office confirms.
///
/// **Guarantee:** zero overspend. A node that cannot reach head office declines every spend, so
/// a balance can never go below zero. **Cost:** nothing can be spent offline.
#[derive(Clone, Debug)]
pub struct OnlineOnly<C> {
    confirmer: C,
}

impl<C: Confirmer> OnlineOnly<C> {
    /// An online-only policy that confirms through `confirmer`.
    #[must_use]
    pub fn new(confirmer: C) -> Self {
        OnlineOnly { confirmer }
    }
}

impl<C: Confirmer> BalancePolicy for OnlineOnly<C> {
    fn kind(&self) -> PolicyKind {
        PolicyKind::OnlineOnly
    }

    fn authorise(
        &self,
        view: &mut NodeView,
        balance: &BalanceId,
        amount: i64,
        now: Millis,
    ) -> Decision {
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
        match self.confirmer.confirm(&entry, now) {
            Confirmation::Confirmed => {
                view.record(entry.clone(), true);
                Decision::Approved(entry)
            }
            Confirmation::Insufficient => Decision::Declined(DeclineReason::Insufficient),
            Confirmation::UnknownBalance => Decision::Declined(DeclineReason::UnknownBalance),
            Confirmation::Unreachable => Decision::Declined(DeclineReason::Offline),
        }
    }

    /// Entries from an online-only node are already on head office's ledger, so they arrive as
    /// repeats. An entry that is not (a node that bypassed confirmation) is applied only if the
    /// balance covers it, and quarantined otherwise.
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
                    let covered = hub
                        .available(&entry.balance)
                        .is_some_and(|available| available >= entry.amount);
                    if covered {
                        apply(hub, entry, &mut outcome)?;
                    } else {
                        refuse(
                            hub,
                            entry,
                            RejectReason::Insufficient,
                            quarantine,
                            &mut outcome,
                        );
                    }
                }
            }
        }
        Ok(reply_for(hub, batch, outcome, now))
    }

    fn limits(&self) -> Limits {
        Limits {
            max_overspend: 0,
            offline_spend: false,
            may_strand_value: false,
        }
    }
}

/// Appends `entry` to its ledger and notes it in the outcome.
pub(crate) fn apply(
    hub: &mut Hub,
    entry: &Entry,
    outcome: &mut Reconciliation,
) -> Result<(), BalanceError> {
    match hub.ledger_mut(&entry.balance)?.append(entry.clone())? {
        Appended::New => outcome.applied.push(entry.id.clone()),
        Appended::Duplicate => outcome.duplicates.push(entry.id.clone()),
    }
    Ok(())
}
