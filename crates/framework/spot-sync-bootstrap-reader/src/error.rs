//! The errors of opening a [`crate::ReaderSource`], with stable `sync_bootstrap_reader.*` codes.
//! A message names a generation, a position or a tenant mismatch, never an image or an id.

/// Why a snapshot could not be opened as a source.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    /// The `SnapshotInfo` the caller holds disagrees with the verified index about the snapshot's
    /// position or generation.
    #[error(
        "the snapshot info says generation {info_generation} position {info_position} but the index says generation {index_generation} position {index_position}"
    )]
    SnapshotMismatch {
        /// The generation in the `SnapshotInfo`.
        info_generation: u64,
        /// The position in the `SnapshotInfo`.
        info_position: u64,
        /// The generation in the verified index.
        index_generation: u64,
        /// The position in the verified index.
        index_position: u64,
    },
    /// The index belongs to another tenant than the one the caller expects.
    #[error("the snapshot index belongs to another tenant than the one expected")]
    TenantMismatch,
}

impl OpenError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::SnapshotMismatch { .. } => "sync_bootstrap_reader.snapshot_mismatch",
            Self::TenantMismatch => "sync_bootstrap_reader.tenant_mismatch",
        }
    }
}
