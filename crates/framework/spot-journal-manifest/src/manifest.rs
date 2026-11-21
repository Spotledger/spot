//! The manifest model: segment summaries, fork records, the manifest, its invariants and the
//! append-only growth rule.

use serde::{Deserialize, Serialize};

use crate::canonical::manifest_hash;
use crate::error::ManifestError;

/// What the manifest knows about one sealed segment: the range of positions it holds, how many
/// entries it carries, the hash of its bytes and the hash of the segment before it.
///
/// The summary is a pointer, not the segment: it never carries entries, and the manifest's
/// signature (see [`crate::SignedManifest`]) commits to these fields, so a segment whose bytes
/// hash to something else is detectable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SegmentSummary {
    /// The stream generation the segment belongs to.
    pub generation: u64,
    /// The first sequence number the segment holds.
    pub first_seq: u64,
    /// The last sequence number the segment holds, inclusive.
    pub last_seq: u64,
    /// How many entries the segment holds.
    pub entry_count: u64,
    /// `SHA-256` of the segment's bytes.
    pub segment_hash: [u8; 32],
    /// The hash of the segment before it in the same generation.
    pub previous_hash: [u8; 32],
}

impl SegmentSummary {
    /// A summary of the positions `first_seq..=last_seq` of `generation`.
    #[must_use]
    pub fn new(
        generation: u64,
        first_seq: u64,
        last_seq: u64,
        entry_count: u64,
        segment_hash: [u8; 32],
        previous_hash: [u8; 32],
    ) -> Self {
        Self {
            generation,
            first_seq,
            last_seq,
            entry_count,
            segment_hash,
            previous_hash,
        }
    }

    /// How many positions the range covers; `0` when it is reversed.
    #[must_use]
    pub fn positions(&self) -> u64 {
        match self.last_seq.checked_sub(self.first_seq) {
            Some(delta) => delta.saturating_add(1),
            None => 0,
        }
    }

    /// Checks the summary on its own: a generation from 1, a range from position 1 that is not
    /// reversed, and an entry count that fills the range exactly.
    ///
    /// # Errors
    /// [`ManifestError::GenerationInvalid`], [`ManifestError::SegmentRangeInvalid`].
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.generation == 0 {
            return Err(ManifestError::GenerationInvalid {
                generation: self.generation,
            });
        }
        let filled = self.first_seq >= 1
            && self.last_seq >= self.first_seq
            && self.entry_count == self.positions();
        if !filled {
            return Err(ManifestError::SegmentRangeInvalid {
                first_seq: self.first_seq,
                last_seq: self.last_seq,
            });
        }
        Ok(())
    }

    /// Whether the two summaries claim any position in common (same generation only).
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.first_seq <= other.last_seq
            && other.first_seq <= self.last_seq
    }
}

/// A record that a generation was abandoned at a position and a new one opened (ADR-061 s6: a
/// point-in-time rebuild forks history).
///
/// A manifest of generation `G` carries every fork record from generation 2 to `G`, so the whole
/// fork history travels with the newest manifest and an abandoned generation is never rewritten.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fork {
    /// The generation this fork opened (2 or later); the abandoned generation is the one before.
    pub generation: u64,
    /// The last sequence number of the abandoned generation that is still valid. Positions after
    /// it belong to history the rebuild discarded.
    pub forked_from_position: u64,
    /// Why the fork happened, in words a reviewer can act on (never a secret).
    pub reason: String,
    /// Unix milliseconds of the fork, supplied by the caller.
    pub at_millis: i64,
}

impl Fork {
    /// A fork record.
    #[must_use]
    pub fn new(
        generation: u64,
        forked_from_position: u64,
        reason: impl Into<String>,
        at_millis: i64,
    ) -> Self {
        Self {
            generation,
            forked_from_position,
            reason: reason.into(),
            at_millis,
        }
    }

    /// Checks the record on its own.
    ///
    /// # Errors
    /// [`ManifestError::ForkInvalid`] when the generation is not a later generation, the fork
    /// point is not a position, or the reason is empty.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.generation < 2 {
            return Err(ManifestError::ForkInvalid(
                "a fork opens a generation after the first",
            ));
        }
        if self.forked_from_position == 0 {
            return Err(ManifestError::ForkInvalid(
                "a fork point is a position, and positions start at 1",
            ));
        }
        if self.reason.trim().is_empty() {
            return Err(ManifestError::ForkInvalid("a fork record needs a reason"));
        }
        Ok(())
    }
}

