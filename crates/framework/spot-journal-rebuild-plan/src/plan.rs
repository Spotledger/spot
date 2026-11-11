//! The rebuild plan and the pure function that computes it.

use serde::{Deserialize, Serialize};

use crate::base::{Base, LiveChain};
use crate::catalog::{
    Backup, FIRST_POSITION, Generation, JournalCatalog, Position, SnapshotInfo, Target,
};
use crate::error::PlanError;

/// One thing a rebuild does, in order.
///
/// A plan is applied through the replication apply path (WP-EDG-06): positions are applied in
/// stream order, without re-running business logic and without enqueuing new events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Apply the entries of one segment range of one generation, in position order.
    ApplySegmentRange {
        /// The generation of the range.
        generation: Generation,
        /// The first position to apply (inclusive).
        first_seq: Position,
        /// The last position to apply (inclusive).
        last_seq: Position,
    },
}

impl Step {
    /// The generation of the range this step applies.
    #[must_use]
    pub fn generation(&self) -> Generation {
        match self {
            Self::ApplySegmentRange { generation, .. } => *generation,
        }
    }

    /// The first position this step applies.
    #[must_use]
    pub fn first_seq(&self) -> Position {
        match self {
            Self::ApplySegmentRange { first_seq, .. } => *first_seq,
        }
    }

    /// The last position this step applies.
    #[must_use]
    pub fn last_seq(&self) -> Position {
        match self {
            Self::ApplySegmentRange { last_seq, .. } => *last_seq,
        }
    }
}

/// Where a point-in-time rebuild cuts history: the position the new generation branches from,
/// and the generation whose positions after it were abandoned (ADR-061 s6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkPoint {
    /// The position the new generation branches from: the last position the two share.
    pub from_position: Position,
    /// The generation that keeps its positions up to the fork and is abandoned after it.
    pub abandoned_generation: Generation,
}

/// What a rebuild does, decided but not yet applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildPlan {
    /// What is restored before any segment is applied.
    pub base: Base,
    /// The segment ranges to apply, in order, contiguous from the base's position.
    pub steps: Vec<Step>,
    /// The last position the rebuilt database holds: the target.
    pub ends_at: Position,
    /// The generation the rebuilt stream continues in, when the rebuild does not keep the
    /// current one.
    pub new_generation: Option<Generation>,
    /// Where the rebuild cuts history; `None` when it rebuilds to the end of the journal.
    pub fork_point: Option<ForkPoint>,
    /// Projectors for derived entities run after the last segment (always, on the rebuild path).
    pub projectors_after: bool,
    /// The invariant suite runs after the projectors (always, on the rebuild path).
    pub invariant_suite_after: bool,
    /// Whether the rebuilt database adopts the journal's positions unchanged: true unless the
    /// rebuild forks, because a fork closes the generation consumers are bookmarked in.
    pub preserves_positions: bool,
}

impl RebuildPlan {
    /// The last position the base holds, which the first step continues from.
    #[must_use]
    pub fn base_position(&self) -> Position {
        self.base.position()
    }
}

