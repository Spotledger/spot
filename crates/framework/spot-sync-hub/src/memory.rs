//! In-memory doubles of the stream and the queue, the reference for the persistent adapters.

use crate::error::HubError;
use crate::model::{PushOutcome, QueuedBundle, StreamEntry};
use crate::traits::{HubStream, SendQueue};
use spot_sync_obligations::Pending;
use std::collections::BTreeMap;

/// A stream held in a vector. Entries must be pushed in ascending position order.
#[derive(Debug, Clone, Default)]
pub struct MemoryStream {
    entries: Vec<StreamEntry>,
}

impl MemoryStream {
    /// An empty stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an entry.
    pub fn append(&mut self, entry: StreamEntry) {
        self.entries.push(entry);
    }

    /// All entries.
    #[must_use]
    pub fn entries(&self) -> &[StreamEntry] {
        &self.entries
    }
}

impl HubStream for MemoryStream {
    fn read_after(&self, after: u64, max: usize) -> Result<Vec<StreamEntry>, HubError> {
        let mut out: Vec<StreamEntry> = Vec::new();
        for e in self.entries.iter().filter(|e| e.position > after) {
            if out.len() >= max && out.last().is_some_and(|last| last.txn != e.txn) {
                break;
            }
            out.push(e.clone());
        }
        Ok(out)
    }

    fn head(&self) -> Result<u64, HubError> {
        Ok(self.entries.last().map_or(0, |e| e.position))
    }
}

type ItemId = (u64, u32, bool);

/// One destination's queue: its items and the newest serve sequence it ever assigned.
#[derive(Debug, Clone, Default)]
struct DestQueue {
    items: BTreeMap<ItemId, Pending<QueuedBundle>>,
    last_seq: u64,
}

/// A queue held in maps, one per destination.
#[derive(Debug, Clone, Default)]
pub struct MemoryQueue {
    dests: BTreeMap<String, DestQueue>,
    /// Destinations whose pushes fail, for tests of failure isolation.
    failing: std::collections::BTreeSet<String>,
}

impl MemoryQueue {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every push to `dest` fail until `fail` is set false.
    pub fn set_failing(&mut self, dest: &str, fail: bool) {
        if fail {
            self.failing.insert(dest.to_owned());
        } else {
            self.failing.remove(dest);
        }
    }

    /// Everything queued for `dest`, in serving order.
    #[must_use]
    pub fn all(&self, dest: &str) -> Vec<Pending<QueuedBundle>> {
        self.peek(dest, usize::MAX).unwrap_or_default()
    }
}

impl SendQueue for MemoryQueue {
    fn push(
        &mut self,
        dest: &str,
        mut item: Pending<QueuedBundle>,
    ) -> Result<PushOutcome, HubError> {
        if self.failing.contains(dest) {
            return Err(HubError::QueueFailed {
                reason: "push refused by the test double".to_owned(),
            });
        }
        let id = (item.position, item.item.index, item.item.redelivery);
        let queue = self.dests.entry(dest.to_owned()).or_default();
        if queue.items.contains_key(&id) {
            return Ok(PushOutcome::Duplicate);
        }
        queue.last_seq += 1;
        item.item.serve_seq = queue.last_seq;
        item.item.serve_after = 0;
        queue.items.insert(id, item);
        Ok(PushOutcome::Queued)
    }

    fn peek(&self, dest: &str, n: usize) -> Result<Vec<Pending<QueuedBundle>>, HubError> {
        let Some(queue) = self.dests.get(dest) else {
            return Ok(Vec::new());
        };
        let mut seqs: Vec<u64> = queue.items.values().map(|p| p.item.serve_seq).collect();
        seqs.sort_unstable();
        let mut all: Vec<&Pending<QueuedBundle>> = queue.items.values().collect();
        all.sort_by_key(|p| (p.lane, p.position, p.item.index));
        // The item with the lowest serve sequence is always in the window. The node's
        // acknowledgement is a prefix of the serve sequence, so an item it settled out of order
        // (a mandatory-lane one served first) stays queued until the lower ones are applied; if
        // those never got a place in the window, the window would be re-served for ever.
        let head = queue.items.values().map(|p| p.item.serve_seq).min();
        let mut window: Vec<&Pending<QueuedBundle>> = all.iter().copied().take(n).collect();
        if let Some(head) = head
            && n > 1
            && !window.iter().any(|p| p.item.serve_seq == head)
            && let Some(first) = all.iter().copied().find(|p| p.item.serve_seq == head)
        {
            window.pop();
            window.push(first);
            window.sort_by_key(|p| (p.lane, p.position, p.item.index));
        }
        Ok(window
            .into_iter()
            .map(|p| {
                let mut out = p.clone();
                let below = seqs.partition_point(|s| *s < p.item.serve_seq);
                out.item.serve_after = below.checked_sub(1).map_or(0, |i| seqs[i]);
                out
            })
            .collect())
    }

    fn ack(&mut self, dest: &str, up_to: u64) -> Result<usize, HubError> {
        let Some(queue) = self.dests.get_mut(dest) else {
            return Ok(0);
        };
        let before = queue.items.len();
        queue
            .items
            .retain(|(position, _, redelivery), _| *redelivery || *position > up_to);
        Ok(before - queue.items.len())
    }

    fn last_serve_seq(&self, dest: &str) -> Result<u64, HubError> {
        Ok(self.dests.get(dest).map_or(0, |q| q.last_seq))
    }

    fn lowest_position_after(&self, dest: &str, through: u64) -> Result<Option<u64>, HubError> {
        Ok(self.dests.get(dest).and_then(|q| {
            q.items
                .values()
                .filter(|p| !p.item.redelivery && p.item.serve_seq > through)
                .map(|p| p.position)
                .min()
        }))
    }

    fn ack_served(&mut self, dest: &str, through: u64) -> Result<usize, HubError> {
        let Some(queue) = self.dests.get_mut(dest) else {
            return Ok(0);
        };
        let before = queue.items.len();
        queue.items.retain(|_, p| p.item.serve_seq > through);
        Ok(before - queue.items.len())
    }

    fn remove_redelivered(&mut self, dest: &str, position: u64) -> Result<usize, HubError> {
        let Some(queue) = self.dests.get_mut(dest) else {
            return Ok(0);
        };
        let before = queue.items.len();
        queue
            .items
            .retain(|(p, _, redelivery), _| !(*redelivery && *p == position));
        Ok(before - queue.items.len())
    }

    fn queued_bytes(&self, dest: &str) -> Result<usize, HubError> {
        Ok(self
            .dests
            .get(dest)
            .map_or(0, |q| q.items.values().map(|p| p.item.bundle.bytes).sum()))
    }
}
