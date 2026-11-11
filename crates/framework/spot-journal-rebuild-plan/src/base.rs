//! The base a rebuild restores first, and the live history it is rolled forward along.
//!
//! Two things live here because they are the same question asked twice: what the journal
//! *currently* holds (the live chain: which ranges are still history and which a fork
//! abandoned), and what the rebuild may *start* from (the base).

use serde::{Deserialize, Serialize};

use crate::catalog::{FIRST_POSITION, ForkInfo, Generation, JournalCatalog, Position};
use crate::error::PlanError;

/// What a rebuild restores before it applies any segment (ADR-061 s6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Base {
    /// A verified snapshot: the records as of its position, in its generation.
    Snapshot {
        /// The position the snapshot holds every record as of.
        position: Position,
        /// The generation that position belongs to.
        generation: Generation,
    },
    /// A database backup, rolled forward from its position.
    Backup {
        /// The position the backup holds every record as of.
        position: Position,
    },
    /// Nothing: the segments apply from [`FIRST_POSITION`].
    Empty,
}

impl Base {
    /// The last position the base already holds, or `0` for [`Base::Empty`] (nothing).
    ///
    /// The first applied segment starts at `position + 1`.
    #[must_use]
    pub fn position(&self) -> Position {
        match self {
            Self::Snapshot { position, .. } | Self::Backup { position } => *position,
            Self::Empty => 0,
        }
    }

    /// The generation the base holds its records in, when it knows one.
    ///
    /// A backup does not carry a generation; the planner takes the generation of the live range
    /// that covers its position.
    #[must_use]
    pub fn generation(&self) -> Option<Generation> {
        match self {
            Self::Snapshot { generation, .. } => Some(*generation),
            Self::Backup { .. } | Self::Empty => None,
        }
    }
}

/// One segment range that is still history, in chain order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiveRange {
    /// The generation of the range.
    pub(crate) generation: Generation,
    /// Its first position.
    pub(crate) first_seq: Position,
    /// Its last position.
    pub(crate) last_seq: Position,
}

/// A stretch of positions a fork abandoned: the parent generation wrote it, the live history
/// does not continue it (ADR-061 s6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AbandonedRegion {
    /// The generation that wrote it.
    pub(crate) generation: Generation,
    /// Its first position.
    pub(crate) first_seq: Position,
    /// Its last position.
    pub(crate) last_seq: Position,
}

/// The journal as it currently stands: the ranges that are history, in position order, and the
/// ranges a fork abandoned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveChain {
    /// The live ranges, sorted by first position, contiguous where they touch.
    pub(crate) ranges: Vec<LiveRange>,
    /// The ranges a fork abandoned, in catalogue order of the forks.
    pub(crate) abandoned: Vec<AbandonedRegion>,
    /// The recorded forks, as `(generation, from_position)`, sorted.
    pub(crate) forks: Vec<(Generation, Position)>,
    /// The current generation: the highest one a segment or a fork names.
    pub(crate) current_generation: Generation,
    /// The first position the live history holds.
    pub(crate) start: Position,
    /// The last position the live history holds.
    pub(crate) end: Position,
}

