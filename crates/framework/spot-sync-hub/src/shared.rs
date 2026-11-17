//! A shared handle to an [`AckBook`], so one acknowledgement ledger serves the relays of several
//! tenants (WP-SYN-02.T11).

use crate::traits::{AckBook, LedgerBook};
use spot_sync_acks::{AckError, NodeReportRow, StreamHead};
use spot_sync_freshness::MandatoryEvent;
use spot_sync_obligations::MandatoryEvent as ObligationEvent;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// A cheap-to-clone handle to one [`AckBook`]. A hub that serves many tenants runs one relay per
/// tenant (`RelayConfig::tenant`); each relay owns a `Box<dyn AckBook>`, so give every relay
/// `Box::new(shared.clone())` and they all work on the same book.
///
/// The handle adds no tenant: every [`AckBook`] method already takes one, and the relay passes its
/// own configured tenant on each call, never a value a node sent. Two relays on one handle
/// therefore keep their obligations apart.
///
/// # Locking
///
/// Each call locks the book for exactly that call and forwards it. The relay calls its book from
/// `&mut self` methods, one call at a time, and never holds the lock between calls. The host must
/// not hold the lock (a [`with_book`](Self::with_book) closure) while a relay call that reaches
/// the same handle is in progress on the same thread: that would deadlock. A single-threaded host
/// never does; a multi-threaded one calls each relay from its own thread and gets one-at-a-time
/// access to the book from the mutex.
///
/// # A poisoned lock
///
/// This crate does not panic, so a poisoned mutex means a store implementation panicked inside a
/// call. The handle recovers the guard (`PoisonError::into_inner`) and carries on. Every ledger
/// operation reads a record and then makes one `save`, so what the book holds afterwards is a whole
/// record or the previous one. A host whose store cannot make that promise must not use this
/// handle. Failing closed is not possible here: `register_mandatory` and `mandatory_events` return
/// no `Result`, and answering them empty would hide mandatory events from the stale-document check.
pub struct SharedBook<B>(Arc<Mutex<B>>);

/// A shared handle to a [`LedgerBook`].
pub type SharedLedgerBook<S, A> = SharedBook<LedgerBook<S, A>>;

impl<B> Clone for SharedBook<B> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<B> SharedBook<B> {
    /// Wraps `book` in a shared handle.
    #[must_use]
    pub fn new(book: B) -> Self {
        Self(Arc::new(Mutex::new(book)))
    }

    /// Runs `f` on the book with the lock held and returns its result. A closure, so no guard
    /// escapes; use it to read state or to register nodes and events through the ledger.
    ///
    /// Do not call a relay method that reaches this handle from inside `f`.
    pub fn with_book<R>(&self, f: impl FnOnce(&mut B) -> R) -> R {
        let mut guard = self.lock();
        f(&mut guard)
    }

    /// The lock, recovered when a previous call panicked (see the type documentation).
    fn lock(&self) -> MutexGuard<'_, B> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<B: AckBook> AckBook for SharedBook<B> {
    fn on_ack(
        &mut self,
        node: &str,
        tenant: &str,
        applied_prefix: u64,
        head: StreamHead,
        at: i64,
    ) -> Result<(), AckError> {
        self.lock().on_ack(node, tenant, applied_prefix, head, at)
    }

    fn on_checkin(&mut self, node: &str, tenant: &str, at: i64) -> Result<(), AckError> {
        self.lock().on_checkin(node, tenant, at)
    }

    fn set_holds(
        &mut self,
        node: &str,
        tenant: &str,
        holds: Vec<u64>,
        head: StreamHead,
        at: i64,
    ) -> Result<(), AckError> {
        self.lock().set_holds(node, tenant, holds, head, at)
    }

    fn register_node(
        &mut self,
        node: &str,
        tenant: &str,
        version: &str,
        at: i64,
    ) -> Result<bool, AckError> {
        self.lock().register_node(node, tenant, version, at)
    }

    fn register_mandatory(&mut self, tenant: &str, event: ObligationEvent) -> bool {
        self.lock().register_mandatory(tenant, event)
    }

    fn mandatory_events(&self, tenant: &str) -> Vec<MandatoryEvent> {
        self.lock().mandatory_events(tenant)
    }

    fn who_is_missing(&self, tenant: &str, position: u64) -> Result<Vec<NodeReportRow>, AckError> {
        self.lock().who_is_missing(tenant, position)
    }
}
