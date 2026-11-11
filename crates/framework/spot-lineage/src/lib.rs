#![forbid(unsafe_code)]
//! `spot-lineage`: the questions users actually ask of the business graph (WS8, WP-DAG-05.T1,
//! ADR-50), expressed once as typed, permission-aware queries.
//!
//! A [`LineageQuery`] is a named traceability question: the full document chain of a record
//! (purchase order to receipt to invoice to payment), a batch traced forward and back, a bill of
//! materials exploded with exact quantities, or where an item is used. Each compiles to a
//! sequence of [`spot_graph_query`] queries — one per level — so a lineage query never bypasses
//! the graph query API's permission filter: every reached record is read through the permission
//! engine, a hidden record is removed with its whole path, and the answer says only that
//! something was removed.
//!
//! # Pieces
//!
//! - [`ChainConfig`]: the per-DocType chain configuration in the metadata text form. The crate's
//!   logic names no ERP DocType; [`ChainConfig::defaults`] carries fixtures for tests and an app
//!   passes its own text to [`ChainConfig::parse`].
//! - [`compile`]: turns a [`LineageQuery`] into a [`Plan`], whose [`Plan::level_query`] is the
//!   graph query for one level.
//! - [`run`]: walks the plan through a [`GraphExecutor`](spot_graph_query::GraphExecutor) and
//!   assembles a [`LineageResult`] with deterministic ordering, per-level totals and a
//!   [`Truncation`] reason when a depth or node limit cut the walk short.
//! - [`Quantity`]: exact decimals for bill-of-materials quantities, never floats (ADR-12).
//! - [`oracle`]: a deliberately naive in-memory implementation over the same fact data, for the
//!   property test that asserts the real path and the oracle agree.

pub mod config;
pub mod error;
pub mod exec;
pub mod oracle;
pub mod plan;
pub mod quantity;
pub mod query;

pub use config::{BomSpec, ChainConfig, ChainSpec};
pub use error::LineageError;
pub use exec::run;
pub use plan::{BomPlan, Plan, compile};
pub use quantity::Quantity;
pub use query::{LevelTotal, LineageEdge, LineageNode, LineageQuery, LineageResult, Truncation};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-lineage";

/// The largest depth a lineage query may ask for (the graph query API's maximum).
pub const MAX_DEPTH: u16 = spot_graph_query::MAX_DEPTH;

/// The default node budget of a lineage query.
pub const DEFAULT_MAX_NODES: u32 = spot_graph_query::DEFAULT_MAX_NODES;

/// The wall-time budget a six-hop trace over a 100,000-edge graph must stay under.
///
/// The nightly benchmark (`tests/bench.rs`, `#[ignore]`) measures the trace against this
/// constant; the measured number is recorded in the pull request. The library itself never reads
/// a clock: time is an argument, and this constant is only a test threshold.
pub const TRACE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
