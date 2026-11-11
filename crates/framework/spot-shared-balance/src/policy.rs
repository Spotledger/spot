//! The policy trait, the answers it gives, and the per-balance-type configuration.

use std::collections::BTreeMap;

use crate::codes;
use crate::error::BalanceError;
use crate::hub::{Hub, Mark, Receivable};
use crate::ledger::{BalanceId, Entry, EntryId, Millis, NodeId};
use crate::node::NodeView;
use crate::quarantine::{QuarantineSink, RejectReason};
use crate::reply::HubReply;

/// Why a node said no to a spend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeclineReason {
    /// The node cannot reach head office and the policy needs it.
    Offline,
    /// The balance does not cover the amount.
    Insufficient,
    /// The node holds no quota for the balance.
    NoQuota,
    /// The node's quota has expired.
    QuotaExpired,
    /// The amount is more than the node's remaining quota.
    QuotaExhausted,
    /// Accepting would put the node past its risk cap.
    OverRiskCap,
    /// The node's last view of the balance shows nothing left to spend.
    NoFunds,
    /// The amount is not a valid spend.
    InvalidAmount,
    /// The node does not know the balance.
    UnknownBalance,
}

impl DeclineReason {
    /// The stable code from [`codes`] for this reason.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            DeclineReason::Offline => codes::DECLINED_OFFLINE,
            DeclineReason::Insufficient => codes::DECLINED_INSUFFICIENT,
            DeclineReason::NoQuota => codes::DECLINED_NO_QUOTA,
            DeclineReason::QuotaExpired => codes::DECLINED_QUOTA_EXPIRED,
            DeclineReason::QuotaExhausted => codes::DECLINED_QUOTA_EXHAUSTED,
            DeclineReason::OverRiskCap => codes::DECLINED_OVER_RISK_CAP,
            DeclineReason::NoFunds => codes::DECLINED_NO_FUNDS,
            DeclineReason::InvalidAmount => codes::DECLINED_INVALID_AMOUNT,
            DeclineReason::UnknownBalance => codes::DECLINED_UNKNOWN_BALANCE,
        }
    }
}

/// A node's answer to a spend request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The spend is allowed; this is the debit the node has recorded.
    Approved(Entry),
    /// The spend is refused, and nothing was recorded.
    Declined(DeclineReason),
}

impl Decision {
    /// True for [`Decision::Approved`].
    #[must_use]
    pub fn is_approved(&self) -> bool {
        matches!(self, Decision::Approved(_))
    }
}

/// What a policy guarantees, in numbers a person can check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most value that can ever be spent beyond a balance, in minor units. Zero means
    /// overspend is impossible.
    pub max_overspend: i64,
    /// Whether a node can spend while it cannot reach head office.
    pub offline_spend: bool,
    /// Whether unused value can sit reserved for a node until it reports in.
    pub may_strand_value: bool,
}

/// One delivery of a node's entries to head office.
#[derive(Clone, Debug)]
pub struct Batch {
    /// The node that wrote the entries.
    pub node: NodeId,
    /// The entries, in any order; a repeat of an earlier delivery is fine.
    pub entries: Vec<Entry>,
    /// True when this is everything the node has: quotas the node holds are closed and their
    /// unused value returned.
    pub complete: bool,
}

/// What head office did with a [`Batch`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reconciliation {
    /// Entries added to the ledger.
    pub applied: Vec<EntryId>,
    /// Entries head office already had.
    pub duplicates: Vec<EntryId>,
    /// Entries refused and handed to the quarantine sink, with the reason.
    pub quarantined: Vec<(EntryId, RejectReason)>,
    /// Shortfalls absorbed into a balance and held as claims.
    pub receivables: Vec<Receivable>,
    /// Unused quota given back to the balance.
    pub returned: i64,
    /// Balances whose grant for this node was closed.
    pub closed_grants: Vec<BalanceId>,
    /// Entries in the batch that another node wrote; quarantined, and not acknowledged to the
    /// sender (they are not the sender's to report).
    pub rejected_foreign: Vec<EntryId>,
    /// Entries head office refused but the quarantine sink could not take. They are not marked
    /// held and are not in [`Reconciliation::handled`], so the sender redelivers them.
    pub deferred: Vec<EntryId>,
}

