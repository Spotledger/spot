//! The [`QuarantineSink`] adapter into the platform's quarantine queue.
//!
//! Head office settles a batch synchronously ([`BalancePolicy::settle`](spot_shared_balance::BalancePolicy::settle)),
//! and the queue's [`QuarantineStore`] is asynchronous. The adapter bridges the two without
//! blocking: [`QuarantineSink::quarantine`] records the refusal in memory, and
//! [`QuarantineAdapter::flush`] stores it. The documented entry point is
//! [`receive_and_flush`](crate::receive_and_flush), which settles, awaits the flush and only then
//! returns the [`Acknowledgeable`](crate::Acknowledgeable) proof that the batch may be
//! acknowledged. An item that cannot be stored stays pending, the batch is not acknowledged, and
//! the sender's redelivery flushes it again — the queue is never the place an entry silently
//! disappears. Calling `flush` by hand after [`receive_batch`](crate::receive_batch) is the
//! older, unenforced path; nothing stops a caller from acknowledging before it.
//!
//! Idempotency is on `(node, entry id)`: the record name is the entry id and the source is the
//! node, so a replay of the same batch — or a second quarantine of the same entry — places one
//! item and leaves the first item's payload untouched.

use std::collections::BTreeSet;
use std::sync::Arc;

use spot_quarantine::{ItemId, NewItem, QuarantineError, QuarantineStore, RecordRef};
use spot_shared_balance::{Entry, EntryId, QuarantineSink, RejectReason, SinkError};

use crate::entry_payload;

/// A [`QuarantineSink`] that places refused entries in a [`QuarantineStore`].
pub struct QuarantineAdapter {
    store: Arc<dyn QuarantineStore>,
    pending: Vec<(Entry, RejectReason)>,
    placed: BTreeSet<(String, u64)>,
    confirmed: Vec<EntryId>,
}

impl QuarantineAdapter {
    /// An adapter that stores into `store`.
    #[must_use]
    pub fn new(store: Arc<dyn QuarantineStore>) -> Self {
        Self {
            store,
            pending: Vec::new(),
            placed: BTreeSet::new(),
            confirmed: Vec::new(),
        }
    }

    /// The entries the queue now holds that the hub has not yet been told about, drained. The
    /// caller confirms each with [`Hub::confirm_held`](spot_shared_balance::Hub::confirm_held):
    /// only then is a redelivery of the entry a repeat. [`receive_and_flush`](crate::receive_and_flush)
    /// does this after every flush, including a flush that failed part-way.
    pub fn take_confirmed(&mut self) -> Vec<EntryId> {
        std::mem::take(&mut self.confirmed)
    }

    /// How many refusals are waiting for a flush. A non-zero count after a failed flush is the
    /// batch the hub must not acknowledge.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Stores every pending refusal and returns the ids of the items it created.
    ///
    /// `at_micros` is the hub's time for the items' first-seen stamp; the adapter reads no
    /// clock. An entry the queue already holds is not stored again. A store failure leaves the
    /// failed and every later refusal pending, so the next flush retries exactly them.
    ///
    /// # Errors
    /// [`QuarantineError`] from the store or from packing an entry. Nothing is acknowledged
    /// while items are pending.
    pub async fn flush(&mut self, at_micros: i64) -> Result<Vec<ItemId>, QuarantineError> {
        let mut created = Vec::new();
        let at = 0;
        while at < self.pending.len() {
            let (entry, reason) = self.pending[at].clone();
            let key = (entry.id.node.0.clone(), entry.id.seq);
            if self.placed.contains(&key) {
                self.confirmed.push(entry.id.clone());
                self.pending.remove(at);
                continue;
            }
            let (source, record) = entry_payload::record_of(&entry, reason)?;
            let name = record.name.clone();
            let version = record.version;
            if self.store.find(&source, &name, version).await?.is_some() {
                // Already held: the same (node, entry id) was placed by an earlier adapter.
                self.placed.insert(key);
                self.confirmed.push(entry.id.clone());
                self.pending.remove(at);
                continue;
            }
            let detail = format!(
                "entry {} on balance {} was refused: {}",
                entry.id,
                entry.balance,
                reason.code()
            );
            let item = NewItem::held(
                source,
                record.seq,
                RecordRef::of(&record),
                reason.code(),
                detail,
                at_micros,
            );
            let stored = self.store.insert(item, record).await?;
            self.placed.insert(key);
            self.confirmed.push(entry.id.clone());
            created.push(stored.id);
            self.pending.remove(at);
        }
        Ok(created)
    }
}

impl QuarantineSink for QuarantineAdapter {
    /// Buffers the refusal; it is in the queue only after [`QuarantineAdapter::flush`].
    ///
    /// An entry the queue already holds is reported for confirmation straight away, so a hub
    /// whose mark was lost (or never confirmed) catches up on the redelivery.
    fn quarantine(&mut self, entry: &Entry, reason: RejectReason) -> Result<(), SinkError> {
        let key = (entry.id.node.0.clone(), entry.id.seq);
        if self.placed.contains(&key) {
            self.confirmed.push(entry.id.clone());
            return Ok(());
        }
        if self
            .pending
            .iter()
            .any(|(held, _)| held.id.node == entry.id.node && held.id.seq == entry.id.seq)
        {
            return Ok(());
        }
        self.pending.push((entry.clone(), reason));
        Ok(())
    }

    /// Custody is the queue write at `flush`, not this call.
    fn confirms_on_accept(&self) -> bool {
        false
    }
}
