//! The reference [`DocumentOps`]: the document engine's public lifecycle API, plus the Case file.
//!
//! [`EngineDocumentOps`] is generic over [`DocumentService`](spot_app_api::context::DocumentService),
//! the engine's public lifecycle API (implemented by `spot-document`). It builds an
//! [`Invocation`] from the Case's actor (or the node's named service principal), runs exactly one
//! lifecycle operation, records the touched document in the Case file, and remembers the receipt
//! under the request key so a replay returns the first result.
//!
//! The crate never writes a document itself: it has no storage backend in its dependency tree and
//! no path to a status column. See the crate docs.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, Name, RecordId, SiteId, Version};
use spot_app_api::{AppError, Document, DocumentPatch};

use crate::block::run_ready;
use crate::case_file::{CaseFileStore, Entry};
use crate::codes;
use crate::failure::DocumentFailure;
use crate::ids::{parse_record_id, record_id_for};
use crate::operation::Operation;
use crate::ops::DocumentOps;
use crate::receipt::{DocumentReceipt, DocumentRequest};
use crate::tx_edges::{CaseFileEdges, EdgeIntent, Settled};

/// The reference document port: the document engine's lifecycle API plus the Case file.
///
/// `E` is any [`DocumentService`]. The port bridges the engine's synchronous node-handler
/// contract to the document engine's asynchronous API with [`run_ready`](crate::block), so it
/// serves a service whose futures are always ready (the in-memory fake, an in-process host). A
/// host with a real database supplies its own [`DocumentOps`] on its own runtime.
pub struct EngineDocumentOps<E: DocumentService> {
    engine: Arc<E>,
    site: SiteId,
    case_file: Recorder,
    receipts: Mutex<BTreeMap<String, DocumentReceipt>>,
}

/// Where the Case-file edge of a touched document is written.
enum Recorder {
    /// After the document operation has committed (the reference store).
    After(Arc<dyn CaseFileStore>),
    /// Inside the document's own transaction, by a registered participant.
    InTransaction(Arc<dyn CaseFileEdges>),
}

impl<E: DocumentService> EngineDocumentOps<E> {
    /// A port running against `engine` at `site`, recording touched documents in `case_file`
    /// after each operation. A crash between the operation and the record leaves a document
    /// with no edge; use [`in_transaction`](Self::in_transaction) where that matters.
    #[must_use]
    pub fn new(engine: Arc<E>, site: SiteId, case_file: Arc<dyn CaseFileStore>) -> Self {
        Self {
            engine,
            site,
            case_file: Recorder::After(case_file),
            receipts: Mutex::new(BTreeMap::new()),
        }
    }

    /// A port whose Case-file edge is written in the document's own transaction.
    ///
    /// `edges` is the [`TxCaseFile`](crate::TxCaseFile) whose
    /// [`participant`](crate::TxCaseFile::participant) is registered with `engine`
    /// (`set_participants`). An operation that succeeds without the participant having written
    /// the edge fails with `bpm_documents.case_file`.
    #[must_use]
    pub fn in_transaction(engine: Arc<E>, site: SiteId, edges: Arc<dyn CaseFileEdges>) -> Self {
        Self {
            engine,
            site,
            case_file: Recorder::InTransaction(edges),
            receipts: Mutex::new(BTreeMap::new()),
        }
    }

    /// The receipt a replay of `key` already produced, when there is one.
    fn previous(&self, key: &str) -> Option<DocumentReceipt> {
        self.receipts().get(key).cloned()
    }

    /// Remembers a receipt so a replay returns it.
    fn remember(&self, key: &str, receipt: &DocumentReceipt) {
        self.receipts().insert(key.to_owned(), receipt.clone());
    }

    fn receipts(&self) -> MutexGuard<'_, BTreeMap<String, DocumentReceipt>> {
        self.receipts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Declares the in-transaction edge of the operation, when the port writes edges that way.
    ///
    /// Returns the key to settle. The document is known before the call: a create derives its id
    /// from the key, the other operations name it.
    fn declare(&self, request: &DocumentRequest) -> Result<Option<String>, DocumentFailure> {
        let Recorder::InTransaction(edges) = &self.case_file else {
            return Ok(None);
        };
        let document = match request.operation {
            Operation::CreateFrom => record_id_for(&format!("{}:document", request.key)),
            _ => request.document.ok_or_else(|| {
                DocumentFailure::new(
                    codes::DOCUMENT_MISSING,
                    "the node names no document to change",
                )
            })?,
        };
        edges
            .expect(EdgeIntent {
                site: self.site.clone(),
                key: request.key.clone(),
                case: request.case.clone(),
                node: request.node.clone(),
                operation: request.operation,
                doctype: request.doctype.clone(),
                document,
                compensation: request.compensation,
            })
            .map_err(|error| DocumentFailure::new(codes::CASE_FILE, error.to_string()))?;
        Ok(Some(request.key.clone()))
    }

    /// Forgets the intent of `key`.
    fn settle(&self, key: &str) -> Result<Settled, DocumentFailure> {
        match &self.case_file {
            Recorder::InTransaction(edges) => edges
                .settle(&self.site, key)
                .map_err(|error| DocumentFailure::new(codes::CASE_FILE, error.to_string())),
            Recorder::After(_) => Ok(Settled::Written),
        }
    }

