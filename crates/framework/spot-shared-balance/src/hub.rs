//! Head office's side: the authoritative ledgers, the quota grants and the receivables.

use std::collections::BTreeMap;

use crate::error::BalanceError;
use crate::ledger::{Appended, BalanceId, Entry, EntryId, EntryKind, Ledger, Millis, NodeId, Sign};
use crate::quarantine::RejectReason;
use crate::store::{RefusalRecord, RefusalStore, StoreError};

/// A quota head office lent a node: value reserved for that node to spend while offline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The node it was lent to.
    pub node: NodeId,
    /// The balance it is carved from.
    pub balance: BalanceId,
    /// The size of the grant.
    pub amount: i64,
    /// How much of it settled entries have used.
    pub consumed: i64,
    /// When the node must stop spending from it.
    pub expires_at: Millis,
    /// When it was granted.
    pub granted_at: Millis,
}

impl Grant {
    /// What is still reserved: granted and not yet used by a settled entry.
    #[must_use]
    pub fn unused(&self) -> i64 {
        self.amount.saturating_sub(self.consumed)
    }
}

/// A shortfall head office absorbed into a balance and now holds as a claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receivable {
    /// The balance that went short.
    pub balance: BalanceId,
    /// The node whose entry took it below zero.
    pub node: NodeId,
    /// How much was short, in minor units.
    pub amount: i64,
    /// The entry that exposed the shortfall.
    pub caused_by: EntryId,
}

/// Value reserved for a node whose grant ran out without settling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stranded {
    /// The node.
    pub node: NodeId,
    /// The balance.
    pub balance: BalanceId,
    /// The unused value held back.
    pub amount: i64,
}

/// How far a refused entry's quarantine has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// The hub has handed the entry to the quarantine sink, which has not yet confirmed it holds
    /// the entry. A redelivery is checked again and offered to the sink again.
    Offered,
    /// The sink confirmed custody. A redelivery is a repeat and the sink is not called again.
    Held,
}

/// What head office remembers about one refused entry: the mark and the reason it refused it.
///
/// The two are one state: a mark restored without its reason could not be offered to the sink
/// with the answer the sender is owed, so they are persisted together (see [`RefusalRecord`]).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Refusal {
    mark: Mark,
    reason: RejectReason,
}

/// Head office: the owner of every balance.
#[derive(Clone, Debug)]
pub struct Hub {
    id: NodeId,
    next_seq: u64,
    ledgers: BTreeMap<BalanceId, Ledger>,
    grants: BTreeMap<(NodeId, BalanceId), Grant>,
    receivables: Vec<Receivable>,
    quarantined: BTreeMap<EntryId, Refusal>,
    /// When head office first saw each entry, by head office's own clock. This is the time
    /// expiry is judged on; an entry's own `at` is the node's word and is information only.
    received: BTreeMap<EntryId, Millis>,
}

impl Hub {
    /// A head office that writes its own entries under `id`.
    #[must_use]
    pub fn new(id: NodeId) -> Hub {
        Hub {
            id,
            next_seq: 1,
            ledgers: BTreeMap::new(),
            grants: BTreeMap::new(),
            receivables: Vec::new(),
            quarantined: BTreeMap::new(),
            received: BTreeMap::new(),
        }
    }

    fn next_id(&mut self) -> EntryId {
        let id = EntryId {
            node: self.id.clone(),
            seq: self.next_seq,
        };
        self.next_seq = self.next_seq.saturating_add(1);
        id
    }

    /// Opens a balance with an opening credit.
    ///
    /// # Errors
    /// [`BalanceError::BalanceExists`] if it exists; [`BalanceError::InvalidAmount`] for an
    /// opening amount of zero or less.
    pub fn open_balance(
        &mut self,
        balance: BalanceId,
        opening: i64,
        at: Millis,
    ) -> Result<(), BalanceError> {
        if self.ledgers.contains_key(&balance) {
            return Err(BalanceError::BalanceExists { balance });
        }
        let entry = Entry::new(
            self.next_id(),
            balance.clone(),
            EntryKind::Credit,
            opening,
            at,
        )?;
        let mut ledger = Ledger::new();
        ledger.append(entry)?;
        self.ledgers.insert(balance, ledger);
        Ok(())
    }

