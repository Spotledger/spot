//! In-memory fakes for every port, and a minimal executor for the tests.
//!
//! These are test doubles: deterministic, dependency-free, and never a production path. The
//! embedding fake is a bag-of-words hash so a query sharing words with a document scores above
//! zero; the read-check fake is built over a real [`PermissionEngine`] through the permission
//! compiler's own `restrict_columns`, so it does not re-implement a field rule.

#![allow(clippy::float_arithmetic)] // the fake embeddings are vectors, the crate's float exception

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Mutex;

use spot_perm_query::PermissionFilter;
use spot_permissions::{Action, Decision, DocFacts, PermissionEngine, Principal};
use spot_storage_contract::{
    Change, ChangeFeed, ChangeOp, Expr, FeedPosition, Name, NodeRef, OutboxRecord, Query,
    QueryPath, RecordId, Selection, StorageResult, Version,
};

use crate::error::RetrievalError;
use crate::ports::{
    DocumentFields, DocumentSource, EmbeddingProvider, FieldView, PositionStore, ReadCheck,
};

/// Runs a future to completion on the current thread.
///
/// The fakes are synchronous inside, so every awaited future is ready on its first poll; this
/// executor exists only so the async [`Indexer::pump_once`](crate::Indexer::pump_once) can be
/// driven in a test without a runtime dependency. It yields on a pending future rather than
/// spinning, so a genuinely blocking future fails by timing out, not by hanging forever.
pub fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// A deterministic bag-of-words embedding over `dim` buckets.
#[derive(Debug)]
pub struct FakeEmbedding {
    dim: usize,
    fail: bool,
}

impl Default for FakeEmbedding {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeEmbedding {
    /// A 64-bucket embedding that never fails.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dim: 64,
            fail: false,
        }
    }

    /// An embedding over `dim` buckets.
    #[must_use]
    pub fn with_dim(dim: usize) -> Self {
        Self {
            dim: dim.max(1),
            fail: false,
        }
    }

    /// An embedding whose every call fails, for the provider-failure tests.
    #[must_use]
    pub fn failing() -> Self {
        Self {
            dim: 64,
            fail: true,
        }
    }

    /// The bag-of-words vector of `text`: one bucket per lower-cased token.
    fn vector(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0f32; self.dim];
        for token in text.split(|c: char| !c.is_alphanumeric()) {
            if token.is_empty() {
                continue;
            }
            let bucket = fnv1a(&token.to_lowercase()) % self.dim as u64;
            vector[bucket as usize] += 1.0;
        }
        vector
    }
}

impl EmbeddingProvider for FakeEmbedding {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, RetrievalError> {
        if self.fail {
            return Err(RetrievalError::Embed {
                detail: "the fake provider is configured to fail".to_owned(),
            });
        }
        Ok(texts.iter().map(|text| self.vector(text)).collect())
    }

    fn max_batch(&self) -> usize {
        8
    }
}

