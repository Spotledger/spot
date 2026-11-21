//! Typed errors with stable `journal_snapshot.*` codes. Messages never carry image content.

use spot_journal_store::StoreError;

/// Every refusal the snapshot writer, reader and lister return.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    /// A writer parameter (tenant, part size, manifest hash) is not usable.
    #[error("the snapshot parameters are not usable: {0}")]
    ParamsInvalid(&'static str),
    /// An image has an empty entity or id, or cannot be encoded.
    #[error("the image at ordinal {ordinal} is not a valid image record")]
    ImageInvalid {
        /// The 1-based ordinal of the image within the batch offered to the writer.
        ordinal: u64,
    },
    /// The input is not strictly ascending by `(entity, id)`.
    #[error("the image at ordinal {ordinal} is out of order or repeats an earlier one")]
    InputNotSorted {
        /// The 1-based ordinal of the first offending image within the batch.
        ordinal: u64,
    },
    /// The writer was given an expected count and the images written differ.
    #[error("the snapshot holds {actual} images but {expected} were expected")]
    CountMismatch {
        /// The count the database reported.
        expected: u64,
        /// The images the writer received.
        actual: u64,
    },
    /// The snapshot has no index object: it is incomplete (or does not exist).
    #[error("the snapshot has no index")]
    IndexMissing,
    /// The index cannot be parsed or contradicts itself.
    #[error("the snapshot index is not valid: {0}")]
    IndexInvalid(&'static str),
    /// The index names another tenant, generation or position than the prefix it was found in.
    #[error("the snapshot index belongs to another tenant, generation or position")]
    IndexMisplaced,
    /// The signature does not verify under the supplied keys.
    #[error("the snapshot signature does not verify")]
    SignatureInvalid,
    /// A part is missing, altered, undecodable or disagrees with the index.
    #[error("the snapshot part {part} is invalid: {reason}")]
    PartInvalid {
        /// The part's object name.
        part: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The key is already taken by different bytes.
    #[error("the snapshot object {key} already exists with different content")]
    Exists {
        /// The object key.
        key: String,
    },
    /// The part transform failed.
    #[error("the part transform failed: {0}")]
    TransformFailed(String),
    /// The object store refused or failed.
    #[error("the object store failed: {0}")]
    Store(#[from] StoreError),
}

impl SnapshotError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ParamsInvalid(_) => "journal_snapshot.params_invalid",
            Self::ImageInvalid { .. } => "journal_snapshot.image_invalid",
            Self::InputNotSorted { .. } => "journal_snapshot.input_not_sorted",
            Self::CountMismatch { .. } => "journal_snapshot.count_mismatch",
            Self::IndexMissing => "journal_snapshot.index_missing",
            Self::IndexInvalid(_) => "journal_snapshot.index_invalid",
            Self::IndexMisplaced => "journal_snapshot.index_misplaced",
            Self::SignatureInvalid => "journal_snapshot.signature_invalid",
            Self::PartInvalid { .. } => "journal_snapshot.part_invalid",
            Self::Exists { .. } => "journal_snapshot.exists",
            Self::TransformFailed(_) => "journal_snapshot.transform_failed",
            Self::Store(_) => "journal_snapshot.store_failed",
        }
    }

    /// Shorthand for a part refusal.
    pub(crate) fn part(part: &str, reason: &'static str) -> Self {
        Self::PartInvalid {
            part: part.to_owned(),
            reason,
        }
    }
}