impl Reconciliation {
    /// Every entry id head office has dealt with (applied, repeated or quarantined). The node
    /// may stop sending these.
    #[must_use]
    pub fn handled(&self) -> Vec<EntryId> {
        let mut ids = self.applied.clone();
        ids.extend(self.duplicates.iter().cloned());
        ids.extend(self.quarantined.iter().map(|(id, _)| id.clone()));
        ids
    }
}

/// How a balance is shared between nodes that can be offline.
///
/// The three implementations trade availability against exposure; see the crate docs.
pub trait BalancePolicy {
    /// Which strategy this is.
    fn kind(&self) -> PolicyKind;

    /// Decides a spend of `amount` minor units against `balance` at `node`, at `now`. On
    /// approval the debit is recorded in `view` and returned.
    fn authorise(
        &self,
        view: &mut NodeView,
        balance: &BalanceId,
        amount: i64,
        now: Millis,
    ) -> Decision;

    /// Applies a node's delivered entries to head office's ledgers, quarantining what the
    /// policy refuses and absorbing what it accepts at a loss.
    ///
    /// The reply is a [`HubReply`](crate::HubReply): what head office did **and** its totals for
    /// the balances the delivery named, stamped with `now` (head office's time). It is the only
    /// thing a node may apply to its view, so a node cannot acknowledge a delivery without
    /// learning the balances that came with it.
    ///
    /// # Errors
    /// A [`BalanceError`] if a ledger cannot take an entry (a reused id or an overflow); entries
    /// handled before the error stay handled, and a repeat of the batch resumes safely.
    fn settle(
        &self,
        hub: &mut Hub,
        batch: &Batch,
        now: Millis,
        quarantine: &mut dyn QuarantineSink,
    ) -> Result<HubReply, BalanceError>;

    /// What this policy guarantees.
    fn limits(&self) -> Limits;
}

/// The three strategies, selectable per balance type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyKind {
    /// Spend only with a live head-office confirmation.
    OnlineOnly,
    /// Spend within a quota head office lent in advance.
    AllocatedQuota,
    /// Spend offline within a per-node risk cap and reconcile later.
    AcceptAndReconcile {
        /// The most unreported debit value one node may hold, in minor units.
        risk_cap: i64,
        /// The most nodes that can be spending at once.
        max_nodes: u32,
    },
}

/// The default policy while the architecture owner's decision is open. **Provisional**: it is the safest of the
/// three and is not a choice of the product.
pub const PROVISIONAL_DEFAULT: PolicyKind = PolicyKind::OnlineOnly;

/// Which policy each balance type uses, set in configuration.
#[derive(Clone, Debug)]
pub struct PolicyTable {
    default: PolicyKind,
    by_type: BTreeMap<String, PolicyKind>,
}

impl Default for PolicyTable {
    fn default() -> Self {
        PolicyTable {
            default: PROVISIONAL_DEFAULT,
            by_type: BTreeMap::new(),
        }
    }
}

impl PolicyTable {
    /// A table whose every type uses the provisional default.
    #[must_use]
    pub fn new() -> PolicyTable {
        PolicyTable::default()
    }

    /// Chooses the policy for one balance type.
    ///
    /// # Errors
    /// [`BalanceError::Config`] for an accept-and-reconcile cap or node count of zero.
    pub fn set(
        &mut self,
        balance_type: impl Into<String>,
        kind: PolicyKind,
    ) -> Result<(), BalanceError> {
        validate(kind)?;
        self.by_type.insert(balance_type.into(), kind);
        Ok(())
    }

    /// Changes the fallback for types with no entry of their own.
    ///
    /// # Errors
    /// [`BalanceError::Config`] for an invalid accept-and-reconcile setting.
    pub fn set_default(&mut self, kind: PolicyKind) -> Result<(), BalanceError> {
        validate(kind)?;
        self.default = kind;
        Ok(())
    }

    /// The policy for a balance type.
    #[must_use]
    pub fn kind_for(&self, balance_type: &str) -> PolicyKind {
        self.by_type
            .get(balance_type)
            .copied()
            .unwrap_or(self.default)
    }
}

