//! The hub's two data seams: the stream it serves on a pull, and the sink it applies a push to.
//!
//! The binding is deliberately ignorant of storage: [`HubSource`] is how a pull reads a node's
//! stream and waits for more, and [`PushSink`] is how a push feeds the receiver state machine.
//! [`MemoryHub`] and [`SharedReceiver`] are the in-process implementations tests (and the
//! loopback transport) use; the storage-backed ones arrive with the apply path (WP-EDG-06).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_events_contract::SourceId;
use spot_sync_protocol::{Batch, Outcome, Receiver, Record};
use tokio::sync::watch;

use crate::BoxFuture;
use crate::error::Result;

/// A node's readable stream, as the pull handler sees it.
pub trait HubSource: Send + Sync + 'static {
    /// Reads up to `max` records at or after `from_seq`, in ascending sequence order.
    ///
    /// # Errors
    ///
    /// [`Error::unavailable`] when the underlying stream cannot be read.
    fn read(&self, source: &SourceId, from_seq: u64, max: usize) -> Result<Vec<Record>>;

    /// Resolves when records at or after `from_seq` may be readable, or when the sender is gone.
    ///
    /// Cancellation-safe: dropping the future loses no notification, because the wait re-checks
    /// the current high-water mark when it starts.
    fn changed(&self, source: &SourceId, from_seq: u64) -> BoxFuture<()>;
}

/// An in-memory [`HubSource`]: a map of source to its records plus a watch channel per source.
///
/// The watch channel carries the source's high-water sequence, so [`HubSource::changed`] is a
/// level-triggered wait: a record appended between a read and the wait is still observed.
#[derive(Debug, Default)]
pub struct MemoryHub {
    records: Mutex<BTreeMap<SourceId, Vec<Record>>>,
    watermarks: Mutex<BTreeMap<SourceId, watch::Sender<u64>>>,
}

impl MemoryHub {
    /// An empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends records to a source, in ascending sequence order, and wakes every waiter.
    ///
    /// The records become readable at once; the high-water mark becomes the highest sequence
    /// appended.
    pub fn append(&self, source: &SourceId, records: impl IntoIterator<Item = Record>) {
        let mut appended = 0_u64;
        {
            let mut all = self.lock_records();
            let entry = all.entry(source.clone()).or_default();
            for record in records {
                appended = appended.max(record.seq);
                entry.push(record);
            }
            entry.sort_by_key(|record| record.seq);
        }
        if appended > 0 {
            let mut watermarks = self.lock_watermarks();
            let sender = watermarks.entry(source.clone()).or_insert_with(|| {
                let (sender, _receiver) = watch::channel(0_u64);
                sender
            });
            let current = *sender.borrow();
            if appended > current {
                let _ = sender.send_replace(appended);
            }
        }
    }

    /// How many records a source holds.
    pub fn len(&self, source: &SourceId) -> usize {
        self.lock_records()
            .get(source)
            .map_or(0, |records| records.len())
    }

    /// Whether a source holds no records.
    pub fn is_empty(&self, source: &SourceId) -> bool {
        self.len(source) == 0
    }

    fn lock_records(&self) -> std::sync::MutexGuard<'_, BTreeMap<SourceId, Vec<Record>>> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_watermarks(&self) -> std::sync::MutexGuard<'_, BTreeMap<SourceId, watch::Sender<u64>>> {
        self.watermarks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl HubSource for MemoryHub {
    fn read(&self, source: &SourceId, from_seq: u64, max: usize) -> Result<Vec<Record>> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let all = self.lock_records();
        let Some(records) = all.get(source) else {
            return Ok(Vec::new());
        };
        Ok(records
            .iter()
            .filter(|record| record.seq >= from_seq)
            .take(max)
            .cloned()
            .collect())
    }

    fn changed(&self, source: &SourceId, from_seq: u64) -> BoxFuture<()> {
        let mut receiver = {
            let mut watermarks = self.lock_watermarks();
            let sender = watermarks.entry(source.clone()).or_insert_with(|| {
                let (sender, _receiver) = watch::channel(0_u64);
                sender
            });
            sender.subscribe()
        };
        Box::pin(async move {
            // A level-triggered wait: `wait_for` checks the current value first, so a record
            // appended before this future is polled still wakes it. Dropping it loses nothing.
            let _ = receiver.wait_for(|watermark| *watermark >= from_seq).await;
        })
    }
}

/// Where a push applies its batch: the receiver state machine, behind a lock.
pub trait PushSink: Send + Sync + 'static {
    /// Applies one batch and returns the reply to send.
    fn on_batch(&self, batch: &Batch) -> Outcome;
}

