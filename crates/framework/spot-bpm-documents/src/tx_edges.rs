//! The Case-file edge written in the document's own transaction.
//!
//! [`CaseFileStore`](crate::CaseFileStore) records the edge after the document operation has
//! committed, so a crash between the two leaves a document with no Case-file edge. This module
//! closes that gap. The document engine runs every [`TxParticipant`] inside the transaction that
//! writes the document, so an edge written there commits or rolls back with the document.
//!
//! # How the participant learns which Case wrote
//!
//! A participant sees the write, not the node that asked for it. Before the engine call the port
//! declares an [`EdgeIntent`] under the operation's request key, which is also the invocation's
//! command id. [`CaseFileParticipant::after_write`] looks the intent up by the command id and
//! writes the edge only for the document and operation the intent names, so any other document
//! the same command writes (a step, a correction) gets no Case-file edge. After the engine call
//! the port [`settle`](CaseFileEdges::settle)s the intent: an operation that succeeded without
//! the participant having run is a failure (the participant was never installed), never a silent
//! gap in the Case file.
//!
//! # Restart, site
//!
//! The intent lives in memory and a process can restart between the commit and the node's
//! answer. The client then repeats the same command id, the engine answers it as a replay and
//! does not run the participant, so no slot is marked written although the edge is committed
//! with the document. [`settle`](CaseFileEdges::settle) therefore asks a durable read port,
//! [`CaseFileEdgeStore::edge_exists`], when no slot was written: an existing edge is a replay of
//! a committed command, a missing one is [`Settled::NotRun`], and a read failure is an error
//! (fail closed: the edge is neither assumed nor denied).
//!
//! A slot is keyed `(site, command id)`, so two sites (tenants) that use one command id never
//! share a slot, and the durable read is asked for the intent's own site.
//!
//! # Idempotency, cancel
//!
//! The edge is an upsert keyed `(case, document)`, so a replay rewrites the same edge. A cancel
//! upserts the same edge with the cancel operation and the new version, in the cancelling
//! operation's transaction: the edge is marked, never left describing a state the document no
//! longer has.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_app_api::prelude::{Name, RecordId, SiteId, Version};
use spot_app_api::{AppError, AppResult, BoxFuture, ErrorClass, Operation as Lifecycle};
use spot_bpm_engine::CaseId;
use spot_dag::NodeId;
use spot_document::engine::{TxParticipant, WriteCause, WriteRecord};
use spot_edge_sync::EdgeWriterFor;
use spot_edge_sync::MemoryEdgeStore;
use spot_storage_contract::{ChangeFeed, Transaction};

use crate::block::run_ready;
use crate::case_file::{CaseFile, CaseFileError, Entry};
use crate::codes;
use crate::operation::Operation;
use crate::receipt::DocumentReceipt;

/// What the port declares before it calls the document engine: the edge the write must leave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeIntent {
    /// The site (tenant) the engine call runs for; with the key it names the slot.
    pub site: SiteId,
    /// The operation's request key, also the invocation's command id.
    pub key: String,
    /// The Case.
    pub case: CaseId,
    /// The node.
    pub node: NodeId,
    /// What the node does.
    pub operation: Operation,
    /// The DocType written.
    pub doctype: Name,
    /// The document written.
    pub document: RecordId,
    /// The inverse operation the node declared.
    pub compensation: Option<Operation>,
}

/// Whether the participant wrote the edge for an intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    /// The edge was written in the document's transaction.
    Written,
    /// The operation returned without the participant writing the edge.
    NotRun,
}

/// The durable read of a committed Case-file edge.
///
/// It answers from the store, not from the caller's transaction and not from memory, so it still
/// answers after a process restart. The PostgreSQL implementation (feature `pg`) is
/// `PgCaseFileEdgeStore`.
pub trait CaseFileEdgeStore: Send + Sync {
    /// Whether the edge `case -> document` exists, written by `node` for `operation`, at `site`.
    ///
    /// A site the store does not serve has no such edge: the answer is `false`.
    ///
    /// # Errors
    /// [`CaseFileError`] when the edge could not be read. The caller must not treat that as
    /// either answer.
    fn edge_exists(
        &self,
        site: &SiteId,
        case: &CaseId,
        document: RecordId,
        node: &NodeId,
        operation: Operation,
    ) -> Result<bool, CaseFileError>;
}

/// Whether `entries` hold the edge of `document` written by `node` for `operation`.
fn holds(entries: &[Entry], document: RecordId, node: &NodeId, operation: Operation) -> bool {
    entries
        .iter()
        .any(|e| e.document == document && &e.node == node && e.operation == operation)
}

/// The in-memory [`CaseFileEdgeStore`]: reads the edges a [`MemoryEdgeStore`] committed through
/// the backend `B` for one site. The reference the tests use.
pub struct MemoryCaseFileEdges<B> {
    store: MemoryEdgeStore,
    backend: B,
    site: SiteId,
}

