//! The document engine's edge participant: the one place the running system writes an edge
//! (WP-DAG-04.T3; ADR-50, ADR-51).
//!
//! [`EdgeParticipant`] is a [`TxParticipant`]. The engine calls it inside the document's own
//! transaction, right after the storage write, with the transaction handle and a read-only view
//! of the write. It derives the edge change from the written document and the loaded one
//! ([`LifecycleHook::after_write`]) and applies it through a writer bound to **that**
//! transaction, so the document and its edges commit or roll back together. There is no other
//! path that writes an edge.
//!
//! # What it guarantees
//!
//! - **Atomic.** The writer is made from the engine's transaction ([`EdgeWriterFor`]), never
//!   from a second connection. A failure returns an error, the engine rolls the transaction
//!   back, and the document goes with its edges. A panic is caught by the engine the same way.
//! - **Complete.** Create, update, post, cancel, delete, rename, correct (the cancel of the
//!   original and the amended draft each fire) and a write by a step or a whitelisted method all
//!   pass through the engine's single write stage, so all reach this participant. A cancel
//!   changes no link, so it changes no edge: a cancelled document keeps its edges, and a delete
//!   removes them.
//! - **Current.** The edge schema is derived from the engine's effective metadata and is
//!   re-derived whenever the metadata generation changes (identity or version), so a DocType or Link field created at
//!   runtime gets its edges with no code change.
//! - **Tenant-bound.** An edge row carries no site: the site is the transaction the writer is
//!   made from. A participant built with [`EdgeParticipant::for_site`] refuses a write whose
//!   invocation names another site (`edge_sync.write_failed`, "another site"), and the factory is told the site
//!   so a shared backend can scope by it.
//! - **No secrets.** A relation edge copies the DocType's other fields as properties. Secret and
//!   raw-bytes fields, masked fields, fields in a protection group and classified fields are
//!   never copied (see [`EdgeSchema`]).
//! - **No re-entrancy.** The participant is given the transaction and the write, never the
//!   engine, so it cannot start another operation.
//!
//! Register it last (`Engine::set_participants`): a participant that runs after it can fail the
//! operation, and on a transactional writer the edges then roll back with the document.
//!
//! # Not run here
//!
//! Replica-apply and import paths (WP-EDG-06) write documents without going through the engine,
//! so this participant does not run for them. Their apply path must derive the edges of every
//! document it applies with [`edges_of`](crate::edges_of) (or [`diff`](crate::diff)) in the same
//! transaction, or the edge set drifts from the link set on a replica. That is open for
//! WP-EDG-06; [`check_invariant`](crate::check_invariant) finds the drift and
//! [`repair`](crate::repair) fixes it.

use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, RwLock};

use spot_app_api::prelude::SiteId;
use spot_app_api::{AppResult, BoxFuture, Operation};
use spot_document::engine::{TxParticipant, WriteRecord};
use spot_graph_model::RelationSpec;
use spot_meta::EffectiveMeta;
use spot_storage_contract::{
    ChangeFeed, FeedPosition, Name, NodeRef, Outbox, OutboxEvent, RecordId, Transaction,
};

use crate::edge::Edge;
use crate::error::{EdgeResult, EdgeSyncError};
use crate::hook::{EdgeSync, LifecycleHook};
use crate::schema::EdgeSchema;
use crate::writer::{EdgeFuture, EdgeWriter, MemoryEdgeWriter};

/// Where the participant reads the engine's current effective metadata.
///
/// A host implements it over whatever it hands `Engine::set_meta`; a closure returning the
/// current `Arc<EffectiveMeta>` is one. It is called once per write, so it must be cheap.
pub trait MetaSource: Send + Sync {
    /// The effective metadata now in force.
    fn effective(&self) -> Arc<EffectiveMeta>;
}

impl<F> MetaSource for F
where
    F: Fn() -> Arc<EffectiveMeta> + Send + Sync,
{
    fn effective(&self) -> Arc<EffectiveMeta> {
        self()
    }
}

/// Makes the [`EdgeWriter`] of one transaction.
///
/// `spot-edge-pg` implements it for `PgTransaction`; [`MemoryEdgeStore`] implements it for the
/// fake backend. The writer must write **through** `tx` (or something that commits and rolls
/// back with it): an edge written on any other connection would survive a rollback.
pub trait EdgeWriterFor<Tx>: Send + Sync {
    /// A writer bound to `tx`, for the edge tables of `site`.
    fn writer<'a>(&'a self, tx: &'a mut Tx, site: &'a SiteId) -> Box<dyn EdgeWriter + 'a>;
}

/// The schema and the metadata version it was derived from.
struct Derived {
    version: u64,
    meta: Arc<EffectiveMeta>,
    schema: Arc<EdgeSchema>,
}

