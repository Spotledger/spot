//! `spot-graph-algo` — deterministic graph algorithms for the Spotledger
//! Platform (blueprint WS-graph, WP-GRA-05, `docs/blueprint/platform.md`).
//!
//! WP-GRA-05 wants the same results for native, WASM and TypeScript callers.
//! This crate makes that possible by writing every algorithm **once**, over
//! the minimal [`GraphView`] trait, with **deterministic** output (no
//! hash-order dependence, a documented tie-break for every ambiguity) and
//! **no recursion** (every traversal is iterative, so a 1,000,000-node chain
//! cannot overflow the stack).
//!
//! The crate is pure and synchronous: no I/O, no async, no clock, no
//! randomness, no float arithmetic (ADR-12), no `unsafe` (workspace lint),
//! no recursion in traversals — so it runs on any host, including a WASM
//! guest. It depends on nothing at runtime but `thiserror`; `petgraph`
//! appears only in `[dev-dependencies]` as the reference oracle for the
//! property tests.
//!
//! # Determinism contract
//!
//! Every function documents its tie-break and returns the same owned result
//! for the same node/edge multiset, regardless of the order edges were
//! inserted in. `tests/determinism.rs` verifies this over 100 shuffled
//! insertion orders.
//!
//! # Work bounds
//!
//! Every function takes a [`Limits`] argument (`max_nodes`, `max_visited`)
//! and returns [`GraphError::LimitExceeded`] when a bound is hit, so a
//! caller can bound work on untrusted graphs.

pub mod error;
pub mod limits;
pub mod paths;
pub mod reach;
pub mod scc;
pub mod traversal;
pub mod view;

pub use error::{Cycle, GraphError, LimitKind};
pub use limits::Limits;
pub use paths::{Path, shortest_path};
pub use reach::{distance_levels, reachable};
pub use scc::{condensation, strongly_connected_components};
pub use traversal::{find_cycle, topological_order};
pub use view::{AdjacencyGraph, GraphView, NodeIx, UNIT, Weight, reversed};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-graph-algo";
