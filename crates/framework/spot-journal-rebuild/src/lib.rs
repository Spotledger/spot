//! The journal rebuild executor (WP-JRN-05.T3, ADR-061 s6).
//!
//! The planner ([`spot_journal_rebuild_plan`]) decides *what* a rebuild does; this crate runs it
//! for one tenant. [`execute`] reads the base snapshot and the planned segment ranges from the
//! journal store, checks them, upcasts each state image to the current schema and hands it to an
//! [`ApplySink`]. The actual database write is the sink's, and the sink is the replication apply
//! path of WP-EDG-06, which implements the trait later. The rule of ADR-061 s6 this crate makes
//! provable: images are applied **in stream order**, with their **stored versions**, running
//! **no business logic** and enqueuing **no events**, and **positions are preserved**.
//!
//! # What happens, in order
//!
//! 1. The options and the plan are checked for consistency ([`RebuildError::PlanInvalid`]).
//! 2. Unless [`RebuildOptions::skip_verify`], the light journal verification runs for every
//!    generation the plan reads ([`RebuildError::VerifyFailed`] on any tampering finding). Then
//!    the signed manifests are loaded and verified: they, not a directory listing, say which
//!    segments exist and what their hashes are.
//! 3. The snapshot index is opened (its signature, tenant, generation and position are checked)
//!    and its entity schemas are compared with the upcaster registry. The planned segments are
//!    read once ([`RebuildOptions::prescan`]). A missing upcaster step
//!    ([`RebuildError::MissingUpcast`]), a gap, a foreign source or a malformed entry is
//!    therefore reported **before the sink has been called at all**.
//! 4. `begin`, then the snapshot images in stored order, then each step's entries in position
//!    order, then `commit` with the last position. Any later failure calls `abort`.
//!
//! # Which entries are replayed
//!
//! Only the **state** class (`doc_state`) and the **audit** class (`sys_*` topics) are handed to
//! the sink (see [`classify`]); domain events are counted and skipped, because the state they
//! caused is in the stream. Replaying domain events would run the consumers' reactions twice.
//!
//! # Ordering and versions
//!
//! Every entry must be at exactly the next position ([`RebuildError::PositionGap`]), also across
//! steps and across a fork's generation change. A state entry whose version is below the version
//! already applied for that record (from the snapshot or earlier in the stream) is
//! [`RebuildError::VersionRegressed`]; the **same** version again is an idempotent re-delivery
//! and is skipped and counted. A delete carries the last version the record had and is applied
//! once; an insert after a delete starts a new life.
//!
//! # Tenancy
//!
//! The executor works for [`RebuildOptions::tenant`] and [`RebuildOptions::source`]. Every key is
//! built from the tenant; every manifest, segment header and envelope must name the source; the
//! snapshot index must name tenant, generation and position. The sink never sees an entry from
//! another source. The executor never decrypts: sealed fields are passed through untouched, and
//! the protect/reveal hook (WP-JRN-08.T4) sits between the executor and the sink.
//!
//! # Forks, progress and cancel
//!
//! A point-in-time plan carries a fork point and the new generation; the outcome reports them and
//! the executor writes nothing to the journal (the manifest's fork record is the relay's).
//! [`RebuildOptions::progress`] is called every N items and [`RebuildOptions::cancel`] is checked
//! between items; a cancel aborts the sink and returns [`RebuildError::Cancelled`].
//!
//! # Memory
//!
//! Segment entries are decoded one block at a time ([`RebuildOutcome::peak_buffered_entries`]).
//! The executor keeps one small record per distinct document (entity, id, version) to check
//! versions; it never keeps images.
//!
//! The crate is a library over traits: no network, no runtime (the functions are plain `async
//! fn`s over the store's boxed futures), no clock, no randomness, no database.

#![forbid(unsafe_code)]

mod entry;
mod error;
mod execute;
mod options;
mod recording;
mod sink;
mod stream;

#[cfg(test)]
mod tests;

pub use error::RebuildError;
pub use execute::{RebuildKeys, execute};
pub use options::{
    AUDIT_PREFIX, EventClass, Progress, RebuildOptions, RebuildOutcome, STATE_TOPIC, classify,
};
pub use recording::{Call, RecordedImage, RecordingSink};
pub use sink::{
    AppliedAudit, AppliedDelete, AppliedImage, ApplySink, PlanSummary, SinkError, SinkFuture,
};
