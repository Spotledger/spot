//! `spot-bpm-documents` — Document nodes and the Case file (WS8, WP-BPM-04.T1; ADR-49, ADR-080).
//!
//! A Document node is a step of a Process that changes a record. It never writes a record
//! itself: it asks the record type's **own lifecycle** to do it, so every validation, permission,
//! posting and side effect runs exactly as on a user's click (ADR-49, ADR-080 section 1). This
//! crate is the node side of that rule:
//!
//! - [`DocumentNodeHandler`] is the [`NodeHandler`](spot_bpm_engine::NodeHandler) a host registers
//!   for [`NodeKind::Document`](spot_bpm_engine::NodeKind::Document). It reads its config, evaluates
//!   the field mapping with the shared expression language (ADR-28) and runs one of
//!   [`Operation`]'s operations through a [`DocumentOps`] port.
//! - [`EngineDocumentOps`] is the reference port: it calls the document engine's public lifecycle
//!   API ([`DocumentService`](spot_app_api::context::DocumentService), implemented by
//!   `spot-document`) and returns an opaque [`DocumentReceipt`].
//! - [`CaseFile`] records one edge `case -> document` per touched document through
//!   `spot-edge-sync`'s [`EdgeWriter`] — the only way an edge is written — and answers
//!   `case_file(case)` with the touched documents and a completeness check.
//!
//! # No status is ever written here
//!
//! The crate has no storage backend in its dependency tree: it cannot write a `doc_state` column,
//! a `status` field or anything else. A document changes only through the lifecycle operations
//! `create`, `update`, `post` and `cancel`, and the tests prove it (a `cargo tree` guard and a
//! source scan).
//!
//! # Idempotency
//!
//! Every operation is keyed by the engine's [`IdempotencyKey`](spot_bpm_engine::IdempotencyKey)
//! (`(case, node, attempt)`), which becomes the document operation's request key: the reference
//! port remembers the first result and returns it on a replay, and a created document's record id
//! is derived from the key, so a replay touches nothing new.
//!
//! # Time is an argument
//!
//! Nothing here reads a clock: [`NodeCtx::now`](spot_bpm_engine::NodeCtx::now) is the instant, and
//! record ids are derived from the idempotency key, never from the wall clock.

#![forbid(unsafe_code)]

pub mod case_file;
pub mod codes;
pub mod engine_ops;
pub mod failure;
pub mod iam;
pub mod ids;
pub mod operation;
pub mod ops;
pub mod receipt;
pub mod service_principal;
pub mod tx_edges;

mod block;
mod config;

pub use case_file::{
    CaseFile, CaseFileError, CaseFileStore, Completeness, Entry, MemoryCaseFile, Touch,
};
pub use engine_ops::EngineDocumentOps;
pub use failure::DocumentFailure;
pub use iam::{
    AuthorPrincipals, IamPublishPermission, IamServicePrincipals, ServicePrincipalDirectory,
    ServicePrincipalEntry, engine_permission_name, validate_process_version,
};
pub use operation::Operation;
pub use ops::{DocumentNodeHandler, DocumentOps};
pub use receipt::{DocumentReceipt, DocumentRequest};
pub use service_principal::{
    CaseAuthors, GuardedNode, MemoryDirectory, MemoryUseAudit, ProcessAuthor, PublishGuard,
    PublishPermissions, PublishViolation, ServicePrincipalAudit, ServicePrincipalPolicy,
    ServicePrincipalUse, ServicePrincipals, Unavailable,
};
pub use tx_edges::{
    CaseFileEdgeStore, CaseFileEdges, CaseFileParticipant, EdgeIntent, MemoryCaseFileEdges,
    Settled, TxCaseFile,
};
#[cfg(feature = "pg")]
pub use tx_edges::{PgCaseFileEdgeStore, PgCaseFileParticipant};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-documents";
