//! The receiver state machine.
//!
//! A receiver applies a batch **durably, then checkpoints** (ADR-062 section 4), in that order:
//! the checkpoint never runs ahead of the data. Idempotency is keyed by record name and version,
//! so a batch delivered twice is applied once (its records are recognised and skipped) and then
//! acknowledged. A batch that does not start at the next expected sequence is refused with
//! [`Nack::Gap`], and the sender re-sends from the checkpoint; sync never skips forward.
//!
//! The receiver is transport-independent: it sees a [`Batch`] and returns an [`Outcome`]. It
//! reads no clock and needs no runtime, so the same code drives the simulator, a loopback test
//! and, later, the HTTP binding (WP-EDG-02.T2).

use std::collections::BTreeMap;

use spot_events_contract::SourceId;

use crate::error::{Error, Result};
use crate::message::{Ack, Batch, Checkpoint, Nack, Outcome};
use crate::store::{ApplySink, CheckpointStore};

/// Flow-control limits for a receiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReceiverConfig {
    /// The largest span (in sequences) the receiver will accept in one batch; at least 1.
    pub max_batch: u64,
}

impl ReceiverConfig {
    /// A configuration; fails on a zero batch size.
    pub fn new(max_batch: u64) -> Result<Self> {
        if max_batch == 0 {
            return Err(Error::invalid_argument(
                "a batch carries at least one record",
            ));
        }
        Ok(Self { max_batch })
    }
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self { max_batch: 4_096 }
    }
}

/// The receiver: the durable apply sink and checkpoint store, plus a volatile checkpoint cache.
///
/// The sink and the store are the durable state and survive a process restart by their own
/// contract; the cache is only a convenience and [`Receiver::crash`] throws it away.
#[derive(Debug)]
pub struct Receiver<A: ApplySink, C: CheckpointStore> {
    config: ReceiverConfig,
    sink: A,
    store: C,
    sessions: BTreeMap<SourceId, Checkpoint>,
}

impl<A: ApplySink, C: CheckpointStore> Receiver<A, C> {
    /// A receiver over a durable sink and store.
    pub fn new(config: ReceiverConfig, sink: A, store: C) -> Result<Self> {
        let config = ReceiverConfig::new(config.max_batch)?;
        Ok(Self {
            config,
            sink,
            store,
            sessions: BTreeMap::new(),
        })
    }

    /// The flow-control configuration.
    pub fn config(&self) -> ReceiverConfig {
        self.config
    }

    /// The durable apply sink.
    pub fn sink(&self) -> &A {
        &self.sink
    }

    /// The durable checkpoint store.
    pub fn store(&self) -> &C {
        &self.store
    }

    /// Consumes the receiver, returning its durable sink and store.
    pub fn into_parts(self) -> (A, C) {
        (self.sink, self.store)
    }

    /// The checkpoint for `source` as the store holds it, if any.
    pub fn stored_checkpoint(&self, source: &SourceId) -> Option<Checkpoint> {
        self.store.load(source)
    }

    /// Simulates a process restart: the volatile checkpoint cache is discarded. The durable sink
    /// and store are untouched, so the next batch reloads the true checkpoint and re-applies
    /// anything that was applied but not yet checkpointed.
    pub fn crash(&mut self) {
        self.sessions.clear();
    }

    /// The next sequence expected for `source`, loading and caching the checkpoint if needed.
    fn expected(&mut self, source: &SourceId) -> u64 {
        if !self.sessions.contains_key(source) {
            let checkpoint = self
                .store
                .load(source)
                .unwrap_or_else(|| Checkpoint::start(source.clone()));
            self.sessions.insert(source.clone(), checkpoint);
        }
        self.sessions
            .get(source)
            .map_or(1, Checkpoint::next_expected)
    }

    /// The last applied sequence cached for `source`.
    fn last_applied(&self, source: &SourceId) -> u64 {
        self.sessions
            .get(source)
            .map_or(0, |checkpoint| checkpoint.last_applied_seq)
    }

