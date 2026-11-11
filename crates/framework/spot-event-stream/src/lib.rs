//! `spot-event-stream`: the event stream engine (WP-INT-02.T1, ADR-078 sections 4 and 6,
//! delivery level **D1**).
//!
//! This is the engine every stream binding calls: the REST pull API, the SSE binding and the
//! `Events.Consume` Connect/gRPC stream. It opens no socket, spawns no task and no timer; the
//! host drives [`StreamEngine::pull`], passes the clock and writes the frames to whatever wire
//! it serves. Nothing here is a transport.
//!
//! # What it does
//!
//! - [`spec::ConsumerSpec`]: the registered definition of a named consumer (topics, filter,
//!   partition count, payload mode, start), with a TOML/JSON form that rejects unknown fields.
//! - [`store::ConsumerStore`]: the registry and the per-partition checkpoints, with a
//!   compare-and-set [`advance`](store::ConsumerStore::advance) so a checkpoint never moves
//!   backwards or sideways; [`store::InMemoryConsumerStore`] and a backend-independent contract
//!   suite (`testing` feature).
//! - [`engine::StreamEngine`]: `pull` reads the outbox from the checkpoint, applies the topic
//!   selector, the filter and the permission filter, and builds [`EventEnvelope`]s in order
//!   within a partition. `ack` is the only thing that advances a checkpoint (ack-before-advance).
//!   A lost position yields [`StreamError::ResyncRequired`]; `restart` continues from a stated
//!   position. One active connection per consumer, through a fenced [`Coordinator`] lease.
//! - [`sse`]: the server-sent-events frame encoder, its heartbeat and the `resync_required`
//!   frame; positions are opaque, versioned and bound to the consumer name.
//! - [`visibility`]: the read filter (`can(Read)`) and the field-level filter (the one field
//!   guard, on [`Channel::Export`](spot_field_guard::Channel::Export) because a data
//!   subscription carries values).
//!
//! # Delivery level D1
//!
//! At-least-once, resumable, nothing lost. A message may arrive more than once, so the receiver
//! deduplicates on `event_id`. There is **no exactly-once claim**. Events are visible only after
//! the transaction that produced them committed (the outbox holds committed entries only), and
//! an acknowledgement means durably applied, never received.
//!
//! # Order
//!
//! In `(generation, seq, ordinal)` order **within one partition**. There is **no ordering
//! promise across partitions**; a consumer that needs a total order uses one partition.
//!
//! [`Coordinator`]: spot_coordinator::Coordinator
//! [`EventEnvelope`]: spot_events_contract::envelope::EventEnvelope

#![warn(missing_docs)]

pub mod audit;
#[cfg(feature = "testing")]
pub mod contract;
pub mod engine;
pub mod error;
pub mod policy;
pub mod position;
pub mod spec;
pub mod sse;
pub mod store;
pub mod visibility;

pub use audit::{AuditRecord, AuditSink, NullAudit, RecordingAudit};
pub use engine::{
    AllowAllStreamRights, Batch, BatchPolicy, ConsumerLease, DenyStreamRights, EngineConfig,
    FeedHead, ParkPolicy, ParkedConsumer, Seams, StreamEngine, StreamRights, is_tenant_admin,
};
pub use error::{CODES, StreamError};
pub use position::{
    encode_position, is_start, parse_position, position_from_feed, position_to_feed,
};
pub use spec::{ConsumerSpec, MAX_PARTITIONS, PayloadMode, Start, TopicSelector};
pub use store::{
    ConsumerRecord, ConsumerStore, InMemoryConsumerStore, RegisterOutcome, SharedStore,
};
pub use visibility::{
    AllVisible, MapPrincipals, MapSubjectFacts, PermissionVisibility, PrincipalSource, Subject,
    SubjectFacts, Visibility,
};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-event-stream";
