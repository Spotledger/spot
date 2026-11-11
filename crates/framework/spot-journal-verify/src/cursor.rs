//! The incremental cursor: what an earlier pass verified, so the next one reads only new segments.

use serde::{Deserialize, Serialize};
use spot_journal_manifest::SegmentSummary;
use spot_journal_segment::segment_hash;

use crate::report::{Expected, Mode};

/// The digest a fresh cursor starts from.
fn origin() -> [u8; 32] {
    segment_hash(b"spot-journal-verify cursor listing v1")
}

/// Folds one segment summary into the running digest.
fn fold(previous: [u8; 32], segment: &SegmentSummary) -> [u8; 32] {
    let mut buffer = Vec::with_capacity(32 + 8 * 4 + 64);
    buffer.extend_from_slice(&previous);
    buffer.extend_from_slice(&segment.generation.to_be_bytes());
    buffer.extend_from_slice(&segment.first_seq.to_be_bytes());
    buffer.extend_from_slice(&segment.last_seq.to_be_bytes());
    buffer.extend_from_slice(&segment.entry_count.to_be_bytes());
    buffer.extend_from_slice(&segment.segment_hash);
    buffer.extend_from_slice(&segment.previous_hash);
    segment_hash(&buffer)
}

/// The digest of an ordered run of segment summaries.
pub(crate) fn digest(segments: &[SegmentSummary]) -> [u8; 32] {
    segments.iter().fold(origin(), fold)
}

/// What an earlier pass verified: a clean prefix of one generation's manifest.
///
/// The cursor remembers how many leading segments of the manifest were verified, the position and
/// hash of the last of them, and a digest of the whole verified listing. A later pass skips those
/// segments only when the manifest it now reads still begins with exactly the listing the digest
/// covers; any change to what was already verified (a rolled-back manifest, a rewritten entry)
/// voids the cursor and the pass reads everything again. A cursor from a [`Mode::Light`] pass does
/// not excuse a [`Mode::Deep`] one.
///
/// It serialises, so a scheduler can keep it between runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyCursor {
    pub(crate) tenant: String,
    pub(crate) source: String,
    pub(crate) generation: u64,
    pub(crate) mode: Mode,
    pub(crate) verified_segments: usize,
    pub(crate) last_seq: u64,
    pub(crate) last_hash: [u8; 32],
    pub(crate) listing_digest: [u8; 32],
}

impl VerifyCursor {
    /// A cursor that has verified nothing.
    #[must_use]
    pub fn empty(expected: &Expected, generation: u64, mode: Mode) -> Self {
        Self {
            tenant: expected.tenant.as_str().to_owned(),
            source: expected.source.clone(),
            generation,
            mode,
            verified_segments: 0,
            last_seq: 0,
            last_hash: [0; 32],
            listing_digest: origin(),
        }
    }

    /// The cursor after `prefix` (the manifest's leading, verified segments).
    pub(crate) fn after(
        expected: &Expected,
        generation: u64,
        mode: Mode,
        prefix: &[SegmentSummary],
    ) -> Self {
        let mut cursor = Self::empty(expected, generation, mode);
        cursor.verified_segments = prefix.len();
        if let Some(last) = prefix.last() {
            cursor.last_seq = last.last_seq;
            cursor.last_hash = last.segment_hash;
        }
        cursor.listing_digest = digest(prefix);
        cursor
    }

    /// How many leading segments of the manifest the cursor covers.
    #[must_use]
    pub fn verified_segments(&self) -> usize {
        self.verified_segments
    }

    /// The last position the cursor covers (`0` when it covers nothing).
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// The hash of the last segment the cursor covers.
    #[must_use]
    pub fn last_hash(&self) -> [u8; 32] {
        self.last_hash
    }

    /// The generation the cursor is about.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The weakest mode any covered segment was verified in.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether the cursor is about this tenant, source and generation.
    pub(crate) fn is_about(&self, expected: &Expected, generation: u64) -> bool {
        self.tenant == expected.tenant.as_str()
            && self.source == expected.source
            && self.generation == generation
    }

    /// Whether `segments` still begins with the listing the cursor verified.
    pub(crate) fn still_covers(&self, segments: &[SegmentSummary]) -> bool {
        segments
            .get(..self.verified_segments)
            .is_some_and(|prefix| digest(prefix) == self.listing_digest)
    }
}
