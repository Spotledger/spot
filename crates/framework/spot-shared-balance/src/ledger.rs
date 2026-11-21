//! The append-only ledger of balance entries.
//!
//! A balance is never a number someone overwrites. It is the sum of its entries, and an entry
//! is added once and never changed or removed. Amounts are integer minor units (`i64`); there is
//! no float anywhere. Adding the same entry twice (a redelivery) is a recognised no-op, and
//! reusing an entry id for different content is an error, so a flaky link can retry freely.

use std::collections::BTreeMap;
use std::fmt;

use crate::error::BalanceError;

/// A point in time, in milliseconds. Time is always an argument; this crate never reads a clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millis(pub u64);

impl Millis {
    /// This instant plus `ms` milliseconds, saturating.
    #[must_use]
    pub fn plus(self, ms: u64) -> Millis {
        Millis(self.0.saturating_add(ms))
    }
}

/// A node (a branch or head office) that writes entries.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub String);

impl NodeId {
    /// A node id from text.
    #[must_use]
    pub fn new(id: impl Into<String>) -> NodeId {
        NodeId(id.into())
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A shared balance (a gift card, a customer credit line, a loyalty pot).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BalanceId(pub String);

impl BalanceId {
    /// A balance id from text.
    #[must_use]
    pub fn new(id: impl Into<String>) -> BalanceId {
        BalanceId(id.into())
    }
}

impl fmt::Display for BalanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The identity of one entry: the node that wrote it and that node's own sequence number.
///
/// This is the idempotency key. A redelivered entry carries the same id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId {
    /// The node that wrote the entry.
    pub node: NodeId,
    /// The node's own, ever-increasing sequence number.
    pub seq: u64,
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.node, self.seq)
    }
}

/// The direction of an adjustment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sign {
    /// Adds to the balance.
    Up,
    /// Takes from the balance.
    Down,
}

/// What an entry does to a balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// Adds value (a top-up, a refund).
    Credit,
    /// Spends value.
    Debit,
    /// A head-office correction; a shortfall written back as a receivable is one.
    Adjustment(Sign),
}

/// One immutable ledger entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The idempotency key.
    pub id: EntryId,
    /// The balance it applies to.
    pub balance: BalanceId,
    /// What it does.
    pub kind: EntryKind,
    /// The size in integer minor units; always above zero.
    pub amount: i64,
    /// When the writing node made it, by that node's clock.
    ///
    /// This is the node's word and travels as information only: head office judges quota expiry
    /// on its own receive time for the entry ([`Hub::received_at`](crate::Hub::received_at)),
    /// never on this value.
    pub at: Millis,
}

impl Entry {
    /// A new entry. The amount must be above zero.
    ///
    /// # Errors
    /// [`BalanceError::InvalidAmount`] for a zero or negative amount.
    pub fn new(
        id: EntryId,
        balance: BalanceId,
        kind: EntryKind,
        amount: i64,
        at: Millis,
    ) -> Result<Entry, BalanceError> {
        if amount <= 0 {
            return Err(BalanceError::InvalidAmount);
        }
        Ok(Entry {
            id,
            balance,
            kind,
            amount,
            at,
        })
    }

    /// The signed effect on the balance.
    #[must_use]
    pub fn delta(&self) -> i64 {
        match self.kind {
            EntryKind::Credit | EntryKind::Adjustment(Sign::Up) => self.amount,
            EntryKind::Debit | EntryKind::Adjustment(Sign::Down) => -self.amount,
        }
    }

    /// True for a debit.
    #[must_use]
    pub fn is_debit(&self) -> bool {
        self.kind == EntryKind::Debit
    }
}

/// What appending an entry did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appended {
    /// The entry was new and is now in the ledger.
    New,
    /// The identical entry was already there; nothing changed.
    Duplicate,
}

/// An append-only list of entries with a running total.
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    entries: Vec<Entry>,
    index: BTreeMap<EntryId, usize>,
    total: i64,
}

impl Ledger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Ledger {
        Ledger::default()
    }

    /// Appends `entry`, or recognises it as a redelivery.
    ///
    /// # Errors
    /// [`BalanceError::EntryConflict`] if the id exists with different content;
    /// [`BalanceError::Overflow`] if the total would overflow. The ledger is unchanged on error.
    pub fn append(&mut self, entry: Entry) -> Result<Appended, BalanceError> {
        if let Some(&at) = self.index.get(&entry.id) {
            return match self.entries.get(at) {
                Some(existing) if *existing == entry => Ok(Appended::Duplicate),
                _ => Err(BalanceError::EntryConflict { id: entry.id }),
            };
        }
        let total = self
            .total
            .checked_add(entry.delta())
            .ok_or(BalanceError::Overflow)?;
        self.index.insert(entry.id.clone(), self.entries.len());
        self.entries.push(entry);
        self.total = total;
        Ok(Appended::New)
    }

    /// The entry with this id, if it is here.
    #[must_use]
    pub fn entry(&self, id: &EntryId) -> Option<&Entry> {
        self.index.get(id).and_then(|&at| self.entries.get(at))
    }

    /// True if an entry with this id is already here.
    #[must_use]
    pub fn contains(&self, id: &EntryId) -> bool {
        self.index.contains_key(id)
    }

    /// The running total, kept as entries are appended.
    #[must_use]
    pub fn balance(&self) -> i64 {
        self.total
    }

    /// The entries in append order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The total recomputed from the entries (the invariant check: equals [`Ledger::balance`]).
    #[must_use]
    pub fn recomputed_balance(&self) -> Option<i64> {
        self.entries
            .iter()
            .try_fold(0_i64, |sum, e| sum.checked_add(e.delta()))
    }
}
