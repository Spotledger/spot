//! The sender state machine.
//!
//! A sender turns one source's readable stream into bounded [`Batch`]es and tracks what the
//! receiver has acknowledged. It never guesses a skip: a [`Nack`] rewinds its cursor to the
//! sequence the receiver named, and a timeout re-sends everything not yet acknowledged. Because
//! delivery is at least once, resending is always safe; the receiver's idempotent apply absorbs
//! the duplicates (ADR-062 G4).
//!
//! Flow control is two numbers: `max_batch` bounds one batch, and `window` bounds how many
//! sequences may be outstanding ahead of the acknowledged high-water mark. When the window is
//! full, [`Sender::next`] returns [`Action::Wait`]; the session waits for an [`Ack`] instead of
//! piling on more.

use std::collections::BTreeMap;

use spot_events_contract::SourceId;

use crate::error::{Error, Result};
use crate::message::{Ack, Action, Batch, Nack, Offer};
use crate::record::Record;

/// Flow-control limits for a sender.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SenderConfig {
    /// The most records one batch may carry; at least 1.
    pub max_batch: usize,
    /// The most sequences that may be outstanding ahead of the acknowledged high-water mark; at
    /// least 1.
    pub window: u64,
}

impl SenderConfig {
    /// A configuration; fails on a zero batch size or window.
    pub fn new(max_batch: usize, window: u64) -> Result<Self> {
        if max_batch == 0 {
            return Err(Error::invalid_argument(
                "a batch carries at least one record",
            ));
        }
        if window == 0 {
            return Err(Error::invalid_argument(
                "the in-flight window is at least one",
            ));
        }
        Ok(Self { max_batch, window })
    }
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            max_batch: 128,
            window: 1_024,
        }
    }
}

/// One source's send cursor.
#[derive(Clone, Copy, Debug)]
struct Session {
    /// The next sequence to send (1 when nothing has been sent).
    next_seq: u64,
    /// The last sequence the receiver acknowledged, or 0.
    acked_through: u64,
    /// The current batch cap, halved when the receiver reports a batch too large.
    batch_cap: usize,
}

/// The sender: one cursor per source.
#[derive(Debug)]
pub struct Sender {
    config: SenderConfig,
    sessions: BTreeMap<SourceId, Session>,
}

impl Sender {
    /// A sender with the given flow control.
    pub fn new(config: SenderConfig) -> Result<Self> {
        // Re-validate through the constructor so a struct literal that bypasses `new` cannot slip
        // through either.
        let config = SenderConfig::new(config.max_batch, config.window)?;
        Ok(Self {
            config,
            sessions: BTreeMap::new(),
        })
    }

    /// The flow-control configuration.
    pub fn config(&self) -> SenderConfig {
        self.config
    }

    /// Produces the next action for one offered source.
    ///
    /// Records at or after the cursor, contiguous from it and inside the window, are packaged
    /// into a batch of at most `max_batch` records. Anything else returns [`Action::Wait`]: there
    /// is nothing new, the offer has a hole before the cursor, or the window is full.
    pub fn next(&mut self, offer: &Offer) -> Action {
        let config = self.config;
        let session = self
            .sessions
            .entry(offer.source.clone())
            .or_insert_with(|| Session {
                next_seq: 1,
                acked_through: 0,
                batch_cap: config.max_batch,
            });
        let limit = session.acked_through.saturating_add(config.window);
        if session.next_seq > limit {
            return Action::Wait;
        }
        let mut selected: Vec<Record> = Vec::new();
        let mut expected = session.next_seq;
        for record in &offer.records {
            if record.seq < expected {
                continue;
            }
            if record.seq != expected || record.seq > limit {
                break;
            }
            selected.push(record.clone());
            expected = expected.saturating_add(1);
            if selected.len() >= session.batch_cap {
                break;
            }
        }
        if selected.is_empty() {
            return Action::Wait;
        }
        match Batch::new(offer.source.clone(), selected) {
            Ok(batch) => {
                session.next_seq = expected;
                Action::Send(batch)
            }
            // The selection is contiguous by construction, so this is unreachable; a malformed
            // offer is treated as "nothing to send" rather than a panic.
            Err(_) => Action::Wait,
        }
    }

    /// Records an acknowledgement and advances the cursor past it.
    pub fn on_ack(&mut self, ack: &Ack) {
        if let Some(session) = self.sessions.get_mut(&ack.source) {
            session.acked_through = session.acked_through.max(ack.through_seq);
            session.next_seq = session
                .next_seq
                .max(session.acked_through.saturating_add(1));
        }
    }

