//! [`BundleTx`]: one transaction that several `spot_replica_apply::apply` calls share.
//!
//! The apply path commits (or rolls back) the transaction it is given, once per record. A bundle
//! must commit **as a whole**, so each record is applied through this wrapper: every operation
//! goes to the one real transaction, but `commit` and `rollback` do nothing. The bundle runner
//! decides: it commits once after the last record (and the ledger rows and the checkpoint), or,
//! when any record is rejected, rolls the real transaction back, so no partial bundle is ever
//! visible.
//!
//! The wrapper also makes the "no event" rule structural: the apply path never enqueues an outbox
//! event, and if it ever tried, [`BundleTx::enqueue`] refuses and counts it, so the runner fails
//! the bundle instead of emitting a change for a replicated record.

use std::future::{Future, ready};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use spot_storage_contract::{
    Catalog, Document, DocumentData, DocumentStore, ErrorKind, GraphTraversal, LockKey, LockMode,
    Locks, Name, Outbox, OutboxEvent, Query, QueryExecutor, RecordId, RowLock, Rows, Savepoint,
    Sequences, SeriesKey, StorageError, StorageResult, Transaction, Traversal, TraversalResult,
    TxOptions, Version, Wait,
};

/// How many outbox events the apply path tried to enqueue (must stay zero).
pub(crate) type EventAttempts = Arc<AtomicUsize>;

/// A transaction handle whose `commit` and `rollback` are no-ops (see the module documentation).
pub(crate) struct BundleTx<'a, T: Transaction> {
    inner: &'a mut T,
    attempts: EventAttempts,
}

impl<'a, T: Transaction> BundleTx<'a, T> {
    pub(crate) fn new(inner: &'a mut T, attempts: EventAttempts) -> Self {
        Self { inner, attempts }
    }
}

impl<T: Transaction> DocumentStore for BundleTx<'_, T> {
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

impl<T: Transaction> QueryExecutor for BundleTx<'_, T> {
    fn query(&mut self, query: &Query) -> impl Future<Output = StorageResult<Rows>> + Send {
        self.inner.query(query)
    }
}

impl<T: Transaction> GraphTraversal for BundleTx<'_, T> {
    fn traverse(
        &mut self,
        traversal: &Traversal,
    ) -> impl Future<Output = StorageResult<TraversalResult>> + Send {
        self.inner.traverse(traversal)
    }
}

impl<T: Transaction> Locks for BundleTx<'_, T> {
    fn lock_key(
        &mut self,
        key: &LockKey,
        mode: LockMode,
        wait: Wait,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        self.inner.lock_key(key, mode, wait)
    }
}

impl<T: Transaction> Sequences for BundleTx<'_, T> {
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

impl<T: Transaction> Outbox for BundleTx<'_, T> {
    /// Refuses: a replicated record is not a new change at this node (ADR-062 section 2).
    fn enqueue(&mut self, _event: &OutboxEvent) -> impl Future<Output = StorageResult<()>> + Send {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        ready(Err(StorageError::new(
            ErrorKind::Unsupported,
            "the replication apply path never emits an outbox event",
        )))
    }
}

impl<T: Transaction> Transaction for BundleTx<'_, T> {
    fn catalog(&self) -> &Catalog {
        self.inner.catalog()
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

    /// Nothing: the bundle runner commits once, after the last record.
    fn commit(self) -> impl Future<Output = StorageResult<()>> + Send {
        ready(Ok(()))
    }

    /// Nothing here: a record that is rejected, a duplicate or a stale one wrote nothing the
    /// bundle must undo, and a rejection makes the runner roll the real transaction back.
    fn rollback(self) -> impl Future<Output = StorageResult<()>> + Send {
        ready(Ok(()))
    }
}
