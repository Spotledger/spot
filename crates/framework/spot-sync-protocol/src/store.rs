//! The durable side of a receiver: where records are applied and where checkpoints are stored.
//!
//! Two traits make the two-phase rule of ADR-062 section 4 explicit:
//!
//! 1. [`ApplySink::apply`] stores a record **durably** and idempotently, keyed by
//!    [`RecordKey`] (name and version). A second apply of the same key changes nothing.
//! 2. [`CheckpointStore::store`] then records how far the stream has been applied, and is only
//!    called after every record in the batch returned from `apply`.
//!
//! A crash between the two leaves the checkpoint behind the applied data. The next delivery
//! re-applies the records (the sink recognises the keys and does nothing) and then stores the
//! checkpoint. `apply` returning `Ok` is the promise that the record survives a crash of this
//! process; nothing in this crate assumes it, but the receiver relies on it for no-loss.
//!
//! [`MemorySink`] and [`MemoryCheckpoints`] are reference in-memory implementations for tests and
//! the simulator, not for production.

use std::collections::BTreeMap;

use spot_events_contract::SourceId;

use crate::error::Result;
use crate::message::Checkpoint;
use crate::record::{Record, RecordKey};

/// Where a receiver applies records durably.
pub trait ApplySink {
    /// Whether `key` from `source` has already been applied durably.
    fn applied(&self, source: &SourceId, key: &RecordKey) -> bool;

    /// Applies one record durably and idempotently, returning once the write is durable.
    ///
    /// Applying a key that [`applied`](ApplySink::applied) already reports must succeed and
    /// change nothing; the receiver may call it again after a crash between apply and
    /// checkpoint.
    fn apply(&mut self, source: &SourceId, record: &Record) -> Result<()>;
}

/// Where a receiver stores the per-source checkpoint after a durable apply.
pub trait CheckpointStore {
    /// The stored checkpoint for `source`, if any.
    fn load(&self, source: &SourceId) -> Option<Checkpoint>;

    /// Stores `checkpoint`, replacing any earlier one, once its batch is durably applied.
    fn store(&mut self, checkpoint: &Checkpoint) -> Result<()>;
}

impl<T: ApplySink + ?Sized> ApplySink for &mut T {
    fn applied(&self, source: &SourceId, key: &RecordKey) -> bool {
        (**self).applied(source, key)
    }

    fn apply(&mut self, source: &SourceId, record: &Record) -> Result<()> {
        (**self).apply(source, record)
    }
}

impl<T: CheckpointStore + ?Sized> CheckpointStore for &mut T {
    fn load(&self, source: &SourceId) -> Option<Checkpoint> {
        (**self).load(source)
    }

    fn store(&mut self, checkpoint: &Checkpoint) -> Result<()> {
        (**self).store(checkpoint)
    }
}

/// One recorded application, in the order the sink accepted it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The stream the record came from.
    pub source: SourceId,
    /// The record's apply key.
    pub key: RecordKey,
    /// The record's stream sequence.
    pub seq: u64,
}

/// An in-memory [`ApplySink`]: a set of applied keys plus the order they were applied in.
///
/// It records every apply so a test can prove the final set and the per-source order, and it
/// deduplicates by key exactly as a real apply path must.
#[derive(Clone, Debug, Default)]
pub struct MemorySink {
    applied: BTreeMap<(SourceId, RecordKey), Record>,
    order: Vec<Applied>,
}

impl MemorySink {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct keys have been applied.
    pub fn len(&self) -> usize {
        self.applied.len()
    }

    /// Whether nothing has been applied.
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }

    /// Whether `key` from `source` is applied.
    pub fn contains(&self, source: &SourceId, key: &RecordKey) -> bool {
        self.applied.contains_key(&(source.clone(), key.clone()))
    }

    /// The stored record for `key`, if applied.
    pub fn get(&self, source: &SourceId, key: &RecordKey) -> Option<&Record> {
        self.applied.get(&(source.clone(), key.clone()))
    }

    /// Every application, in application order.
    pub fn order(&self) -> &[Applied] {
        &self.order
    }

    /// Every applied key for `source`, in key order.
    pub fn keys_for(&self, source: &SourceId) -> Vec<RecordKey> {
        self.applied
            .keys()
            .filter(|(from, _)| from == source)
            .map(|(_, key)| key.clone())
            .collect()
    }

    /// Whether every `(source, key)` pair has been applied.
    pub fn has_all<'a>(
        &self,
        expected: impl IntoIterator<Item = (&'a SourceId, &'a RecordKey)>,
    ) -> bool {
        expected
            .into_iter()
            .all(|(source, key)| self.contains(source, key))
    }
}

impl ApplySink for MemorySink {
    fn applied(&self, source: &SourceId, key: &RecordKey) -> bool {
        self.applied.contains_key(&(source.clone(), key.clone()))
    }

    fn apply(&mut self, source: &SourceId, record: &Record) -> Result<()> {
        let key = record.key();
        if self
            .applied
            .insert((source.clone(), key.clone()), record.clone())
            .is_none()
        {
            self.order.push(Applied {
                source: source.clone(),
                key,
                seq: record.seq,
            });
        }
        Ok(())
    }
}

/// An in-memory [`CheckpointStore`].
#[derive(Clone, Debug, Default)]
pub struct MemoryCheckpoints {
    map: BTreeMap<SourceId, Checkpoint>,
}

impl MemoryCheckpoints {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every stored checkpoint.
    pub fn all(&self) -> impl Iterator<Item = &Checkpoint> {
        self.map.values()
    }
}

impl CheckpointStore for MemoryCheckpoints {
    fn load(&self, source: &SourceId) -> Option<Checkpoint> {
        self.map.get(source).cloned()
    }

    fn store(&mut self, checkpoint: &Checkpoint) -> Result<()> {
        self.map
            .insert(checkpoint.source.clone(), checkpoint.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::record::RecordName;

    fn source() -> SourceId {
        SourceId::new("head.office.app#hub").unwrap()
    }

    fn record(seq: u64) -> Record {
        Record::new(RecordName::new("d").unwrap(), seq, seq, "x").unwrap()
    }

    #[test]
    fn memory_sink_deduplicates_and_keeps_order() {
        let mut sink = MemorySink::new();
        sink.apply(&source(), &record(1)).unwrap();
        sink.apply(&source(), &record(2)).unwrap();
        sink.apply(&source(), &record(1)).unwrap();
        assert_eq!(sink.len(), 2);
        let seqs: Vec<u64> = sink.order().iter().map(|a| a.seq).collect();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[test]
    fn a_cached_checkpoint_round_trips() {
        let mut store = MemoryCheckpoints::new();
        store
            .store(&Checkpoint {
                source: source(),
                last_applied_seq: 9,
            })
            .unwrap();
        assert_eq!(store.load(&source()).unwrap().last_applied_seq, 9);
    }
}