    /// Runs the lifecycle operation of `request` against the engine.
    fn run(
        &self,
        inv: &Invocation,
        request: &DocumentRequest,
    ) -> Result<Document, DocumentFailure> {
        let outcome = match request.operation {
            Operation::CreateFrom => {
                let id = record_id_for(&format!("{}:document", request.key));
                let mut document = Document::new(request.doctype.clone(), id);
                for (name, value) in &request.fields {
                    document.set(name.clone(), value.clone());
                }
                run_ready(self.engine.create(inv, document))
            }
            Operation::Update => {
                let (id, version) = self.target(inv, request)?;
                let patch = DocumentPatch {
                    fields: request.fields.clone(),
                    tables: BTreeMap::new(),
                };
                run_ready(
                    self.engine
                        .update(inv, request.doctype.as_str(), id, patch, version),
                )
            }
            Operation::Submit => {
                let (id, version) = self.target(inv, request)?;
                run_ready(self.engine.post(inv, request.doctype.as_str(), id, version))
            }
            Operation::Cancel => {
                let (id, version) = self.target(inv, request)?;
                run_ready(
                    self.engine
                        .cancel(inv, request.doctype.as_str(), id, version),
                )
            }
            Operation::Map => {
                return Err(DocumentFailure::new(
                    codes::CONFIG_INVALID,
                    "a map operation writes no document",
                ));
            }
        };
        outcome
            .map(|loaded| loaded.value)
            .map_err(|error| failure_of(&error))
    }

    /// The document and version an `update`, `submit` or `cancel` targets.
    fn target(
        &self,
        inv: &Invocation,
        request: &DocumentRequest,
    ) -> Result<(RecordId, Version), DocumentFailure> {
        let id = request.document.ok_or_else(|| {
            DocumentFailure::new(
                codes::DOCUMENT_MISSING,
                "the node names no document to change",
            )
        })?;
        let version = match request.expected {
            Some(version) => version,
            None => {
                let loaded = run_ready(self.engine.load(inv, request.doctype.as_str(), id))
                    .map_err(|error| failure_of(&error))?
                    .value;
                loaded.version().ok_or_else(|| {
                    DocumentFailure::new(codes::DOCUMENT_MISSING, "the document has no version")
                })?
            }
        };
        Ok((id, version))
    }
}

impl<E: DocumentService> DocumentOps for EngineDocumentOps<E> {
    fn execute(&self, request: &DocumentRequest) -> Result<DocumentReceipt, DocumentFailure> {
        if let Some(previous) = self.previous(&request.key) {
            return Ok(previous);
        }
        let inv = Invocation {
            site: self.site.clone(),
            actor: actor_of(request.principal.as_deref()),
            command_id: CommandId::new(&request.key).ok(),
            language: None,
            now: request.now,
        };
        let intent = self.declare(request)?;
        let outcome = self.run(&inv, request);
        let settled = intent.as_ref().map(|key| self.settle(key)).transpose();
        let outcome = outcome?;
        let settled = settled?;
        let document = outcome;
        let version = document.version().unwrap_or(Version::FIRST);
        let receipt = DocumentReceipt::new(
            request.operation,
            request.doctype.clone(),
            document.id(),
            version,
        );
        let entry = Entry {
            case: request.case.clone(),
            node: request.node.clone(),
            operation: request.operation,
            doctype: request.doctype.clone(),
            document: document.id(),
            version,
            compensation: request.compensation,
            receipt: receipt.receipt_json(),
        };
        match &self.case_file {
            Recorder::After(store) => store
                .record(&entry)
                .map_err(|error| DocumentFailure::new(codes::CASE_FILE, error.to_string()))?,
            Recorder::InTransaction(_) => {
                if settled != Some(Settled::Written) {
                    return Err(DocumentFailure::new(
                        codes::CASE_FILE,
                        "the Case-file participant did not write the edge in the document's transaction; is it registered with the document engine?",
                    ));
                }
            }
        }
        self.remember(&request.key, &receipt);
        Ok(receipt)
    }
}

/// The actor a node runs as: a user id when the principal is a record id, a named system
/// component otherwise, and the platform's process component when there is none.
#[must_use]
pub fn actor_of(principal: Option<&str>) -> Actor {
    match principal {
        Some(text) if !text.is_empty() => match parse_record_id(text) {
            Some(id) => Actor::User { id, via: None },
            None => Actor::System {
                component: component_name(text),
            },
        },
        _ => Actor::System {
            component: component_name("bpm"),
        },
    }
}

/// A system component name, falling back to the process component for text that is not a name.
fn component_name(text: &str) -> Name {
    Name::new(text).unwrap_or_else(|_| match Name::new("bpm") {
        Ok(name) => name,
        Err(_) => unreachable!("`bpm` is a valid storage name"),
    })
}

/// A failure from the document engine, without any field value.
///
/// The engine's message is generic; the field paths it reports are added, so the error branch
/// says *which* field failed. A field path names a field, never its value.
fn failure_of(error: &AppError) -> DocumentFailure {
    let mut reason = error.message.clone();
    if !error.fields.is_empty() {
        let paths: Vec<String> = error
            .fields
            .iter()
            .map(|field| field.field.to_string())
            .collect();
        reason = format!("{reason} ({})", paths.join(", "));
    }
    DocumentFailure::new(error.code.clone(), reason)
}
