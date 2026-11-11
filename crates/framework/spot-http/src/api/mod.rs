//! The `/api/v1` native API endpoint layer (WP-PLT-05, ADR-055): the HTTP surface of the
//! contract, coding against the [`service`] traits rather than the document engine.
//!
//! - [`service`]: object-safe `MetaService` / `ResourceService` / `LookupService` /
//!   `AuthService` and the domain types they exchange with the router;
//! - [`codec`]: JSON ↔ `Value` per the contract's Values table, plus the round-trip property;
//! - [`filter`]: the contract `Filter` JSON parsed into a typed AST and validated against
//!   `DocTypeMeta`;
//! - [`files`]: the `FilesPort` seam behind `file_upload` and `file_get`, and an in-memory fake;
//! - [`print`]: the `PrintPort` seam behind `print_get`, and an in-memory fake;
//! - [`idempotency`]: the `Idempotency-Key` store port (async, fail-closed) and its in-memory
//!   double (24 h TTL via `spot_core::Clock`); `idempotency_contract` (feature `testing`) is the
//!   suite every store passes;
//! - [`lookup`]: the lookup operation's request/page/spec types (ADR-068 §3, contract
//!   0.2.0);
//! - [`registry`]: the code → (status, retryable) table that must equal the contract's
//!   `x-spot-error-codes`;
//! - [`ports`]: the typed port map and `PortKey`, one file per port of a registered route group;
//! - [`router`]: `api::router(services)` → an `axum::Router` to mount with
//!   `ServerBuilder::site_routes`.
//!
//! Realtime (`/api/v1/realtime`) is WP-PLT-08.T2 and is **not** mounted here.

pub mod codec;
pub mod files;
pub mod filter;
pub mod idempotency;
#[cfg(any(test, feature = "testing"))]
pub mod idempotency_contract;
pub mod lookup;
pub mod meta;
pub mod permissions;
pub mod ports;
pub mod print;
pub mod registry;
pub mod router;
pub mod service;
pub mod storage_error;

pub use files::{
    BoxByteStream, ByteStream, ChunkStream, FileDownload, FileGetRequest, FileGetStatus,
    FilePortError, FileRecord, FileUpload, FilesPort, MemoryFilesPort,
};
pub use idempotency::{
    DEFAULT_MAX_BODY_BYTES, IdempotencyError, IdempotencyStore, KeyScope, MemoryIdempotencyStore,
    Reservation, StoreFuture, StoredResponse,
};
pub use lookup::{LookupColumn, LookupPage, LookupRequest, LookupRow, LookupSpec};
pub use meta::FieldHints;
pub use ports::{PortKey, Ports};
pub use print::{MemoryPrintPort, PdfBytes, PrintPort, PrintPortError};
pub use registry::Code;
pub use router::routes::{MountReport, Registration, RouteState, SkipReason};
pub use router::{ApiServices, HAND_WIRED_SERVES, router, router_with, served_operations};
pub use service::{
    ApiError, AuthService, Credential, DocTypeMeta, ErrorCode, FieldIssue, FieldValue, LoginBody,
    LookupService, Message, MetaService, Page, PermissionService, QuerySpec, ReportColumn,
    RequestCtx, ResourceService, RevealService, Session, SessionInfo, UserInfo, WriteResult,
};
pub use storage_error::storage_error_to_api;
