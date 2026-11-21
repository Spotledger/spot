//! Catch-up and bootstrap hosted (WP-RTB-02.T7, ADR-066).
//!
//! The planner (`spot-sync-catchup`), the snapshot bootstrap (`spot-sync-bootstrap`) and the skew
//! holds (`spot-sync-skew`) are libraries; this crate is the hub-side orchestration of them:
//!
//! - [`Director`]: routes a returning node to live relay, a coalesced dependency-ordered plan or a
//!   snapshot bootstrap, bounds parallel bootstraps with a retry-after, keeps resumable progress
//!   per node ([`ProgressStore`]) and reports `catching_up` / `bootstrapping` for the queue;
//! - [`SkewGate`]: records a node's decode capability at hello, says when a node upgraded (so its
//!   holds are released in position order), and which records to hold;
//! - [`CatchupConfig`]: `services.sync_hub.catchup.*` with documented bounds.
//!
//! # Security and tenancy
//!
//! A bootstrap serves only the records in the requesting node's scope, filtered by the hub's
//! destination scope over the node's *registered* subscription (never a scope the node sends) and
//! redacted by `excluded_fields.down`; the snapshot source is the tenant's own verified journal
//! snapshot; a chunk key outside the snapshot index is refused and chunk hashes are verified
//! before apply. Personal fields stay sealed: an edge receives the sealed form or the erased
//! placeholder, never the subject key (an edge that must display a personal field gets it as
//! application data, not from the journal).
//!
//! # Serving and applying chunks (WP-RTB-02.T14)
//!
//! - [`ChunkServer`] (hub, blocking): bootstrap chunks cut from the latest **verified** snapshot
//!   for the node's registered scope ([`SnapshotProvider`], [`SeamsProvider`]), catch-up chunks
//!   from a planner plan, each sealed with its hash ([`WireChunk`]); served idempotently, the
//!   director moves only on [`ChunkServer::ack`].
//! - [`ChunkApplier`] (node): verifies the hash, refuses an index that is not the next one,
//!   applies through the node's apply path ([`ApplyTarget`]) and only then moves its checkpoint,
//!   so a cut repeats at most the chunk in flight and the last bootstrap chunk gives `P + 1`.
//!
//! The hub and node hosts' own files (WP-RTB-02.T4, T5) call these; see the task report.
#![forbid(unsafe_code)]

mod apply;
mod config;
mod director;
mod error;
pub mod pg;
mod serve;
mod skew;
mod wire;

pub use apply::{Applied, ApplyTarget, ChunkApplier};
pub use config::CatchupConfig;
pub use director::{ChunkAck, Director, MemoryProgress, Phase, Progress, ProgressStore, Route};
pub use error::HostError;
pub use pg::PgProgress;
pub use serve::{ChunkServer, NodeSeams, SeamsProvider, ServeError, SnapshotProvider};
pub use skew::{Hello, SkewGate};
pub use wire::{CatchUpChunk, ChunkBody, WireChunk, catch_up_chunks};
