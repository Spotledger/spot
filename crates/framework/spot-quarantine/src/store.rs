//! The [`QuarantineStore`] trait and its in-memory reference implementation.
//!
//! The store keeps two things per item: the item's **metadata** ([`QuarantineItem`]) and its
//! **payload** (the received record image). [`QuarantineStore::update`] can change the metadata
//! of an **open** item only; there is deliberately no method that edits or deletes the payload,
//! so the queue can never lose the sender's data (WP-EDG-06.T2 "must not"). A closed item
//! (released or discarded) is terminal: `update` refuses it, and the only way to close an item is
//! [`QuarantineStore::close`], which records who and when and needs a [`CloseAuthority`].
//!
//! The store assigns the item id, so the queue keeps its insertion order across a restart: a new
//! intake over the same store sees the same items in the same order.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use spot_replica_apply::ReceivedRecord;
use spot_sync_protocol::{RecordName, SourceId};

use crate::admin::CloseAuthority;
use crate::error::QuarantineError;
use crate::item::{self, Closure, ClosureRecord, ItemFilter, ItemId, NewItem, QuarantineItem};

/// The boxed future of every store method (the trait stays object-safe, as in `spot-effect-hold`).
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, QuarantineError>> + Send + 'a>>;

/// Where quarantine items and their payloads live.
///
/// The trait is object-safe so the intake and the admin surface can hold an
/// `Arc<dyn QuarantineStore>`. A persistent implementation (a later work package) stores the
/// payload as the record image JSON ([`ReceivedRecord::payload_json`]); the in-memory one keeps
/// the typed record.
pub trait QuarantineStore: Send + Sync {
    /// Stores a new item with its immutable payload and returns it with its assigned id.
    ///
    /// # Errors
    /// [`QuarantineError::TooLarge`] when the payload, detail, reason code or source name is over
    /// its limit (nothing is stored or truncated), [`QuarantineError::Store`] when the store
    /// fails.
    fn insert<'a>(
        &'a self,
        item: NewItem,
        record: ReceivedRecord,
    ) -> StoreFuture<'a, QuarantineItem>;

    /// The metadata for one item, or `None`.
    ///
    /// # Errors
    /// [`QuarantineError::Store`] when the store fails.
    fn item<'a>(&'a self, id: ItemId) -> StoreFuture<'a, Option<QuarantineItem>>;

    /// The stored payload for one item, or `None`. The payload is never edited.
    ///
    /// # Errors
    /// [`QuarantineError::Store`] when the store fails.
    fn record<'a>(&'a self, id: ItemId) -> StoreFuture<'a, Option<ReceivedRecord>>;

    /// Replaces an open item's metadata (`detail`, `attempts`, reason, and a move between `Held`
    /// and `Retrying`), leaving its payload untouched. The identity fields cannot change, and the
    /// new state must be open: closing goes through [`QuarantineStore::close`].
    ///
    /// # Errors
    /// [`QuarantineError::NotFound`] when the id is unknown, [`QuarantineError::ItemClosed`] when
    /// the stored item is closed or `item` asks for a closed state, or
    /// [`QuarantineError::Store`].
    fn update<'a>(&'a self, item: &'a QuarantineItem) -> StoreFuture<'a, ()>;

    /// Closes an open item: it becomes `Released` or `Discarded`, and `closure` records who closed
    /// it and when. Both are set once and never change. The payload is kept.
    ///
    /// `authority` proves the closure was authorised (see [`CloseAuthority`]); it must have been
    /// issued for this item and this outcome.
    ///
    /// # Errors
    /// [`QuarantineError::NotFound`] when the id is unknown, [`QuarantineError::ItemClosed`] when
    /// the item is already closed, [`QuarantineError::Denied`] when the token is for another item
    /// or outcome, or [`QuarantineError::Store`]. The provided body fails closed with
    /// [`QuarantineError::Store`]: a store that keeps items must override it (both stores in this
    /// repository do).
    fn close<'a>(
        &'a self,
        id: ItemId,
        closure: Closure,
        authority: CloseAuthority,
    ) -> StoreFuture<'a, QuarantineItem> {
        // Fail closed: a store that has not implemented `close` cannot close an item.
        let _ = (closure, authority);
        Box::pin(async move {
            let _ = id;
            Err(QuarantineError::Store {
                why: "this store does not support closing items".to_owned(),
            })
        })
    }

    /// Who closed an item and when, or `None` when it is open or unknown.
    ///
    /// # Errors
    /// [`QuarantineError::Store`] when the store fails.
    fn closure<'a>(&'a self, id: ItemId) -> StoreFuture<'a, Option<ClosureRecord>> {
        // A store that has not implemented `closure` reports none.
        let _ = id;
        Box::pin(async move { Ok(None) })
    }

    /// Every item that passes `filter`, in insertion order.
    ///
    /// # Errors
    /// [`QuarantineError::Store`] when the store fails.
    fn list<'a>(&'a self, filter: &'a ItemFilter) -> StoreFuture<'a, Vec<QuarantineItem>>;

    /// The open item with this apply key from this sender, or `None`.
    ///
    /// # Errors
    /// [`QuarantineError::Store`] when the store fails.
    fn find<'a>(
        &'a self,
        source: &'a SourceId,
        name: &'a RecordName,
        version: u64,
    ) -> StoreFuture<'a, Option<QuarantineItem>>;
}

/// One stored item: its metadata and its immutable payload.
struct Stored {
    item: QuarantineItem,
    record: ReceivedRecord,
    closure: Option<ClosureRecord>,
}

