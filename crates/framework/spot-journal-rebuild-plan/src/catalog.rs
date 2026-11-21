//! What the journal holds, as the planner takes it: snapshots, segment ranges and forks.
//!
//! Every structure here is an argument. The planner reads no manifest, no store and no clock:
//! the caller describes the journal and the planner decides which base to restore and which
//! segments to apply (ADR-061 s6).

use serde::{Deserialize, Serialize};

/// A position in the journal: the sequence number of an entry **within its generation**.
///
/// Positions are integers, never floating point. A new generation starts again at
/// [`FIRST_POSITION`] (WP-JRN-03.T10: the journal numbers the stream as the manifest and the feed
/// do), so a sequence number names a record only together with its generation. The wire
/// `Position` of `contracts/events` pairs the two; the planner keeps them apart because every
/// structure it reads already carries the generation beside the position (a snapshot is
/// `(position, generation)`, a segment range is `(generation, first_seq, last_seq)`). Where a
/// bare position is offered it is explicit about which generation it is in: see [`Target`] and
/// [`Backup`].
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
    /// The generation a fork branched from: the newest generation below the fork's that holds
    /// segments, or the one just before it when none does.
    fn parent_of(&self, fork: &ForkInfo) -> Generation {
        self.segments
            .iter()
            .map(|s| s.generation)
            .filter(|g| *g < fork.generation)
            .max()
            .unwrap_or_else(|| fork.generation.saturating_sub(1))
    }

    /// Whether a fork cut history before `snapshot`: the snapshot is of the generation the fork
    /// branched from and lies after the fork point.
    fn abandoned_by_fork(&self, snapshot: &SnapshotInfo) -> bool {
        self.forks.iter().any(|f| {
            self.parent_of(f) == snapshot.generation && snapshot.position > f.from_position
        })
    }

    /// The snapshot a rebuild to position `target` of `generation` starts from: the latest
    /// **verified** snapshot at or before it, in the journal's order `(generation, position)`.
    ///
    /// Ties are broken by the later `taken_at_millis`, then by the later entry in
    /// [`Self::snapshots`], so the choice is deterministic for one catalogue.
    ///
    /// A snapshot of a generation that a fork abandoned, taken after the fork point, holds
    /// history the live journal no longer contains, so it is never a base.
    #[must_use]
    pub fn best_snapshot(&self, generation: Generation, target: Position) -> Option<&SnapshotInfo> {
        self.snapshots
            .iter()
            .filter(|s| {
                s.verified
                    && (s.generation, s.position) <= (generation, target)
                    && !self.abandoned_by_fork(s)
            })
            .max_by(|a, b| {
                (a.generation, a.position, a.taken_at_millis).cmp(&(
                    b.generation,
                    b.position,
                    b.taken_at_millis,
                ))
            })
    }
}

/// A database backup the journal is rolled forward from (the normal disaster path, ADR-061 s6).
///
/// A backup carries the position it holds every record as of, and (since positions restart in
/// each generation) should carry the generation too. Without one, the generation is that of the
/// single live range that covers the position; when two generations cover it the planner refuses
/// ([`PlanError::BackupAmbiguous`](crate::PlanError::BackupAmbiguous)) instead of choosing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    /// The journal position the backup holds every record as of.
    pub position: Position,
    /// The generation that position belongs to, when the backup knows it.
    #[serde(default)]
    pub generation: Option<Generation>,
}

impl Backup {
    /// A backup at `position` whose generation is taken from the live history.
    #[must_use]
    pub const fn new(position: Position) -> Self {
        Self {
            position,
            generation: None,
        }
    }

    /// A backup at `position` of `generation`.
    #[must_use]
    pub const fn in_generation(position: Position, generation: Generation) -> Self {
        Self {
            position,
            generation: Some(generation),
        }
    }
}

/// Where a rebuild should stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Rebuild to the end of the journal: the stream keeps its generation.
    End,
    /// Rebuild to a position **of the current generation** (the generation of the newest live
    /// segment): a point in time, which forks the stream. A position of an older generation is
    /// named with [`Target::In`], so a bare number can never pick the wrong generation.
    At(Position),
    /// Rebuild to a position of a named generation. Only the current generation is a valid
    /// point-in-time target: an older generation's live history was continued by the next one,
    /// and a fork from inside it cannot be recorded in the manifest chain, so it is refused
    /// ([`PlanError::TargetBeforeFork`](crate::PlanError::TargetBeforeFork)).
    In {
        /// The generation the position belongs to.
        generation: Generation,
        /// The position within that generation.
        position: Position,
    },
}
