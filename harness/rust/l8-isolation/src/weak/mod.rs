//! Weakened backends: wrappers that break **one** isolation rule, on purpose.
//!
//! A suite that cannot see a broken promise proves nothing, so each wrapper delegates to a real
//! backend and then breaks exactly one rule. The checker must report the matching anomaly with
//! a witness, and must not report anomalies the weakening cannot cause.
//!
//! | [`Weakness`] | What it breaks | The anomaly it must cause |
//! | --- | --- | --- |
//! | [`Weakness::DirtyRead`] | a read sees another transaction's uncommitted write | G1a/G1b/G1c at read committed |
//! | [`Weakness::NoLockLostUpdate`] | a write overwrites the row without checking the version | lost update |
//! | [`Weakness::SnapshotPerStatement`] | a repeatable-read transaction re-reads the latest committed data on every statement | read skew: G-single / non-repeatable read |
//! | [`Weakness::SerializableNoValidation`] | a serializable transaction runs as repeatable read (no serialisation validation) | write skew: G2 |
//! | [`Weakness::StaleReplica`] | some reads are served from an older copy | stale read / real-time anomaly |
//!
//! Each fault is applied only to workload transactions; the setup transaction and the checker's
//! own reads are never faulted. The wrappers are for the L8 test entity; the dirty-read overlay
//! and the stale copy are keyed by record id, so they also work for any document entity whose
//! writes go through `update`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use spot_storage_contract::{
    Capabilities, Catalog, Change, ChangeFeed, Document, DocumentData, DocumentStore, ErrorKind,
    FeedPosition, Fields, GraphTraversal, HealthReport, Isolation, LockKey, LockMode, Locks, Name,
    Outbox, OutboxEvent, OutboxRecord, Query, QueryExecutor, ReadSource, RecordId, RowLock, Rows,
    Savepoint, Sequences, SeriesKey, StorageBackend, StorageError, StorageResult, Transaction,
    Traversal, TraversalResult, TxOptions, Version, Wait,
};

/// One deliberately broken isolation rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weakness {
    /// Reads see uncommitted writes.
    DirtyRead,
    /// Writes interleave without a version check (last writer wins).
    NoLockLostUpdate,
    /// A snapshot is refreshed per statement while claiming repeatable read.
    SnapshotPerStatement,
    /// No serialisation validation while claiming serializable.
    SerializableNoValidation,
    /// Some reads are served from a stale copy.
    StaleReplica,
}

