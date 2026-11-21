//! `spot-sync-bootstrap`: snapshot bootstrap of a new or restored node (ADR-066 sections 3, 4, 7;
//! WP-SYN-07.T3).
//!
//! A node with nothing, or with a state that cannot be trusted, is brought to the hub's state for
//! its scope from a journal snapshot at position `P`, then continues incrementally from `P + 1`.
//! This crate is a pure library over traits: no network, no database, no object store, no async
//! runtime, no clock, no randomness. The snapshot comes through [`SnapshotSource`], the scope
//! through [`ScopeView`], links and redaction through the closure crate's traits.
//!
//! * [`decide_start`] is the restored-node rule: a pure function from the node's state, the hub's
//!   head and the snapshots available to a [`StartMode`]. A stale or foreign generation, or a
//!   position ahead of the hub's head, is a full bootstrap with a [`StartReason`], never an
//!   incremental continue.
//! * [`Bootstrap::start`] plans a [`Session`]: one forward pass keeps **keys, sizes and link edges
//!   only**, drops every record the scope does not cover **before** anything else reads it, and
//!   orders the rest by dependency (targets first, a cycle kept whole). [`Session::next_chunk`]
//!   serves whole groups within the limits and [`Budget`]; every chunk carries a [`Cursor`], and
//!   the final one carries `resume_from = P + 1`.
//! * [`Session::resume`] restarts from a persisted cursor without repeating applied chunks.
//! * [`Admission`] spreads many returning nodes: a concurrency limit, a per-node chunk token
//!   bucket, and a retry delay jittered deterministically from the node id.
//!
//! # Security and tenancy
//!
//! * Scope is strict and checked twice: while planning (a record outside it is never planned) and
//!   again when a chunk is built (a source that returns more than it was asked, or a record the
//!   scope does not cover, is `ScopeViolation`). A link to a record outside the scope is **not**
//!   followed: the target is not sent and the link is only counted ([`Session::dangling_links`]);
//!   the node parks the referrer and repairs by pull-on-miss (ADR-064).
//! * The snapshot must be of the node's tenant, must be marked verified unless the caller sets
//!   [`StartOptions::allow_untrusted`], and must belong to the hub's current generation and not
//!   lie beyond its head.
//! * A [`Cursor`] holds positions and a plan digest, never record content, a key or an identity.
//!   It is bound to tenant, generation, position, node and scope fingerprint.
//! * Errors carry entity names, counts and limits only; never an image or a tenant id.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod admission;
mod error;
mod graph;
mod hash;
mod plan;
mod session;
mod source;
mod start;

pub use admission::{Admission, AdmissionSettings, Decision};
pub use error::{
    BootstrapError, COMPONENT_TOO_LARGE, CURSOR_MISMATCH, INVALID_SETTINGS, NOT_ADMITTED,
    POSITION_OVERFLOW, REFERENCE_UNREADABLE, SCOPE_VIOLATION, SNAPSHOT_CHANGED, SNAPSHOT_MISMATCH,
    SOURCE_FAILED, TENANT_MISMATCH, UNTRUSTED_SNAPSHOT,
};
pub use session::{Bootstrap, Budget, Bundle, Chunk, ChunkRecord, Cursor, Session, StartOptions};
pub use source::{MemorySource, RecordKey, ScopeView, Seams, SnapshotSource};
pub use start::{HubHead, NodeState, StartMode, StartReason, decide_start};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-bootstrap";
