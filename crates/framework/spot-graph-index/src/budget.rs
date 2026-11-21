//! The memory budget and the per-tenant store of hot graphs.
//!
//! A tenant may hold several graphs. The store accounts every graph's estimated bytes and, when
//! the total exceeds the cap, drops the **least-recently-used cold graph** — the one whose last
//! `get` or `apply` is oldest. A dropped graph is not lost: the next query falls back to the
//! database path (the executor sees no index and asks storage). A **pinned** graph is never a
//! victim.
//!
//! Recency is a logical tick the caller passes in; the store never reads the wall clock, so
//! eviction is deterministic and testable. Ties break by graph key, so the same inputs always
//! drop the same graph.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::GraphIndexError;
use crate::event::{Applied, EdgeEvent};
use crate::index::GraphIndex;
use crate::key::GraphKey;
use crate::source::GraphSource;

/// The byte cap for a tenant's hot graphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryBudget {
    cap_bytes: u64,
}

impl MemoryBudget {
    /// A budget of `cap_bytes` bytes. A cap of 0 holds nothing.
    #[must_use]
    pub fn new(cap_bytes: u64) -> Self {
        Self { cap_bytes }
    }

    /// The cap.
    #[must_use]
    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }
}

/// One stored graph: the graph, its accounted bytes, its recency and its pin.
struct Entry {
    index: Arc<GraphIndex>,
    bytes: u64,
    last_used: u64,
    pinned: bool,
}

/// The hot graphs of one tenant, bounded by a [`MemoryBudget`].
pub struct IndexStore {
    budget: MemoryBudget,
    graphs: BTreeMap<GraphKey, Entry>,
    used: u64,
}

impl IndexStore {
    /// An empty store with `budget`.
    #[must_use]
    pub fn new(budget: MemoryBudget) -> Self {
        Self {
            budget,
            graphs: BTreeMap::new(),
            used: 0,
        }
    }

    /// The budget.
    #[must_use]
    pub fn budget(&self) -> MemoryBudget {
        self.budget
    }

