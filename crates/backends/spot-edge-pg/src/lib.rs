//! `spot-edge-pg` — the PostgreSQL edge store for the business graph (WS8, WP-DAG-04.T2; ADR-50).
//!
//! [`spot-edge-sync`](spot_edge_sync) (WP-DAG-04.T1) derives a document's edge set, diffs it and
//! checks the invariant; this crate runs it on PostgreSQL:
//!
//! - [`PgEdgeWriter`] implements [`EdgeWriter`](spot_edge_sync::EdgeWriter) over the `spot_graph_edge`
//!   table, in the caller's [`PgTransaction`], so a document and its edges commit or roll back
//!   together;
//! - [`PgEdgeWriters`] is the [`EdgeWriterFor<PgTransaction>`](spot_edge_sync::EdgeWriterFor)
//!   factory a host installs with `EdgeParticipant::new`;
//! - [`EdgeVerifyJob`] is the `edge_verify` background job: it pages a tenant's documents in key
//!   order, runs [`check_invariant`](spot_edge_sync::check_invariant) and writes a
//!   [`DriftReport`] — it never changes tenant data by itself;
//! - [`EdgeRepair`] is the `edge_repair` operation: it applies a report's changes in batches,
//!   each with one `spot_edge_repair_audit` row, and never touches an edge a report does not name.
//!
//! The tables (`spot_graph_edge`, the report tables) are this crate's own and are created by
//! [`schema::prepare`]. The graph traversal over the link columns is the storage backend's
//! (WP-STO-07); the edge store mirrors the link set so the acceptance — **the edge set always
//! equals the link set** — can be checked and repaired.
//!
//! The crate reads no clock (instants are arguments, ADR-077) and never uses floats.

#![forbid(unsafe_code)]

pub mod convert;
pub mod error;
pub mod repair;
pub mod report;
pub mod schema;
pub mod verify;
pub mod writer;

pub use error::{EdgePgError, Result};
pub use repair::{BatchAudit, DEFAULT_BATCH_SIZE, EdgeRepair, RepairReport};
pub use report::{DriftEntry, DriftKind, DriftReport, read_report, write_report};
pub use verify::{DEFAULT_PAGE_SIZE, EdgeVerifyJob, handler};
pub use writer::{PgEdgeWriter, PgEdgeWriters};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-edge-pg";
