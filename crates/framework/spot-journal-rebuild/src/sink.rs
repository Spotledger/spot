//! The apply sink: where the executor hands the records it reads.
//!
//! The executor reads and checks; a sink writes. The real database sink is the replication apply
//! path of WP-EDG-06, which implements [`ApplySink`] over the storage contract.
//!
//! # The contract every sink must hold (ADR-061 s6)
//!
//! * **Stored versions.** A record is written with exactly the version and position it is handed,
//!   never a newly allocated one.
//! * **No business logic.** No validation rules, hooks, defaults, number series, workflows,
//!   permission checks or derived-field computation run: the image is already the final state.
//! * **No triggers and no side effects.** Nothing is sent, scheduled or posted.
//! * **No event enqueue.** The sink writes no outbox row and no change-feed entry of its own: the
//!   rebuilt database adopts the journal's positions, so its outbox continues at the next one.
//! * **Sealed fields stay sealed.** An image reaches the sink as the journal stored it; revealing
//!   or protecting personal fields (WP-JRN-08.T4) happens between the executor and the sink.
//! * **Atomic as a whole.** Nothing is visible as a rebuilt database until [`ApplySink::commit`];
//!   [`ApplySink::abort`] discards what was applied.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use spot_events_contract::envelope::Position;
use spot_image_upcast::Image;
use spot_journal_rebuild_plan::{Base, ForkPoint};

/// The future every sink method returns (boxed, so the trait is object-safe and the crate needs
/// no async runtime), the same shape as the store seam.
pub type SinkFuture<'a> = Pin<Box<dyn Future<Output = Result<(), SinkError>> + Send + 'a>>;

/// A sink refusal. The reason names a thing (a record id, a position), never a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinkError {
    /// Why the sink refused.
    pub reason: String,
}

impl SinkError {
    /// A refusal with `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// What the executor is about to do, handed to [`ApplySink::begin`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    /// The tenant being rebuilt.
    pub tenant: String,
    /// The source whose stream is replayed.
    pub source: String,
    /// What is restored first. For a backup base the database is already restored; nothing is
    /// streamed for it.
    pub base: Base,
    /// How many segment ranges follow.
    pub steps: usize,
    /// The last position the rebuilt database will hold.
    pub ends_at: u64,
    /// The generation the rebuilt stream continues in, when it forks.
    pub new_generation: Option<u64>,
    /// Where history is cut, when it forks.
    pub fork_point: Option<ForkPoint>,
    /// Whether the rebuilt database adopts the journal's positions unchanged.
    pub preserves_positions: bool,
    /// The schema the images are upcast to, for entities without an override.
    pub target_schema: u64,
}

/// One state image to store: the whole document, upcast to the target schema.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedImage {
    /// The entity.
    pub entity: String,
    /// The record id.
    pub id: String,
    /// The version to store.
    pub version: u64,
    /// The position this version was recorded at: the entry's position, or, for a snapshot image,
    /// the snapshot's.
    pub position: Position,
    /// The document, in the shape of the target schema.
    pub image: Image,
}

/// One tombstone from the stream: the record is deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedDelete {
    /// The entity.
    pub entity: String,
    /// The record id.
    pub id: String,
    /// The last version the record had.
    pub version: u64,
    /// The position of the delete.
    pub position: Position,
}

/// One audit-class entry from the stream. It is part of the recoverable record (ADR-061 s2) and
/// is handed over untouched; no image, no schema.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedAudit {
    /// The event id.
    pub event_id: String,
    /// The topic (`sys_...`).
    pub topic: String,
    /// The position of the entry.
    pub position: Position,
    /// The subject's entity.
    pub entity: String,
    /// The subject's record id.
    pub id: String,
    /// The topic data, as stored (sealed fields stay sealed).
    pub data: serde_json::Value,
}

/// Where a rebuild's records go. See the module docs for the contract.
pub trait ApplySink: Send {
    /// Called once, after every check that can be made before applying has passed.
    fn begin<'a>(&'a mut self, summary: &'a PlanSummary) -> SinkFuture<'a>;
    /// Store one document at its recorded version.
    fn apply_image(&mut self, image: AppliedImage) -> SinkFuture<'_>;
    /// Remove one document: a tombstone in the stream.
    fn apply_delete(&mut self, delete: AppliedDelete) -> SinkFuture<'_>;
    /// Keep one audit-class entry.
    fn apply_audit(&mut self, audit: AppliedAudit) -> SinkFuture<'_>;
    /// The rebuild reached its end; `last_position` is the last position applied.
    fn commit(&mut self, last_position: Position) -> SinkFuture<'_>;
    /// The rebuild stopped (an error or a cancel); discard what was applied. `reason` is a stable
    /// `journal_rebuild.*` code.
    fn abort<'a>(&'a mut self, reason: &'a str) -> SinkFuture<'a>;
}
