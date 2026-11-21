//! The serve cache: what a pull is answered from, and how a pull acknowledges.
//!
//! The relay's queue is durable and synchronous; the pull handler is async and must not touch
//! it. The driver thread ([`crate::driver::drive`]) therefore publishes, per destination, the
//! window of queued items as wire records, and [`ServeCache`] serves pulls from that memory.
//!
//! **A pull is the acknowledgement.** The hub's wire has no ack route: a node asks for
//! `from_seq = n` only after it applied everything below `n`, so `read(from_seq = n)` sends
//! [`AckNote`]`{ through: n - 1 }` to the driver, which trims the queue by serve sequence
//! (`HubRelay::on_ack_served_at`, WP-SYN-11.T2). A repeated or stale note is harmless: the relay
//! treats a claim at or below one it heard as a repeat, and a claim above what the queue ever
//! issued marks the destination suspect and trims nothing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_events_contract::SourceId;
use spot_sync_http::{BoxFuture, HubSource, Result};
use spot_sync_protocol::Record;
use tokio::sync::{mpsc, watch};

/// The destination a source names: the partition of `<hub site>#<node>`.
#[must_use]
pub fn dest_of(source: &SourceId) -> Option<&str> {
    source.partition()
}

/// A node's acknowledgement, by serve sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AckNote {
    /// The destination (node) that acknowledged.
    pub dest: String,
    /// Everything through this serve sequence is applied.
    pub through: u64,
}

struct Window {
    records: Vec<Record>,
    high: watch::Sender<u64>,
}

/// The windows of every destination, readable from any thread without blocking on I/O.
pub struct ServeCache {
    windows: Mutex<BTreeMap<String, Window>>,
    heads: Mutex<BTreeMap<String, u64>>,
    acks: mpsc::UnboundedSender<AckNote>,
}

impl ServeCache {
    /// An empty cache and the receiver the driver drains.
    #[must_use]
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<AckNote>) {
        let (acks, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                windows: Mutex::new(BTreeMap::new()),
                heads: Mutex::new(BTreeMap::new()),
                acks,
            }),
            receiver,
        )
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Window>> {
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replaces `dest`'s window with `records` (ascending serve sequence) and wakes waiting
    /// pulls when it grew.
    pub fn publish(&self, dest: &str, records: Vec<Record>) {
        let high = records.last().map_or(0, |record| record.seq);
        let mut windows = self.lock();
        let window = windows.entry(dest.to_owned()).or_insert_with(|| Window {
            records: Vec::new(),
            high: watch::channel(0_u64).0,
        });
        window.records = records;
        window.high.send_if_modified(|current| {
            if high > *current {
                *current = high;
                true
            } else {
                false
            }
        });
    }

    /// Records `dest`'s queue head: the newest serve sequence the relay ever assigned for it
    /// (the whole queue, not only the published window). The driver calls it once per round when
    /// its core can say ([`crate::driver::HubCore::head_serve`]).
    pub fn publish_head(&self, dest: &str, head: u64) {
        self.heads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(dest.to_owned(), head);
    }

    /// The queue head of `dest` as last published, or `None` when the relay core does not report
    /// one (the caller then falls back to the window: front plus [`depth`](Self::depth)).
    ///
    /// The head is the position an edge that has applied everything is at, so the number of items
    /// still owed to an edge that acknowledged through `n` is `head - n`, however deep the queue
    /// is beyond the published window.
    #[must_use]
    pub fn head(&self, dest: &str) -> Option<u64> {
        self.heads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(dest)
            .copied()
    }

    /// The destinations that have a window.
    #[must_use]
    pub fn destinations(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// How many records `dest` has waiting.
    #[must_use]
    pub fn depth(&self, dest: &str) -> usize {
        self.lock()
            .get(dest)
            .map_or(0, |window| window.records.len())
    }
}

impl HubSource for ServeCache {
    fn read(&self, source: &SourceId, from_seq: u64, max: usize) -> Result<Vec<Record>> {
        let Some(dest) = dest_of(source) else {
            return Ok(Vec::new());
        };
        if from_seq > 1 {
            // The node asks for `from_seq` only after applying everything below it. A closed
            // channel means the driver is gone; the pull then simply finds nothing new.
            let _ = self.acks.send(AckNote {
                dest: dest.to_owned(),
                through: from_seq - 1,
            });
        }
        if max == 0 {
            return Ok(Vec::new());
        }
        let windows = self.lock();
        let Some(window) = windows.get(dest) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<Record> = Vec::new();
        for record in window.records.iter().filter(|r| r.seq >= from_seq) {
            // A batch is a contiguous run: a gap ends it.
            if out.last().is_some_and(|last| record.seq != last.seq + 1) {
                break;
            }
            out.push(record.clone());
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    fn changed(&self, source: &SourceId, from_seq: u64) -> BoxFuture<()> {
        let dest = dest_of(source).unwrap_or_default().to_owned();
        let mut receiver = {
            let mut windows = self.lock();
            windows
                .entry(dest)
                .or_insert_with(|| Window {
                    records: Vec::new(),
                    high: watch::channel(0_u64).0,
                })
                .high
                .subscribe()
        };
        Box::pin(async move {
            // Level-triggered: the current high-water mark is checked first.
            let _ = receiver.wait_for(|high| *high >= from_seq).await;
        })
    }
}