/// FNV-1a over the bytes of `text`.
fn fnv1a(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Documents held in memory, keyed by `(doctype, id, version)`.
#[derive(Debug, Default)]
pub struct FakeDocuments {
    documents: Mutex<BTreeMap<(String, RecordId, Version), DocumentFields>>,
}

impl FakeDocuments {
    /// An empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `values` for `(doctype, id, version)`.
    pub fn put(&self, doctype: &str, id: RecordId, version: Version, values: DocumentFields) {
        self.lock()
            .insert((doctype.to_owned(), id, version), values);
    }

    /// Stores `pairs` for `(doctype, id, version)`.
    pub fn put_str(&self, doctype: &str, id: RecordId, version: Version, pairs: &[(&str, &str)]) {
        let values = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        self.put(doctype, id, version, DocumentFields { values });
    }

    /// Removes `(doctype, id)` at every version.
    pub fn remove(&self, doctype: &str, id: RecordId) {
        self.lock()
            .retain(|(dt, doc, _), _| !(dt == doctype && *doc == id));
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<(String, RecordId, Version), DocumentFields>> {
        self.documents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl DocumentSource for FakeDocuments {
    fn load(&self, doctype: &str, id: RecordId, version: Version) -> Option<DocumentFields> {
        self.lock().get(&(doctype.to_owned(), id, version)).cloned()
    }
}

/// A durable position held in memory, with a save counter.
#[derive(Debug, Default)]
struct PositionState {
    position: Option<FeedPosition>,
    saves: u64,
}

/// A [`PositionStore`] held in memory.
#[derive(Debug, Default)]
pub struct FakePositions {
    state: Mutex<PositionState>,
}

impl FakePositions {
    /// An empty store (no position saved yet).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times `save` was called.
    #[must_use]
    pub fn saves(&self) -> u64 {
        self.lock().saves
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PositionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PositionStore for FakePositions {
    fn load(&self) -> Option<FeedPosition> {
        self.lock().position
    }

    fn save(&self, position: FeedPosition) -> Result<(), RetrievalError> {
        let mut state = self.lock();
        state.position = Some(position);
        state.saves += 1;
        Ok(())
    }
}

/// A [`ReadCheck`] over a real [`PermissionEngine`] and an in-memory facts table.
pub struct FakeReadCheck {
    engine: PermissionEngine,
    facts: Mutex<BTreeMap<(String, RecordId), DocFacts>>,
}

impl FakeReadCheck {
    /// A read check over `engine`.
    #[must_use]
    pub fn new(engine: PermissionEngine) -> Self {
        Self {
            engine,
            facts: Mutex::new(BTreeMap::new()),
        }
    }

    /// Records the document facts `(doctype, id)` a decision is asked about.
    pub fn put_facts(&self, doctype: &str, id: RecordId, facts: DocFacts) {
        self.lock().insert((doctype.to_owned(), id), facts);
    }

    /// The engine the check reads.
    #[must_use]
    pub fn engine(&self) -> &PermissionEngine {
        &self.engine
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, RecordId), DocFacts>> {
        self.facts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ReadCheck for FakeReadCheck {
    fn can_read(&self, principal: &Principal, doctype: &str, id: RecordId) -> Decision {
        let facts = self
            .lock()
            .get(&(doctype.to_owned(), id))
            .cloned()
            .unwrap_or_else(|| DocFacts {
                doctype: doctype.to_owned(),
                id: Some(id.to_string()),
                ..DocFacts::default()
            });
        self.engine.can(principal, Action::Read, &facts)
    }

    fn readable_fields(&self, principal: &Principal, doctype: &str) -> FieldView {
        let Some(facts) = self.engine.snapshot().facts(doctype) else {
            return FieldView::Only(BTreeSet::new());
        };
        let Ok(entity) = Name::new(doctype) else {
            return FieldView::Only(BTreeSet::new());
        };
        let mut query = Query::from(&entity, &entity, QueryPath::Transactional);
        for field in &facts.fields {
            if let Ok(name) = Name::new(&field.path) {
                query.select.push(Selection {
                    expr: Expr::field(&entity, &name),
                    alias: name,
                });
            }
        }
        let restricted =
            PermissionFilter::restrict_columns(&self.engine, principal, doctype, &query);
        let masked: BTreeSet<String> = restricted
            .masks
            .columns
            .iter()
            .map(|column| column.column.as_str().to_owned())
            .collect();
        let allowed = restricted
            .query
            .select
            .iter()
            .map(|selection| selection.alias.as_str().to_owned())
            .filter(|alias| !masked.contains(alias))
            .collect();
        FieldView::Only(allowed)
    }
}

/// A change feed held in memory: a fixed list of committed changes.
#[derive(Debug, Default)]
pub struct FakeFeed {
    changes: Vec<Change>,
}

impl FakeFeed {
    /// A feed over `changes`, which must already be in position order.
    #[must_use]
    pub fn new(changes: Vec<Change>) -> Self {
        Self { changes }
    }
}

impl ChangeFeed for FakeFeed {
    async fn read_outbox(
        &self,
        _after: FeedPosition,
        _limit: u32,
    ) -> StorageResult<Vec<OutboxRecord>> {
        Ok(Vec::new())
    }

    async fn read_changes(&self, after: FeedPosition, limit: u32) -> StorageResult<Vec<Change>> {
        Ok(self
            .changes
            .iter()
            .filter(|change| change.position > after)
            .take(limit as usize)
            .cloned()
            .collect())
    }
}

/// Builds a change entry, or `None` when `doctype` is not a valid storage name.
#[must_use]
pub fn change(
    seq: u64,
    doctype: &str,
    id: RecordId,
    op: ChangeOp,
    version: Version,
) -> Option<Change> {
    Some(Change {
        position: FeedPosition::new(1, seq),
        transaction: RecordId::new_v7(),
        document: NodeRef {
            entity: Name::new(doctype).ok()?,
            id,
        },
        op,
        version,
    })
}
