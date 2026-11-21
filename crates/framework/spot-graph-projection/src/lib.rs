//! `spot-graph-projection`: the optional one-way projection of the business graph to an external
//! graph engine (WS8, WP-GRA-07.T1; ADR-50).
//!
//! ADR-50 keeps graph capability in the platform: PostgreSQL stores the data, an in-memory index
//! (WP-GRA-02) answers hot queries, and an **external graph engine is only ever an optional read
//! projection**. This crate is that projection, built and shipped but **off** unless a measured
//! workload demands it. It names no engine and depends on no graph database: it speaks one
//! write-only [`GraphSink`] seam, fed from the outbox, and its acceptance is an invariant — *the
//! projection matches the source graph and no writes flow back*.
//!
//! # Pieces
//!
//! - [`GraphSink`]: `apply(batch)`, `checkpoint()`, `reset()`, object-safe and write-only. The
//!   reference [`InMemorySink`] and the JSON-lines [`FileSink`] implement it; [`SinkReader`] is
//!   the separate read-only view [`verify`] uses.
//! - [`Projector`]: committed [`OutboxRecord`](spot_storage_contract::OutboxRecord)s in, one
//!   [`ProjectionReport`] out, exactly like the webhook intake. It keeps a durable
//!   [`CheckpointStore`], is idempotent and resumable, and preserves per-record order.
//! - [`verify`]: the drift list between a sink and a [`GraphSource`]; empty is the invariant.
//!   [`Projector::rebuild`] streams a full snapshot and clears the checkpoint.
//! - [`ProjectionConfig`] and [`FieldSensitivity`]: the permission model. The projection runs as
//!   the system principal but carries only the configured fields, and [`MetaSensitivity`]
//!   excludes fields the metadata marks sensitive **by construction**.
//!
//! # What this crate is not
//!
//! It names no engine, reads no wall clock, uses no floats, no `unsafe`, and never reads
//! storage: a sink cannot obtain a transaction (the compile-fail test on [`GraphSink`] proves
//! it), so a projection can never write back into the business database.

#![forbid(unsafe_code)]

pub mod checkpoint;
pub mod config;
pub mod decode;
pub mod error;
pub mod projector;
pub mod sink;
pub mod verify;
pub mod wire;

#[cfg(test)]
mod tests;

pub use checkpoint::{CheckpointStore, InMemoryCheckpointStore};
pub use config::{FieldSensitivity, MetaSensitivity, NothingSensitive, ProjectionConfig};
pub use decode::{
    EDGE_SCHEMA_VERSION, EDGE_TOPIC, EdgeTopic, RETIRED_EDGE_TOPIC, decode_record,
    decode_record_with_topic,
};
pub use error::{CHECKPOINT, DECODE, GAP, ProjectionError, RESYNC, SINK, SOURCE, WRONG_GRAPH};
pub use projector::{Projected, ProjectionReport, Projector, RebuildReport};
pub use sink::{EdgeBatch, FileSink, GraphSink, InMemorySink, SinkOutcome, SinkReader, replay};
pub use verify::verify;
// The verifier's vocabulary is the in-memory index's, so a hot index and an external sink are
// checked against the same types.
pub use spot_graph_index::{Drift, GraphSource};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-graph-projection";