impl<B> MemoryCaseFileEdges<B> {
    /// A reader of what `store` committed through `backend`, serving `site` only.
    #[must_use]
    pub fn new(store: MemoryEdgeStore, backend: B, site: SiteId) -> Self {
        Self {
            store,
            backend,
            site,
        }
    }
}

impl<B: ChangeFeed + Send + Sync> CaseFileEdgeStore for MemoryCaseFileEdges<B> {
    fn edge_exists(
        &self,
        site: &SiteId,
        case: &CaseId,
        document: RecordId,
        node: &NodeId,
        operation: Operation,
    ) -> Result<bool, CaseFileError> {
        if site != &self.site {
            return Ok(false);
        }
        let entries = run_ready(async {
            let mut writer = self.store.committed(&self.backend).await?;
            CaseFile::entries(&mut writer, case).await
        })
        .map_err(|_| CaseFileError::new("the Case-file edge could not be read"))?;
        Ok(holds(&entries, document, node, operation))
    }
}

/// The port the document port declares its Case-file edge through.
pub trait CaseFileEdges: Send + Sync {
    /// Declares the edge the next write under `(intent.site, intent.key)` must leave.
    ///
    /// # Errors
    /// [`CaseFileError`] when the key cannot be a command id (the participant could not find the
    /// intent) or another operation already holds it.
    fn expect(&self, intent: EdgeIntent) -> Result<(), CaseFileError>;

    /// Forgets the intent of `(site, key)` and says whether its edge is there: written by the
    /// participant in this call, or already committed by an earlier run of the same command
    /// (a replay after a restart).
    ///
    /// # Errors
    /// [`CaseFileError`] when the durable read failed. The edge is then neither assumed nor
    /// denied.
    fn settle(&self, site: &SiteId, key: &str) -> Result<Settled, CaseFileError>;
}

struct Slot {
    intent: EdgeIntent,
    written: bool,
}

type Slots = Arc<Mutex<BTreeMap<(SiteId, String), Slot>>>;

/// The intents in flight, shared between the port and the participant.
#[derive(Clone)]
pub struct TxCaseFile {
    slots: Slots,
    store: Arc<dyn CaseFileEdgeStore>,
}

impl std::fmt::Debug for TxCaseFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxCaseFile").finish_non_exhaustive()
    }
}

impl TxCaseFile {
    /// No intent in flight; `store` is the durable read `settle` falls back to.
    #[must_use]
    pub fn new(store: Arc<dyn CaseFileEdgeStore>) -> Self {
        Self {
            slots: Arc::default(),
            store,
        }
    }

