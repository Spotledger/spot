//! The Document node, hosted for one site (WP-BPM-04.T6; ADR-49, ADR-080).
//!
//! `spot-bpm-documents` defines the Document node handler, the service-principal policy and the
//! Case file as pure logic over injected ports (WP-BPM-04.T1 to T5). This module is the host: it
//! builds those ports against the site's own running document engine and PostgreSQL database, and
//! registers the handler with the process engine's [`HandlerBuilder`] port
//! (`crate::services::process`), so a Document node acts **only** through the document engine's
//! public lifecycle API — the same `create`, `update`, `post` and `cancel` a user's click runs —
//! with every permission check, validation, field guard, audit and numbering exactly as for a
//! user. Nothing here writes a document directly; this module holds no storage write API of its
//! own kind (`cargo tree` shows no SQL crate it issues raw statements against) and the module
//! scan test in `tests/process_documents.rs` proves it never calls a storage write primitive.
//!
//! # Service principals
//!
//! A Document node may name a `principal` (`services.process_documents.principals` below). A
//! node's principal must be registered, active, and the version's author must be on the
//! principal's allow-list — [`ServicePrincipalPolicy`] (from `spot-bpm-documents`) checks and
//! audits this before the node acts, and never falls back to the Run's initiator or a system
//! actor (`PRINCIPAL_NOT_REGISTERED`/`PRINCIPAL_NOT_PERMITTED` otherwise, fail closed on every
//! read failure).
//!
//! The principal's **rights** are its own role grants, not the initiator's: this module resolves
//! a registered principal's name to the backing platform user id its settings entry names, and
//! runs the operation as that user (`Actor::User`), so the document engine's own RBAC and field
//! guard decide exactly as they would for that user acting directly — never as `Actor::System`,
//! which the engine would let do anything. A plain user id passed as the Case's own actor (no
//! named principal) is used as-is, exactly as `spot-bpm-documents::ops` already validates.
//!
//! # The Case-file edge
//!
//! [`spot_bpm_documents::case_file`] documents that the document engine does not yet expose the
//! transaction-bound participant seam a Case-file edge would need to commit atomically with the
//! document (WP-DOC-04.T3, not landed; the host's own `services::participants` table is a second,
//! independent reason: it is built once in `host.rs` before any hosting module registers, with no
//! seam for a module to add a participant — see `## Open questions`). This module therefore
//! records the edge immediately **after** the lifecycle call commits, through a small PostgreSQL
//! [`CaseFileStore`] built on its own connection.
//!
//! # Idempotency without the engine's own command-id replay
//!
//! The document engine does not yet replay a repeated command id (WP-DOC-04.T8, not landed): a
//! retried `create` after a crash would create a second document. Before calling the engine for a
//! node attempt, this module asks [`CaseFileEdgeStore::edge_exists`] for `(case, document, node,
//! operation)`; when the edge already exists (the write committed, but the process crashed before
//! the node recorded its outcome), it settles from the document's current state instead of calling
//! the engine again. Once WP-DOC-04.T8 lands the engine answers the repeat itself and this check
//! stays as a cheap first line, as the brief directs.
//!
//! # Configuration
//!
//! `[services.process_documents]`: `enabled` (default `true`) and a list of
//! `[[services.process_documents.principals]]` entries, each `name` (the principal a node
//! config names), `user` (the record id of the platform user whose role grants the principal
//! runs with) and `grants` (`"platform"`, `"user:<id>"` or `"process:<id>"`: who may use this
//! principal). See `docs/running-spot/process_documents.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Deserialize;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, Name, RecordId, Version};
use spot_app_api::{AppError, Document, DocumentPatch};
use spot_bpm_documents::{
    AuthorPrincipals, CaseAuthors, CaseFile, CaseFileError, CaseFileEdgeStore, CaseFileStore,
    DocumentFailure, DocumentNodeHandler, DocumentOps, DocumentReceipt, DocumentRequest, Entry,
    GuardedNode, IamPublishPermission, Operation, PgCaseFileEdgeStore, ProcessAuthor,
    PublishGuard, PublishPermissions, PublishViolation, ServicePrincipalAudit,
    ServicePrincipalPolicy, ServicePrincipalUse, ServicePrincipals, Unavailable,
};
use spot_bpm_engine::CaseId;
use spot_core::SiteId;
use spot_edge_pg::PgEdgeWriter;
use spot_edge_sync::{Edge, EdgeWriter, PropValue};
use spot_graph_model::EdgeTypeId;
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use spot_storage_contract::{Catalog, Isolation, NodeRef, StorageBackend, Transaction, TxOptions};

