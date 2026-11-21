//! What a verification reports: findings that name the tenant, generation and position range.

use std::cmp::Ordering;

use spot_journal_manifest::Finding;

/// What is wrong, in the vocabulary of the manifest crate where it fits.
///
/// The `Ord` order is only the report's tie-break.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FindingKind {
    /// The manifest of the generation asked about is not in the store.
    MissingManifest,
    /// The manifest bytes do not decode, are not signed by a key trusted for the generation, or
    /// break a manifest invariant. `reason` is the `journal_manifest.*` code of the refusal.
    ManifestRejected {
        /// The code of the manifest crate's refusal.
        reason: &'static str,
    },
    /// The manifest is authentic but belongs to another source, or to another generation than the
    /// key it was found under (a manifest copied from another prefix, or a stale file).
    ForeignManifest,
    /// A segment's header names another source than the one expected.
    ForeignSegment,
    /// The manifest lists a segment and the store does not hold it.
    MissingSegment,
    /// The store holds a segment the manifest does not list (an anomaly, not an error).
    UnlistedSegment,
    /// A listed segment's bytes hash to something else than the manifest says.
    HashMismatch,
    /// Positions between the manifest's coverage and later stored segments are held by nobody.
    PositionGap,
    /// A listed segment sits at another position in the store.
    Reordered,
    /// The store holds a generation, or positions, the fork history does not account for.
    ForkNotRecorded,
    /// A segment cannot be opened or a block checksum fails.
    SegmentCorrupt,
    /// A segment's header does not link to the segment the manifest lists before it.
    ChainBroken,
    /// A segment's header disagrees with the manifest's summary of it.
    HeaderMismatch,
    /// The manifest no longer lists what an earlier verified pass saw (fewer segments, or earlier
    /// segments changed): a rolled-back or rewritten manifest.
    ManifestRegressed,
    /// The store holds a manifest of a higher generation than the pinned one. The pin is stale,
    /// which is information for the control plane, not tampering. The finding names the newest
    /// generation present.
    NewerGenerationPresent,
}

impl FindingKind {
    /// The short stable name (`missing_segment`, `foreign_manifest`, ...).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::MissingManifest => "missing_manifest",
            Self::ManifestRejected { .. } => "manifest_rejected",
            Self::ForeignManifest => "foreign_manifest",
            Self::ForeignSegment => "foreign_segment",
            Self::MissingSegment => "missing_segment",
            Self::UnlistedSegment => "unlisted_segment",
            Self::HashMismatch => "hash_mismatch",
            Self::PositionGap => "position_gap",
            Self::Reordered => "reordered",
            Self::ForkNotRecorded => "fork_not_recorded",
            Self::SegmentCorrupt => "segment_corrupt",
            Self::ChainBroken => "chain_broken",
            Self::HeaderMismatch => "header_mismatch",
            Self::ManifestRegressed => "manifest_regressed",
            Self::NewerGenerationPresent => "newer_generation_present",
        }
    }

    /// The stable catalogue code (`journal_verify.<name>`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingManifest => "journal_verify.missing_manifest",
            Self::ManifestRejected { .. } => "journal_verify.manifest_rejected",
            Self::ForeignManifest => "journal_verify.foreign_manifest",
            Self::ForeignSegment => "journal_verify.foreign_segment",
            Self::MissingSegment => "journal_verify.missing_segment",
            Self::UnlistedSegment => "journal_verify.unlisted_segment",
            Self::HashMismatch => "journal_verify.hash_mismatch",
            Self::PositionGap => "journal_verify.position_gap",
            Self::Reordered => "journal_verify.reordered",
            Self::ForkNotRecorded => "journal_verify.fork_not_recorded",
            Self::SegmentCorrupt => "journal_verify.segment_corrupt",
            Self::ChainBroken => "journal_verify.chain_broken",
            Self::HeaderMismatch => "journal_verify.header_mismatch",
            Self::ManifestRegressed => "journal_verify.manifest_regressed",
            Self::NewerGenerationPresent => "journal_verify.newer_generation_present",
        }
    }

    /// Whether the kind is evidence that someone changed or removed journal data.
    ///
    /// Everything is, except an unlisted segment (the ordinary crash between sealing and the
    /// manifest update), a position gap beyond the manifest's coverage (data that never reached
    /// a manifest), as in the manifest crate, and a newer generation than the pinned one (a stale
    /// pin).
    #[must_use]
    pub fn is_tampering(&self) -> bool {
        !matches!(
            self,
            Self::UnlistedSegment | Self::PositionGap | Self::NewerGenerationPresent
        )
    }
}

/// One thing wrong with a tenant's journal.
///
/// A finding carries ids, positions and codes only; it never carries object bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VerifyFinding {
    /// The tenant the verification was about.
    pub tenant: String,
    /// The generation the finding is about (`0` when no manifest was found to name one).
    pub generation: u64,
    /// The first position of the range the finding names (`0` when it names none).
    pub first_seq: u64,
    /// The last position of the range, inclusive (`0` when it names none).
    pub last_seq: u64,
    /// What is wrong.
    pub kind: FindingKind,
}

impl VerifyFinding {
    /// Whether the finding is evidence of tampering or loss; see [`FindingKind::is_tampering`].
    #[must_use]
    pub fn is_tampering(&self) -> bool {
        self.kind.is_tampering()
    }

    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// Whether the finding makes the segment at `first_seq..=last_seq` of `generation` untrusted.
    pub(crate) fn touches(&self, generation: u64, first_seq: u64, last_seq: u64) -> bool {
        self.generation == generation
            && self.kind.is_tampering()
            && !matches!(self.kind, FindingKind::ManifestRegressed)
            && self.first_seq <= last_seq
            && first_seq <= self.last_seq
    }
}

impl PartialOrd for VerifyFinding {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VerifyFinding {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.generation, self.first_seq, self.last_seq, self.kind).cmp(&(
            other.generation,
            other.first_seq,
            other.last_seq,
            other.kind,
        ))
    }
}

/// Maps a manifest crate finding into this crate's vocabulary.
pub(crate) fn from_manifest(tenant: &str, finding: &Finding) -> VerifyFinding {
    let (first_seq, last_seq) = finding.range();
    let kind = match finding {
        Finding::MissingSegment { .. } => FindingKind::MissingSegment,
        Finding::UnlistedSegment { .. } => FindingKind::UnlistedSegment,
        Finding::HashMismatch { .. } => FindingKind::HashMismatch,
        Finding::PositionGap { .. } => FindingKind::PositionGap,
        Finding::Reordered { .. } => FindingKind::Reordered,
        Finding::ForkNotRecorded { .. } => FindingKind::ForkNotRecorded,
    };
    VerifyFinding {
        tenant: tenant.to_owned(),
        generation: finding.generation(),
        first_seq,
        last_seq,
        kind,
    }
}