    /// Adds value to a balance (a top-up or a refund).
    ///
    /// # Errors
    /// [`BalanceError::UnknownBalance`], [`BalanceError::InvalidAmount`] or
    /// [`BalanceError::Overflow`].
    pub fn credit(
        &mut self,
        balance: &BalanceId,
        amount: i64,
        at: Millis,
    ) -> Result<EntryId, BalanceError> {
        let id = self.next_id();
        let entry = Entry::new(id.clone(), balance.clone(), EntryKind::Credit, amount, at)?;
        self.ledger_mut(balance)?.append(entry)?;
        Ok(id)
    }

    /// The balance's ledger, if it exists.
    #[must_use]
    pub fn ledger(&self, balance: &BalanceId) -> Option<&Ledger> {
        self.ledgers.get(balance)
    }

    pub(crate) fn ledger_mut(&mut self, balance: &BalanceId) -> Result<&mut Ledger, BalanceError> {
        self.ledgers
            .get_mut(balance)
            .ok_or_else(|| BalanceError::UnknownBalance {
                balance: balance.clone(),
            })
    }

    /// The balance's current total, if it exists.
    #[must_use]
    pub fn balance(&self, balance: &BalanceId) -> Option<i64> {
        self.ledger(balance).map(Ledger::balance)
    }

    /// The balances head office holds, in order.
    pub fn balances(&self) -> impl Iterator<Item = &BalanceId> {
        self.ledgers.keys()
    }

    /// Value reserved by open grants on the balance and not yet used.
    #[must_use]
    pub fn reserved(&self, balance: &BalanceId) -> i64 {
        self.grants
            .values()
            .filter(|g| &g.balance == balance)
            .fold(0_i64, |sum, g| sum.saturating_add(g.unused()))
    }

    /// What a new grant or a strict spend may take: the total less what is reserved.
    #[must_use]
    pub fn available(&self, balance: &BalanceId) -> Option<i64> {
        self.balance(balance)
            .map(|total| total.saturating_sub(self.reserved(balance)))
    }

    /// Lends `node` a quota of `amount` on `balance` until `expires_at`.
    ///
    /// The value is reserved: no other spend can take it, so the node can spend it offline
    /// without any risk of overspending the balance.
    ///
    /// # Errors
    /// [`BalanceError::InvalidAmount`], [`BalanceError::UnknownBalance`],
    /// [`BalanceError::GrantExists`] if the node's last grant is not closed, or
    /// [`BalanceError::InsufficientAvailable`].
    pub fn grant_quota(
        &mut self,
        node: &NodeId,
        balance: &BalanceId,
        amount: i64,
        expires_at: Millis,
        now: Millis,
    ) -> Result<Grant, BalanceError> {
        if amount <= 0 {
            return Err(BalanceError::InvalidAmount);
        }
        let available = self
            .available(balance)
            .ok_or_else(|| BalanceError::UnknownBalance {
                balance: balance.clone(),
            })?;
        let key = (node.clone(), balance.clone());
        if self.grants.contains_key(&key) {
            return Err(BalanceError::GrantExists {
                node: node.clone(),
                balance: balance.clone(),
            });
        }
        if available < amount {
            return Err(BalanceError::InsufficientAvailable {
                balance: balance.clone(),
            });
        }
        let grant = Grant {
            node: node.clone(),
            balance: balance.clone(),
            amount,
            consumed: 0,
            expires_at,
            granted_at: now,
        };
        self.grants.insert(key, grant.clone());
        Ok(grant)
    }