use crate::services::process::{HandlerBuilder, ProcessHost};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the process engine (1100), which provides [`HandlerBuilder`] and
/// [`ProcessHost`] before this module runs.
pub const ORDER: u32 = 1110;

/// The business-graph edge type the service-principal audit trail is written as.
const PRINCIPAL_AUDIT_EDGE: &str = "bpm_principal_use";

/// `[services.process_documents]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module does anything at all.
    pub enabled: bool,
    /// The tenant's registered service principals.
    pub principals: Vec<PrincipalConfig>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            principals: Vec::new(),
        }
    }
}

/// One registered service principal, as `[[services.process_documents.principals]]` configures
/// it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrincipalConfig {
    /// The name a node's `principal` names.
    pub name: String,
    /// The platform user whose role grants the principal runs with.
    pub user: String,
    /// Who may use this principal: `"platform"`, `"user:<id>"` or `"process:<id>"`.
    #[serde(default)]
    pub grants: Vec<String>,
}

/// Parses one `grants` entry into a [`ProcessAuthor`].
fn parse_grant(token: &str) -> Option<ProcessAuthor> {
    if token == "platform" {
        return Some(ProcessAuthor::Platform);
    }
    if let Some(id) = token.strip_prefix("user:") {
        return Some(ProcessAuthor::User(id.to_owned()));
    }
    if let Some(id) = token.strip_prefix("process:") {
        return Some(ProcessAuthor::Process(id.to_owned()));
    }
    None
}

/// The tenant's service principals, read from [`Settings`]: who is registered, who may use
/// them, and the platform user each one runs as.
pub struct ConfiguredPrincipals {
    entries: BTreeMap<String, (RecordId, BTreeSet<ProcessAuthor>)>,
}

impl ConfiguredPrincipals {
    /// Parses `configs`, or names the first entry that does not validate.
    ///
    /// # Errors
    /// A malformed `user` id, a malformed `grants` entry, or a duplicate `name`.
    pub fn parse(configs: &[PrincipalConfig]) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for config in configs {
            let user = spot_bpm_documents::ids::parse_record_id(&config.user).ok_or_else(|| {
                Error(format!(
                    "services.process_documents.principals: `{}`'s `user` is not a record id",
                    config.name
                ))
            })?;
            let mut grants = BTreeSet::new();
            for token in &config.grants {
                let author = parse_grant(token).ok_or_else(|| {
                    Error(format!(
                        "services.process_documents.principals: `{}`'s grant `{token}` is \
                         neither `platform`, `user:<id>` nor `process:<id>`",
                        config.name
                    ))
                })?;
                grants.insert(author);
            }
            if entries
                .insert(config.name.clone(), (user, grants))
                .is_some()
            {
                return Err(Error(format!(
                    "services.process_documents.principals: `{}` is declared twice",
                    config.name
                )));
            }
        }
        Ok(Self { entries })
    }

    /// The platform user a registered principal runs as, when `name` is registered.
    #[must_use]
    pub fn user_of(&self, name: &str) -> Option<RecordId> {
        self.entries.get(name).map(|(user, _)| *user)
    }
}

impl ServicePrincipals for ConfiguredPrincipals {
    fn is_registered(&self, _tenant: &SiteId, name: &str) -> std::result::Result<bool, Unavailable> {
        Ok(self.entries.contains_key(name))
    }

    fn may_use(
        &self,
        _tenant: &SiteId,
        process_author: &ProcessAuthor,
        name: &str,
    ) -> std::result::Result<bool, Unavailable> {
        Ok(self
            .entries
            .get(name)
            .is_some_and(|(_, grants)| grants.contains(process_author)))
    }
}

/// Resolves a [`ProcessAuthor::User`] to the permission engine's [`Principal`] for the publish
/// check; every other author (a process, the platform handled separately) is unknown here.
struct ConfiguredAuthorPrincipals;

