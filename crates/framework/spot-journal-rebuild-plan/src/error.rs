//! Typed errors with stable `journal_rebuild_plan.*` codes.
//!
//! A refusal always names a position or a generation, never a value of a record: the codes are
//! catalogued in `crates/framework/spot-errors/catalogue/journal_rebuild_plan.toml` and the
//! recovery report turns them into the operator's sentence.

use crate::catalog::{Generation, Position};

/// Why a rebuild plan could not be computed.
///
/// Every variant is a refusal to plan: the planner never returns a partial plan, because a
/// plan that silently skips positions would be applied and lose history.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// The catalogue holds no segment range at all: there is no journal to rebuild from.
    #[error("the catalogue holds no segment range for any generation")]
    NoSegments,
    /// There is nothing to start from: no verified snapshot at or before the target, no usable
    /// backup, and no segments from the first position.
    #[error("no verified snapshot, backup or segment range covers the start of the target")]
    NoBase,
    /// The journal does not cover a position between the base and the target.
    #[error("the journal does not cover position {position}")]
    GapAt {
        /// The first position missing between the base and the target.
        position: Position,
    },
    /// The position lies in a part of a generation that a fork abandoned.
    #[error(
        "position {position} is in the abandoned part of generation {generation}, which the live history does not continue"
    )]
    TargetInsideAbandonedGeneration {
        /// The abandoned generation.
        generation: Generation,
        /// The position inside it.
        position: Position,
    },
    /// The target is after the last position the journal holds.
    #[error("the target position {position} is after the end of the journal at {end}")]
    TargetBeyondEnd {
        /// The end of the journal: the last position it can rebuild to.
        end: Position,
        /// The position that was asked for.
        position: Position,
    },
    /// A segment range is reversed, so it holds no position.
    #[error("the segment range {first_seq}..={last_seq} of generation {generation} is reversed")]
    SegmentRangeInvalid {
        /// The generation of the range.
        generation: Generation,
        /// The first sequence number it claims.
        first_seq: Position,
        /// The last sequence number it claims.
        last_seq: Position,
    },
    /// Two segment ranges disagree about the same positions, or a range straddles a fork point.
    #[error(
        "the segment ranges of generation {generation} are inconsistent at position {position}"
    )]
    SegmentsInconsistent {
        /// The generation of the offending range.
        generation: Generation,
        /// The position where the ranges overlap or the fork falls.
        position: Position,
    },
    /// Applying the segments would enter a generation that no fork records.
    #[error("generation {generation} begins at position {position} with no matching fork record")]
    UnrecordedFork {
        /// The generation the plan would enter.
        generation: Generation,
        /// The first position of that generation in the plan.
        position: Position,
    },
}

impl PlanError {
    /// The stable catalogue code of this refusal.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoSegments => "journal_rebuild_plan.no_segments",
            Self::NoBase => "journal_rebuild_plan.no_base",
            Self::GapAt { .. } => "journal_rebuild_plan.gap_at",
            Self::TargetInsideAbandonedGeneration { .. } => {
                "journal_rebuild_plan.target_inside_abandoned_generation"
            }
            Self::TargetBeyondEnd { .. } => "journal_rebuild_plan.target_beyond_end",
            Self::SegmentRangeInvalid { .. } => "journal_rebuild_plan.segment_range_invalid",
            Self::SegmentsInconsistent { .. } => "journal_rebuild_plan.segments_inconsistent",
            Self::UnrecordedFork { .. } => "journal_rebuild_plan.unrecorded_fork",
        }
    }
}
