//! What the journal holds, as the planner takes it: snapshots, segment ranges and forks.
//!
//! Every structure here is an argument. The planner reads no manifest, no store and no clock:
//! the caller describes the journal and the planner decides which base to restore and which
//! segments to apply (ADR-061 s6).

use serde::{Deserialize, Serialize};

/// A position in the journal: the sequence number of an entry within its generation.
///
/// Positions are integers, never floating point. The wire `Position` of `contracts/events`
/// pairs this sequence number with its generation; the planner keeps the two apart because
/// every structure it reads already carries the generation beside the position (a snapshot is
/// `(position, generation)`, a segment range is `(generation, first_seq, last_seq)`), and a
/// target position has to be resolvable against the journal on its own — see
/// [`Target::At`].
pub type Position = u64;

/// A stream generation (ADR-061 s6): rebuilding to a point in time forks into `generation + 1`.
pub type Generation = u64;

/// The first position of a journal: a rebuild with an [`Base::Empty`](crate::Base::Empty) base
/// starts here.
pub const FIRST_POSITION: Position = 1;

/// One snapshot of the journal: every captured record as of `position`, in `generation`.
///
/// A snapshot is trusted only once a rebuild test has verified it (ADR-061 s6 and s8), so
/// [`Self::verified`] is what makes it usable as a base.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotInfo {
    /// The journal position the snapshot holds every record as of.
    pub position: Position,
    /// The generation that position belongs to.
    pub generation: Generation,
    /// Whether a rebuild test has verified the snapshot. Unverified snapshots are never trusted.
    pub verified: bool,
    /// When the snapshot was taken, in milliseconds since the Unix epoch; `i64`, not a float.
    pub taken_at_millis: i64,
}

/// One sealed segment of the journal: positions `first_seq..=last_seq` of `generation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentRange {
    /// The generation the segment belongs to.
    pub generation: Generation,
    /// The first sequence number the segment holds.
    pub first_seq: Position,
    /// The last sequence number the segment holds.
    pub last_seq: Position,
}

/// A recorded fork: `generation` began at `from_position`, so the parent generation's positions
/// after it were abandoned (ADR-061 s6).
///
/// The parent of a fork is the newest generation below [`Self::generation`] that holds
/// segments; the fork chain is linear and its generations are ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkInfo {
    /// The generation the fork created.
    pub generation: Generation,
    /// The position the new generation branched from: the last position the two share.
    pub from_position: Position,
}

/// What the journal holds right now: its snapshots, its segment ranges and its forks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalCatalog {
    /// The snapshots, in no particular order; only the verified ones are ever trusted.
    pub snapshots: Vec<SnapshotInfo>,
    /// The sealed segments, in no particular order.
    pub segments: Vec<SegmentRange>,
    /// The recorded forks, in no particular order.
    pub forks: Vec<ForkInfo>,
}

impl JournalCatalog {
    /// Whether a fork cut history before `snapshot`: the snapshot is of an older generation than
    /// the fork's and lies after the fork point.
    fn abandoned_by_fork(&self, snapshot: &SnapshotInfo) -> bool {
        self.forks
            .iter()
            .any(|f| snapshot.generation < f.generation && snapshot.position > f.from_position)
    }

    /// The snapshot a rebuild to `target` starts from: the latest **verified** snapshot at or
    /// before `target`.
    ///
    /// Ties are broken by the higher generation, then by the later `taken_at_millis`, then by
    /// the later entry in [`Self::snapshots`], so the choice is deterministic for one catalogue.
    ///
    /// A snapshot of a generation that a fork abandoned, taken after the fork point, holds
    /// history the live journal no longer contains, so it is never a base.
    #[must_use]
    pub fn best_snapshot(&self, target: Position) -> Option<&SnapshotInfo> {
        self.snapshots
            .iter()
            .filter(|s| s.verified && s.position <= target && !self.abandoned_by_fork(s))
            .max_by(|a, b| {
                (a.position, a.generation, a.taken_at_millis).cmp(&(
                    b.position,
                    b.generation,
                    b.taken_at_millis,
                ))
            })
    }
}

/// A database backup the journal is rolled forward from (the normal disaster path, ADR-061 s6).
///
/// A backup carries only the position it holds every record as of; its generation is the
/// generation of the live range that covers that position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    /// The journal position the backup holds every record as of.
    pub position: Position,
}

/// Where a rebuild should stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Rebuild to the end of the journal: the stream keeps its generation.
    End,
    /// Rebuild to a position (point in time); a position before the end forks the stream.
    At(Position),
}
