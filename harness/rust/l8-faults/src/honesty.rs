//! Seeded failures of the harness itself, so a green run means something.
//!
//! A suite that passes on a client which lies about its commits proves nothing. This module
//! supplies the two lies the task brief names, both caught by the durability audit and the L7
//! invariants:
//!
//! - [`LossyClient`]: a backend wrapper that **reports success for a commit it never sent** — it
//!   rolls the transaction back and returns `Ok`. The driver folds the effects into its oracle as
//!   committed, but the store does not hold them, so the audit's `acknowledged_effect_absent`
//!   fires (and the framework invariants see the missing documents).
//! - [`indeterminate_as_success`]: rewrites a history so an unresolved or resolved-absent
//!   indeterminate commit is recorded as a clean success, the exact mistake the brief warns
//!   about. The audit catches it because the effects are not in the store.
//!
//! The third proof — a cluster started with `fsync = off` and `synchronous_commit = off` that
//! **sometimes** loses acknowledged commits after a `SIGKILL` — needs a real server and is
//! `#[ignore]`d; it lives with the cluster tests. It exists to show the audit *can* fail, never
//! as part of the green suite.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_l7_invariants::history::{History, HistoryEntry, Outcome};
use spot_storage_contract::{
    Capabilities, Catalog, Change, ChangeFeed, Document, DocumentData, DocumentStore, FeedPosition,
    GraphTraversal, HealthReport, LockKey, LockMode, Locks, Name, Outbox, OutboxEvent,
    OutboxRecord, Query, QueryExecutor, ReadSource, RecordId, RowLock, Rows, Savepoint, Sequences,
    SeriesKey, StorageBackend, StorageResult, Transaction, Traversal, TraversalResult, TxOptions,
    Version, Wait,
};

/// Which promises the [`LossyClient`] breaks.
#[derive(Clone, Copy, Debug, Default)]
pub struct LossyPlan {
    /// Report success for every N-th write transaction's commit, after rolling it back (0: never).
    pub lie_one_in: u64,
}

/// A backend that sometimes reports a commit it never sent.
pub struct LossyClient<B: StorageBackend> {
    inner: Arc<B>,
    plan: Arc<LossyPlan>,
    commits: AtomicU64,
}

impl<B: StorageBackend> LossyClient<B> {
    /// Wraps `inner`.
    pub fn new(inner: Arc<B>, plan: LossyPlan) -> LossyClient<B> {
        LossyClient {
            inner,
            plan: Arc::new(plan),
            commits: AtomicU64::new(0),
        }
    }
}

/// The driver's own transactions, which are the only ones the lie may touch.
fn faulted_label(options: &TxOptions) -> bool {
    !matches!(
        options.label.as_str(),
        "l7_setup" | "l7_check" | "l7_probe" | "l7_read" | "l7_burn"
    )
}

fn every(n: u64, counter: &AtomicU64) -> bool {
    n > 0 && counter.fetch_add(1, Ordering::Relaxed) % n == n - 1
}

/// A transaction of a [`LossyClient`].
pub struct LossyTx<B: StorageBackend> {
    inner: B::Tx,
    catalog: Arc<Catalog>,
    lie: bool,
}

impl<B: StorageBackend> ChangeFeed for LossyClient<B> {
    fn read_outbox(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> impl Future<Output = StorageResult<Vec<OutboxRecord>>> + Send {
        self.inner.read_outbox(after, limit)
    }

    fn read_changes(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> impl Future<Output = StorageResult<Vec<Change>>> + Send {
        self.inner.read_changes(after, limit)
    }
}

impl<B: StorageBackend> StorageBackend for LossyClient<B> {
    type Tx = LossyTx<B>;

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn prepare(&self, catalog: &Catalog) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.prepare(catalog)
    }

    async fn begin(&self, catalog: Arc<Catalog>, options: TxOptions) -> StorageResult<LossyTx<B>> {
        let lie = faulted_label(&options) && every(self.plan.lie_one_in, &self.commits);
        let inner = self.inner.begin(catalog.clone(), options).await?;
        Ok(LossyTx {
            inner,
            catalog,
            lie,
        })
    }

    fn read(
        &self,
        catalog: Arc<Catalog>,
        source: ReadSource,
        query: &Query,
    ) -> impl Future<Output = StorageResult<Rows>> + Send {
        self.inner.read(catalog, source, query)
    }

    fn health(&self) -> impl Future<Output = HealthReport> + Send {
        self.inner.health()
    }
}

impl<B: StorageBackend> DocumentStore for LossyTx<B> {
    fn insert(
        &mut self,
        doc: &DocumentData,
    ) -> impl Future<Output = StorageResult<Version>> + Send {
        self.inner.insert(doc)
    }