    /// The bytes the store currently holds.
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.used
    }

    /// How many graphs the store holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.graphs.len()
    }

    /// Whether the store holds no graphs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.graphs.is_empty()
    }

    /// Whether the store holds `key`.
    #[must_use]
    pub fn contains(&self, key: &GraphKey) -> bool {
        self.graphs.contains_key(key)
    }

    /// The graph for `key`, touching its recency. `None` when it is cold.
    #[must_use]
    pub fn get(&mut self, key: &GraphKey, now: u64) -> Option<Arc<GraphIndex>> {
        let entry = self.graphs.get_mut(key)?;
        entry.last_used = now;
        Some(entry.index.clone())
    }

    /// The graph for `key` without touching its recency, for inspection and tests.
    #[must_use]
    pub fn peek(&self, key: &GraphKey) -> Option<&Arc<GraphIndex>> {
        self.graphs.get(key).map(|entry| &entry.index)
    }

    /// Whether `key` is pinned against eviction.
    #[must_use]
    pub fn is_pinned(&self, key: &GraphKey) -> bool {
        self.graphs.get(key).is_some_and(|entry| entry.pinned)
    }

    /// Pins `key` so it is never evicted. Pinning a cold graph does nothing.
    pub fn pin(&mut self, key: &GraphKey) {
        if let Some(entry) = self.graphs.get_mut(key) {
            entry.pinned = true;
        }
    }

    /// Unpins `key`.
    pub fn unpin(&mut self, key: &GraphKey) {
        if let Some(entry) = self.graphs.get_mut(key) {
            entry.pinned = false;
        }
    }

    /// Removes `key` from the store, returning the graph when it was held.
    pub fn remove(&mut self, key: &GraphKey) -> Option<Arc<GraphIndex>> {
        let entry = self.graphs.remove(key)?;
        self.used = self.used.saturating_sub(entry.bytes);
        Some(entry.index)
    }

    /// Inserts a freshly built graph, replacing any graph for the same key. A replaced graph's
    /// pin is carried over.
    ///
    /// # Errors
    ///
    /// [`GraphIndexError::BudgetExceeded`] when the cap cannot be met without dropping a pinned
    /// graph.
    pub fn insert(
        &mut self,
        index: GraphIndex,
        now: u64,
    ) -> Result<Arc<GraphIndex>, GraphIndexError> {
        let key = index.key().clone();
        let pinned = self.is_pinned(&key);
        let arc = Arc::new(index);
        let bytes = arc.estimated_bytes();
        let previous = self.graphs.insert(
            key.clone(),
            Entry {
                index: arc.clone(),
                bytes,
                last_used: now,
                pinned,
            },
        );
        match previous {
            Some(old) => {
                self.used = self.used.saturating_sub(old.bytes).saturating_add(bytes);
            }
            None => self.used = self.used.saturating_add(bytes),
        }
        self.enforce_cap(Some(&key))?;
        Ok(arc)
    }

    /// Applies one event, creating the graph if it is cold, then re-accounts and evicts cold
    /// graphs if the cap is exceeded.
    ///
    /// # Errors
    ///
    /// Whatever [`GraphIndex::apply`] fails with, and
    /// [`GraphIndexError::BudgetExceeded`] when the cap cannot be met.
    pub fn apply(&mut self, event: &EdgeEvent, now: u64) -> Result<Applied, GraphIndexError> {
        let key = event.graph.clone();
        if !self.graphs.contains_key(&key) {
            self.insert(GraphIndex::new(key.clone()), now)?;
        }
        let entry = self
            .graphs
            .get_mut(&key)
            .ok_or_else(|| GraphIndexError::source("the graph vanished during apply"))?;
        let applied = Arc::make_mut(&mut entry.index).apply(event);
        entry.last_used = now;
        self.reaccount(&key);
        self.enforce_cap(Some(&key))?;
        applied
    }

    /// Rebuilds `key` from `source`, building the new index off to the side and swapping it in
    /// with one store operation, so a reader that captured the old graph keeps reading the old
    /// complete graph and a reader that arrives after the swap sees only the new complete graph.
    ///
    /// # Errors
    ///
    /// Whatever the source fails with, [`GraphIndexError::InvalidEdge`] for a malformed streamed
    /// edge, and [`GraphIndexError::BudgetExceeded`].
    pub fn rebuild(
        &mut self,
        key: &GraphKey,
        source: &dyn GraphSource,
        now: u64,
    ) -> Result<Arc<GraphIndex>, GraphIndexError> {
        let edges = source.stream_edges(key)?;
        let watermark = source.watermark(key)?;
        let index = GraphIndex::build(key.clone(), watermark, edges)?;
        self.insert(index, now)
    }

    /// Drops the least-recently-used unpinned graph until the total fits the cap.
    ///
    /// `exempt` names the graph a caller is currently writing, so a mutation never evicts the
    /// graph it just changed out from under itself.
    ///
    /// # Errors
    ///
    /// [`GraphIndexError::BudgetExceeded`] when the cap is still exceeded and no unpinned graph
    /// remains to drop.
    pub fn enforce_cap(&mut self, exempt: Option<&GraphKey>) -> Result<(), GraphIndexError> {
        while self.used > self.budget.cap_bytes {
            let victim = self
                .graphs
                .iter()
                .filter(|(key, entry)| !entry.pinned && Some(*key) != exempt)
                .min_by(|(ka, ea), (kb, eb)| {
                    ea.last_used.cmp(&eb.last_used).then_with(|| ka.cmp(kb))
                })
                .map(|(key, _)| key.clone());
            match victim {
                Some(key) => {
                    self.remove(&key);
                }
                None => {
                    return Err(GraphIndexError::BudgetExceeded {
                        cap: self.budget.cap_bytes,
                        used: self.used,
                    });
                }
            }
        }
        Ok(())
    }

    /// Refreshes one graph's accounted bytes after a mutation.
    fn reaccount(&mut self, key: &GraphKey) {
        if let Some(entry) = self.graphs.get_mut(key) {
            let bytes = entry.index.estimated_bytes();
            self.used = self.used.saturating_sub(entry.bytes).saturating_add(bytes);
            entry.bytes = bytes;
        }
    }
}

impl std::fmt::Debug for IndexStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexStore")
            .field("budget", &self.budget)
            .field("graphs", &self.graphs.len())
            .field("used", &self.used)
            .finish()
    }
}
