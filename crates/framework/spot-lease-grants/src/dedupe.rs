//! The bounded memory of recent requests, so a retry gets the same range.
//!
//! A grant is answered from the log, but the *answer* can be lost: the node reconnects and asks
//! again with the same envelope id. Remembering `(node, series, request_id) -> the range` for
//! the last few requests of each node turns that retry into the same range instead of a fresh
//! one, which would burn numbers the node never saw.
//!
//! The memory is **bounded per node** ([`GrantPolicy`](crate::GrantPolicy)'s
//! `max_remembered_requests`, 512 by default) and evicts oldest first: a hub that remembered
//! every id a node ever sent would grow with the node's asking. A retry older than the node's
//! last few hundred requests is treated as a new request — it is still safe, it only costs a
//! range.
//!
//! The index is rebuilt from the [`GrantLog`](crate::GrantLog) when the service starts
//! ([`DedupeIndex::from_records`]), which is what makes a retry that crosses a restart return
//! the range the node never received.

use std::collections::{BTreeMap, VecDeque};

use spot_partition::RangeLease;

use crate::identity::RequestId;
use crate::log::GrantRecord;

/// What one request is remembered by within a node: the series and the envelope id.
type Key = (String, String);

/// The requests remembered for one node, oldest first.
#[derive(Debug, Default)]
struct NodeMemory {
    /// The keys in the order they were remembered, for eviction.
    order: VecDeque<Key>,
    /// The range each key was answered with.
    ranges: BTreeMap<Key, RangeLease>,
}

/// `(node, series, request_id)` to the range the request was answered with.
#[derive(Debug)]
pub(crate) struct DedupeIndex {
    per_node: BTreeMap<String, NodeMemory>,
    cap: usize,
    remembered: usize,
}

impl DedupeIndex {
    /// An empty index that remembers at most `cap` requests per node (`0` remembers none).
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            per_node: BTreeMap::new(),
            cap,
            remembered: 0,
        }
    }

    /// An index rebuilt from `records`, oldest first, keeping the newest `cap` per node.
    pub(crate) fn from_records(records: &[GrantRecord], cap: usize) -> Self {
        let mut index = Self::new(cap);
        for record in records {
            if let Some(id) = &record.request_id {
                index.remember(
                    record.node.as_str(),
                    record.lease.series.as_str(),
                    id,
                    record.lease.clone(),
                );
            }
        }
        index
    }

    /// The range `(node, series, id)` was answered with, when it is still remembered.
    ///
    /// `id` is the request's envelope id; a request without one has no key and is never a
    /// replay.
    pub(crate) fn lookup(
        &self,
        node: &str,
        series: &str,
        id: Option<&RequestId>,
    ) -> Option<RangeLease> {
        let id = id?;
        let key = (series.to_owned(), id.as_str().to_owned());
        self.per_node
            .get(node)
            .and_then(|memory| memory.ranges.get(&key))
            .cloned()
    }

    /// Remembers that `(node, series, id)` was answered with `lease`, evicting the node's
    /// oldest entry when it already holds `cap`.
    pub(crate) fn remember(&mut self, node: &str, series: &str, id: &RequestId, lease: RangeLease) {
        if self.cap == 0 {
            return;
        }
        let key = (series.to_owned(), id.as_str().to_owned());
        let memory = self.per_node.entry(node.to_owned()).or_default();
        if memory.ranges.insert(key.clone(), lease).is_some() {
            // The same key again (a replay that was then remembered twice): still one entry.
            return;
        }
        memory.order.push_back(key);
        self.remembered += 1;
        while memory.order.len() > self.cap {
            let Some(oldest) = memory.order.pop_front() else {
                break;
            };
            if memory.ranges.remove(&oldest).is_some() {
                self.remembered -= 1;
            }
        }
    }

    /// How many requests are remembered across all nodes.
    pub(crate) fn len(&self) -> usize {
        self.remembered
    }
}
