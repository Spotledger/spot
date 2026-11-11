//! What storage holds against what the manifest lists.
//!
//! Every finding names a position range, so an operator can act on it without reading the
//! segment bytes. The distinction that matters is between data the manifest promises and cannot
//! find (tampering or loss) and data storage holds that the manifest does not mention yet (an
//! ordinary crash between sealing a segment and updating the manifest), which
//! [`Finding::is_tampering`] keeps apart.

use std::cmp::Ordering;

use crate::manifest::SegmentSummary;
use crate::verify::Verified;

/// One difference between a verified manifest and the segments storage holds.
///
/// The variants carry the generation and the inclusive position range they are about: the range
/// the manifest promises ([`Finding::MissingSegment`], [`Finding::HashMismatch`]), the range
/// storage holds ([`Finding::UnlistedSegment`], [`Finding::Reordered`],
/// [`Finding::ForkNotRecorded`]) or the range nobody holds ([`Finding::PositionGap`]).
///
/// Findings order by generation, then by position, then by kind, so a report reads in journal
/// order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Finding {
    /// The manifest lists a segment and storage does not hold it.
    MissingSegment {
        /// The generation of the missing segment.
        generation: u64,
        /// The first sequence number the manifest lists for it.
        first_seq: u64,
        /// The last sequence number the manifest lists for it.
        last_seq: u64,
    },
    /// Storage holds a segment the manifest does not list: an ordinary crash between sealing and
    /// the manifest update, or a repeated copy. Reported, never called tampering.
    UnlistedSegment {
        /// The generation of the extra segment.
        generation: u64,
        /// Its first sequence number.
        first_seq: u64,
        /// Its last sequence number.
        last_seq: u64,
    },
    /// A segment at a position the manifest lists is not the segment the manifest described: its
    /// bytes, or its link to the segment before it, hash to something else.
    HashMismatch {
        /// The generation of the segment.
        generation: u64,
        /// The first sequence number the manifest lists for it.
        first_seq: u64,
        /// The last sequence number the manifest lists for it.
        last_seq: u64,
    },
    /// The segments storage holds leave positions unheld between the manifest's last position and
    /// a later segment.
    PositionGap {
        /// The generation the hole is in.
        generation: u64,
        /// The first unheld sequence number.
        first_seq: u64,
        /// The last unheld sequence number.
        last_seq: u64,
    },
    /// A segment the manifest lists sits at another position in storage (or claims another
    /// position than the manifest gave it): its hash is the hash of a listed segment, but its
    /// range is not that segment's.
    Reordered {
        /// The generation of the segment.
        generation: u64,
        /// The first sequence number storage reports for it.
        first_seq: u64,
        /// The last sequence number storage reports for it.
        last_seq: u64,
    },
    /// Storage holds a generation, or positions in a generation, the manifest's fork history does
    /// not account for: a newer generation without a recorded fork, or positions beyond a recorded
    /// fork point in an abandoned one.
    ///
    /// The second case is a rewritable history problem, not a gap: the fork said where the
    /// abandoned generation ended, and storage now holds data past it.
    ForkNotRecorded {
        /// The generation storage holds.
        generation: u64,
        /// The first sequence number storage reports for it.
        first_seq: u64,
        /// The last sequence number storage reports for it.
        last_seq: u64,
    },
}

impl Finding {
    /// The order findings sort in: generation, first position, last position, kind.
    fn sort_key(&self) -> (u64, u64, u64, u8) {
        let kind = match self {
            Self::MissingSegment { .. } => 0,
            Self::HashMismatch { .. } => 1,
            Self::UnlistedSegment { .. } => 2,
            Self::PositionGap { .. } => 3,
            Self::Reordered { .. } => 4,
            Self::ForkNotRecorded { .. } => 5,
        };
        let (first_seq, last_seq) = self.range();
        (self.generation(), first_seq, last_seq, kind)
    }

    /// A short stable name for the finding, for logs and reports (`missing_segment`, ...).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MissingSegment { .. } => "missing_segment",
            Self::UnlistedSegment { .. } => "unlisted_segment",
            Self::HashMismatch { .. } => "hash_mismatch",
            Self::PositionGap { .. } => "position_gap",
            Self::Reordered { .. } => "reordered",
            Self::ForkNotRecorded { .. } => "fork_not_recorded",
        }
    }

    /// The generation the finding is about.
    #[must_use]
    pub fn generation(&self) -> u64 {
        match self {
            Self::MissingSegment { generation, .. }
            | Self::UnlistedSegment { generation, .. }
            | Self::HashMismatch { generation, .. }
            | Self::PositionGap { generation, .. }
            | Self::Reordered { generation, .. }
            | Self::ForkNotRecorded { generation, .. } => *generation,
        }
    }

    /// The inclusive position range the finding names.
    #[must_use]
    pub fn range(&self) -> (u64, u64) {
        match self {
            Self::MissingSegment {
                first_seq,
                last_seq,
                ..
            }
            | Self::UnlistedSegment {
                first_seq,
                last_seq,
                ..
            }
            | Self::HashMismatch {
                first_seq,
                last_seq,
                ..
            }
            | Self::PositionGap {
                first_seq,
                last_seq,
                ..
            }
            | Self::ForkNotRecorded {
                first_seq,
                last_seq,
                ..
            } => (*first_seq, *last_seq),
            Self::Reordered {
                first_seq,
                last_seq,
                ..
            } => (*first_seq, *last_seq),
        }
    }

    /// Whether the finding is evidence that someone changed or removed journal data.
    ///
    /// A missing, altered, moved or unaccounted-for segment is; an extra segment storage holds
    /// that the manifest has not caught up with is not (a crash between sealing and the manifest
    /// update leaves exactly that), and neither is a hole beyond the manifest's coverage, which is
    /// data that never reached a manifest.
    #[must_use]
    pub fn is_tampering(&self) -> bool {
        match self {
            Self::MissingSegment { .. }
            | Self::HashMismatch { .. }
            | Self::Reordered { .. }
            | Self::ForkNotRecorded { .. } => true,
            Self::UnlistedSegment { .. } | Self::PositionGap { .. } => false,
        }
    }
}

