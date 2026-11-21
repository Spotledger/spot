//! `spot-dag` — the DAG definition layer of the Spotledger Platform
//! (blueprint WS8, WP-DAG-01, ADR-13).
//!
//! A process (BPM-01 later) is described as a [`model::DagDefinition`]:
//! typed nodes with opaque `kind`/`config` and conditions on edges. This
//! crate validates the graph ( [`validate`], every error at once), compiles
//! it into an immutable, `Arc`-shared [`compile::CompiledDag`] with the
//! indexes a runtime needs, and versions definitions with a stable content
//! hash and an in-memory registry ([`version`]).
//!
//! The crate is pure and synchronous: no I/O, no async, no storage, no clock,
//! no randomness, no float arithmetic (ADR-12), no recursion in traversals —
//! so it runs on any host, including a WASM guest. `kind`, `config` and edge
//! `condition`s are opaque here; the engine interprets them.

pub mod compile;
pub mod error;
pub mod model;
pub mod validate;
pub mod version;

pub use compile::{CompiledDag, CompiledEdge, Progress};
pub use error::DagError;
pub use model::{DagDefinition, Edge, JoinRule, Node, NodeId};
pub use validate::{Limits, validate};
pub use version::{ContentHash, DagRegistry, Published, RegistryError};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-dag";
