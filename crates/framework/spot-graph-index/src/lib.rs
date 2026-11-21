//! `spot-graph-index`: the in-memory per-tenant graph index (WS8, WP-GRA-02.T1; ADR-50).
//!
//! Hot graphs — hierarchies, BOMs — are answered from memory, so the graph query API
//! (WP-GRA-03) can serve "what is connected to this record" without a database traversal. The
//! index is only a **cache of the database**: correctness is *the index equals the database after
//! any sequence of changes*, and a restart (or a detected hole) rebuilds it.
//!
//! # Pieces
//!
//! - [`GraphIndex`]: one graph per `(tenant, graph name)` on a `petgraph` `StableGraph`. Node
//!   weights are record keys; edge weights are the canonical edge-type name and the edge's typed
//!   properties. Edges run in storage orientation, so a hot answer and a database answer agree.
//! - [`EdgeEvent`] and [`GraphIndex::apply`]: the incremental path. Events are applied by strictly
//!   increasing sequence number; a duplicate or an older event is ignored (idempotent
//!   redelivery); a hole is [`GraphIndexError::Gap`] and marks the graph stale rather than
//!   guessing.
//! - [`GraphSource`] and [`IndexStore::rebuild`]: the rebuild path. A new index is built off to
//!   the side and swapped in with one store operation, so no reader sees a half-built graph.
//! - [`IndexStore`] and [`MemoryBudget`]: the memory budget. Over the cap, the least-recently-used
//!   cold graph is dropped and the next query falls back to the database; a pin list is honoured.
//! - [`IndexExecutor`]: the [`GraphExecutor`](spot_graph_query::GraphExecutor) that answers from
//!   the index when hot and from the database path when cold or stale, reusing the query API's
//!   `compile` and `walk` unchanged.
//! - [`check_equal`]: the invariant checker. Empty means the index equals its source, including
//!   the watermark.
//!
//! # What this crate is not
//!
//! It reads no clock (recency is a caller's logical tick), touches no storage itself, holds no
//! permissions and no floats, and uses no `unsafe`. The outbox decoder that turns a published
//! event into an [`EdgeEvent`] is the projector's job (WP-GRA-07).

#![forbid(unsafe_code)]

pub mod budget;
pub mod error;
pub mod event;
pub mod exec;
pub mod index;
pub mod key;
pub mod source;

pub use budget::{IndexStore, MemoryBudget};
pub use error::{BUDGET_EXCEEDED, GAP, GraphIndexError, INVALID_EDGE, SOURCE, WRONG_GRAPH};
pub use event::{Applied, Edge, EdgeEvent, EdgeKey, EdgeOp};
pub use exec::IndexExecutor;
pub use index::GraphIndex;
pub use key::GraphKey;
pub use source::{Drift, GraphSource, check_equal};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-graph-index";
