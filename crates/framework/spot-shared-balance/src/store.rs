//! The hub's store port for refused-entry marks.
//!
//! A hub that keeps its [`Mark`] values only in memory forgets them on restart: an entry that
//! was *held* comes back as unknown, and an entry that was only *offered* — handed to the sink
//! but not yet in the queue — is forgotten entirely, so nothing offers it again and the refusal
//! is silently lost. This module is the port a host implements to make the marks survive, and
//! [`MemoryRefusalStore`] is the reference implementation the in-memory hub uses.
//!
//! # The contract
//!
//! - A [`RefusalRecord`] carries the mark **and** the reason it was refused: the two are one
//!   piece of state, because a mark without its reason cannot be re-offered correctly.
//! - [`RefusalStore::save`] writes every record it is given in **one transaction**: all of them
//!   land, or none does. A crash never leaves a *held* mark beside an entry the queue does not
//!   hold, or an *offered* mark under a queue item that exists.
//! - [`RefusalStore::save`] is an upsert keyed by entry id; a later save replaces the record.
//! - [`RefusalStore::load`] returns every record, ordered by entry id.
//!
//! [`testing::assert_refusal_store_contract`](crate::testing) runs exactly these checks against
//! any durable store, so a new implementation is proven before a hub is built on it.

use std::collections::BTreeMap;

use crate::hub::Mark;
use crate::ledger::EntryId;
use crate::quarantine::RejectReason;

/// One refused entry as the hub persists it: how far its quarantine got and why it was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalRecord {
    /// The entry that was refused.
    pub id: EntryId,
    /// Whether the sink confirmed custody (the mark is a real, persisted state).
    pub mark: Mark,
    /// The reason head office refused it, so a restart re-offers it unchanged.
    pub reason: RejectReason,
}

/// The store could not write or read the refusal marks.
///
/// It carries a short, static reason and never an entry, a balance value or a payload. The
/// store belongs to the host, so a failure keeps the port's own vocabulary instead of raising a
/// new `shared_balance.*` code: a store is infrastructure, not a balance operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the refusal store failed: {why}")]
pub struct StoreError {
    /// What the store could not do.
    pub why: &'static str,
}

/// Where a hub persists its refusal marks, so a restart neither forgets a held entry nor loses
/// an offered one.
///
/// A durable implementation is the host's (this crate writes nothing): the port fixes what it
/// must guarantee, and [`testing::assert_refusal_store_contract`](crate::testing) proves it.
pub trait RefusalStore {
    /// Writes `records` in one transaction: every record lands, or none does.
    ///
    /// # Errors
    /// [`StoreError`] when the transaction could not be committed. The store is unchanged on
    /// error, so the caller retries the same records.
    fn save(&mut self, records: &[RefusalRecord]) -> Result<(), StoreError>;

    /// Every persisted record, ordered by entry id.
    ///
    /// # Errors
    /// [`StoreError`] when the store cannot be read.
    fn load(&self) -> Result<Vec<RefusalRecord>, StoreError>;
}

/// The in-memory [`RefusalStore`]: the reference implementation, and what an in-memory hub keeps
/// its marks in.
///
/// A save builds the whole next state and swaps it in, so a save that fails (see
/// [`MemoryRefusalStore::failing`]) changes nothing: the all-or-nothing rule holds here too.
#[derive(Clone, Debug, Default)]
pub struct MemoryRefusalStore {
    records: BTreeMap<EntryId, RefusalRecord>,
    /// For tests: the next this-many saves fail and write nothing.
    fail_next: u32,
}

impl MemoryRefusalStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> MemoryRefusalStore {
        MemoryRefusalStore::default()
    }

    /// A store whose next `calls` saves fail with [`StoreError`].
    #[must_use]
    pub fn failing(calls: u32) -> MemoryRefusalStore {
        MemoryRefusalStore {
            records: BTreeMap::new(),
            fail_next: calls,
        }
    }

    /// How many records the store holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True when the store holds no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Makes the next `calls` saves fail, the way a test drives a broken transaction.
    pub fn fail_next_saves(&mut self, calls: u32) {
        self.fail_next = calls;
    }
}

impl RefusalStore for MemoryRefusalStore {
    fn save(&mut self, records: &[RefusalRecord]) -> Result<(), StoreError> {
        if self.fail_next > 0 {
            self.fail_next -= 1;
            return Err(StoreError {
                why: "the store is unavailable",
            });
        }
        let mut next = self.records.clone();
        for record in records {
            next.insert(record.id.clone(), record.clone());
        }
        self.records = next;
        Ok(())
    }

    fn load(&self) -> Result<Vec<RefusalRecord>, StoreError> {
        Ok(self.records.values().cloned().collect())
    }
}
