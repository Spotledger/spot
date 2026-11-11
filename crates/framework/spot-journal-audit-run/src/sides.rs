//! The seams of the runner: the live side, the scratch side and the clock.
//!
//! The real database sides are another task's; the runner only sees these traits, so it can be
//! tested with the in-memory sides of [`crate::memory`].

use spot_journal_audit::{Fingerprint, Position, RecordSummary};
use spot_journal_rebuild::ApplySink;

/// A sorted stream of record summaries (entity, then id, strictly ascending).
pub type Summaries<'a> = Box<dyn Iterator<Item = RecordSummary> + 'a>;

/// The live database, read-only by trait: the runner can ask, never write.
pub trait LiveSide {
    /// The tenant this side belongs to; checked against the run's tenant.
    fn tenant(&self) -> &str;
    /// The journal position the live database has reached now.
    fn head(&self) -> Position;
    /// The summary of every record as of `as_of`, sorted by entity then id. A live side that has
    /// moved past `as_of` answers as of `as_of` all the same.
    fn summaries(&self, as_of: Position) -> Summaries<'_>;
    /// The financial fingerprints as of `as_of`.
    fn fingerprints(&self, as_of: Position) -> Vec<Fingerprint>;
}

/// The scratch store a rebuild is applied to. It is always [`ScratchSide::discard`]ed at the end
/// of a run, on every path.
pub trait ScratchSide: ApplySink {
    /// The tenant this side belongs to; checked against the run's tenant.
    fn tenant(&self) -> &str;
    /// The summary of every rebuilt record, sorted by entity then id.
    fn summaries(&self) -> Summaries<'_>;
    /// The financial fingerprints of the rebuilt data.
    fn fingerprints(&self) -> Vec<Fingerprint>;
    /// Drops everything the rebuild left behind.
    fn discard(&mut self);
}

/// The injected clock; the library never reads the wall clock itself.
pub trait Clock {
    /// Now, in unix milliseconds. Used for the rebuild duration and the verified-at time.
    fn now_millis(&self) -> i64;
}
