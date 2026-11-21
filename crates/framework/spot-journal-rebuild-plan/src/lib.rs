//! The journal rebuild planner (WP-JRN-05.T1, ADR-061 s6).
//!
//! A rebuild is "the latest valid snapshot at or before the target position, then the segments
//! after it, applied in stream order, then projectors for derived entities, then the invariant
//! suite" (ADR-061 s6). This crate computes *that plan* and nothing else: it applies nothing,
//! reads no manifest and opens no store, so the safety-critical part — which base to trust,
//! which positions must follow, whether the rebuild forks the stream — is decided and tested on
//! its own. The apply path is WP-EDG-06's.
//!
//! # What the planner takes
//!
//! A [`JournalCatalog`] describing what the journal holds — [`SnapshotInfo`] entries (each with
//! its position, generation, a verified flag and when it was taken), [`SegmentRange`]s (each
//! with its generation and first and last sequence number) and [`ForkInfo`] records — plus an
//! optional [`Backup`] to roll forward and a [`Target`]: [`Target::End`], [`Target::At`] a position
//! of the current generation, or [`Target::In`] a named generation. Positions and generations
//! start at one ([`FIRST_POSITION`]), and **positions start again at one in every generation**
//! (WP-JRN-03.T10): the journal's order is `(generation, position)`, as the manifest, the relay and
//! the feed already number it.
//!
//! # The rules it enforces
//!
//! * Only **verified** snapshots are trusted; the latest verified one at or before the target
//!   wins (ties: higher generation, then later `taken_at_millis`, then later in the catalogue).
//! * Rebuilding to the end of the journal keeps the generation and preserves positions: every
//!   consumer's bookmark stays valid.
//! * Rebuilding to a position before the end **forks**: [`RebuildPlan::new_generation`] is the
//!   current generation plus one, [`RebuildPlan::fork_point`] records where the cut is and
//!   which generation it abandons, and `preserves_positions` is false because consumers
//!   bookmarked in the abandoned generation must resync (ADR-053's `ResyncRequired`).
//! * A target inside a generation a fork abandoned is refused with
//!   [`PlanError::TargetInsideAbandonedGeneration`]; a hole in coverage between the base and the
//!   target is [`PlanError::GapAt`], naming the **first** missing position; a target with no
//!   base at all is [`PlanError::NoBase`].
//! * A backup newer than the best snapshot is preferred for roll-forward **only** when the
//!   segments cover its position up to the target; otherwise the snapshot is used.
//! * Segments are applied contiguously and in order, and a step may only enter a new generation
//!   at a recorded fork point, at that generation's first position.
//! * A point-in-time target must lie in the current generation: a fork from inside an older
//!   generation cannot be recorded in the manifest chain ([`PlanError::TargetBeforeFork`]).
//!
//! Time is an argument (`taken_at_millis`), never read; the crate has no I/O, no clock, no
//! randomness and no floats, and `plan` is deterministic: the same catalogue, backup and target
//! give the same plan.
//!
//! # Example
//!
//! ```
//! use spot_journal_rebuild_plan::{
//!     JournalCatalog, SegmentRange, SnapshotInfo, Target, plan,
//! };
//!
//! let catalog = JournalCatalog {
//!     snapshots: vec![SnapshotInfo {
//!         position: 40,
//!         generation: 1,
//!         verified: true,
//!         taken_at_millis: 1_728_000_000_000,
//!     }],
//!     segments: vec![
//!         SegmentRange {
//!             generation: 1,
//!             first_seq: 41,
//!             last_seq: 80,
//!         },
//!         SegmentRange {
//!             generation: 1,
//!             first_seq: 81,
//!             last_seq: 120,
//!         },
//!     ],
//!     forks: Vec::new(),
//! };
//!
//! let plan = plan(&catalog, None, Target::End).expect("the journal is complete");
//! assert_eq!(plan.ends_at, 120);
//! assert_eq!(plan.steps.len(), 2);
//! assert!(plan.preserves_positions);
//! ```

mod base;
mod catalog;
mod error;
mod explain;
mod plan;
pub mod unsealed;

#[cfg(test)]
mod tests;

pub use base::Base;
pub use catalog::{
    Backup, FIRST_POSITION, ForkInfo, Generation, JournalCatalog, Position, SegmentRange,
    SnapshotInfo, Target,
};
pub use error::PlanError;
pub use explain::{ExplainLine, explain};
pub use plan::{ForkPoint, RebuildPlan, Step, plan};