    fn load(
        &mut self,
        entity: &Name,
        id: RecordId,
        lock: RowLock,
    ) -> impl Future<Output = StorageResult<Option<Document>>> + Send {
        self.inner.load(entity, id, lock)
    }

    fn update(
        &mut self,
        doc: &DocumentData,
        expected: Version,
    ) -> impl Future<Output = StorageResult<Version>> + Send {
        self.inner.update(doc, expected)
    }

    fn delete(
        &mut self,
        entity: &Name,
        id: RecordId,
        expected: Version,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.delete(entity, id, expected)
    }
}

impl<B: StorageBackend> QueryExecutor for LossyTx<B> {
    fn query(&mut self, query: &Query) -> impl Future<Output = StorageResult<Rows>> + Send {
        self.inner.query(query)
    }
}

impl<B: StorageBackend> GraphTraversal for LossyTx<B> {
    fn traverse(
        &mut self,
        traversal: &Traversal,
    ) -> impl Future<Output = StorageResult<TraversalResult>> + Send {
        self.inner.traverse(traversal)
    }
}

impl<B: StorageBackend> Locks for LossyTx<B> {
    fn lock_key(
        &mut self,
        key: &LockKey,
        mode: LockMode,
        wait: Wait,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.lock_key(key, mode, wait)
    }
}

impl<B: StorageBackend> Sequences for LossyTx<B> {
    fn allocate(
        &mut self,
        series: &SeriesKey,
        count: u32,
    ) -> impl Future<Output = StorageResult<u64>> + Send {
        self.inner.allocate(series, count)
    }

    fn set_next(
        &mut self,
        series: &SeriesKey,
        next: u64,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.set_next(series, next)
    }
}

impl<B: StorageBackend> Outbox for LossyTx<B> {
    fn enqueue(&mut self, event: &OutboxEvent) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.enqueue(event)
    }
}

impl<B: StorageBackend> Transaction for LossyTx<B> {
    fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    fn options(&self) -> &TxOptions {
        self.inner.options()
    }

    fn savepoint(&mut self) -> impl Future<Output = StorageResult<Savepoint>> + Send {
        self.inner.savepoint()
    }

    fn rollback_to(
        &mut self,
        savepoint: Savepoint,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.rollback_to(savepoint)
    }

    fn release(&mut self, savepoint: Savepoint) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.release(savepoint)
    }

    async fn commit(self) -> StorageResult<()> {
        if self.lie {
            // The lie: nothing was committed, but the client is told it was.
            self.inner.rollback().await?;
            return Ok(());
        }
        self.inner.commit().await
    }

    async fn rollback(self) -> StorageResult<()> {
        self.inner.rollback().await
    }
}

/// Rewrites `history` so every unresolved or resolved-absent indeterminate commit is recorded as
/// a clean success — the mistake a driver that treats `indeterminate` as success makes.
pub fn indeterminate_as_success(history: &History) -> History {
    let mut out = history.clone();
    for entry in &mut out.entries {
        if matches!(
            entry.outcome,
            Outcome::Indeterminate {
                resolved: None | Some(false)
            }
        ) {
            entry.outcome = Outcome::Committed;
        }
    }
    out
}

/// The number of entries an [`indeterminate_as_success`] rewrite changed.
pub fn indeterminate_rewritten(before: &History, after: &History) -> usize {
    before
        .entries
        .iter()
        .zip(after.entries.iter())
        .filter(|(b, a)| b.outcome != a.outcome)
        .count()
}

/// The history entries a rewrite changed, for a test's own reporting.
pub fn rewritten_entries(history: &History) -> Vec<&HistoryEntry> {
    history
        .entries
        .iter()
        .filter(|e| {
            matches!(
                e.outcome,
                Outcome::Indeterminate {
                    resolved: None | Some(false)
                }
            )
        })
        .collect()
}