/// A [`PushSink`] over the durable receiver state machine, shared behind a mutex.
///
/// The lock is held only for the synchronous apply, never across an await, so it is a plain
/// `std::sync::Mutex`.
#[derive(Debug)]
pub struct SharedReceiver<A: spot_sync_protocol::ApplySink, C: spot_sync_protocol::CheckpointStore>
{
    inner: Arc<Mutex<Receiver<A, C>>>,
}

impl<A: spot_sync_protocol::ApplySink, C: spot_sync_protocol::CheckpointStore> Clone
    for SharedReceiver<A, C>
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<A: spot_sync_protocol::ApplySink, C: spot_sync_protocol::CheckpointStore>
    SharedReceiver<A, C>
{
    /// Wraps a receiver.
    pub fn new(receiver: Receiver<A, C>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(receiver)),
        }
    }

    /// Runs `f` against the receiver under the lock.
    pub fn with<R>(&self, f: impl FnOnce(&mut Receiver<A, C>) -> R) -> R {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    }
}

impl<A, C> PushSink for SharedReceiver<A, C>
where
    A: spot_sync_protocol::ApplySink + Send + 'static,
    C: spot_sync_protocol::CheckpointStore + Send + 'static,
{
    fn on_batch(&self, batch: &Batch) -> Outcome {
        self.with(|receiver| receiver.on_batch(batch))
    }
}

/// A [`HubSource`] and [`PushSink`] pair that a test can read back, over one in-memory hub.
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use spot_sync_protocol::{MemoryCheckpoints, MemorySink, ReceiverConfig, RecordName};

    fn source(partition: &str) -> SourceId {
        SourceId::new(&format!("head-office#{partition}")).unwrap()
    }

    fn record(seq: u64) -> Record {
        Record::new(RecordName::new(&format!("d{seq}")).unwrap(), 1, seq, "x").unwrap()
    }

    #[test]
    fn the_hub_serves_from_a_sequence() {
        let hub = MemoryHub::new();
        let source = source("hub");
        hub.append(&source, [record(1), record(2), record(3)]);
        let read = hub.read(&source, 2, 10).unwrap();
        assert_eq!(read.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(hub.read(&source, 1, 1).unwrap().len(), 1);
        assert_eq!(hub.len(&source), 3);
    }

    #[tokio::test]
    async fn changed_wakes_on_a_later_append() {
        let hub = Arc::new(MemoryHub::new());
        let source = source("hub");
        let waiter = {
            let hub = Arc::clone(&hub);
            let source = source.clone();
            tokio::spawn(async move { hub.changed(&source, 1).await })
        };
        tokio::task::yield_now().await;
        hub.append(&source, [record(1)]);
        waiter.await.unwrap();
    }

    #[tokio::test]
    async fn a_sink_applies_a_batch() {
        let receiver = Receiver::new(
            ReceiverConfig::default(),
            MemorySink::new(),
            MemoryCheckpoints::new(),
        )
        .unwrap();
        let sink = SharedReceiver::new(receiver);
        let batch = Batch::new(source("hub"), vec![record(1), record(2)]).unwrap();
        assert!(sink.on_batch(&batch).is_ack());
        assert_eq!(sink.with(|r| r.sink().len()), 2);
    }
}
