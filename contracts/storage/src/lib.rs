//! Storage contract v0.3 for the Spotledger Platform (WP-CON-01, ADR-053).
//!
//! This crate is the frozen interface between the framework and storage backends. It holds
//! traits, types, pure validation and the contract test suite; no backend code. Every backend
//! (the in-memory fake of WP-CON-01.T2, `spot-postgres`) must pass [`contract_tests`].
//!
//! # Shape
//!
//! - One [`StorageBackend`] serves one tenant, i.e. one logical database (ADR-34).
//! - Every read and write that must be consistent runs in a [`Transaction`], opened with
//!   [`TxOptions`] that declare the transaction type and its isolation level (ADR-46).
//! - Documents are a parent record plus child rows, loaded and saved atomically, keyed by
//!   binary UUIDv7 [`RecordId`]s (ADR-33). Storage identifiers come from a validated
//!   [`Catalog`]; SQL is never built from strings (ADR-45).
//! - Queries are a typed IR ([`Query`]); graph walks are [`Traversal`]s (ADR-50).
//! - Hot rows use row locks, guard locks and gapless [`Sequences`] (ADR-46).
//! - Business events go to the [`Outbox`] in the same transaction as the document (ADR-11);
//!   the [`ChangeFeed`] reports every committed document write in commit order.
//! - A command ledger answers a repeated command id with the original outcome
//!   ([`CommandRecord`], `Transaction::command_get`, `command_put`, `command_prune`).
//! - Every failure is a typed, explained [`StorageError`] (ADR-26).
//!
//! # Runtime
//!
//! Futures returned by the traits are `Send` and runtime-agnostic; the platform runs them on
//! Tokio (ADR-52). Dropping a [`Transaction`] without committing rolls it back.

pub mod backend;
pub mod commands;
pub mod contract_tests;
pub mod document;
pub mod error;
pub mod feed;
pub mod graph;
pub mod id;
pub mod limits;
pub mod locks;
pub mod query;
pub mod schema;
pub mod sequence;
pub mod tx;
pub mod value;

pub use backend::{
    Capabilities, HealthCheck, HealthReport, HealthStatus, ReadSource, StorageBackend,
};
pub use commands::{CommandRecord, MAX_COMMAND_KEY_BYTES, MAX_COMMAND_OUTCOME_BYTES};
pub use document::{ChildRow, Document, DocumentData, DocumentStore, Fields};
pub use error::{ErrorKind, StorageError, StorageResult};
pub use feed::{Change, ChangeFeed, ChangeOp, FeedPosition, Outbox, OutboxEvent, OutboxRecord};
pub use graph::{
    Direction, EdgeSpec, GraphTraversal, NodeRef, Reached, Traversal, TraversalResult,
};
pub use id::{Name, RecordId, Version};
pub use limits::{
    GEO_SCALE, GEO_SRID, MAX_GEO_RADIUS_M, MAX_RANGE_SPAN_DAYS, MAX_SEARCH_FIELDS,
    MAX_SEARCH_TERM_CHARS,
};
pub use locks::{LockKey, LockMode, Locks, RowLock, Wait};
pub use query::{
    AggFunc, ArithOp, CmpOp, Constructs, Expr, FieldRef, GeoTest, Join, JoinKind, Nulls, Order,
    OrderDir, Query, QueryExecutor, QueryPath, RangeTest, Rows, SearchMode, SearchTerm, Selection,
    Source, TextMatch, TextMode,
};
pub use schema::{
    Catalog, ChildTableDef, Collation, EntityDef, EntityKind, ExclusionDef, FieldDef, FieldType,
    IndexDef, IndexMethod, LinkCheck, LinkTarget, OpClass, RangeOf, SearchDef, SearchField,
    SearchWeight,
};
pub use sequence::{Sequences, SeriesKey};
pub use tx::{Access, Isolation, Savepoint, Transaction, TxOptions};
pub use value::{GeoPoint, RangePoint, RangeValue, Value};

/// The semantic version of this contract. Implementers pin it (blueprint §4, contracts-first rule).
pub const CONTRACT_VERSION: &str = "0.3.0";