impl LiveChain {
    /// Reads a catalogue into the live chain, refusing a catalogue that cannot describe a
    /// journal: no segments, reversed ranges, overlapping ranges, a range that straddles a fork
    /// point, or a generation entered with no fork recorded at the position it is entered at.
    pub(crate) fn build(catalog: &JournalCatalog) -> Result<Self, PlanError> {
        if catalog.segments.is_empty() {
            return Err(PlanError::NoSegments);
        }
        for range in &catalog.segments {
            if range.first_seq > range.last_seq {
                return Err(PlanError::SegmentRangeInvalid {
                    generation: range.generation,
                    first_seq: range.first_seq,
                    last_seq: range.last_seq,
                });
            }
        }
        let mut generations: Vec<Generation> =
            catalog.segments.iter().map(|s| s.generation).collect();
        generations.sort_unstable();
        generations.dedup();
        let mut live: Vec<LiveRange> = catalog
            .segments
            .iter()
            .map(|s| LiveRange {
                generation: s.generation,
                first_seq: s.first_seq,
                last_seq: s.last_seq,
            })
            .collect();
        let mut forks: Vec<(Generation, Position)> = catalog
            .forks
            .iter()
            .map(|f: &ForkInfo| (f.generation, f.from_position))
            .collect();
        forks.sort_unstable();
        let mut abandoned: Vec<AbandonedRegion> = Vec::new();
        for &(generation, from_position) in &forks {
            let Some(parent) = generations.iter().rev().copied().find(|g| *g < generation) else {
                // A fork of the first generation names nothing below it: nothing is abandoned.
                continue;
            };
            let mut kept: Vec<LiveRange> = Vec::with_capacity(live.len());
            for range in live {
                if range.generation != parent {
                    kept.push(range);
                } else if range.first_seq > from_position {
                    abandoned.push(AbandonedRegion {
                        generation: parent,
                        first_seq: range.first_seq,
                        last_seq: range.last_seq,
                    });
                } else if range.last_seq > from_position {
                    // The fork point falls inside a sealed segment: the caller's catalogue and
                    // its fork record disagree about where history was cut.
                    return Err(PlanError::SegmentsInconsistent {
                        generation: parent,
                        position: from_position,
                    });
                } else {
                    kept.push(range);
                }
            }
            live = kept;
        }
        live.sort_by_key(|r| (r.first_seq, r.generation));
        for pair in live.windows(2) {
            if pair[1].first_seq <= pair[0].last_seq {
                return Err(PlanError::SegmentsInconsistent {
                    generation: pair[1].generation,
                    position: pair[1].first_seq,
                });
            }
        }
        for pair in live.windows(2) {
            if pair[1].generation != pair[0].generation
                && !forks.contains(&(pair[1].generation, pair[0].last_seq))
            {
                return Err(PlanError::UnrecordedFork {
                    generation: pair[1].generation,
                    position: pair[1].first_seq,
                });
            }
        }
        let (Some(first), Some(last)) = (live.first(), live.last()) else {
            // Every range was abandoned: nothing is left to rebuild from.
            return Err(PlanError::NoSegments);
        };
        let current_generation = live
            .iter()
            .map(|r| r.generation)
            .chain(forks.iter().map(|(g, _)| *g))
            .max()
            .unwrap_or(first.generation);
        Ok(Self {
            start: first.first_seq,
            end: last.last_seq,
            current_generation,
            ranges: live,
            abandoned,
            forks,
        })
    }

    /// Whether a fork records that `generation` began at `from_position`.
    pub(crate) fn forked_at(&self, generation: Generation, from_position: Position) -> bool {
        self.forks.contains(&(generation, from_position))
    }

    /// The refusal for a position the live history does not hold: a fork explains some of those
    /// positions, and the rest are a coverage gap.
    pub(crate) fn classify_missing(&self, position: Position) -> PlanError {
        for region in &self.abandoned {
            if region.first_seq <= position && position <= region.last_seq {
                return PlanError::TargetInsideAbandonedGeneration {
                    generation: region.generation,
                    position,
                };
            }
        }
        PlanError::GapAt { position }
    }

    /// The refusal for a target the live history does not reach.
    pub(crate) fn classify_target(&self, position: Position) -> PlanError {
        let explained = self
            .abandoned
            .iter()
            .any(|r| r.first_seq <= position && position <= r.last_seq);
        if explained {
            self.classify_missing(position)
        } else {
            PlanError::TargetBeyondEnd {
                end: self.end,
                position,
            }
        }
    }

    /// The first live range that ends after `position`, if any: the range the walk starts in.
    pub(crate) fn first_range_after(&self, position: Position) -> usize {
        self.ranges
            .iter()
            .position(|r| r.last_seq > position)
            .unwrap_or(self.ranges.len())
    }

    /// The generation the base's position belongs to: the range that covers it, or the range
    /// that starts right after it, or `None` when the position is before the live history.
    pub(crate) fn generation_at(&self, position: Position) -> Option<Generation> {
        if position < FIRST_POSITION {
            return None;
        }
        self.ranges
            .iter()
            .find(|r| r.first_seq <= position && position <= r.last_seq)
            .or_else(|| self.ranges.iter().find(|r| r.first_seq > position))
            .map(|r| r.generation)
    }
}