    /// Records a refusal and rewinds the cursor to the sequence the receiver named.
    pub fn on_nack(&mut self, nack: &Nack) {
        match nack {
            Nack::Gap { source, expected } => self.rewind(source, *expected),
            Nack::Corrupt { source, from_seq } => self.rewind(source, *from_seq),
            // The receiver refused a batch as too large; halve this source's batch cap and retry
            // from the last acknowledged sequence, where the request will be smaller.
            Nack::BatchTooLarge { source, .. } => {
                let cap = self.config.max_batch;
                let session = self.sessions.entry(source.clone()).or_insert(Session {
                    next_seq: 1,
                    acked_through: 0,
                    batch_cap: cap,
                });
                session.batch_cap = (session.batch_cap / 2).max(1);
                session.next_seq = session.acked_through.saturating_add(1);
            }
            // Session-level refusals are handled by the session, not the cursor.
            Nack::VersionMismatch { .. } | Nack::MetadataEpoch { .. } => {}
        }
    }

    /// Re-sends everything not yet acknowledged: the receiver may have dropped the answer.
    pub fn on_timeout(&mut self, source: &SourceId) {
        if let Some(session) = self.sessions.get_mut(source) {
            session.next_seq = session.acked_through.saturating_add(1);
        }
    }

    /// Rewinds one source to `to`, never below the acknowledged high-water mark.
    pub fn rewind(&mut self, source: &SourceId, to: u64) {
        if let Some(session) = self.sessions.get_mut(source) {
            let floor = session.acked_through.saturating_add(1);
            session.next_seq = to.max(floor);
        }
    }

    /// Adopts a receiver's checkpoint as this source's cursor (used by a pull session: the edge
    /// names where it will resume).
    pub fn resume(&mut self, source: &SourceId, from_seq: u64) {
        let cap = self.config.max_batch;
        let session = self.sessions.entry(source.clone()).or_insert(Session {
            next_seq: 1,
            acked_through: 0,
            batch_cap: cap,
        });
        session.acked_through = session.acked_through.max(from_seq.saturating_sub(1));
        session.next_seq = session.acked_through.saturating_add(1);
    }

    /// The last acknowledged sequence for `source`.
    pub fn acked_through(&self, source: &SourceId) -> u64 {
        self.sessions
            .get(source)
            .map_or(0, |session| session.acked_through)
    }

    /// The next sequence the sender will send for `source`.
    pub fn next_seq(&self, source: &SourceId) -> u64 {
        self.sessions
            .get(source)
            .map_or(1, |session| session.next_seq)
    }

    /// Simulates a process restart: volatile cursor drift is discarded and every source resumes
    /// from its last acknowledged sequence, so anything unacknowledged is re-sent.
    pub fn crash(&mut self) {
        for session in self.sessions.values_mut() {
            session.next_seq = session.acked_through.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::message::Checkpoint;
    use crate::record::RecordName;

    fn source() -> SourceId {
        SourceId::new("head.office.app#hub").unwrap()
    }

    fn offer(from: u64, count: u64) -> Offer {
        let records = (0..count)
            .map(|index| {
                Record::new(
                    RecordName::new(&format!("d{}", from + index)).unwrap(),
                    1,
                    from + index,
                    "x",
                )
                .unwrap()
            })
            .collect();
        Offer::new(source(), records)
    }

    #[test]
    fn next_batches_up_to_the_limit_and_respects_the_window() {
        let mut sender = Sender::new(SenderConfig::new(2, 3).unwrap()).unwrap();
        let Action::Send(first) = sender.next(&offer(1, 10)) else {
            panic!("expected a batch");
        };
        assert_eq!((first.from_seq, first.to_seq), (1, 2));
        assert_eq!(sender.next_seq(&source()), 3);
        let Action::Send(second) = sender.next(&offer(1, 10)) else {
            panic!("expected a batch");
        };
        assert_eq!((second.from_seq, second.to_seq), (3, 3));
        // Window 3 from acked 0 means sequences 1..=3, so the cursor is now 4 and blocked.
        assert!(sender.next(&offer(1, 10)).is_wait());
        sender.on_ack(&Ack {
            source: source(),
            through_seq: 3,
        });
        let Action::Send(third) = sender.next(&offer(1, 10)) else {
            panic!("expected a batch after the ack");
        };
        assert_eq!(third.from_seq, 4);
    }

    #[test]
    fn a_gap_nack_rewinds_to_the_named_sequence() {
        let mut sender = Sender::new(SenderConfig::default()).unwrap();
        let _ = sender.next(&offer(1, 5));
        sender.on_nack(&Nack::Gap {
            source: source(),
            expected: 2,
        });
        assert_eq!(sender.next_seq(&source()), 2);
        let Action::Send(batch) = sender.next(&offer(1, 5)) else {
            panic!("expected a resend");
        };
        assert_eq!(batch.from_seq, 2);
    }

    #[test]
    fn resume_adopts_a_checkpoint() {
        let mut sender = Sender::new(SenderConfig::default()).unwrap();
        sender.resume(&source(), 7);
        assert_eq!(sender.next_seq(&source()), 7);
        assert_eq!(sender.acked_through(&source()), 6);
        let checkpoint = Checkpoint {
            source: source(),
            last_applied_seq: 6,
        };
        assert_eq!(checkpoint.next_expected(), sender.next_seq(&source()));
    }
}