impl AuthorPrincipals for ConfiguredAuthorPrincipals {
    fn principal_of(
        &self,
        _tenant: &SiteId,
        author: &ProcessAuthor,
    ) -> std::result::Result<Option<Principal>, Unavailable> {
        Ok(match author {
            ProcessAuthor::User(id) => Some(Principal {
                user: id.clone(),
                ..Principal::default()
            }),
            ProcessAuthor::Platform | ProcessAuthor::Process(_) => None,
        })
    }
}

/// Who authored the version a Case runs, read from the process engine (which process and
/// version the Case is pinned to) and the durable definition store (`published_by`,
/// `spot-bpm-definitions`): `"platform"` is the platform, a record-id-shaped value is a user,
/// anything else is another process.
pub struct EngineCaseAuthors {
    host: Arc<ProcessHost>,
    definitions: Arc<spot_bpm_defs_pg::PgDefinitionStore>,
}

impl EngineCaseAuthors {
    /// Reads a Case's author through `host`'s engine and `definitions`'s `published_by`.
    #[must_use]
    pub fn new(
        host: Arc<ProcessHost>,
        definitions: Arc<spot_bpm_defs_pg::PgDefinitionStore>,
    ) -> Self {
        Self { host, definitions }
    }
}

/// Who `text` names: `"platform"` the platform, a record-id-shaped value a user, anything else
/// another process.
#[must_use]
pub fn author_of_text(text: &str) -> ProcessAuthor {
    if text == "platform" {
        ProcessAuthor::Platform
    } else if let Some(id) = spot_bpm_documents::ids::parse_record_id(text) {
        ProcessAuthor::User(id.to_string())
    } else {
        ProcessAuthor::Process(text.to_owned())
    }
}

impl CaseAuthors for EngineCaseAuthors {
    fn author_of(
        &self,
        _tenant: &SiteId,
        case: &CaseId,
    ) -> std::result::Result<Option<ProcessAuthor>, Unavailable> {
        use spot_bpm_definitions::DefinitionStore;
        let engine = self.host.engine().map_err(|_| Unavailable)?;
        let record = match block_on(engine.load(case)) {
            Ok(record) => record,
            Err(_) => return Ok(None),
        };
        let stored = block_on(self.definitions.get(&record.run.dag_key, record.run.dag_version))
            .map_err(|_| Unavailable)?;
        Ok(stored.map(|version| author_of_text(&version.published_by)))
    }
}

/// Runs an async call from the synchronous ports `spot-bpm-documents` requires
/// ([`DocumentOps`], [`CaseFileStore`], [`CaseAuthors`], [`ServicePrincipalAudit`]).
///
/// From inside a multi-thread Tokio runtime (every `spot serve` process) this hands the
/// worker's other tasks to another thread (`block_in_place`) before blocking, the pattern Tokio
/// documents for a blocking call inside an async task (the same bridge
/// `spot-tenancy-adapters::block` and the backend crates' `blocking` modules use). It panics on
/// a current-thread runtime, which this module's own tests therefore never use.
fn block_on<T>(future: impl Future<Output = T>) -> T {
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| handle.block_on(future))
}

/// Opens a transaction on `backend` labelled `name`.
async fn open_tx(
    backend: &PgBackend,
    catalog: &Arc<Catalog>,
    name: &str,
) -> std::result::Result<spot_postgres::PgTransaction, String> {
    let label = Name::new(name).map_err(|e| e.to_string())?;
    let options = TxOptions::new(label, Isolation::ReadCommitted);
    backend
        .begin(Arc::clone(catalog), options)
        .await
        .map_err(|e| e.to_string())
}

