//! The base a rebuild restores first, and the live history it is rolled forward along.
//!
//! Two things live here because they are the same question asked twice: what the journal
//! *currently* holds (the live chain: which ranges are still history and which a fork
//! abandoned), and what the rebuild may *start* from (the base).

use serde::{Deserialize, Serialize};

use crate::catalog::{ForkInfo, Generation, JournalCatalog, Position};
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
        /// The generation that position belongs to: the backup's own, or the one the planner
        /// found for it in the live history.
        generation: Generation,
    },
    /// Nothing: the segments apply from [`crate::FIRST_POSITION`] of the first live generation.
    Empty,
}

impl Base {
    /// The last position the base already holds, or `0` for [`Base::Empty`] (nothing).
    ///
    /// The first applied segment starts at `position + 1`.
    #[must_use]
    pub fn position(&self) -> Position {
        match self {
            Self::Snapshot { position, .. } | Self::Backup { position, .. } => *position,
            Self::Empty => 0,
        }
    }

    /// The generation the base holds its records in, when it has one: a snapshot's, or the
    /// backup's (named by the caller or found by the planner). An empty base has none.
    #[must_use]
    pub fn generation(&self) -> Option<Generation> {
        match self {
            Self::Snapshot { generation, .. } | Self::Backup { generation, .. } => {
                Some(*generation)
            }
            Self::Empty => None,
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
    /// The live ranges, sorted by `(generation, first position)`: each generation's positions
    /// restart at 1, so the journal's order is the generation first.
    pub(crate) ranges: Vec<LiveRange>,
    /// The ranges a fork abandoned, in catalogue order of the forks.
    pub(crate) abandoned: Vec<AbandonedRegion>,
    /// The recorded forks, as `(generation, from_position)`, sorted.
    pub(crate) forks: Vec<(Generation, Position)>,
    /// The current generation: the highest one a segment or a fork names.
    pub(crate) current_generation: Generation,
    /// The first position the live history holds, with its generation.
    pub(crate) start: (Generation, Position),
    /// The last position the live history holds, with its generation.
    pub(crate) end: (Generation, Position),
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
        live.sort_by_key(|r| (r.generation, r.first_seq));
        for pair in live.windows(2) {
            if pair[1].generation == pair[0].generation {
                if pair[1].first_seq <= pair[0].last_seq {
                    return Err(PlanError::SegmentsInconsistent {
                        generation: pair[1].generation,
                        position: pair[1].first_seq,
                    });
                }
            } else if !forks.contains(&(pair[1].generation, pair[0].last_seq)) {
                // A generation is entered only at the position the previous one is cut at.
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
            start: (first.generation, first.first_seq),
            end: (last.generation, last.last_seq),
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
    pub(crate) fn classify_missing(&self, generation: Generation, position: Position) -> PlanError {
        for region in &self.abandoned {
            if region.generation == generation
                && region.first_seq <= position
                && position <= region.last_seq
            {
                return PlanError::TargetInsideAbandonedGeneration {
                    generation: region.generation,
                    position,
                };
            }
        }
        PlanError::GapAt {
            generation,
            position,
        }
    }

    /// The refusal for a target the live history does not reach, or reaches only as an ancestor.
    pub(crate) fn classify_target(&self, generation: Generation, position: Position) -> PlanError {
        let explained = self.abandoned.iter().any(|r| {
            r.generation == generation && r.first_seq <= position && position <= r.last_seq
        });
        if explained {
            return self.classify_missing(generation, position);
        }
        if (generation, position) > self.end {
            return PlanError::TargetBeyondEnd {
                generation,
                end_generation: self.end.0,
                end: self.end.1,
                position,
            };
        }
        let live = self.ranges.iter().any(|r| {
            r.generation == generation && r.first_seq <= position && position <= r.last_seq
        });
        if live && generation < self.end.0 {
            return PlanError::TargetBeforeFork {
                generation,
                position,
                current_generation: self.end.0,
            };
        }
        PlanError::GapAt {
            generation,
            position,
        }
    }

    /// The index of the first live range that ends after `(generation, position)`: the range the
    /// walk starts in.
    pub(crate) fn first_range_after(&self, generation: Generation, position: Position) -> usize {
        self.ranges
            .iter()
            .position(|r| (r.generation, r.last_seq) > (generation, position))
            .unwrap_or(self.ranges.len())
    }

    /// The generations whose live ranges cover `position`, oldest first.
    pub(crate) fn generations_covering(&self, position: Position) -> Vec<Generation> {
        let mut found: Vec<Generation> = self
            .ranges
            .iter()
            .filter(|r| r.first_seq <= position && position <= r.last_seq)
            .map(|r| r.generation)
            .collect();
        found.dedup();
        found
    }
}
