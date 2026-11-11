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

/// A queue held in maps, one per destination.
#[derive(Debug, Clone, Default)]
pub struct MemoryQueue {
    items: BTreeMap<String, BTreeMap<ItemId, Pending<QueuedBundle>>>,
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
    fn push(&mut self, dest: &str, item: Pending<QueuedBundle>) -> Result<PushOutcome, HubError> {
        if self.failing.contains(dest) {
            return Err(HubError::QueueFailed {
                reason: "push refused by the test double".to_owned(),
            });
        }
        let id = (item.position, item.item.index, item.item.redelivery);
        let map = self.items.entry(dest.to_owned()).or_default();
        if map.contains_key(&id) {
            return Ok(PushOutcome::Duplicate);
        }
        map.insert(id, item);
        Ok(PushOutcome::Queued)
    }

    fn peek(&self, dest: &str, n: usize) -> Result<Vec<Pending<QueuedBundle>>, HubError> {
        let mut all: Vec<&Pending<QueuedBundle>> = self
            .items
            .get(dest)
            .map(|m| m.values().collect())
            .unwrap_or_default();
        all.sort_by_key(|p| (p.lane, p.position, p.item.index));
        Ok(all.into_iter().take(n).cloned().collect())
    }

    fn ack(&mut self, dest: &str, up_to: u64) -> Result<usize, HubError> {
        let Some(map) = self.items.get_mut(dest) else {
            return Ok(0);
        };
        let before = map.len();
        map.retain(|(position, _, redelivery), _| *redelivery || *position > up_to);
        Ok(before - map.len())
    }

    fn remove_redelivered(&mut self, dest: &str, position: u64) -> Result<usize, HubError> {
        let Some(map) = self.items.get_mut(dest) else {
            return Ok(0);
        };
        let before = map.len();
        map.retain(|(p, _, redelivery), _| !(*redelivery && *p == position));
        Ok(before - map.len())
    }

    fn queued_bytes(&self, dest: &str) -> Result<usize, HubError> {
        Ok(self
            .items
            .get(dest)
            .map_or(0, |m| m.values().map(|p| p.item.bundle.bytes).sum()))
    }
}
