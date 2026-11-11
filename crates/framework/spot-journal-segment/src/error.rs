//! Typed errors with stable `journal_segment.*` codes. Messages never carry an entry line.

/// Every error the writer and reader return.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SegmentError {
    /// The writer configuration is unusable.
    #[error("the segment configuration is invalid: {0}")]
    ConfigInvalid(&'static str),
    /// An entry line is not one line of valid UTF-8, or is too large.
    #[error("the entry line is invalid: {0}")]
    EntryInvalid(&'static str),
    /// A position is not strictly after the previous one (out of order or a duplicate).
    #[error("position {generation}/{seq} is not after the previous position")]
    PositionOutOfOrder {
        /// The offered generation.
        generation: u64,
        /// The offered sequence number.
        seq: u64,
    },
    /// A position skips a sequence number inside the segment.
    #[error("position {generation}/{seq} leaves a gap after the previous position")]
    PositionGap {
        /// The offered generation.
        generation: u64,
        /// The offered sequence number.
        seq: u64,
    },
    /// A position is not in the generation of the segment.
    #[error("position generation {offered} is not the segment generation {expected}")]
    GenerationMismatch {
        /// The generation of the segment.
        expected: u64,
        /// The generation offered.
        offered: u64,
    },
    /// The bytes are not a well-formed segment.
    #[error("the segment is malformed: {0}")]
    FormatInvalid(&'static str),
    /// The segment has a format version this build does not read.
    #[error("segment format version {0} is not supported")]
    UnsupportedVersion(u16),
    /// A checksum does not match.
    #[error("the {0} checksum does not match")]
    ChecksumMismatch(&'static str),
    /// A block could not be compressed or decompressed.
    #[error("block compression failed: {0}")]
    Compression(&'static str),
    /// A range with `first` after `last`.
    #[error("the requested range is empty or reversed")]
    RangeInvalid,
}

impl SegmentError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid(_) => "journal_segment.config_invalid",
            Self::EntryInvalid(_) => "journal_segment.entry_invalid",
            Self::PositionOutOfOrder { .. } => "journal_segment.position_out_of_order",
            Self::PositionGap { .. } => "journal_segment.position_gap",
            Self::GenerationMismatch { .. } => "journal_segment.generation_mismatch",
            Self::FormatInvalid(_) => "journal_segment.format_invalid",
            Self::UnsupportedVersion(_) => "journal_segment.unsupported_version",
            Self::ChecksumMismatch(_) => "journal_segment.checksum_mismatch",
            Self::Compression(_) => "journal_segment.compression",
            Self::RangeInvalid => "journal_segment.range_invalid",
        }
    }
}

/// Why a chain of segments does not verify, naming the first bad segment by its index in the
/// slice given to the verifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    /// A segment fails its own checks, or the segment after it was chained to different bytes.
    #[error("segment {index} was altered: {reason}")]
    Altered {
        /// Index of the bad segment.
        index: usize,
        /// What failed.
        reason: String,
    },
    /// One or more segments are missing before this one (or before the first, when the chain
    /// does not start at the genesis or the given anchor).
    #[error("one or more segments are missing before segment {index}")]
    Missing {
        /// Index of the segment after the hole.
        index: usize,
    },
    /// The segment belongs later in the chain.
    #[error("segment {index} is out of order")]
    Reordered {
        /// Index of the segment that is out of place.
        index: usize,
    },
    /// Positions leave a gap or overlap between this segment and the one before.
    #[error("positions of segment {index} do not continue the previous segment")]
    PositionGap {
        /// Index of the segment.
        index: usize,
    },
    /// The generation goes backwards.
    #[error("segment {index} has a generation lower than the previous segment")]
    GenerationMismatch {
        /// Index of the segment.
        index: usize,
    },
    /// The source id differs from the previous segment.
    #[error("segment {index} comes from a different source")]
    SourceMismatch {
        /// Index of the segment.
        index: usize,
    },
}

impl ChainError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Altered { .. } => "journal_segment.chain_altered",
            Self::Missing { .. } => "journal_segment.chain_missing",
            Self::Reordered { .. } => "journal_segment.chain_reordered",
            Self::PositionGap { .. } => "journal_segment.chain_position_gap",
            Self::GenerationMismatch { .. } => "journal_segment.chain_generation_mismatch",
            Self::SourceMismatch { .. } => "journal_segment.chain_source_mismatch",
        }
    }
}