impl Weakness {
    /// The name used in reports.
    pub fn name(self) -> &'static str {
        match self {
            Weakness::DirtyRead => "dirty-read",
            Weakness::NoLockLostUpdate => "no-lock-lost-update",
            Weakness::SnapshotPerStatement => "snapshot-per-statement",
            Weakness::SerializableNoValidation => "serializable-no-validation",
            Weakness::StaleReplica => "stale-replica",
        }
    }

    /// The anomaly class this weakness is expected to cause.
    pub fn expected(self) -> &'static str {
        match self {
            Weakness::DirtyRead => "G1a/G1b/G1c",
            Weakness::NoLockLostUpdate => "lost-update",
            Weakness::SnapshotPerStatement => "G-single",
            Weakness::SerializableNoValidation => "G2",
            Weakness::StaleReplica => "stale-read",
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// State shared across the transactions of a weakened backend.
#[derive(Default)]
struct WeakState {
    /// Last written fields per record, for the dirty-read overlay (never cleared, so an aborted
    /// write stays visible as a G1a read).
    overlay: Mutex<HashMap<RecordId, Fields>>,
    /// An older copy of the fields per record, for the stale replica.
    previous: Mutex<HashMap<RecordId, Fields>>,
    /// A counter so the stale replica serves a deterministic share of reads.
    reads: AtomicU64,
}

/// A backend that breaks the rule named by its [`Weakness`].
pub struct Weakened<B: StorageBackend> {
    inner: Arc<B>,
    weakness: Weakness,
    state: Arc<WeakState>,
}

impl<B: StorageBackend> Weakened<B> {
    /// Wraps `inner`, breaking `weakness`.
    pub fn new(inner: Arc<B>, weakness: Weakness) -> Weakened<B> {
        Weakened {
            inner,
            weakness,
            state: Arc::new(WeakState::default()),
        }
    }

    /// The weakness.
    pub fn weakness(&self) -> Weakness {
        self.weakness
    }
}

/// A transaction of a [`Weakened`] backend.
pub struct WeakTx<B: StorageBackend> {
    inner: B::Tx,
    /// The backend, so the lost-update weakness can apply an immediate write of its own.
    backend: Arc<B>,
    weakness: Weakness,
    options: TxOptions,
    catalog: Arc<Catalog>,
    state: Arc<WeakState>,
    dirty_ids: HashSet<RecordId>,
    /// Writes this transaction applied immediately (the lost-update weakness), so its own reads
    /// still see them even though the outer snapshot does not.
    own_writes: HashMap<RecordId, (Fields, Version)>,
}

impl<B: StorageBackend> ChangeFeed for Weakened<B> {
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

impl<B: StorageBackend> StorageBackend for Weakened<B> {
    type Tx = WeakTx<B>;

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn prepare(&self, catalog: &Catalog) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.prepare(catalog)
    }

    async fn begin(&self, catalog: Arc<Catalog>, options: TxOptions) -> StorageResult<WeakTx<B>> {
        let mut inner_options = options.clone();
        match self.weakness {
            Weakness::SnapshotPerStatement if options.isolation != Isolation::ReadCommitted => {
                inner_options.isolation = Isolation::ReadCommitted;
            }
            Weakness::SerializableNoValidation if options.isolation == Isolation::Serializable => {
                inner_options.isolation = Isolation::RepeatableRead;
            }
            _ => {}
        }
        let inner = self.inner.begin(catalog.clone(), inner_options).await?;
        Ok(WeakTx {
            inner,
            backend: self.inner.clone(),
            weakness: self.weakness,
            options,
            catalog,
            state: self.state.clone(),
            dirty_ids: HashSet::new(),
            own_writes: HashMap::new(),
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

impl<B: StorageBackend> DocumentStore for WeakTx<B> {
    async fn insert(&mut self, doc: &DocumentData) -> StorageResult<Version> {
        let version = self.inner.insert(doc).await?;
        match self.weakness {
            Weakness::DirtyRead => {
                lock(&self.state.overlay).insert(doc.id, doc.fields.clone());
            }
            Weakness::StaleReplica => {
                lock(&self.state.previous).insert(doc.id, doc.fields.clone());
            }
            _ => {}
        }
        Ok(version)
    }

    async fn load(
        &mut self,
        entity: &Name,
        id: RecordId,
        row_lock: RowLock,
    ) -> StorageResult<Option<Document>> {
        let inner_doc = self.inner.load(entity, id, row_lock).await?;
        let fallback = || inner_doc.as_ref().map(|d| d.version).unwrap_or(Version(1));
        match self.weakness {
            Weakness::NoLockLostUpdate => {
                if let Some((fields, version)) = self.own_writes.get(&id) {
                    return Ok(Some(Document {
                        data: DocumentData {
                            entity: entity.clone(),
                            id,
                            fields: fields.clone(),
                            children: BTreeMap::new(),
                        },
                        version: *version,
                    }));
                }
            }
            Weakness::DirtyRead => {
                if let Some(fields) = lock(&self.state.overlay).get(&id).cloned() {
                    self.dirty_ids.insert(id);
                    return Ok(Some(Document {
                        data: DocumentData {
                            entity: entity.clone(),
                            id,
                            fields,
                            children: BTreeMap::new(),
                        },
                        version: fallback(),
                    }));
                }
            }
            Weakness::StaleReplica => {
                let n = self.state.reads.fetch_add(1, Ordering::Relaxed);
                if n % 2 == 1
                    && let Some(fields) = lock(&self.state.previous).get(&id).cloned()
                {
                    return Ok(Some(Document {
                        data: DocumentData {
                            entity: entity.clone(),
                            id,
                            fields,
                            children: BTreeMap::new(),
                        },
                        version: fallback(),
                    }));
                }
            }
            _ => {}
        }
        Ok(inner_doc)
    }

    async fn update(&mut self, doc: &DocumentData, expected: Version) -> StorageResult<Version> {
        match self.weakness {
            Weakness::NoLockLostUpdate => {
                // No read lock and no version check: apply the write at once, in its own
                // read-committed transaction, overwriting whatever is there. This is what makes
                // the update lost when two transactions write the value they both read.
                let mut options =
                    TxOptions::new(Name::new("l8_weak_write")?, Isolation::ReadCommitted);
                options.lock_timeout = self.options.lock_timeout;
                options.statement_timeout = self.options.statement_timeout;
                let mut tx = self.backend.begin(self.catalog.clone(), options).await?;
                let mut expected = expected;
                let outcome = loop {
                    match tx.update(doc, expected).await {
                        Ok(v) => break Ok(v),
                        Err(e) if e.kind == ErrorKind::StaleVersion => {
                            match tx.load(&doc.entity, doc.id, RowLock::None).await {
                                Ok(Some(current)) => expected = current.version,
                                Ok(None) => break Err(e),
                                Err(e) => break Err(e),
                            }
                        }
                        Err(e) => break Err(e),
                    }
                };
                match outcome {
                    Ok(v) => {
                        tx.commit().await?;
                        // Keep read-your-writes within this transaction: the outer snapshot does
                        // not see the immediate write, so remember it for later reads here.
                        self.own_writes.insert(doc.id, (doc.fields.clone(), v));
                        Ok(v)
                    }
                    Err(e) => {
                        let _ = tx.rollback().await;
                        Err(e)
                    }
                }
            }
            Weakness::DirtyRead => {
                // Publish before the write commits, so a concurrent reader can see it.
                lock(&self.state.overlay).insert(doc.id, doc.fields.clone());
                let expected = if self.dirty_ids.contains(&doc.id) {
                    match self.inner.load(&doc.entity, doc.id, RowLock::None).await? {
                        Some(current) => current.version,
                        None => expected,
                    }
                } else {
                    expected
                };
                self.inner.update(doc, expected).await
            }
            Weakness::StaleReplica => {
                if let Some(current) = self.inner.load(&doc.entity, doc.id, RowLock::None).await? {
                    lock(&self.state.previous).insert(doc.id, current.data.fields);
                }
                self.inner.update(doc, expected).await
            }
            _ => self.inner.update(doc, expected).await,
        }
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

impl<B: StorageBackend> QueryExecutor for WeakTx<B> {
    fn query(&mut self, query: &Query) -> impl Future<Output = StorageResult<Rows>> + Send {
        self.inner.query(query)
    }
}

impl<B: StorageBackend> GraphTraversal for WeakTx<B> {
    fn traverse(
        &mut self,
        traversal: &Traversal,
    ) -> impl Future<Output = StorageResult<TraversalResult>> + Send {
        self.inner.traverse(traversal)
    }
}

impl<B: StorageBackend> Locks for WeakTx<B> {
    fn lock_key(
        &mut self,
        key: &LockKey,
        mode: LockMode,
        wait: Wait,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.lock_key(key, mode, wait)
    }
}

impl<B: StorageBackend> Sequences for WeakTx<B> {
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

impl<B: StorageBackend> Outbox for WeakTx<B> {
    fn enqueue(&mut self, event: &OutboxEvent) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.enqueue(event)
    }
}

impl<B: StorageBackend> Transaction for WeakTx<B> {
    fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    fn options(&self) -> &TxOptions {
        &self.options
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

    fn commit(self) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.commit()
    }

    fn rollback(self) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.rollback()
    }
}

/// Refuses to build a weakened backend for a weakness this build does not support.
pub fn weakness_from_name(name: &str) -> Option<Weakness> {
    match name {
        "dirty-read" => Some(Weakness::DirtyRead),
        "no-lock-lost-update" => Some(Weakness::NoLockLostUpdate),
        "snapshot-per-statement" => Some(Weakness::SnapshotPerStatement),
        "serializable-no-validation" => Some(Weakness::SerializableNoValidation),
        "stale-replica" => Some(Weakness::StaleReplica),
        _ => None,
    }
}

/// The error a weakened backend returns instead of a real one, for clarity.
pub fn weakened_error(what: &str) -> StorageError {
    StorageError::new(ErrorKind::Internal, format!("weakened backend: {what}"))
}