/// A [`TxParticipant`] that keeps the edge set equal to the link set, inside the engine's
/// transaction. See the [module documentation](self).
pub struct EdgeParticipant<Tx, F> {
    meta: Arc<dyn MetaSource>,
    relations: Vec<RelationSpec>,
    factory: F,
    hook: EdgeSync,
    site: Option<SiteId>,
    derived: RwLock<Option<Derived>>,
    // The participant holds no transaction; the type only selects which engine it serves.
    tx: PhantomData<fn(&mut Tx)>,
}

impl<Tx, F> EdgeParticipant<Tx, F> {
    /// A participant over `meta` that writes through `factory`, for every site the engine
    /// serves. Use [`for_site`](Self::for_site) when the engine serves one tenant.
    pub fn new(meta: Arc<dyn MetaSource>, factory: F) -> Self {
        Self {
            meta,
            relations: Vec::new(),
            factory,
            hook: EdgeSync::new(),
            site: None,
            derived: RwLock::new(None),
            tx: PhantomData,
        }
    }

    /// Refuses any write whose invocation names a site other than `site`.
    #[must_use]
    pub fn for_site(mut self, site: SiteId) -> Self {
        self.site = Some(site);
        self
    }

    /// The relation DocTypes that are one typed edge with properties (see [`EdgeSchema::derive`]).
    #[must_use]
    pub fn with_relations(mut self, relations: Vec<RelationSpec>) -> Self {
        self.relations = relations;
        self
    }

    /// The edge schema for the metadata now in force.
    ///
    /// Cached against the identity (`Arc::ptr_eq`) of the metadata it was derived from and its
    /// version: `EffectiveMeta::version()` stays the same across a runtime customisation, so the
    /// version alone is not a safe key. An unchanged generation costs one pointer comparison;
    /// any other value re-derives (fail toward re-deriving). No lock is held across a derive,
    /// and the schema returned is always one whole derive of the metadata read here.
    #[must_use]
    pub fn current_schema(&self) -> Arc<EdgeSchema> {
        let meta = self.meta.effective();
        {
            let guard = self.derived.read().unwrap_or_else(|p| p.into_inner());
            if let Some(derived) = guard.as_ref()
                && Arc::ptr_eq(&derived.meta, &meta)
                && derived.version == meta.version()
            {
                return derived.schema.clone();
            }
        }
        let schema = Arc::new(EdgeSchema::derive(&meta, &self.relations));
        let mut guard = self.derived.write().unwrap_or_else(|p| p.into_inner());
        *guard = Some(Derived {
            version: meta.version(),
            meta,
            schema: schema.clone(),
        });
        schema
    }
}

impl<Tx, F> std::fmt::Debug for EdgeParticipant<Tx, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeParticipant")
            .field("site", &self.site)
            .finish_non_exhaustive()
    }
}

impl<Tx, F> TxParticipant<Tx> for EdgeParticipant<Tx, F>
where
    Tx: Transaction + 'static,
    F: EdgeWriterFor<Tx>,
{
    fn after_write<'a>(
        &'a self,
        tx: &'a mut Tx,
        write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if let Some(site) = &self.site
                && *site != write.invocation.site
            {
                return Err(EdgeSyncError::WrongSite.into());
            }
            // Fail closed: a write the edges cannot be derived from is refused, never skipped.
            let missing = match write.operation {
                Operation::Delete => write.old.is_none(),
                _ => write.new.is_none(),
            };
            if missing {
                return Err(EdgeSyncError::MissingDocument {
                    operation: format!("{:?}", write.operation),
                }
                .into());
            }
            let schema = self.current_schema();
            if !schema.knows(write.doctype) {
                return Err(EdgeSyncError::UnknownDocType {
                    doctype: write.doctype.to_owned(),
                }
                .into());
            }
            let mut writer = self.factory.writer(tx, &write.invocation.site);
            self.hook
                .after_write(&schema, write.old, write.new, &mut *writer)
                .await?;
            Ok(())
        })
    }
}

/// The topic of the commit markers [`MemoryEdgeStore`] enqueues.
const MARKER_TOPIC: &str = "edge_sync_change";

/// One staged change to the edge set.
#[derive(Clone, Debug)]
enum Change {
    Put(Edge),
    Remove(Edge),
}

/// An edge store for the fake backend that commits and rolls back **with the document**: the
/// reference [`EdgeWriterFor`] the engine tests use.
///
/// The fake backend has no edge table, so each change is kept in a side log and a *marker* naming
/// it is enqueued on the transaction's outbox. The outbox commits or rolls back with the
/// transaction, so [`committed`](Self::committed) (which replays the markers that survived) holds
/// exactly the edges of committed documents, and a rolled-back write leaves nothing. The markers
/// are test scaffolding: a real backend writes its edge table in the transaction instead, and
/// nothing else should be fed from this topic.
///
/// Cloning shares the store.
#[derive(Clone, Debug, Default)]
pub struct MemoryEdgeStore {
    log: Arc<Mutex<Vec<Change>>>,
}