impl PartialOrd for Finding {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Finding {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

/// Compares a verified manifest with the segment summaries storage holds.
///
/// The manifest is authoritative for its generation: a listed range storage does not hold is a
/// [`Finding::MissingSegment`], a listed range whose segment hashes differently is a
/// [`Finding::HashMismatch`], and a segment whose hash is a listed segment's but whose range is
/// not is a [`Finding::Reordered`]. Anything else storage holds is a
/// [`Finding::UnlistedSegment`] (never tampering: it may be the ordinary crash between sealing and
/// the manifest update), except that a hole between the manifest's coverage and later data is a
/// [`Finding::PositionGap`], and any data of a generation the fork history does not account for is
/// a [`Finding::ForkNotRecorded`].
///
/// `observed` is not validated: a malformed summary is simply a summary the manifest does not
/// list. The findings come back sorted by generation, position and kind, without exact duplicates,
/// so the same inputs always produce the same report (a manifest that is fine yields an empty
/// `Vec`).
#[must_use]
pub fn check_against(verified: &Verified, observed: &[SegmentSummary]) -> Vec<Finding> {
    /// What the comparison found for one manifest entry.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Held {
        /// No segment at its position in storage.
        Missing,
        /// The segment storage holds is the one the manifest described.
        Same,
        /// A segment is there and hashes differently.
        Altered,
    }

    let manifest = verified.manifest();
    let generation = manifest.generation;
    let mut findings = Vec::new();
    let mut status = vec![Held::Missing; manifest.segments.len()];
    let mut extra: Vec<&SegmentSummary> = Vec::new();

    let mut listed_here: Vec<&SegmentSummary> = observed
        .iter()
        .filter(|segment| segment.generation == generation)
        .collect();
    listed_here.sort_by_key(|segment| (segment.first_seq, segment.last_seq));

    for segment in listed_here {
        let listed = manifest.segments.iter().position(|entry| {
            entry.first_seq == segment.first_seq && entry.last_seq == segment.last_seq
        });
        match listed {
            Some(index) => {
                let entry = &manifest.segments[index];
                if status[index] != Held::Missing {
                    // A second copy of a range the manifest lists once.
                    extra.push(segment);
                    continue;
                }
                let same = segment.segment_hash == entry.segment_hash
                    && segment.previous_hash == entry.previous_hash
                    && segment.entry_count == entry.entry_count;
                status[index] = if same { Held::Same } else { Held::Altered };
            }
            None => {
                if manifest
                    .segments
                    .iter()
                    .any(|entry| entry.segment_hash == segment.segment_hash)
                {
                    findings.push(Finding::Reordered {
                        generation,
                        first_seq: segment.first_seq,
                        last_seq: segment.last_seq,
                    });
                } else {
                    extra.push(segment);
                }
            }
        }
    }

    for (index, entry) in manifest.segments.iter().enumerate() {
        match status[index] {
            Held::Missing => findings.push(Finding::MissingSegment {
                generation,
                first_seq: entry.first_seq,
                last_seq: entry.last_seq,
            }),
            Held::Altered => findings.push(Finding::HashMismatch {
                generation,
                first_seq: entry.first_seq,
                last_seq: entry.last_seq,
            }),
            Held::Same => {}
        }
    }

    for segment in &extra {
        findings.push(Finding::UnlistedSegment {
            generation,
            first_seq: segment.first_seq,
            last_seq: segment.last_seq,
        });
    }

    // Holes: storage continues after the manifest's coverage only if nothing is missing in
    // between. Sealed segments tile their generation, so a later segment with a hole before it
    // means the segments in the hole are gone.
    let mut cursor = manifest.last_seq();
    let mut positions: Vec<(u64, u64)> = observed
        .iter()
        .filter(|segment| segment.generation == generation)
        .map(|segment| (segment.first_seq, segment.last_seq))
        .collect();
    positions.sort_unstable();
    for (first, last) in positions {
        if first > cursor {
            if first > cursor.saturating_add(1) {
                findings.push(Finding::PositionGap {
                    generation,
                    first_seq: cursor.saturating_add(1),
                    last_seq: first.saturating_sub(1),
                });
            }
            cursor = last.max(cursor);
        }
    }

    // Generations: the manifest's own is compared above; every other generation storage holds is
    // either a newer one no fork opened, or an abandoned one whose fork point it has run past.
    for segment in observed
        .iter()
        .filter(|segment| segment.generation != generation)
    {
        let accounted = segment.generation < generation
            && manifest
                .forks
                .iter()
                .find(|fork| fork.generation == segment.generation.saturating_add(1))
                .is_some_and(|fork| segment.first_seq <= fork.forked_from_position);
        if !accounted {
            findings.push(Finding::ForkNotRecorded {
                generation: segment.generation,
                first_seq: segment.first_seq,
                last_seq: segment.last_seq,
            });
        }
    }

    findings.sort_unstable();
    findings.dedup();
    findings
}
