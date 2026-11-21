//! Weakened backends: wrappers that break one promise of a correct backend, silently.
//!
//! They exist to prove the *driver and the invariants* are honest: a suite that passes on a
//! backend which loses outbox events, or hands numbers out for transactions that rolled back, would
//! prove nothing. Each fault is a [`FaultPlan`] field; the faults apply to the driver's operation
//! transactions only (labels `l7_create_*`, `l7_update_*`, ... ), never to its setup, its checks
//! or its read-only snapshots.
//!
//! - `drop_outbox_one_in`: every N-th write transaction's `enqueue` calls succeed and do nothing,
//!   so a committed transaction is missing its events (caught by `outbox-complete`).
//! - `burn_numbers_on_rollback`: a rolled-back allocation is not returned to the series, as with a
//!   database sequence (caught by `seq-gapless`).
//! - `lose_commit_ack_one_in`: every N-th commit reports `CommitOutcomeUnknown`; with
//!   `lost_ack_applies` the write was applied, otherwise it was not. The driver must resolve each
//!   one by reading back.
//! - `fail_commits_with`: every commit fails with this (retryable) error and the transaction is
//!   rolled back, to show retry exhaustion is recorded as failure, never success.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_storage_contract::{
    Capabilities, Catalog, Change, ChangeFeed, Document, DocumentData, DocumentStore, ErrorKind,
    FeedPosition, GraphTraversal, HealthReport, LockKey, LockMode, Locks, Name, Outbox,
    OutboxEvent, OutboxRecord, Query, QueryExecutor, ReadSource, RecordId, RowLock, Rows,
    Savepoint, Sequences, SeriesKey, StorageBackend, StorageError, StorageResult, Transaction,
    Traversal, TraversalResult, TxOptions, Version, Wait,
};

/// Which promises to break.
#[derive(Clone, Debug, Default)]
pub struct FaultPlan {
    /// Drop the outbox events of every N-th write transaction (0: never).
    pub drop_outbox_one_in: u64,
    /// Keep numbers allocated by transactions that roll back.
    pub burn_numbers_on_rollback: bool,
    /// Report `CommitOutcomeUnknown` for every N-th commit (0: never).
    pub lose_commit_ack_one_in: u64,
    /// Whether a commit whose acknowledgement is lost was applied.
    pub lost_ack_applies: bool,
    /// Fail every commit with this error kind, rolling the transaction back.
    pub fail_commits_with: Option<ErrorKind>,
}

/// A backend that breaks the promises in its [`FaultPlan`].
pub struct Faulty<B: StorageBackend> {
    inner: Arc<B>,
    plan: Arc<FaultPlan>,
    begun: AtomicU64,
    commits: AtomicU64,
}

impl<B: StorageBackend> Faulty<B> {
    /// Wraps `inner`.
    pub fn new(inner: Arc<B>, plan: FaultPlan) -> Faulty<B> {
        Faulty {
            inner,
            plan: Arc::new(plan),
            begun: AtomicU64::new(0),
            commits: AtomicU64::new(0),
        }
    }
}

fn faulted_label(options: &TxOptions) -> bool {
    !matches!(
        options.label.as_str(),
        "l7_setup" | "l7_check" | "l7_probe" | "l7_read" | "l7_burn"
    )
}

fn every(n: u64, counter: &AtomicU64) -> bool {
    n > 0 && counter.fetch_add(1, Ordering::Relaxed) % n == n - 1
}

/// A transaction of a [`Faulty`] backend.
pub struct FaultyTx<B: StorageBackend> {
    inner: B::Tx,
    backend: Arc<B>,
    catalog: Arc<Catalog>,
    plan: Arc<FaultPlan>,
    faulted: bool,
    drop_events: bool,
    lose_ack: bool,
    fail_commit: bool,
    allocated: Vec<(SeriesKey, u32)>,
}

impl<B: StorageBackend> ChangeFeed for Faulty<B> {
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

impl<B: StorageBackend> StorageBackend for Faulty<B> {
    type Tx = FaultyTx<B>;

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn prepare(&self, catalog: &Catalog) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.prepare(catalog)
    }

    async fn begin(&self, catalog: Arc<Catalog>, options: TxOptions) -> StorageResult<FaultyTx<B>> {
        let faulted = faulted_label(&options);
        let drop_events = faulted && every(self.plan.drop_outbox_one_in, &self.begun);
        let lose_ack = faulted && every(self.plan.lose_commit_ack_one_in, &self.commits);
        let fail_commit = faulted && self.plan.fail_commits_with.is_some();
        let inner = self.inner.begin(catalog.clone(), options).await?;
        Ok(FaultyTx {
            inner,
            backend: self.inner.clone(),
            catalog,
            plan: self.plan.clone(),
            faulted,
            drop_events,
            lose_ack,
            fail_commit,
            allocated: Vec::new(),
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

impl<B: StorageBackend> DocumentStore for FaultyTx<B> {
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

impl<B: StorageBackend> QueryExecutor for FaultyTx<B> {
    fn query(&mut self, query: &Query) -> impl Future<Output = StorageResult<Rows>> + Send {
        self.inner.query(query)
    }
}

impl<B: StorageBackend> GraphTraversal for FaultyTx<B> {
    fn traverse(
        &mut self,
        traversal: &Traversal,
    ) -> impl Future<Output = StorageResult<TraversalResult>> + Send {
        self.inner.traverse(traversal)
    }
}

impl<B: StorageBackend> Locks for FaultyTx<B> {
    fn lock_key(
        &mut self,
        key: &LockKey,
        mode: LockMode,
        wait: Wait,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.lock_key(key, mode, wait)
    }
}

impl<B: StorageBackend> Sequences for FaultyTx<B> {
    async fn allocate(&mut self, series: &SeriesKey, count: u32) -> StorageResult<u64> {
        let first = self.inner.allocate(series, count).await?;
        self.allocated.push((series.clone(), count));
        Ok(first)
    }

    fn set_next(
        &mut self,
        series: &SeriesKey,
        next: u64,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.set_next(series, next)
    }
}

impl<B: StorageBackend> Outbox for FaultyTx<B> {
    async fn enqueue(&mut self, event: &OutboxEvent) -> StorageResult<()> {
        if self.drop_events {
            return Ok(());
        }
        self.inner.enqueue(event).await
    }
}

impl<B: StorageBackend> Transaction for FaultyTx<B> {
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
        if self.fail_commit {
            let kind = self.plan.fail_commits_with.unwrap_or(ErrorKind::Internal);
            self.inner.rollback().await?;
            return Err(StorageError::new(kind, "injected commit failure"));
        }
        if self.lose_ack {
            if self.plan.lost_ack_applies {
                self.inner.commit().await?;
            } else {
                self.inner.rollback().await?;
            }
            return Err(StorageError::new(
                ErrorKind::CommitOutcomeUnknown,
                "injected: the commit acknowledgement was lost",
            ));
        }
        self.inner.commit().await
    }

    async fn rollback(self) -> StorageResult<()> {
        self.inner.rollback().await?;
        if self.faulted && self.plan.burn_numbers_on_rollback && !self.allocated.is_empty() {
            // The numbers are consumed anyway, as a database sequence would: take them again in
            // a transaction of their own and commit it.
            let options = TxOptions::new(
                Name::new("l7_burn")?,
                spot_storage_contract::Isolation::ReadCommitted,
            );
            let mut tx = self.backend.begin(self.catalog.clone(), options).await?;
            for (series, count) in &self.allocated {
                tx.allocate(series, *count).await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }
}