impl MemoryEdgeStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The edges of every committed transaction, in a writer for convenience (it can answer
    /// [`MemoryEdgeWriter::all`], `len` and `get`).
    ///
    /// # Errors
    ///
    /// The backend's error, when the outbox cannot be read or a marker names no change.
    pub async fn committed<B: ChangeFeed>(&self, backend: &B) -> EdgeResult<MemoryEdgeWriter> {
        let mut edges = MemoryEdgeWriter::new();
        let mut after = FeedPosition::START;
        loop {
            let page = backend
                .read_outbox(after, 500)
                .await
                .map_err(|e| write_failed(&e))?;
            let Some(last) = page.last() else {
                return Ok(edges);
            };
            after = last.position;
            for record in page
                .iter()
                .filter(|r| r.event.topic.as_str() == MARKER_TOPIC)
            {
                let index: usize =
                    record
                        .event
                        .payload
                        .parse()
                        .map_err(|_| EdgeSyncError::WriteFailed {
                            message: "an edge marker names no change".to_owned(),
                        })?;
                let change = self
                    .log
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(index)
                    .cloned()
                    .ok_or_else(|| EdgeSyncError::WriteFailed {
                        message: "an edge marker names a change this store never staged".to_owned(),
                    })?;
                match change {
                    Change::Put(edge) => edges.insert(&edge),
                    Change::Remove(edge) => edges.delete(&edge),
                }
            }
        }
    }

    /// Every committed edge, sorted.
    ///
    /// # Errors
    ///
    /// As [`committed`](Self::committed).
    pub async fn all<B: ChangeFeed>(&self, backend: &B) -> EdgeResult<BTreeSet<Edge>> {
        Ok(self.committed(backend).await?.all())
    }

    /// Stages a change and marks it on the transaction's outbox; returns once both are done.
    async fn stage<Tx: Outbox>(&self, tx: &mut Tx, change: Change) -> EdgeResult<()> {
        let owner = match &change {
            Change::Put(edge) | Change::Remove(edge) => edge.owner.clone(),
        };
        let index = {
            let mut log = self.log.lock().unwrap_or_else(|p| p.into_inner());
            log.push(change);
            log.len() - 1
        };
        let topic = Name::new(MARKER_TOPIC).map_err(|e| write_failed(&e))?;
        let event = OutboxEvent {
            id: marker_id(index)?,
            topic,
            subject: owner,
            subject_version: None,
            schema_version: 1,
            payload: index.to_string(),
        };
        tx.enqueue(&event).await.map_err(|e| write_failed(&e))
    }
}

/// A deterministic record id for the marker of change `index`.
fn marker_id(index: usize) -> EdgeResult<RecordId> {
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&(index as u64).to_be_bytes());
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80;
    RecordId::from_bytes(bytes).map_err(|e| write_failed(&e))
}

fn write_failed(error: &dyn std::fmt::Display) -> EdgeSyncError {
    EdgeSyncError::WriteFailed {
        message: error.to_string(),
    }
}

/// The [`EdgeWriter`] a [`MemoryEdgeStore`] hands out for one transaction.
struct StoreWriter<'a, Tx> {
    store: &'a MemoryEdgeStore,
    tx: &'a mut Tx,
}

impl<Tx: Transaction> EdgeWriter for StoreWriter<'_, Tx> {
    fn add<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()> {
        Box::pin(self.store.stage(self.tx, Change::Put(edge.clone())))
    }

    fn remove<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()> {
        Box::pin(self.store.stage(self.tx, Change::Remove(edge.clone())))
    }

    fn update<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()> {
        self.add(edge)
    }

    /// Not served: the committed edges are read with [`MemoryEdgeStore::committed`] on the
    /// backend, and a transaction's own staged changes are not visible to it. Fails closed.
    fn scan_by_source<'a>(&'a mut self, _source: &'a NodeRef) -> EdgeFuture<'a, Vec<Edge>> {
        Box::pin(async {
            Err(EdgeSyncError::WriteFailed {
                message: "the in-memory edge store cannot scan inside a transaction".to_owned(),
            })
        })
    }
}

impl<Tx: Transaction> EdgeWriterFor<Tx> for MemoryEdgeStore {
    fn writer<'a>(&'a self, tx: &'a mut Tx, _site: &'a SiteId) -> Box<dyn EdgeWriter + 'a> {
        Box::new(StoreWriter { store: self, tx })
    }
}
