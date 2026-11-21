//! Policy 2: allocated quotas. Head office lends each node a slice to spend offline.

use crate::error::BalanceError;
use crate::hub::Hub;
use crate::ledger::{BalanceId, Entry, EntryKind, Millis};
use crate::node::NodeView;
use crate::online::apply;
use crate::policy::{
    BalancePolicy, Batch, Decision, DeclineReason, Gate, Limits, PolicyKind, Reconciliation, gate,
    ordered, refuse, refuse_foreign,
};
use crate::quarantine::{QuarantineSink, RejectReason};
use crate::reply::{HubReply, reply_for};

/// Spend within a quota head office reserved in advance.
///
/// **Guarantee:** overspend is impossible. The quota is carved out of the balance when it is
/// granted, so what a node spends offline is already set aside for it. **Cost:** value in a
/// quota is unavailable to everyone else until the node settles, and if a node never reports in
/// the unused part stays reserved ("stranded"; see [`Hub::stranded`]). Reclaiming it after a
/// grace period is a decision for a later ADR.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllocatedQuota;

impl BalancePolicy for AllocatedQuota {
    fn kind(&self) -> PolicyKind {
        PolicyKind::AllocatedQuota
    }

    fn authorise(
        &self,
        view: &mut NodeView,
        balance: &BalanceId,
        amount: i64,
        now: Millis,
    ) -> Decision {
        let remaining = match view.quota(balance) {
            None => return Decision::Declined(DeclineReason::NoQuota),
            Some(quota) if now >= quota.expires_at => {
                return Decision::Declined(DeclineReason::QuotaExpired);
            }
            Some(quota) => quota.amount.saturating_sub(quota.spent),
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
        if amount > remaining {
            return Decision::Declined(DeclineReason::QuotaExhausted);
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
                Gate::Proceed => match check_against_grant(hub, entry) {
                    Some(reason) => refuse(hub, entry, reason, quarantine, &mut outcome),
                    None => {
                        apply(hub, entry, &mut outcome)?;
                        if let Some(grant) = hub.grant_mut(&entry.id.node, &entry.balance) {
                            grant.consumed = grant.consumed.saturating_add(entry.amount);
                        }
                    }
                },
            }
        }
        if batch.complete {
            for (balance, unused) in hub.close_grants(&batch.node) {
                outcome.returned = outcome.returned.saturating_add(unused);
                outcome.closed_grants.push(balance);
            }
        }
        Ok(reply_for(hub, batch, outcome, now))
    }

    fn limits(&self) -> Limits {
        Limits {
            max_overspend: 0,
            offline_spend: true,
            may_strand_value: true,
        }
    }
}

/// Why head office must refuse a debit against the node's grant, if it must.
fn check_against_grant(hub: &Hub, entry: &Entry) -> Option<RejectReason> {
    let Some(grant) = hub.grant(&entry.id.node, &entry.balance) else {
        return Some(RejectReason::NoGrant);
    };
    // Expiry is judged on head office's own clock, when it first saw this entry, never on the
    // node's `at`: a node that backdates an entry gains nothing. An entry head office
    // has no receive time for cannot be judged, so it is refused rather than trusted.
    let Some(received) = hub.received_at(&entry.id) else {
        return Some(RejectReason::NotPermitted);
    };
    if received >= grant.expires_at {
        return Some(RejectReason::AfterExpiry);
    }
    if grant.consumed.saturating_add(entry.amount) > grant.amount {
        return Some(RejectReason::OverQuota);
    }
    let covered = hub
        .balance(&entry.balance)
        .is_some_and(|total| total >= entry.amount);
    if covered {
        None
    } else {
        Some(RejectReason::Insufficient)
    }
}