/// Writes one edge in its own committed transaction.
async fn write_edge(
    backend: &PgBackend,
    catalog: &Arc<Catalog>,
    name: &str,
    edge: &Edge,
) -> std::result::Result<(), String> {
    let mut tx = open_tx(backend, catalog, name).await?;
    let result = {
        let mut writer = PgEdgeWriter::new(&mut tx);
        writer.add(edge).await.map_err(|e| e.to_string())
    };
    match result {
        Ok(()) => tx.commit().await.map_err(|e| e.to_string()),
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// Every edge a Case node has touched, read from a rolled-back (read-only) transaction.
async fn read_case_entries(
    backend: &PgBackend,
    catalog: &Arc<Catalog>,
    name: &str,
    case: &CaseId,
) -> std::result::Result<Vec<Entry>, String> {
    let mut tx = open_tx(backend, catalog, name).await?;
    let result = {
        let mut writer = PgEdgeWriter::new(&mut tx);
        CaseFile::entries(&mut writer, case).await.map_err(|e| e.to_string())
    };
    let _ = tx.rollback().await;
    result
}

/// The PostgreSQL [`CaseFileStore`]: records a touched document as a `bpm_case_file` edge, on
/// its own connection, immediately after the lifecycle call that touched it commits (see the
/// module docs on the Case-file edge).
pub struct PgAfterCaseFile {
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl PgAfterCaseFile {
    /// A Case-file writer on `backend`, scoped by `catalog`.
    #[must_use]
    pub fn new(backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self { backend, catalog }
    }
}

impl CaseFileStore for PgAfterCaseFile {
    fn record(&self, entry: &Entry) -> std::result::Result<(), CaseFileError> {
        block_on(async {
            let mut tx = open_tx(&self.backend, &self.catalog, "bpm_documents_case_file_write")
                .await
                .map_err(CaseFileError::new)?;
            let result = {
                let mut writer = PgEdgeWriter::new(&mut tx);
                CaseFile::record(&mut writer, entry)
                    .await
                    .map_err(|e| CaseFileError::new(e.to_string()))
            };
            match result {
                Ok(()) => tx.commit().await.map_err(|e| CaseFileError::new(e.to_string())),
                Err(e) => {
                    let _ = tx.rollback().await;
                    Err(e)
                }
            }
        })
    }

    fn entries(&self, case: &CaseId) -> std::result::Result<Vec<Entry>, CaseFileError> {
        block_on(read_case_entries(
            &self.backend,
            &self.catalog,
            "bpm_documents_case_file_read",
            case,
        ))
        .map_err(CaseFileError::new)
    }
}

/// The edge one service-principal use becomes: `case -> principal_use`, keyed so a later use of
/// the same node by the same principal updates the edge rather than creating a second one.
fn audit_edge(usage: &ServicePrincipalUse) -> std::result::Result<Edge, Unavailable> {
    let case_entity = spot_bpm_documents::ids::case_entity();
    let from = NodeRef {
        entity: case_entity,
        id: spot_bpm_documents::ids::record_id_for(&format!("case:{}", usage.case.as_str())),
    };
    let to_entity = Name::new("bpm_principal_use").map_err(|_| Unavailable)?;
    let to = NodeRef {
        entity: to_entity,
        id: spot_bpm_documents::ids::record_id_for(&format!(
            "principal_use:{}:{}:{}",
            usage.case.as_str(),
            usage.node.as_str(),
            usage.principal
        )),
    };
    Ok(Edge::new(EdgeTypeId::of(PRINCIPAL_AUDIT_EDGE), from, to)
        .with_property("principal", PropValue::Text(usage.principal.clone()))
        .with_property("author", PropValue::Text(usage.author.label()))
        .with_property("node", PropValue::Text(usage.node.as_str().to_owned()))
        .with_property("at", PropValue::Int(usage.at.unix_timestamp())))
}

/// The durable, PostgreSQL-backed [`ServicePrincipalAudit`]: one `bpm_principal_use` edge per
/// use, named by the principal, the author of the running version, and the node — queryable
/// from the Case's own business graph, exactly like the Case file.
pub struct PgPrincipalAudit {
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl PgPrincipalAudit {
    /// An audit sink on `backend`, scoped by `catalog`.
    #[must_use]
    pub fn new(backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self { backend, catalog }
    }

    /// The principal names a Case's own audit edges name, read back for a test to assert on.
    ///
    /// # Errors
    /// The database read failed.
    pub fn principals_used(&self, case: &CaseId) -> std::result::Result<Vec<String>, String> {
        let case_entity = spot_bpm_documents::ids::case_entity();
        let source = NodeRef {
            entity: case_entity,
            id: spot_bpm_documents::ids::record_id_for(&format!("case:{}", case.as_str())),
        };
        block_on(async {
            let mut tx = open_tx(&self.backend, &self.catalog, "bpm_documents_principal_read")
                .await?;
            let result = {
                let mut writer = PgEdgeWriter::new(&mut tx);
                writer.scan_by_source(&source).await.map_err(|e| e.to_string())
            };
            let _ = tx.rollback().await;
            let edges = result?;
            Ok(edges
                .into_iter()
                .filter(|edge| edge.edge_type == EdgeTypeId::of(PRINCIPAL_AUDIT_EDGE))
                .filter_map(|edge| match edge.properties.get("principal") {
                    Some(PropValue::Text(text)) => Some(text.clone()),
                    _ => None,
                })
                .collect())
        })
    }
}

impl ServicePrincipalAudit for PgPrincipalAudit {
    fn record(&self, usage: &ServicePrincipalUse) -> std::result::Result<(), Unavailable> {
        let edge = audit_edge(usage)?;
        block_on(write_edge(
            &self.backend,
            &self.catalog,
            "bpm_documents_principal_audit",
            &edge,
        ))
        .map_err(|_| Unavailable)
    }
}

/// A failure from the document engine, without any field value (see
/// `spot_bpm_documents::engine_ops`'s private twin: the port in this module answers the same
/// shape, on its own blocking bridge).
fn failure_of(error: &AppError) -> DocumentFailure {
    // The engine's own `message` is not copied: it is free text a handler or a validator wrote,
    // and nothing guarantees it never quotes a field value. The stable code carries the rule;
    // the reason names only the refused field paths (never their values).
    let mut reason = String::from("the document engine refused the operation");
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

/// The [`DocumentOps`] port that runs a node's operation against the site's own running
/// document engine, resolving a registered principal to the platform user it runs as, checking
/// the durable idempotency pre-check, and recording the Case-file edge after the operation
/// commits.
pub struct HostDocumentOps {
    engine: Arc<spot_document::engine::Engine<PgBackend>>,
    site: SiteId,
    principals: Arc<ConfiguredPrincipals>,
    case_file: PgAfterCaseFile,
    edge_store: PgCaseFileEdgeStore,
    replies: Mutex<BTreeMap<String, DocumentReceipt>>,
}

impl HostDocumentOps {
    /// A port running against `engine` for `site`, resolving a registered principal through
    /// `principals`, recording the Case-file edge through `case_file`, and pre-checking a
    /// repeated attempt through `edge_store` before it calls the engine again.
    #[must_use]
    pub fn new(
        engine: Arc<spot_document::engine::Engine<PgBackend>>,
        site: SiteId,
        principals: Arc<ConfiguredPrincipals>,
        case_file: PgAfterCaseFile,
        edge_store: PgCaseFileEdgeStore,
    ) -> Self {
        Self {
            engine,
            site,
            principals,
            case_file,
            edge_store,
            replies: Mutex::new(BTreeMap::new()),
        }
    }

    fn replies(&self) -> MutexGuard<'_, BTreeMap<String, DocumentReceipt>> {
        self.replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The actor a node runs as: the backing user of a registered principal, or the Case's own
    /// user-id actor (`ops.rs` only ever hands this port one or the other).
    ///
    /// Fails closed: a request with no principal at all (a trigger- or timer-started Run has no
    /// acting user), or whose text is neither a registered principal nor a user id, is refused
    /// with `PRINCIPAL_INVALID`. This port never builds `Actor::System`, which the document
    /// engine lets do anything, and a caller never obtains it by any input.
    fn actor_of(principal: Option<&str>, principals: &ConfiguredPrincipals) -> std::result::Result<Actor, DocumentFailure> {
        let refuse = |reason: &str| DocumentFailure::new(spot_bpm_documents::codes::PRINCIPAL_INVALID, reason);
        let Some(text) = principal else {
            return Err(refuse(
                "the node has no acting principal (neither a service principal nor a user), so it will not act as the system",
            ));
        };
        if let Some(user) = principals.user_of(text) {
            return Ok(Actor::User { id: user, via: None });
        }
        if let Some(user) = spot_bpm_documents::ids::parse_record_id(text) {
            return Ok(Actor::User { id: user, via: None });
        }
        Err(refuse(
            "the node's acting principal is neither a registered service principal nor a user id",
        ))
    }

    fn invocation(
        &self,
        request: &DocumentRequest,
    ) -> std::result::Result<Invocation, DocumentFailure> {
        Ok(Invocation {
            site: self.site.clone(),
            actor: Self::actor_of(request.principal.as_deref(), &self.principals)?,
            command_id: CommandId::new(&request.key).ok(),
            language: None,
            now: request.now,
        })
    }

    /// The document an `update`, `submit` or `cancel` targets, and the version to expect.
    fn target(
        &self,
        inv: &Invocation,
        request: &DocumentRequest,
    ) -> std::result::Result<(RecordId, Version), DocumentFailure> {
        let id = request.document.ok_or_else(|| {
            DocumentFailure::new(
                spot_bpm_documents::codes::DOCUMENT_MISSING,
                "the node names no document to change",
            )
        })?;
        let version = match request.expected {
            Some(version) => version,
            None => {
                let loaded = block_on(self.engine.load(inv, request.doctype.as_str(), id))
                    .map_err(|e| failure_of(&e))?
                    .value;
                loaded.version().ok_or_else(|| {
                    DocumentFailure::new(
                        spot_bpm_documents::codes::DOCUMENT_MISSING,
                        "the document has no version",
                    )
                })?
            }
        };
        Ok((id, version))
    }

    fn run(
        &self,
        inv: &Invocation,
        request: &DocumentRequest,
    ) -> std::result::Result<Document, DocumentFailure> {
        let outcome = match request.operation {
            Operation::CreateFrom => {
                let id = spot_bpm_documents::ids::record_id_for(&format!(
                    "{}:document",
                    request.key
                ));
                let mut document = Document::new(request.doctype.clone(), id);
                for (name, value) in &request.fields {
                    document.set(name.clone(), value.clone());
                }
                match block_on(self.engine.create(inv, document)) {
                    // Crash window between the lifecycle commit and the Case-file edge: the
                    // replay finds no edge, calls `create` with the same derived id and the
                    // engine answers `document.duplicate`. The id is a pure function of this
                    // node attempt's key, so the document it names is the one this attempt
                    // already created: load it (as the same actor, so the permission checks
                    // still apply) and settle on it instead of failing the node.
                    // When no document with the derived id exists, the duplicate was a
                    // unique-field collision with another document: that refusal stands.
                    Err(e) if e.code == "document.duplicate" => {
                        block_on(self.engine.load(inv, request.doctype.as_str(), id))
                            .map_err(|_| e)
                    }
                    other => other,
                }
            }
            Operation::Update => {
                let (id, version) = self.target(inv, request)?;
                let patch = DocumentPatch {
                    fields: request.fields.clone(),
                    tables: BTreeMap::new(),
                };
                block_on(
                    self.engine
                        .update(inv, request.doctype.as_str(), id, patch, version),
                )
            }
            Operation::Submit => {
                let (id, version) = self.target(inv, request)?;
                block_on(self.engine.post(inv, request.doctype.as_str(), id, version))
            }
            Operation::Cancel => {
                let (id, version) = self.target(inv, request)?;
                block_on(
                    self.engine
                        .cancel(inv, request.doctype.as_str(), id, version),
                )
            }
            Operation::Map => {
                return Err(DocumentFailure::new(
                    spot_bpm_documents::codes::CONFIG_INVALID,
                    "a map operation writes no document",
                ));
            }
        };
        outcome.map(|loaded| loaded.value).map_err(|e| failure_of(&e))
    }

    /// The document id a `create_from` will produce, or the one an `update`, `submit` or
    /// `cancel` already names — the idempotency pre-check asks about this document before
    /// calling the engine.
    fn expected_document(&self, request: &DocumentRequest) -> Option<RecordId> {
        match request.operation {
            Operation::CreateFrom => Some(spot_bpm_documents::ids::record_id_for(&format!(
                "{}:document",
                request.key
            ))),
            Operation::Map => None,
            _ => request.document,
        }
    }
}

impl DocumentOps for HostDocumentOps {
    fn execute(
        &self,
        request: &DocumentRequest,
    ) -> std::result::Result<DocumentReceipt, DocumentFailure> {
        if let Some(previous) = self.replies().get(&request.key).cloned() {
            return Ok(previous);
        }
        let inv = self.invocation(request)?;
        // The idempotency pre-check (brief item 5): before calling the engine, ask the durable
        // Case-file edge store whether this node attempt already committed its write. A crash
        // between the lifecycle commit and this node recording its outcome leaves exactly that
        // edge with no in-memory record of it, so a restart would otherwise call the engine
        // again and create a second document (WP-DOC-04.T8, the engine's own replay, is not
        // landed yet).
        if let Some(document) = self.expected_document(request) {
            match self.edge_store.edge_exists(
                &self.site,
                &request.case,
                document,
                &request.node,
                request.operation,
            ) {
                Ok(true) => {
                    let loaded = block_on(self.engine.load(&inv, request.doctype.as_str(), document))
                        .map_err(|e| failure_of(&e))?
                        .value;
                    let version = loaded.version().unwrap_or(Version::FIRST);
                    let receipt =
                        DocumentReceipt::new(request.operation, request.doctype.clone(), document, version);
                    self.replies().insert(request.key.clone(), receipt.clone());
                    return Ok(receipt);
                }
                Ok(false) => {}
                Err(_) => {
                    return Err(DocumentFailure::new(
                        spot_bpm_documents::codes::CASE_FILE,
                        "the Case-file edge could not be read, so the idempotency check could \
                         not run; the node did not act",
                    ));
                }
            }
        }
        let document = self.run(&inv, request)?;
        let version = document.version().unwrap_or(Version::FIRST);
        let receipt =
            DocumentReceipt::new(request.operation, request.doctype.clone(), document.id(), version);
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
        self.case_file
            .record(&entry)
            .map_err(|e| DocumentFailure::new(spot_bpm_documents::codes::CASE_FILE, e.to_string()))?;
        self.replies().insert(request.key.clone(), receipt.clone());
        Ok(receipt)
    }
}

/// The hosted service: nothing runs continuously (the node handler and the publish guard are
/// plain ports), so this only reports readiness.
struct ProcessDocumentsService {
    host: Arc<ProcessHost>,
}

impl Service for ProcessDocumentsService {
    fn name(&self) -> &'static str {
        "process_documents"
    }

    fn level(&self) -> Level {
        // A Document node that cannot run degrades the processes that use one; it never takes
        // the whole site down (a process with no Document node is unaffected).
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.host
                .engine()
                .map(|_| ())
                .map_err(|e| format!("the process engine has not finished starting: {e}"))
        })
    }

    fn run(&self, stop: tokio::sync::watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}

/// Registers the Document node handler with the process engine, and provides the [`PublishGuard`]
/// port the publish path (WP-BPM-09.T4, not yet hosted) will check a version against before it
/// accepts a service-principal Document node (brief item 4).
///
/// # Errors
/// A setting that does not decode or validate, or a port the process module (a smaller `ORDER`)
/// did not provide.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_documents")?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = SiteId::new(&env.site.site).map_err(crate::error)?;
    let principals = Arc::new(ConfiguredPrincipals::parse(&settings.principals)?);

    let builder = ctx.require_port::<HandlerBuilder>()?;
    let process_host = ctx.require_port::<ProcessHost>()?;

    let engine = env
        .live
        .engine()
        .ok_or_else(|| Error("the site has no running document engine yet".into()))?;
    let backend = env.backend.clone();
    let catalog = Arc::clone(&env.permission.catalog);

    let ops = Arc::new(HostDocumentOps::new(
        engine,
        site.clone(),
        Arc::clone(&principals),
        PgAfterCaseFile::new(backend.clone(), Arc::clone(&catalog)),
        PgCaseFileEdgeStore::new(backend.clone(), Arc::clone(&catalog), site.clone()),
    ));
    let audit = Arc::new(PgPrincipalAudit::new(backend.clone(), Arc::clone(&catalog)));
    let definitions = Arc::new(
        spot_bpm_defs_pg::PgDefinitionStore::new(backend.pool()).with_tenant(site.as_str()),
    );
    let authors = Arc::new(EngineCaseAuthors::new(Arc::clone(&process_host), definitions));
    let policy = ServicePrincipalPolicy::new(
        site.clone(),
        Arc::clone(&principals) as Arc<dyn ServicePrincipals>,
        authors as Arc<dyn CaseAuthors>,
        audit as Arc<dyn ServicePrincipalAudit>,
    );
    let handler = Arc::new(
        DocumentNodeHandler::new(ops as Arc<dyn DocumentOps>).with_service_principals(policy),
    );
    builder.register(handler);

    // The publish-time guard (brief item 4): exposed as a port for the publish path to require
    // once it is hosted. `IamPublishPermission` reads the site's own permission snapshot, so a
    // grant made while the server runs is seen on the next check, with no reload of this module.
    let publish_permissions = Arc::new(IamPublishPermission::new(
        site.clone(),
        Arc::clone(&env.permission.decision),
        Arc::new(ConfiguredAuthorPrincipals) as Arc<dyn AuthorPrincipals>,
    ));
    let guard = Arc::new(PublishGuard::new(
        Arc::clone(&principals) as Arc<dyn ServicePrincipals>,
        publish_permissions as Arc<dyn PublishPermissions>,
    ));
    ctx.provide(guard)?;

    ctx.add(Arc::new(ProcessDocumentsService { host: process_host }));
    Ok(())
}

/// Runs the [`PublishGuard`] a hosted site provides over every Document node of a compiled
/// process version — the single call the publish path (WP-BPM-09.T4) needs, re-exported here so
/// it does not have to depend on `spot-bpm-documents::iam` directly for the node extraction.
#[must_use]
pub fn check_publish(
    guard: &PublishGuard,
    tenant: &SiteId,
    author: &ProcessAuthor,
    nodes: &[GuardedNode<'_>],
) -> Vec<PublishViolation> {
    guard.check(tenant, author, nodes).err().unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn principal_config(name: &str, user: &str, grants: &[&str]) -> PrincipalConfig {
        PrincipalConfig {
            name: name.to_owned(),
            user: user.to_owned(),
            grants: grants.iter().map(|g| (*g).to_owned()).collect(),
        }
    }

    #[test]
    fn settings_default_to_enabled_with_no_principals() {
        let settings = Settings::default();
        assert!(settings.enabled);
        assert!(settings.principals.is_empty());
    }

    #[test]
    fn grants_parse_platform_user_and_process() {
        assert_eq!(parse_grant("platform"), Some(ProcessAuthor::Platform));
        assert_eq!(
            parse_grant("user:abc"),
            Some(ProcessAuthor::User("abc".to_owned()))
        );
        assert_eq!(
            parse_grant("process:p1"),
            Some(ProcessAuthor::Process("p1".to_owned()))
        );
        assert_eq!(parse_grant("nonsense"), None);
    }

    #[test]
    fn configured_principals_register_and_check_grants() {
        let user = spot_bpm_documents::ids::record_id_for("billing-user");
        let configs = vec![principal_config("billing_bot", &user.to_string(), &["platform"])];
        let principals = ConfiguredPrincipals::parse(&configs).unwrap();
        let tenant = SiteId::new("acme").unwrap();
        assert!(principals.is_registered(&tenant, "billing_bot").unwrap());
        assert!(!principals.is_registered(&tenant, "other").unwrap());
        assert!(
            principals
                .may_use(&tenant, &ProcessAuthor::Platform, "billing_bot")
                .unwrap()
        );
        assert!(
            !principals
                .may_use(
                    &tenant,
                    &ProcessAuthor::User("someone".to_owned()),
                    "billing_bot"
                )
                .unwrap()
        );
        assert_eq!(principals.user_of("billing_bot"), Some(user));
        assert_eq!(principals.user_of("other"), None);
    }

    #[test]
    fn a_malformed_user_id_refuses_to_parse() {
        let configs = vec![principal_config("bot", "not-a-record-id", &[])];
        assert!(ConfiguredPrincipals::parse(&configs).is_err());
    }

    #[test]
    fn a_malformed_grant_refuses_to_parse() {
        let user = spot_bpm_documents::ids::record_id_for("u");
        let configs = vec![principal_config("bot", &user.to_string(), &["nonsense"])];
        assert!(ConfiguredPrincipals::parse(&configs).is_err());
    }

    #[test]
    fn a_duplicate_name_refuses_to_parse() {
        let user = spot_bpm_documents::ids::record_id_for("u");
        let configs = vec![
            principal_config("bot", &user.to_string(), &[]),
            principal_config("bot", &user.to_string(), &[]),
        ];
        assert!(ConfiguredPrincipals::parse(&configs).is_err());
    }

    #[test]
    fn author_of_text_maps_platform_user_and_process() {
        assert_eq!(author_of_text("platform"), ProcessAuthor::Platform);
        let id = spot_bpm_documents::ids::record_id_for("x");
        assert_eq!(
            author_of_text(&id.to_string()),
            ProcessAuthor::User(id.to_string())
        );
        assert_eq!(
            author_of_text("some-process"),
            ProcessAuthor::Process("some-process".to_owned())
        );
    }
}
