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
/// Positions restart at 1 in every generation, so the journal's order is `(generation, position)`
/// and the walk keeps a cursor of that pair: inside a generation positions are contiguous, and a
/// generation is entered only at the position the previous one was cut at, from its position 1.
///
/// The base is a backup newer than the best verified snapshot at or before the target **when the
/// segments cover its position up to the target**; otherwise the latest verified snapshot at or
/// before the target; otherwise nothing at all, when the segments run from [`FIRST_POSITION`].
/// A snapshot the segments cannot continue is a refusal, not a reason to replay from the start.
///
/// Rebuilding to the end of the journal keeps the generation and preserves positions;
/// rebuilding to an earlier position of the current generation forks, naming the point in
/// [`RebuildPlan::fork_point`] and the new generation in [`RebuildPlan::new_generation`]. A
/// point-in-time target in an older generation is refused: see [`Target::In`].
///
/// # Errors
/// [`PlanError`]: no segments, no base, a gap between the base and the target, a target in an
/// abandoned generation, before a later fork or beyond the end, a reversed range, inconsistent
/// ranges, a generation entered with no fork recorded, or a backup two generations claim.
pub fn plan(
    catalog: &JournalCatalog,
    backup: Option<&Backup>,
    target: Target,
) -> Result<RebuildPlan, PlanError> {
    let chain = LiveChain::build(catalog)?;
    let target: (Generation, Position) = match target {
        Target::End => chain.end,
        Target::At(position) => (chain.end.0, position),
        Target::In {
            generation,
            position,
        } => (generation, position),
    };
    if target > chain.end || target.0 < chain.end.0 {
        return Err(chain.classify_target(target.0, target.1));
    }
    let snapshot: Option<&SnapshotInfo> = catalog.best_snapshot(target.0, target.1);
    let backup: Option<(Backup, Generation)> = match backup {
        Some(b) => resolve_backup(&chain, b)?.filter(|(b, g)| (*g, b.position) <= target),
        None => None,
    };
    // A backup newer than the best snapshot is tried first, and only kept when the segments
    // cover its position up to the target. A verified snapshot is the documented base: if the
    // segments cannot continue it, that is the refusal, because replaying the whole journal
    // instead would hide a snapshot the journal no longer agrees with.
    let mut refusal: Option<PlanError> = None;
    if let Some((backup, generation)) = backup
        && snapshot.is_none_or(|s| (generation, backup.position) > (s.generation, s.position))
    {
        match walk(&chain, (generation, backup.position), target) {
            Ok(steps) => {
                return Ok(finish(
                    &chain,
                    Base::Backup {
                        position: backup.position,
                        generation,
                    },
                    steps,
                    target,
                ));
            }
            Err(error) => refusal = Some(error),
        }
    }
    if let Some(snapshot) = snapshot {
        let steps = walk(&chain, (snapshot.generation, snapshot.position), target)?;
        return Ok(finish(
            &chain,
            Base::Snapshot {
                position: snapshot.position,
                generation: snapshot.generation,
            },
            steps,
            target,
        ));
    }
    // No snapshot at all: the segments may still carry the rebuild from the first position, and
    // if they do not, their first hole is the answer rather than a missing base.
    if chain.start.1 == FIRST_POSITION {
        let steps = walk(&chain, (chain.start.0, 0), target)?;
        return Ok(finish(&chain, Base::Empty, steps, target));
    }
    Err(refusal.unwrap_or(PlanError::NoBase))
}

/// The generation of a backup: the one it names, or the one live range that covers its position.
/// `None` when the position is before every live range that could hold it.
fn resolve_backup(
    chain: &LiveChain,
    backup: &Backup,
) -> Result<Option<(Backup, Generation)>, PlanError> {
    if let Some(generation) = backup.generation {
        return Ok(Some((*backup, generation)));
    }
    let covering = chain.generations_covering(backup.position);
    match covering.as_slice() {
        [generation] => Ok(Some((*backup, *generation))),
        [] => Ok(chain
            .ranges
            .iter()
            .find(|r| r.first_seq > backup.position)
            .map(|r| (*backup, r.generation))),
        _ => Err(PlanError::BackupAmbiguous {
            position: backup.position,
        }),
    }
}

/// Applies the live ranges from the cursor `from` (exclusive) to `target` (inclusive).
fn walk(
    chain: &LiveChain,
    from: (Generation, Position),
    target: (Generation, Position),
) -> Result<Vec<Step>, PlanError> {
    let mut steps: Vec<Step> = Vec::new();
    let (mut generation, mut position) = from;
    for range in &chain.ranges[chain.first_range_after(generation, position)..] {
        if (generation, position) >= target || range.generation > target.0 {
            break;
        }
        if range.generation != generation {
            // A generation is entered only at the position the previous one was cut at, and its
            // positions start again at the beginning.
            if !chain.forked_at(range.generation, position) {
                return Err(PlanError::UnrecordedFork {
                    generation: range.generation,
                    position: range.first_seq,
                });
            }
            generation = range.generation;
            position = 0;
        }
        if range.first_seq > position.saturating_add(1) {
            return Err(chain.classify_missing(generation, position.saturating_add(1)));
        }
        let first_seq = range.first_seq.max(position.saturating_add(1));
        let last_seq = if range.generation == target.0 {
            range.last_seq.min(target.1)
        } else {
            range.last_seq
        };
        if first_seq > last_seq {
            break;
        }
        steps.push(Step::ApplySegmentRange {
            generation,
            first_seq,
            last_seq,
        });
        position = last_seq;
    }
    if (generation, position) < target {
        return Err(chain.classify_missing(generation, position.saturating_add(1)));
    }
    Ok(steps)
}

/// Fills in the shape of a plan that reaches the target: whether it forks, and what runs after.
fn finish(
    chain: &LiveChain,
    base: Base,
    steps: Vec<Step>,
    ends_at: (Generation, Position),
) -> RebuildPlan {
    let fork_point = (ends_at < chain.end).then_some(ForkPoint {
        from_position: ends_at.1,
        abandoned_generation: ends_at.0,
    });
    RebuildPlan {
        base,
        steps,
        ends_at: ends_at.1,
        new_generation: fork_point
            .is_some()
            .then(|| chain.current_generation.saturating_add(1)),
        fork_point,
        projectors_after: true,
        invariant_suite_after: true,
        preserves_positions: fork_point.is_none(),
    }
}