/// Plans a rebuild of `catalog` to `target`, optionally rolling a `backup` forward.
///
/// The base is a backup newer than the best verified snapshot at or before the target **when the
/// segments cover its position up to the target**; otherwise the latest verified snapshot at or
/// before the target; otherwise nothing at all, when the segments run from [`FIRST_POSITION`].
/// A snapshot the segments cannot continue is a refusal, not a reason to replay from the start.
///
/// Rebuilding to the end of the journal keeps the generation and preserves positions;
/// rebuilding to an earlier position forks, naming the point in [`RebuildPlan::fork_point`] and
/// the new generation in [`RebuildPlan::new_generation`].
///
/// # Errors
/// [`PlanError`]: no segments, no base, a gap between the base and the target, a target in an
/// abandoned generation or beyond the end, a reversed range, inconsistent ranges, or a
/// generation entered with no fork recorded.
pub fn plan(
    catalog: &JournalCatalog,
    backup: Option<&Backup>,
    target: Target,
) -> Result<RebuildPlan, PlanError> {
    let chain = LiveChain::build(catalog)?;
    let target_position = match target {
        Target::End => chain.end,
        Target::At(position) => position,
    };
    if target_position > chain.end {
        return Err(chain.classify_target(target_position));
    }
    let snapshot: Option<&SnapshotInfo> = catalog.best_snapshot(target_position);
    let backup: Option<Backup> = backup.filter(|b| b.position <= target_position).copied();
    // A backup newer than the best snapshot is tried first, and only kept when the segments
    // cover its position up to the target. A verified snapshot is the documented base: if the
    // segments cannot continue it, that is the refusal, because replaying the whole journal
    // instead would hide a snapshot the journal no longer agrees with.
    let mut refusal: Option<PlanError> = None;
    if let Some(backup) = backup
        && snapshot.is_none_or(|s| backup.position > s.position)
    {
        match walk(&chain, backup.position, None, target_position) {
            Ok(steps) => {
                return Ok(finish(
                    &chain,
                    Base::Backup {
                        position: backup.position,
                    },
                    steps,
                    target_position,
                ));
            }
            Err(error) => refusal = Some(error),
        }
    }
    if let Some(snapshot) = snapshot {
        let steps = walk(
            &chain,
            snapshot.position,
            Some(snapshot.generation),
            target_position,
        )?;
        return Ok(finish(
            &chain,
            Base::Snapshot {
                position: snapshot.position,
                generation: snapshot.generation,
            },
            steps,
            target_position,
        ));
    }
    // No snapshot at all: the segments may still carry the rebuild from the first position, and
    // if they do not, their first hole is the answer rather than a missing base.
    if chain.start == FIRST_POSITION {
        let steps = walk(&chain, 0, None, target_position)?;
        return Ok(finish(&chain, Base::Empty, steps, target_position));
    }
    Err(refusal.unwrap_or(PlanError::NoBase))
}

/// Applies the live ranges from `from` (exclusive) to `target` (inclusive).
fn walk(
    chain: &LiveChain,
    from: Position,
    generation: Option<Generation>,
    target: Position,
) -> Result<Vec<Step>, PlanError> {
    let mut steps: Vec<Step> = Vec::new();
    let mut position = from;
    let mut generation = generation;
    for range in &chain.ranges[chain.first_range_after(position)..] {
        if position >= target {
            break;
        }
        if range.first_seq > position.saturating_add(1) {
            return Err(chain.classify_missing(position.saturating_add(1)));
        }
        let first_seq = range.first_seq.max(position.saturating_add(1));
        if first_seq > target {
            break;
        }
        let last_seq = range.last_seq.min(target);
        if let Some(previous) = generation
            && previous != range.generation
            && !chain.forked_at(range.generation, position)
        {
            return Err(PlanError::UnrecordedFork {
                generation: range.generation,
                position: first_seq,
            });
        }
        steps.push(Step::ApplySegmentRange {
            generation: range.generation,
            first_seq,
            last_seq,
        });
        position = last_seq;
        generation = Some(range.generation);
    }
    if position < target {
        return Err(chain.classify_missing(position.saturating_add(1)));
    }
    Ok(steps)
}

/// Fills in the shape of a plan that reaches the target: whether it forks, and what runs after.
fn finish(chain: &LiveChain, base: Base, steps: Vec<Step>, ends_at: Position) -> RebuildPlan {
    let fork_point = if ends_at < chain.end {
        let abandoned_generation = steps
            .last()
            .map(Step::generation)
            .or_else(|| base.generation())
            .or_else(|| chain.generation_at(base.position()))
            .unwrap_or(chain.current_generation);
        Some(ForkPoint {
            from_position: ends_at,
            abandoned_generation,
        })
    } else {
        None
    };
    RebuildPlan {
        base,
        steps,
        ends_at,
        new_generation: fork_point
            .is_some()
            .then(|| chain.current_generation.saturating_add(1)),
        fork_point,
        projectors_after: true,
        invariant_suite_after: true,
        preserves_positions: fork_point.is_none(),
    }
}