/// The ordered list of one generation's sealed segments, the fork history up to that generation,
/// and the link to the manifest before it.
///
/// A manifest describes exactly one generation: every summary it lists carries
/// [`Manifest::generation`], the ranges are contiguous from position 1, each segment links to the
/// one before it, and the fork history opens every generation from 2 to the manifest's own. Those
/// invariants are what [`Manifest::validate`] checks and what [`crate::verify`] refuses to hand
/// back an unverified manifest for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The source (a tenant site, or `<site>#<partition>`) whose journal this is.
    pub source: String,
    /// The stream generation this manifest describes.
    pub generation: u64,
    /// The generation's segments, in position order.
    pub segments: Vec<SegmentSummary>,
    /// The fork records from generation 2 to [`Manifest::generation`], in order.
    pub forks: Vec<Fork>,
    /// The hash of the previous manifest of the source (`None` for the first generation).
    pub previous_manifest_hash: Option<[u8; 32]>,
    /// Unix milliseconds the manifest was issued, supplied by the caller.
    pub issued_at_millis: i64,
}

impl Manifest {
    /// The first manifest of a source: generation 1, no segments, no forks, no previous link.
    ///
    /// # Errors
    /// [`ManifestError::SourceInvalid`] when the source id is empty.
    pub fn first(source: &str, issued_at_millis: i64) -> Result<Self, ManifestError> {
        let manifest = Self {
            source: source.to_owned(),
            generation: 1,
            segments: Vec::new(),
            forks: Vec::new(),
            previous_manifest_hash: None,
            issued_at_millis,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// The last sequence number the manifest lists (`0` when it lists no segment).
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.segments.last().map_or(0, |segment| segment.last_seq)
    }

    /// The first sequence number the manifest expects a next segment to hold.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.last_seq().saturating_add(1)
    }

    /// The hash of this manifest's canonical bytes; the next manifest carries it as
    /// [`Manifest::previous_manifest_hash`].
    #[must_use]
    pub fn manifest_hash(&self) -> [u8; 32] {
        manifest_hash(self)
    }

    /// Checks every invariant of the manifest.
    ///
    /// # Errors
    /// [`ManifestError::SourceInvalid`], [`ManifestError::GenerationInvalid`],
    /// [`ManifestError::PreviousManifestInvalid`], [`ManifestError::SegmentGeneration`],
    /// [`ManifestError::SegmentRangeInvalid`], [`ManifestError::DuplicateSegment`],
    /// [`ManifestError::ManifestGap`], [`ManifestError::HistoryRewritten`],
    /// [`ManifestError::ForkInvalid`].
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.source.trim().is_empty() {
            return Err(ManifestError::SourceInvalid);
        }
        if self.generation == 0 {
            return Err(ManifestError::GenerationInvalid {
                generation: self.generation,
            });
        }
        match (self.generation, self.previous_manifest_hash) {
            (1, None) => {}
            (1, Some(_)) | (_, None) => return Err(ManifestError::PreviousManifestInvalid),
            (_, Some(_)) => {}
        }

        let mut previous: Option<&SegmentSummary> = None;
        for segment in &self.segments {
            segment.validate()?;
            if segment.generation != self.generation {
                return Err(ManifestError::SegmentGeneration {
                    expected: self.generation,
                    offered: segment.generation,
                });
            }
            match previous {
                None => {
                    if segment.first_seq != 1 {
                        return Err(ManifestError::ManifestGap {
                            first_seq: 1,
                            last_seq: segment.first_seq.saturating_sub(1),
                        });
                    }
                }
                Some(before) => {
                    if segment.first_seq <= before.last_seq {
                        return Err(ManifestError::DuplicateSegment {
                            first_seq: segment.first_seq,
                            last_seq: segment.last_seq,
                        });
                    }
                    if segment.first_seq != before.last_seq.saturating_add(1) {
                        return Err(ManifestError::ManifestGap {
                            first_seq: before.last_seq.saturating_add(1),
                            last_seq: segment.first_seq.saturating_sub(1),
                        });
                    }
                    if segment.previous_hash != before.segment_hash {
                        return Err(ManifestError::HistoryRewritten {
                            first_seq: segment.first_seq,
                        });
                    }
                }
            }
            previous = Some(segment);
        }