    /// The node's open grant on the balance, if any.
    #[must_use]
    pub fn grant(&self, node: &NodeId, balance: &BalanceId) -> Option<&Grant> {
        self.grants.get(&(node.clone(), balance.clone()))
    }

    pub(crate) fn grant_mut(&mut self, node: &NodeId, balance: &BalanceId) -> Option<&mut Grant> {
        self.grants.get_mut(&(node.clone(), balance.clone()))
    }

    /// Closes every open grant of `node`, returning the unused value to the balance, and lists
    /// the balances and amounts returned.
    pub(crate) fn close_grants(&mut self, node: &NodeId) -> Vec<(BalanceId, i64)> {
        let keys: Vec<(NodeId, BalanceId)> = self
            .grants
            .keys()
            .filter(|(n, _)| n == node)
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|key| {
                self.grants
                    .remove(&key)
                    .map(|grant| (grant.balance.clone(), grant.unused()))
            })
            .collect()
    }

    /// Grants that expired without settling, with the value still held back for each.
    ///
    /// This is the cost of the quota policy: the value stays reserved until the node reports in
    /// and the grant is closed. Note that expiry is judged on when head office *received* an
    /// entry ([`Hub::received_at`]), so a node that reports in after its lease ran out has that
    /// spending refused — the lease must therefore outlast a node's realistic offline window.
    #[must_use]
    pub fn stranded(&self, now: Millis) -> Vec<Stranded> {
        self.grants
            .values()
            .filter(|g| g.expires_at <= now && g.unused() > 0)
            .map(|g| Stranded {
                node: g.node.clone(),
                balance: g.balance.clone(),
                amount: g.unused(),
            })
            .collect()
    }

    /// Writes a shortfall back as an adjustment up and records the receivable.
    ///
    /// # Errors
    /// Any [`BalanceError`] from appending the adjustment.
    pub(crate) fn absorb_shortfall(
        &mut self,
        balance: &BalanceId,
        node: &NodeId,
        amount: i64,
        caused_by: &EntryId,
        at: Millis,
    ) -> Result<(), BalanceError> {
        let id = self.next_id();
        let entry = Entry::new(
            id,
            balance.clone(),
            EntryKind::Adjustment(Sign::Up),
            amount,
            at,
        )?;
        let appended = self.ledger_mut(balance)?.append(entry)?;
        if appended == Appended::New {
            self.receivables.push(Receivable {
                balance: balance.clone(),
                node: node.clone(),
                amount,
                caused_by: caused_by.clone(),
            });
        }
        Ok(())
    }

    /// Notes that head office first saw `id` at `at`. The first time wins: a redelivery never
    /// moves an entry's receive time, and the time is head office's, never the node's.
    pub(crate) fn note_received(&mut self, id: &EntryId, at: Millis) {
        self.received.entry(id.clone()).or_insert(at);
    }

    /// When head office first saw `id`, if it ever did.
    ///
    /// Expiry is judged on this, not on [`Entry::at`](crate::Entry::at): a node's clock is its
    /// own word, and a backdated entry would otherwise buy an expired quota a fresh life.
    #[must_use]
    pub fn received_at(&self, id: &EntryId) -> Option<Millis> {
        self.received.get(id).copied()
    }

    /// Notes that `id` has been handed to the quarantine sink, refused for `reason`. A mark
    /// already recorded keeps its first answer: a redelivery is not re-adjudicated.
    pub(crate) fn mark_offered(&mut self, id: &EntryId, reason: RejectReason) {
        self.quarantined.entry(id.clone()).or_insert(Refusal {
            mark: Mark::Offered,
            reason,
        });
    }

    /// Why head office refused `id`, if it did.
    pub(crate) fn refused_for(&self, id: &EntryId) -> Option<RejectReason> {
        self.quarantined.get(id).map(|refusal| refusal.reason)
    }

    /// Records that the quarantine sink holds `id`: from now on a redelivery of it is a repeat.
    ///
    /// For a sink whose custody is confirmed later (see
    /// [`QuarantineSink::confirms_on_accept`](crate::QuarantineSink::confirms_on_accept)), the
    /// caller calls this once the queue write has completed. Returns `false`, and changes
    /// nothing, for an entry the hub never offered to the sink.
    pub fn confirm_held(&mut self, id: &EntryId) -> bool {
        match self.quarantined.get_mut(id) {
            Some(refusal) => {
                refusal.mark = Mark::Held;
                true
            }
            None => false,
        }
    }

    /// True if head office has refused this entry id and the quarantine sink confirmed it holds
    /// the entry. An entry that was only offered is not quarantined yet: it is offered again on
    /// redelivery.
    #[must_use]
    pub fn is_quarantined(&self, id: &EntryId) -> bool {
        self.mark(id) == Some(Mark::Held)
    }

    /// The state of an entry's quarantine, if the hub has refused it.
    #[must_use]
    pub fn mark(&self, id: &EntryId) -> Option<Mark> {
        self.quarantined.get(id).map(|refusal| refusal.mark)
    }

    /// Entries handed to the quarantine sink and never confirmed, in id order. After a restart or
    /// a failed queue write these are the entries a redelivery must offer again, so
    /// [`Hub::restore_refusals`] must bring them back.
    #[must_use]
    pub fn offered_unconfirmed(&self) -> Vec<EntryId> {
        self.quarantined
            .iter()
            .filter(|(_, refusal)| refusal.mark == Mark::Offered)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Every refusal mark head office holds, in id order: what a host persists so a restart
    /// neither forgets a held entry nor loses an offered one.
    #[must_use]
    pub fn refusals(&self) -> Vec<RefusalRecord> {
        self.quarantined
            .iter()
            .map(|(id, refusal)| RefusalRecord {
                id: id.clone(),
                mark: refusal.mark,
                reason: refusal.reason,
            })
            .collect()
    }

    /// Persists every refusal mark in one transaction ([`RefusalStore::save`]).
    ///
    /// Call it in the same step as the queue write that makes an entry held: a mark that reaches
    /// the store on its own leaves the next reader unable to tell what the queue holds.
    ///
    /// # Errors
    /// [`StoreError`] from the store. The hub is unchanged, so the call can be retried.
    pub fn persist_refusals(&self, store: &mut dyn RefusalStore) -> Result<(), StoreError> {
        store.save(&self.refusals())
    }

    /// Rebuilds the refusal marks from `store`, the way a hub starts after a restart.
    ///
    /// Restored marks behave exactly as they did before: a `Held` entry is a repeat on
    /// redelivery and is never applied, and an `Offered` entry is listed by
    /// [`Hub::offered_unconfirmed`] so it is offered to the sink again.
    ///
    /// # Errors
    /// [`StoreError`] when the store cannot be read, when it returns two records for one entry
    /// id, or when this hub already holds an entry's mark. A failure leaves the hub unchanged.
    pub fn restore_refusals(&mut self, store: &dyn RefusalStore) -> Result<(), StoreError> {
        let records = store.load()?;
        if !self.quarantined.is_empty() {
            return Err(StoreError {
                why: "the hub already holds refusal marks",
            });
        }
        let mut restored: BTreeMap<EntryId, Refusal> = BTreeMap::new();
        for record in records {
            let refusal = Refusal {
                mark: record.mark,
                reason: record.reason,
            };
            if restored.insert(record.id, refusal).is_some() {
                return Err(StoreError {
                    why: "the store returned one entry id twice",
                });
            }
        }
        self.quarantined = restored;
        Ok(())
    }

    /// Every receivable head office holds, in the order they arose.
    #[must_use]
    pub fn receivables(&self) -> &[Receivable] {
        &self.receivables
    }

    /// The sum of all receivables: the value given away beyond a balance.
    #[must_use]
    pub fn receivable_total(&self) -> i64 {
        self.receivables
            .iter()
            .fold(0_i64, |sum, r| sum.saturating_add(r.amount))
    }
}