fn validate(kind: PolicyKind) -> Result<(), BalanceError> {
    match kind {
        PolicyKind::AcceptAndReconcile { risk_cap, .. } if risk_cap <= 0 => {
            Err(BalanceError::Config {
                why: "the risk cap must be above zero",
            })
        }
        PolicyKind::AcceptAndReconcile { max_nodes: 0, .. } => Err(BalanceError::Config {
            why: "the node count must be above zero",
        }),
        _ => Ok(()),
    }
}

/// Entries sorted the way every policy applies them: by the node's time, then by id.
pub(crate) fn ordered(entries: &[Entry]) -> Vec<&Entry> {
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
    sorted
}

/// The checks every policy makes before its own: a repeat is a no-op, an entry already offered to
/// the quarantine sink is never applied, a node may only send debits, and the balance must exist.
pub(crate) enum Gate {
    Foreign,
    Repeat,
    /// Already handed to the quarantine sink but not yet held there: the entry goes into the
    /// queue, never onto the ledger, so an id is applied or queued and never both.
    Queued(RejectReason),
    Refuse(RejectReason),
    Proceed,
}

pub(crate) fn gate(hub: &mut Hub, sender: &NodeId, entry: &Entry, now: Millis) -> Gate {
    // A batch may only carry its sender's own entries: otherwise a node could spend another
    // node's grant, or pre-claim another node's entry ids. Not marked, so the real owner's
    // entry with that id is unaffected.
    if &entry.id.node != sender {
        return Gate::Foreign;
    }
    // Head office stamps the entry the first time it sees it: expiry is judged on this time, so
    // the node's own, backdatable `at` can never buy an expired quota a fresh life.
    hub.note_received(&entry.id, now);
    match hub.mark(&entry.id) {
        // Only an entry the sink confirmed holding is a repeat.
        Some(Mark::Held) => return Gate::Repeat,
        // An entry the sink was already offered is completed into the queue, with the answer
        // head office gave it the first time. Re-running the policy against a changed balance
        // would apply an entry whose refusal is still on its way to the queue.
        Some(Mark::Offered) => {
            return Gate::Queued(
                hub.refused_for(&entry.id)
                    .unwrap_or(RejectReason::NotPermitted),
            );
        }
        None => {}
    }
    match hub.ledger(&entry.balance) {
        None => Gate::Refuse(RejectReason::UnknownBalance),
        // Same id with different content is not a redelivery: never acknowledge it as one.
        Some(ledger) if ledger.entry(&entry.id).is_some_and(|held| held != entry) => {
            Gate::Refuse(RejectReason::NotPermitted)
        }
        Some(ledger) if ledger.contains(&entry.id) => Gate::Repeat,
        Some(_) if !entry.is_debit() => Gate::Refuse(RejectReason::NotPermitted),
        Some(_) => Gate::Proceed,
    }
}

/// Quarantines an entry that is not the sender's own, without remembering its id.
pub(crate) fn refuse_foreign(
    entry: &Entry,
    sink: &mut dyn QuarantineSink,
    outcome: &mut Reconciliation,
) {
    match sink.quarantine(entry, RejectReason::NotPermitted) {
        Ok(()) => outcome.rejected_foreign.push(entry.id.clone()),
        Err(_) => outcome.deferred.push(entry.id.clone()),
    }
}

/// Refuses `entry`: marks it offered with `reason`, hands it to the sink and records it in the
/// outcome.
///
/// The mark becomes held (a redelivery is then a repeat) only when the sink has the entry in
/// custody: at once for a sink that confirms on accept, later through [`Hub::confirm_held`] for a
/// buffering one. A sink that fails leaves the entry offered and deferred, so the sender
/// redelivers it and the sink is offered it again.
///
/// The same call completes an entry that was already offered: [`gate`] routes it here as
/// [`Gate::Queued`], so an id whose quarantine is on its way is never applied as well.
pub(crate) fn refuse(
    hub: &mut Hub,
    entry: &Entry,
    reason: RejectReason,
    sink: &mut dyn QuarantineSink,
    outcome: &mut Reconciliation,
) {
    hub.mark_offered(&entry.id, reason);
    match sink.quarantine(entry, reason) {
        Ok(()) => {
            if sink.confirms_on_accept() {
                hub.confirm_held(&entry.id);
            }
            outcome.quarantined.push((entry.id.clone(), reason));
        }
        Err(_) => outcome.deferred.push(entry.id.clone()),
    }
}
