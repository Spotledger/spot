//! The sync protocol: transport-independent state machines for outbox-based, resumable,
//! checkpointed, versioned sync (WP-EDG-02.T1, ADR-062, ADR-023).
//!
//! Sync is a **consumer of the stream** (ADR-062 section 2): a node's outbound data is its
//! stream, and a destination's sync consumer reads it and applies it. This crate is the part that
//! moves **stream batches** between a hub and an edge and proves that nothing is lost or repeated
//! **in effect**. It carries no transport (no HTTP, no sockets, no runtime), no policy and no
//! conflict resolution: ownership is the rule (ADR-023), and a later binding (WP-EDG-02.T2) puts
//! these state machines on a wire.
//!
//! # The machines
//!
//! * [`Sender`] turns one source's readable stream into bounded [`Batch`]es and tracks what a
//!   receiver has acknowledged. [`Sender::next`] takes an [`Offer`] and returns an [`Action`].
//! * [`Receiver`] verifies, applies durably and checkpoints, then replies an [`Outcome`].
//!   [`Receiver::on_batch`] is idempotent keyed by record name and version.
//! * [`Hello`] carries node identity and the [`ProtocolRange`] both peers negotiate.
//!
//! # The guarantees it implements
//!
//! * **Integrity.** Every batch carries a SHA-256 [`Checksum`] over a canonical encoding; a
//!   mismatch is refused (`sync_protocol.corrupt_batch`) and re-requested (ADR-062 G3).
//! * **No skip, no gap.** A batch that does not start at the receiver's next expected sequence is
//!   refused with [`Nack::Gap`] and re-sent from the checkpoint.
//! * **At-least-once, idempotent apply.** Duplicates are acknowledged but not re-applied
//!   (ADR-062 G4): the durable [`ApplySink`] recognises `(name, version)`.
//! * **Two-phase checkpoint.** The checkpoint ([`CheckpointStore`]) advances only after the apply
//!   is durable; a crash between the two re-delivers, and idempotency absorbs it.
//! * **Edge-initiated only.** A [`Batch`] is always an answer to an edge's request; no message is
//!   one a hub must send unprompted (ADR-062 section 3).
//! * **Flow control.** A bounded batch size and in-flight window, with backpressure
//!   ([`Action::Wait`]).
//!
//! [`sim`] drives all of this over a seeded, fault-injecting network (drops, duplicates,
//! reordering, partitions and crashes at every protocol step) and is the acceptance test: for
//! every seed, the consumer's applied set equals the produced set exactly once, in per-source
//! order.
//!
//! # Time and randomness are arguments
//!
//! No function reads the wall clock or an ambient random source. A session's deadline is a
//! [`Deadline`] passed in, and a retry's jitter is an injected integer; see [`resilience`]. The
//! kit it stands in for (ADR-072) is not merged yet, so [`resilience`] holds the same shapes
//! locally.

#![warn(missing_docs)]

pub mod checksum;
pub mod error;
pub mod ids;
pub mod message;
pub mod receiver;
pub mod record;
pub mod resilience;
pub mod sender;
pub mod sim;
pub mod store;

pub use checksum::Checksum;
pub use error::{Error, Result};
pub use ids::{CURRENT_PROTOCOL, NodeId, ProtocolRange, ProtocolVersion};
pub use message::{Ack, Action, Batch, Checkpoint, Hello, Nack, Offer, Outcome};
pub use receiver::{Receiver, ReceiverConfig};
pub use record::{Record, RecordKey, RecordName};
pub use resilience::{Deadline, RetryPolicy};
pub use sender::{Sender, SenderConfig};
pub use sim::{Direction, NetworkConfig, SimConfig, SimError, SimReport, run, run_seeds};
pub use spot_events_contract::SourceId;
pub use store::{Applied, ApplySink, CheckpointStore, MemoryCheckpoints, MemorySink};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-protocol";
