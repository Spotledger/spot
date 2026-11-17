//! The push side: the node's own outbox as an [`Offer`], with a dense push cursor.
//!
//! The wire numbers a batch's records contiguously (`from_seq..=to_seq`), but the outbox has gaps
//! (an event the hub does not forward, an event of another class). So the **push sequence** is a
//! dense counter, and the [`PushCursor`] stores the pair `(seq, position)`: the highest push
//! sequence the hub acknowledged and the outbox position of that record. After a cut the next
//! offer re-reads the outbox after `position` and numbers it from `seq + 1`; the outbox is
//! append-only and ordered, so the same records get the same numbers and the hub's
//! `(name, version)` idempotence makes a repeat harmless (G3, G4).
//!
//! Ownership (ADR-062 G1): only records of partitions the node owns are pushed. The node's outbox
//! holds only its own writes (a replicated record enqueues no event, and the write gate refuses a
//! write to a record the node does not own), and the reader still marks each entry `owned`; an
//! entry marked not owned is **never pushed** and fails the offer with `sync_node_host.not_owned`,
//! so a bug upstream is loud, not silent. The hub checks again on its side.

use spot_events_contract::SourceId;
use spot_sync_protocol::{Offer, Record, RecordName};

use crate::error::{Error, Result};

/// One entry of the node's own outbox, as the reader maps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxEntry {
    /// The outbox position (strictly ascending, may skip).
    pub position: u64,
    /// The record name the hub keys it by (`<entity>/<id>`).
    pub name: String,
    /// The record version (at least 1).
    pub version: u64,
    /// The encoded record image or event. Never logged.
    pub payload: String,
    /// Whether the node owns the record's partition.
    pub owned: bool,
}

/// Reads the node's own outbox (blocking; lives on a [`Worker`](crate::worker::Worker) thread).
pub trait OutboxSource {
    /// Entries with a position above `after`, ascending, at most about `max`, whole source
    /// transactions only.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    fn read_after(&mut self, after: u64, max: usize) -> Result<Vec<OutboxEntry>>;

    /// How many forwarded entries lie above `after` (the queue the operator sees).
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    fn queued_after(&mut self, after: u64) -> Result<u64>;
}

/// The acknowledged push position, stored durably.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PushCursor {
    /// The highest push sequence the hub acknowledged.
    pub seq: u64,
    /// The outbox position of the record with that sequence.
    pub position: u64,
}

/// Where the [`PushCursor`] is kept.
pub trait PushCursorStore {
    /// The stored cursor (all zero when none).
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    fn load(&mut self) -> Result<PushCursor>;

    /// Stores a cursor; never called with one below the stored.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    fn store(&mut self, cursor: PushCursor) -> Result<()>;
}

/// The loop's view of the push side (object-safe, so one worker type serves any outbox).
pub trait PushSide {
    /// The node's own stream.
    fn source(&self) -> SourceId;

    /// The offer of the node's source: every record after the acknowledged position, numbered
    /// densely from the cursor's `seq + 1`, at most `max` records.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`, `sync_node_host.not_owned`.
    fn offer(&mut self, max: usize) -> Result<Offer>;

    /// The hub acknowledged the push stream through `seq`; persists the cursor.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    fn acked(&mut self, through_seq: u64) -> Result<()>;

    /// The stored cursor's push sequence (what the hub acknowledged so far).
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    fn acked_seq(&mut self) -> Result<u64>;

    /// How many changes are queued above the acknowledged position.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    fn queued(&mut self) -> Result<u64>;
}

/// [`PushSide`] over an [`OutboxSource`] and a [`PushCursorStore`].
pub struct OutboxSender<O: OutboxSource, S: PushCursorStore> {
    outbox: O,
    store: S,
    source: SourceId,
    cursor: Option<PushCursor>,
    /// `(seq, position)` of the last offer, to turn an acknowledged sequence into a position.
    window: Vec<(u64, u64)>,
}

impl<O: OutboxSource, S: PushCursorStore> OutboxSender<O, S> {
    /// A sender of the node's own stream `source`.
    pub fn new(outbox: O, store: S, source: SourceId) -> Self {
        Self {
            outbox,
            store,
            source,
            cursor: None,
            window: Vec::new(),
        }
    }

    fn cursor(&mut self) -> Result<PushCursor> {
        if let Some(cursor) = self.cursor {
            return Ok(cursor);
        }
        let cursor = self.store.load()?;
        self.cursor = Some(cursor);
        Ok(cursor)
    }
}

impl<O: OutboxSource, S: PushCursorStore> PushSide for OutboxSender<O, S> {
    fn source(&self) -> SourceId {
        self.source.clone()
    }

    fn offer(&mut self, max: usize) -> Result<Offer> {
        let cursor = self.cursor()?;
        let entries = self.outbox.read_after(cursor.position, max.max(1))?;
        let mut records = Vec::with_capacity(entries.len());
        let mut window = Vec::with_capacity(entries.len());
        let mut last_position = cursor.position;
        let mut seq = cursor.seq;
        for entry in entries.into_iter().take(max.max(1)) {
            if entry.position <= last_position {
                return Err(Error::store_failed(
                    "the outbox returned a position that does not ascend",
                ));
            }
            if !entry.owned {
                // Never pushed: a record this node does not own (G1). Loud on purpose.
                return Err(Error::not_owned(format!(
                    "the outbox entry at position {} names a partition this node does not own",
                    entry.position
                )));
            }
            seq = seq.saturating_add(1);
            let name = RecordName::new(&entry.name).map_err(|_| {
                Error::store_failed(format!(
                    "the outbox entry at position {} has an unusable record name",
                    entry.position
                ))
            })?;
            let record = Record::new(name, entry.version.max(1), seq, entry.payload)
                .map_err(|error| Error::store_failed(error.to_string()))?;
            records.push(record);
            window.push((seq, entry.position));
            last_position = entry.position;
        }
        self.window = window;
        Ok(Offer::new(self.source.clone(), records))
    }

    fn acked(&mut self, through_seq: u64) -> Result<()> {
        let cursor = self.cursor()?;
        if through_seq <= cursor.seq {
            return Ok(());
        }
        let Some((seq, position)) = self
            .window
            .iter()
            .rev()
            .find(|(seq, _)| *seq <= through_seq)
            .copied()
        else {
            // An acknowledgement for a sequence the last offer did not carry: ignore it (the
            // next offer is re-read from the stored position).
            return Ok(());
        };
        let next = PushCursor { seq, position };
        self.store.store(next)?;
        self.cursor = Some(next);
        self.window.retain(|(s, _)| *s > seq);
        Ok(())
    }

    fn acked_seq(&mut self) -> Result<u64> {
        Ok(self.cursor()?.seq)
    }

    fn queued(&mut self) -> Result<u64> {
        let cursor = self.cursor()?;
        self.outbox.queued_after(cursor.position)
    }
}