        let mut expected = 2_u64;
        for fork in &self.forks {
            fork.validate()?;
            if fork.generation != expected {
                return Err(ManifestError::ForkInvalid(
                    "the fork history opens every generation from 2 to the manifest's own, once, in order",
                ));
            }
            expected = expected.saturating_add(1);
        }
        if expected != self.generation.saturating_add(1) {
            return Err(ManifestError::ForkInvalid(
                "the fork history does not reach the manifest generation",
            ));
        }
        Ok(())
    }

    /// The manifest of the next generation, forked at `forked_from_position` of this one.
    ///
    /// The new manifest lists no segment: the segments of the abandoned generation stay in the
    /// abandoned manifest, which this one links to by [`Manifest::previous_manifest_hash`], and
    /// the new generation grows from position 1 through [`append_segments`].
    ///
    /// # Errors
    /// Anything [`Manifest::validate`] reports for `self`, plus
    /// [`ManifestError::ForkInvalid`] when the position is not one this generation lists, and
    /// [`ManifestError::GenerationInvalid`] when the generation cannot advance.
    pub fn fork(
        &self,
        forked_from_position: u64,
        reason: &str,
        at_millis: i64,
        issued_at_millis: i64,
    ) -> Result<Self, ManifestError> {
        self.validate()?;
        if forked_from_position == 0 || forked_from_position > self.last_seq() {
            return Err(ManifestError::ForkInvalid(
                "the fork point is not a position this generation lists",
            ));
        }
        let generation =
            self.generation
                .checked_add(1)
                .ok_or(ManifestError::GenerationInvalid {
                    generation: self.generation,
                })?;
        let mut forks = self.forks.clone();
        let fork = Fork::new(generation, forked_from_position, reason, at_millis);
        fork.validate()?;
        forks.push(fork);
        let next = Self {
            source: self.source.clone(),
            generation,
            segments: Vec::new(),
            forks,
            previous_manifest_hash: Some(self.manifest_hash()),
            issued_at_millis,
        };
        next.validate()?;
        Ok(next)
    }

    /// Grows this manifest by `new` segments; see [`append_segments`].
    ///
    /// # Errors
    /// Whatever [`append_segments`] reports.
    pub fn append_segments(&self, new: &[SegmentSummary]) -> Result<Self, ManifestError> {
        append_segments(self, new)
    }
}

/// The only way a manifest grows: appends `new` to the end of `prev`, keeping every earlier
/// summary exactly as it was.
///
/// It refuses a gap (the first new segment must start at [`Manifest::next_seq`]), an overlap (a
/// position already listed), a segment of another generation (a new generation is opened by
/// [`Manifest::fork`], which records the fork, never by appending), and a changed earlier summary
/// (a new segment whose `previous_hash` disagrees with the hash of the segment before it would
/// rewrite the chain).
///
/// The result carries `prev`'s issuance: it keeps `prev.issued_at_millis` and
/// `prev.previous_manifest_hash`, because appending is a pure structural step and this crate reads
/// no clock. The caller sets `issued_at_millis` and signs when it publishes a new manifest
/// version; the previous-manifest link stays the one of the generation.
///
/// # Errors
/// Anything [`Manifest::validate`] reports for `prev`, plus [`ManifestError::AppendGap`],
/// [`ManifestError::AppendOverlap`], [`ManifestError::GenerationChange`] and
/// [`ManifestError::HistoryRewritten`].
pub fn append_segments(prev: &Manifest, new: &[SegmentSummary]) -> Result<Manifest, ManifestError> {
    prev.validate()?;
    if new.is_empty() {
        return Ok(prev.clone());
    }
    let mut segments = prev.segments.clone();
    let mut expected = prev.next_seq();
    for segment in new {
        segment.validate()?;
        if segment.generation != prev.generation {
            return Err(ManifestError::GenerationChange {
                expected: prev.generation,
                offered: segment.generation,
            });
        }
        if segment.first_seq < expected {
            return Err(ManifestError::AppendOverlap {
                expected,
                offered: segment.first_seq,
            });
        }
        if segment.first_seq > expected {
            return Err(ManifestError::AppendGap {
                expected,
                offered: segment.first_seq,
            });
        }
        if let Some(before) = segments.last()
            && segment.previous_hash != before.segment_hash
        {
            return Err(ManifestError::HistoryRewritten {
                first_seq: segment.first_seq,
            });
        }
        expected = segment.last_seq.saturating_add(1);
        segments.push(*segment);
    }
    Ok(Manifest {
        source: prev.source.clone(),
        generation: prev.generation,
        segments,
        forks: prev.forks.clone(),
        previous_manifest_hash: prev.previous_manifest_hash,
        issued_at_millis: prev.issued_at_millis,
    })
}
