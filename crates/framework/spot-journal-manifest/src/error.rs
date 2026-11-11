//! Typed errors with stable `journal_manifest.*` codes. A message never carries key material or
//! a signature's bytes.

/// Every error the manifest model, its signature, its verifier and its findings can return.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    /// The source id of a manifest is empty.
    #[error("the manifest source id is empty")]
    SourceInvalid,
    /// A generation is zero, and generations start at one.
    #[error("generation {generation} is not a generation: they start at 1")]
    GenerationInvalid {
        /// The generation offered.
        generation: u64,
    },
    /// A segment in the manifest belongs to another generation than the manifest.
    #[error("segment generation {offered} is not the manifest generation {expected}")]
    SegmentGeneration {
        /// The generation of the manifest.
        expected: u64,
        /// The generation of the segment.
        offered: u64,
    },
    /// A segment's position range is empty or reversed, or its entry count does not fill it.
    #[error(
        "the segment range {first_seq}..{last_seq} is reversed, empty, or does not match its entry count"
    )]
    SegmentRangeInvalid {
        /// The first sequence number of the segment.
        first_seq: u64,
        /// The last sequence number of the segment.
        last_seq: u64,
    },
    /// An appended segment skips positions.
    #[error("appending a segment at {offered} skips the positions from {expected}")]
    AppendGap {
        /// The first sequence number the manifest expects next.
        expected: u64,
        /// The first sequence number of the segment offered.
        offered: u64,
    },
    /// An appended segment repeats positions the manifest already lists.
    #[error(
        "appending a segment at {offered} overlaps the positions already listed up to {expected}"
    )]
    AppendOverlap {
        /// The first sequence number the manifest expects next.
        expected: u64,
        /// The first sequence number of the segment offered.
        offered: u64,
    },
    /// A segment no longer links to the segment before it, so history would be rewritten.
    #[error("the segment at {first_seq} no longer links to the segment before it")]
    HistoryRewritten {
        /// The first sequence number of the segment whose link disagrees.
        first_seq: u64,
    },
    /// A segment of another generation was offered without a fork record.
    #[error(
        "a segment of generation {offered} cannot be appended to a manifest of generation {expected} without a fork record"
    )]
    GenerationChange {
        /// The generation of the manifest.
        expected: u64,
        /// The generation of the segment offered.
        offered: u64,
    },
    /// A fork record is malformed or does not fit the manifest's fork history.
    #[error("the fork record is invalid: {0}")]
    ForkInvalid(&'static str),
    /// The manifest's segments leave positions unlisted.
    #[error("the manifest leaves the positions {first_seq}..{last_seq} unlisted")]
    ManifestGap {
        /// The first missing sequence number.
        first_seq: u64,
        /// The last missing sequence number.
        last_seq: u64,
    },
    /// The manifest lists the same positions twice.
    #[error("the manifest lists the positions {first_seq}..{last_seq} twice")]
    DuplicateSegment {
        /// The first sequence number of the repeated range.
        first_seq: u64,
        /// The last sequence number of the repeated range.
        last_seq: u64,
    },
    /// The previous-manifest link contradicts the manifest's generation.
    #[error(
        "the previous manifest hash does not match the generation: a first-generation manifest has none and every later one has a link"
    )]
    PreviousManifestInvalid,
    /// The bytes are not the canonical encoding of a signed manifest.
    #[error("the bytes are not a canonical signed manifest: {0}")]
    FormatInvalid(&'static str),
    /// The signature does not verify under the key it names.
    #[error("the manifest signature does not verify")]
    SignatureInvalid,
    /// The verifying key set does not know the key the manifest names.
    #[error("the verifying key set has no key with id {0}")]
    KeyUnknown(String),
    /// The key is known but not valid for the manifest's generation.
    #[error("key {key_id} is not valid for manifest generation {generation}")]
    KeyNotValidForGeneration {
        /// The id of the key.
        key_id: String,
        /// The manifest generation asked about.
        generation: u64,
    },
    /// A key window is empty, or it overlaps another window of the same key id.
    #[error("the key window is invalid: {0}")]
    KeyWindowInvalid(&'static str),
    /// The bytes are not an Ed25519 public key.
    #[error("the bytes are not an Ed25519 public key")]
    KeyInvalid,
}

impl ManifestError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::SourceInvalid => "journal_manifest.source_invalid",
            Self::GenerationInvalid { .. } => "journal_manifest.generation_invalid",
            Self::SegmentGeneration { .. } => "journal_manifest.segment_generation",
            Self::SegmentRangeInvalid { .. } => "journal_manifest.segment_range_invalid",
            Self::AppendGap { .. } => "journal_manifest.append_gap",
            Self::AppendOverlap { .. } => "journal_manifest.append_overlap",
            Self::HistoryRewritten { .. } => "journal_manifest.history_rewritten",
            Self::GenerationChange { .. } => "journal_manifest.generation_change",
            Self::ForkInvalid(_) => "journal_manifest.fork_invalid",
            Self::ManifestGap { .. } => "journal_manifest.manifest_gap",
            Self::DuplicateSegment { .. } => "journal_manifest.duplicate_segment",
            Self::PreviousManifestInvalid => "journal_manifest.previous_manifest_invalid",
            Self::FormatInvalid(_) => "journal_manifest.format_invalid",
            Self::SignatureInvalid => "journal_manifest.signature_invalid",
            Self::KeyUnknown(_) => "journal_manifest.key_unknown",
            Self::KeyNotValidForGeneration { .. } => {
                "journal_manifest.key_not_valid_for_generation"
            }
            Self::KeyWindowInvalid(_) => "journal_manifest.key_window_invalid",
            Self::KeyInvalid => "journal_manifest.key_invalid",
        }
    }
}