    /// Handles one batch and returns the reply to send.
    ///
    /// The steps, in order: verify the checksum and the run's shape, enforce the batch-size limit,
    /// apply every sequence at or after the checkpoint **durably and idempotently**, advance the
    /// checkpoint only now, and acknowledge. A gap is refused without applying anything.
    pub fn on_batch(&mut self, batch: &Batch) -> Outcome {
        let source = batch.source.clone();
        if batch.verify().is_err() {
            return Outcome::Nack(Nack::Corrupt {
                source,
                from_seq: batch.from_seq,
            });
        }
        if batch.len() > self.config.max_batch {
            return Outcome::Nack(Nack::BatchTooLarge {
                source,
                limit: self.config.max_batch,
                got: batch.len(),
            });
        }
        let expected = self.expected(&source);
        if batch.to_seq < expected {
            // Entirely below the checkpoint: a duplicate. Acknowledge what is already stored and
            // apply nothing.
            let through_seq = self.last_applied(&source);
            return Outcome::Ack(Ack {
                source,
                through_seq,
            });
        }
        if batch.from_seq > expected {
            // A hole before the batch: never skip. The sender re-sends from the checkpoint.
            return Outcome::Nack(Nack::Gap { source, expected });
        }
        for record in &batch.records {
            if record.seq < expected {
                continue;
            }
            let key = record.key();
            if self.sink.applied(&source, &key) {
                continue;
            }
            if let Err(error) = self.sink.apply(&source, record) {
                return Outcome::Failed(error);
            }
        }
        // Every record is durable; only now may the checkpoint move.
        let checkpoint = Checkpoint {
            source: source.clone(),
            last_applied_seq: batch.to_seq,
        };
        if let Err(error) = self.store.store(&checkpoint) {
            return Outcome::Failed(error);
        }
        self.sessions.insert(source.clone(), checkpoint.clone());
        Outcome::Ack(Ack {
            source,
            through_seq: checkpoint.last_applied_seq,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::record::{Record, RecordName};
    use crate::store::{MemoryCheckpoints, MemorySink};

    fn source() -> SourceId {
        SourceId::new("head.office.app#hub").unwrap()
    }

    fn records(from: u64, count: u64) -> Vec<Record> {
        (0..count)
            .map(|index| {
                Record::new(
                    RecordName::new(&format!("d{}", from + index)).unwrap(),
                    1,
                    from + index,
                    "x",
                )
                .unwrap()
            })
            .collect()
    }

    fn receiver() -> Receiver<MemorySink, MemoryCheckpoints> {
        Receiver::new(
            ReceiverConfig::default(),
            MemorySink::new(),
            MemoryCheckpoints::new(),
        )
        .unwrap()
    }

    #[test]
    fn applies_in_order_then_checkpoints() {
        let mut receiver = receiver();
        let batch = Batch::new(source(), records(1, 3)).unwrap();
        let outcome = receiver.on_batch(&batch);
        assert!(outcome.is_ack());
        assert_eq!(
            receiver
                .stored_checkpoint(&source())
                .unwrap()
                .last_applied_seq,
            3
        );
        assert_eq!(receiver.sink().len(), 3);
    }

    #[test]
    fn a_duplicate_is_acknowledged_but_not_reapplied() {
        let mut receiver = receiver();
        let batch = Batch::new(source(), records(1, 3)).unwrap();
        assert!(receiver.on_batch(&batch).is_ack());
        assert!(receiver.on_batch(&batch).is_ack());
        assert_eq!(receiver.sink().len(), 3);
        let seqs: Vec<u64> = receiver.sink().order().iter().map(|a| a.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn a_gap_is_refused_without_skipping() {
        let mut receiver = receiver();
        let gap = Batch::new(source(), records(3, 2)).unwrap();
        match receiver.on_batch(&gap) {
            Outcome::Nack(Nack::Gap { expected, .. }) => assert_eq!(expected, 1),
            other => panic!("expected a gap nack, got {other:?}"),
        }
        assert_eq!(receiver.sink().len(), 0);
    }

    #[test]
    fn a_corrupt_batch_is_refused() {
        let mut receiver = receiver();
        let mut batch = Batch::new(source(), records(1, 2)).unwrap();
        batch.records[1].payload = "tampered".to_owned();
        match receiver.on_batch(&batch) {
            Outcome::Nack(Nack::Corrupt { from_seq, .. }) => assert_eq!(from_seq, 1),
            other => panic!("expected a corrupt nack, got {other:?}"),
        }
    }
}