    /// The participant that writes the edges, through `factory` (a backend's
    /// [`EdgeWriterFor`]). Register it with the document engine's `set_participants`.
    #[must_use]
    pub fn participant<Tx, F>(&self, factory: F) -> CaseFileParticipant<Tx, F> {
        CaseFileParticipant {
            slots: self.slots.clone(),
            factory,
            tx: PhantomData,
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<(SiteId, String), Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl CaseFileEdges for TxCaseFile {
    fn expect(&self, intent: EdgeIntent) -> Result<(), CaseFileError> {
        if spot_app_api::prelude::CommandId::new(&intent.key).is_err() {
            return Err(CaseFileError::new(
                "the request key is not a valid command id, so the Case-file edge cannot be tied to the write",
            ));
        }
        let mut slots = self.lock();
        let slot_key = (intent.site.clone(), intent.key.clone());
        if slots.contains_key(&slot_key) {
            return Err(CaseFileError::new(
                "another operation already holds this request key",
            ));
        }
        slots.insert(
            slot_key,
            Slot {
                intent,
                written: false,
            },
        );
        Ok(())
    }

    fn settle(&self, site: &SiteId, key: &str) -> Result<Settled, CaseFileError> {
        // The lock is released before the durable read.
        let removed = self.lock().remove(&(site.clone(), key.to_owned()));
        let Some(slot) = removed else {
            return Ok(Settled::NotRun);
        };
        if slot.written {
            return Ok(Settled::Written);
        }
        let intent = slot.intent;
        let exists = self.store.edge_exists(
            &intent.site,
            &intent.case,
            intent.document,
            &intent.node,
            intent.operation,
        )?;
        Ok(if exists {
            Settled::Written
        } else {
            Settled::NotRun
        })
    }
}

/// The [`TxParticipant`] that writes the Case-file edge in the document's transaction.
///
/// Generic over the backend's transaction and its [`EdgeWriterFor`], exactly like
/// `spot_edge_sync::EdgeParticipant`: on PostgreSQL it is [`PgCaseFileParticipant`].
pub struct CaseFileParticipant<Tx, F> {
    slots: Slots,
    factory: F,
    tx: PhantomData<fn(&mut Tx)>,
}

/// The lifecycle operation a node operation writes the document with.
fn lifecycle_of(operation: Operation) -> Option<Lifecycle> {
    match operation {
        Operation::CreateFrom => Some(Lifecycle::Create),
        Operation::Update => Some(Lifecycle::Update),
        Operation::Submit => Some(Lifecycle::Post),
        Operation::Cancel => Some(Lifecycle::Cancel),
        Operation::Map => None,
    }
}

fn refuse(message: &str) -> AppError {
    AppError::new(ErrorClass::Internal, codes::CASE_FILE, message)
}

impl<Tx, F> TxParticipant<Tx> for CaseFileParticipant<Tx, F>
where
    Tx: Transaction + 'static,
    F: EdgeWriterFor<Tx> + 'static,
{
    fn after_write<'a>(
        &'a self,
        tx: &'a mut Tx,
        write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let Some(command) = write.invocation.command_id.as_ref() else {
                return Ok(());
            };
            let Some(new) = write.new else {
                return Ok(());
            };
            let entry = {
                let slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
                let Some(slot) =
                    slots.get(&(write.invocation.site.clone(), command.as_str().to_owned()))
                else {
                    // Not a process write.
                    return Ok(());
                };
                let intent = &slot.intent;
                // Only the document the node named, written directly for the node's operation:
                // a step's or a correction's write of another document is not in the Case file.
                if !matches!(write.cause, WriteCause::Direct)
                    || write.doctype != intent.doctype.as_str()
                    || new.id() != intent.document
                    || lifecycle_of(intent.operation) != Some(write.operation)
                {
                    return Ok(());
                }
                let version = write
                    .version
                    .or_else(|| new.version())
                    .unwrap_or(Version::FIRST);
                let receipt = DocumentReceipt::new(
                    intent.operation,
                    intent.doctype.clone(),
                    intent.document,
                    version,
                );
                Entry {
                    case: intent.case.clone(),
                    node: intent.node.clone(),
                    operation: intent.operation,
                    doctype: intent.doctype.clone(),
                    document: intent.document,
                    version,
                    compensation: intent.compensation,
                    receipt: receipt.receipt_json(),
                }
            };
            let mut writer = self.factory.writer(tx, &write.invocation.site);
            CaseFile::record(&mut *writer, &entry).await.map_err(|_| {
                refuse("the Case-file edge could not be written; the operation was rolled back")
            })?;
            drop(writer);
            let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
            match slots.get_mut(&(write.invocation.site.clone(), command.as_str().to_owned())) {
                Some(slot) => slot.written = true,
                None => return Err(refuse("the Case-file intent vanished during the write")),
            }
            Ok(())
        })
    }
}

/// The Case-file participant over the PostgreSQL edge writer.
#[cfg(feature = "pg")]
pub type PgCaseFileParticipant =
    CaseFileParticipant<spot_postgres::PgTransaction, spot_edge_pg::PgEdgeWriters>;

/// The PostgreSQL [`CaseFileEdgeStore`]: reads the edge on its own connection, in a read-only
/// transaction apart from the document's. One database is one site (ADR-34), so it serves the
/// site it was built for and answers `false` for any other.
///
/// Like [`EngineDocumentOps`](crate::EngineDocumentOps) it polls the backend's futures to
/// completion, so call it from a thread that is not a runtime worker (the port runs on a
/// blocking thread).
#[cfg(feature = "pg")]
pub struct PgCaseFileEdgeStore {
    backend: spot_postgres::PgBackend,
    catalog: Arc<spot_storage_contract::Catalog>,
    site: SiteId,
}

#[cfg(feature = "pg")]
impl PgCaseFileEdgeStore {
    /// A reader on `backend` (its own connection pool, not the document engine's), serving
    /// `site`.
    #[must_use]
    pub fn new(
        backend: spot_postgres::PgBackend,
        catalog: Arc<spot_storage_contract::Catalog>,
        site: SiteId,
    ) -> Self {
        Self {
            backend,
            catalog,
            site,
        }
    }
}

#[cfg(feature = "pg")]
impl CaseFileEdgeStore for PgCaseFileEdgeStore {
    fn edge_exists(
        &self,
        site: &SiteId,
        case: &CaseId,
        document: RecordId,
        node: &NodeId,
        operation: Operation,
    ) -> Result<bool, CaseFileError> {
        use spot_storage_contract::{Isolation, StorageBackend, TxOptions};
        let unreadable = || CaseFileError::new("the Case-file edge could not be read");
        if site != &self.site {
            return Ok(false);
        }
        let name = Name::new("case_file_read").map_err(|_| unreadable())?;
        run_ready(async {
            let options = TxOptions::new(name, Isolation::ReadCommitted);
            let mut tx = self
                .backend
                .begin(self.catalog.clone(), options)
                .await
                .map_err(|_| unreadable())?;
            let read = {
                let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
                CaseFile::entries(&mut writer, case).await
            };
            tx.rollback().await.map_err(|_| unreadable())?;
            let entries = read.map_err(|_| unreadable())?;
            Ok(holds(&entries, document, node, operation))
        })
    }
}