#[derive(Default)]
struct Inner {
    next: u64,
    items: Vec<Stored>,
}

/// An in-memory [`QuarantineStore`]: the reference implementation, for tests and early starts.
///
/// It is `Send + Sync` and cheap to share; the payload is the typed record, so a release re-applies
/// exactly what arrived.
#[derive(Default)]
pub struct InMemoryStore {
    inner: Mutex<Inner>,
}

impl InMemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many items the store holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    /// Whether the store holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl QuarantineStore for InMemoryStore {
    fn insert<'a>(
        &'a self,
        item: NewItem,
        record: ReceivedRecord,
    ) -> StoreFuture<'a, QuarantineItem> {
        Box::pin(async move {
            item.check_sizes()?;
            item::check_payload(&record)?;
            let mut inner = self.lock();
            inner.next = inner.next.saturating_add(1);
            let stored = QuarantineItem {
                id: ItemId(inner.next),
                source: item.source,
                seq: item.seq,
                record_ref: item.record_ref,
                reason_code: item.reason_code,
                detail: item.detail,
                first_seen: item.first_seen,
                attempts: 0,
                state: crate::item::State::Held,
            };
            inner.items.push(Stored {
                item: stored.clone(),
                record,
                closure: None,
            });
            Ok(stored)
        })
    }

    fn item<'a>(&'a self, id: ItemId) -> StoreFuture<'a, Option<QuarantineItem>> {
        Box::pin(async move {
            let inner = self.lock();
            Ok(inner
                .items
                .iter()
                .find(|s| s.item.id == id)
                .map(|s| s.item.clone()))
        })
    }

    fn record<'a>(&'a self, id: ItemId) -> StoreFuture<'a, Option<ReceivedRecord>> {
        Box::pin(async move {
            let inner = self.lock();
            Ok(inner
                .items
                .iter()
                .find(|s| s.item.id == id)
                .map(|s| s.record.clone()))
        })
    }

    fn update<'a>(&'a self, item: &'a QuarantineItem) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let sizes = NewItem {
                source: item.source.clone(),
                seq: item.seq,
                record_ref: item.record_ref.clone(),
                reason_code: item.reason_code.clone(),
                detail: item.detail.clone(),
                first_seen: item.first_seen,
            };
            sizes.check_sizes()?;
            let mut inner = self.lock();
            let stored = inner
                .items
                .iter_mut()
                .find(|s| s.item.id == item.id)
                .ok_or(QuarantineError::NotFound { id: item.id })?;
            // A closed item is terminal, and an update cannot close one (that is `close`).
            if !stored.item.state.is_open() || !item.state.is_open() {
                return Err(QuarantineError::ItemClosed { id: item.id });
            }
            if item.state != stored.item.state && !stored.item.state.can_become(item.state) {
                return Err(QuarantineError::ItemClosed { id: item.id });
            }
            // The identity of the item is not metadata.
            if item.source != stored.item.source
                || item.seq != stored.item.seq
                || item.record_ref != stored.item.record_ref
                || item.first_seen != stored.item.first_seen
            {
                return Err(QuarantineError::Store {
                    why: "an item's identity cannot change".to_owned(),
                });
            }
            // Only the metadata changes; `stored.record` is left exactly as it arrived.
            stored.item = item.clone();
            Ok(())
        })
    }

    fn close<'a>(
        &'a self,
        id: ItemId,
        closure: Closure,
        authority: CloseAuthority,
    ) -> StoreFuture<'a, QuarantineItem> {
        Box::pin(async move {
            if authority.item() != id || authority.outcome() != closure.outcome {
                return Err(QuarantineError::Denied {
                    access: "close",
                    id,
                });
            }
            let mut inner = self.lock();
            let stored = inner
                .items
                .iter_mut()
                .find(|s| s.item.id == id)
                .ok_or(QuarantineError::NotFound { id })?;
            if !stored.item.state.can_become(closure.outcome.state()) {
                return Err(QuarantineError::ItemClosed { id });
            }
            stored.item.state = closure.outcome.state();
            stored.closure = Some(ClosureRecord {
                by: closure.by,
                at: closure.at,
            });
            Ok(stored.item.clone())
        })
    }

    fn closure<'a>(&'a self, id: ItemId) -> StoreFuture<'a, Option<ClosureRecord>> {
        Box::pin(async move {
            let inner = self.lock();
            Ok(inner
                .items
                .iter()
                .find(|s| s.item.id == id)
                .and_then(|s| s.closure.clone()))
        })
    }

    fn list<'a>(&'a self, filter: &'a ItemFilter) -> StoreFuture<'a, Vec<QuarantineItem>> {
        Box::pin(async move {
            let inner = self.lock();
            Ok(inner
                .items
                .iter()
                .map(|s| &s.item)
                .filter(|item| filter.matches(item))
                .cloned()
                .collect())
        })
    }

    fn find<'a>(
        &'a self,
        source: &'a SourceId,
        name: &'a RecordName,
        version: u64,
    ) -> StoreFuture<'a, Option<QuarantineItem>> {
        Box::pin(async move {
            let inner = self.lock();
            Ok(inner
                .items
                .iter()
                .find(|s| {
                    s.item.state.is_open()
                        && &s.item.source == source
                        && &s.item.record_ref.name == name
                        && s.item.record_ref.version == version
                })
                .map(|s| s.item.clone()))
        })
    }
}
