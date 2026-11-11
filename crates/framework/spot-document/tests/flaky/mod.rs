//! A storage backend that fails commits on request, wrapped around a real one.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use spot_storage_contract::*;

/// One injected commit failure.
#[derive(Clone, Copy, Debug)]
pub struct Fault {
    pub kind: ErrorKind,
    /// Commit for real, then report the failure: a commit whose outcome is unknown.
    pub commit_first: bool,
}

impl Fault {
    pub fn rolled_back(kind: ErrorKind) -> Self {
        Self {
            kind,
            commit_first: false,
        }
    }
}

#[derive(Default)]
struct Shared {
    plan: Mutex<VecDeque<Fault>>,
    begins: AtomicU32,
}

pub struct FlakyBackend<B> {
    inner: B,
    shared: Arc<Shared>,
}

impl<B> FlakyBackend<B> {
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            shared: Arc::default(),
        }
    }

    /// The next commit (and each one after, in order) fails with `fault`.
    pub fn inject(&self, fault: Fault) {
        self.shared.plan.lock().unwrap().push_back(fault);
    }

    /// How many transactions were opened.
    pub fn begins(&self) -> u32 {
        self.shared.begins.load(Ordering::SeqCst)
    }
}

impl<B: ChangeFeed> ChangeFeed for FlakyBackend<B> {
    async fn read_outbox(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> StorageResult<Vec<OutboxRecord>> {
        self.inner.read_outbox(after, limit).await
    }

    async fn read_changes(&self, after: FeedPosition, limit: u32) -> StorageResult<Vec<Change>> {
        self.inner.read_changes(after, limit).await
    }
}

impl<B: StorageBackend> StorageBackend for FlakyBackend<B> {
    type Tx = FlakyTx<B::Tx>;

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn prepare(&self, catalog: &Catalog) -> StorageResult<()> {
        self.inner.prepare(catalog).await
    }

    async fn begin(&self, catalog: Arc<Catalog>, options: TxOptions) -> StorageResult<Self::Tx> {
        self.shared.begins.fetch_add(1, Ordering::SeqCst);
        Ok(FlakyTx {
            inner: self.inner.begin(catalog, options).await?,
            shared: self.shared.clone(),
        })
    }

    async fn read(
        &self,
        catalog: Arc<Catalog>,
        source: ReadSource,
        query: &Query,
    ) -> StorageResult<Rows> {
        self.inner.read(catalog, source, query).await
    }

    async fn health(&self) -> HealthReport {
        self.inner.health().await
    }
}

pub struct FlakyTx<T> {
    inner: T,
    shared: Arc<Shared>,
}

impl<T: Transaction> DocumentStore for FlakyTx<T> {
    async fn insert(&mut self, doc: &DocumentData) -> StorageResult<Version> {
        self.inner.insert(doc).await
    }

    async fn load(
        &mut self,
        entity: &Name,
        id: RecordId,
        lock: RowLock,
    ) -> StorageResult<Option<Document>> {
        self.inner.load(entity, id, lock).await
    }

    async fn update(&mut self, doc: &DocumentData, expected: Version) -> StorageResult<Version> {
        self.inner.update(doc, expected).await
    }

    async fn delete(
        &mut self,
        entity: &Name,
        id: RecordId,
        expected: Version,
    ) -> StorageResult<()> {
        self.inner.delete(entity, id, expected).await
    }
}

impl<T: Transaction> QueryExecutor for FlakyTx<T> {
    async fn query(&mut self, query: &Query) -> StorageResult<Rows> {
        self.inner.query(query).await
    }
}

impl<T: Transaction> GraphTraversal for FlakyTx<T> {
    async fn traverse(&mut self, traversal: &Traversal) -> StorageResult<TraversalResult> {
        self.inner.traverse(traversal).await
    }
}

impl<T: Transaction> Locks for FlakyTx<T> {
    async fn lock_key(&mut self, key: &LockKey, mode: LockMode, wait: Wait) -> StorageResult<()> {
        self.inner.lock_key(key, mode, wait).await
    }
}

impl<T: Transaction> Sequences for FlakyTx<T> {
    async fn allocate(&mut self, series: &SeriesKey, count: u32) -> StorageResult<u64> {
        self.inner.allocate(series, count).await
    }

    async fn set_next(&mut self, series: &SeriesKey, next: u64) -> StorageResult<()> {
        self.inner.set_next(series, next).await
    }
}

impl<T: Transaction> Outbox for FlakyTx<T> {
    async fn enqueue(&mut self, event: &OutboxEvent) -> StorageResult<()> {
        self.inner.enqueue(event).await
    }
}

impl<T: Transaction> Transaction for FlakyTx<T> {
    fn catalog(&self) -> &Catalog {
        self.inner.catalog()
    }

    fn options(&self) -> &TxOptions {
        self.inner.options()
    }

    async fn savepoint(&mut self) -> StorageResult<Savepoint> {
        self.inner.savepoint().await
    }

    async fn rollback_to(&mut self, savepoint: Savepoint) -> StorageResult<()> {
        self.inner.rollback_to(savepoint).await
    }

    async fn release(&mut self, savepoint: Savepoint) -> StorageResult<()> {
        self.inner.release(savepoint).await
    }

    async fn commit(self) -> StorageResult<()> {
        let fault = self.shared.plan.lock().unwrap().pop_front();
        match fault {
            None => self.inner.commit().await,
            Some(fault) => {
                if fault.commit_first {
                    self.inner.commit().await?;
                } else {
                    self.inner.rollback().await?;
                }
                Err(StorageError::new(fault.kind, "injected by the test"))
            }
        }
    }

    async fn rollback(self) -> StorageResult<()> {
        self.inner.rollback().await
    }
}
