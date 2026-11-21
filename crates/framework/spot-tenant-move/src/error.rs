//! Typed errors with stable `tenant_move.*` codes (catalogued in
//! `spot-errors/catalogue/tenant_move.toml`).
//!
//! No message carries a secret, a credential, a connection string or raw SQL. A port's
//! failure is a fixed reason, chosen by the adapter, never free text from the backend.

use crate::codes;
use crate::plan::MoveState;

/// Anything a move can refuse to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The plan is malformed.
    #[error("the move plan is not valid: {reason}")]
    InvalidPlan {
        /// The fixed reason.
        reason: &'static str,
    },
    /// The move id is malformed.
    #[error("the move id is not valid: {reason}")]
    InvalidMoveId {
        /// The fixed reason.
        reason: &'static str,
    },
    /// No move is recorded for the tenant.
    #[error("no move for tenant {tenant} was found")]
    NotFound {
        /// The tenant id.
        tenant: String,
    },
    /// The tenant already has a move record.
    #[error("tenant {tenant} already has a move")]
    AlreadyExists {
        /// The tenant id.
        tenant: String,
    },
    /// The state machine does not allow the operation.
    #[error("the move of tenant {tenant} is {state}, which does not allow {operation}")]
    WrongState {
        /// The tenant id.
        tenant: String,
        /// The current state.
        state: MoveState,
        /// The refused operation.
        operation: &'static str,
    },
    /// The residency policy forbids the target region.
    #[error("the residency policy does not allow moving tenant {tenant} to region {region}")]
    ResidencyRefused {
        /// The tenant id.
        tenant: String,
        /// The refused region.
        region: String,
    },
    /// The admission gate or the side-effect hold failed.
    #[error("the move could not quiesce or resume tenant {tenant}: {reason}")]
    QuiesceFailed {
        /// The tenant id.
        tenant: String,
        /// The fixed reason.
        reason: &'static str,
    },
    /// The transfer port failed.
    #[error("the transfer of tenant {tenant} failed: {reason}")]
    TransferFailed {
        /// The tenant id.
        tenant: String,
        /// The fixed reason.
        reason: &'static str,
    },
    /// A chunk failed its checksum.
    #[error("chunk {sequence} of the move of tenant {tenant} failed its checksum")]
    ChunkCorrupt {
        /// The tenant id.
        tenant: String,
        /// The chunk's sequence number.
        sequence: u64,
    },
    /// The move store failed or its optimistic revision did not match.
    #[error("the move store failed for tenant {tenant}: {reason}")]
    StoreFailed {
        /// The tenant id.
        tenant: String,
        /// The fixed reason.
        reason: &'static str,
    },
    /// A fingerprint could not be measured.
    #[error("the fingerprint of tenant {tenant} could not be computed: {reason}")]
    FingerprintFailed {
        /// The tenant id.
        tenant: String,
        /// The fixed reason.
        reason: &'static str,
    },
    /// The source and target fingerprints differ.
    #[error(
        "the fingerprints of tenant {tenant} differ in {differences} place(s); the move halted before cut-over"
    )]
    FingerprintMismatch {
        /// The tenant id.
        tenant: String,
        /// How many rows or measures differ.
        differences: u64,
    },
    /// The placement flip or cache invalidation failed.
    #[error("the cut-over of tenant {tenant} failed: {reason}")]
    CutOverFailed {
        /// The tenant id.
        tenant: String,
        /// The fixed reason.
        reason: &'static str,
    },
    /// The move has already cut over.
    #[error("the move of tenant {tenant} has already cut over and cannot be aborted")]
    AbortTooLate {
        /// The tenant id.
        tenant: String,
    },
    /// A step returned without advancing the state machine.
    #[error("the move of tenant {tenant} made no progress at {state}")]
    StepFailed {
        /// The tenant id.
        tenant: String,
        /// The state that did not advance.
        state: MoveState,
    },
}

impl Error {
    /// The stable dotted code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPlan { .. } => codes::INVALID_PLAN,
            Self::InvalidMoveId { .. } => codes::INVALID_MOVE_ID,
            Self::NotFound { .. } => codes::NOT_FOUND,
            Self::AlreadyExists { .. } => codes::ALREADY_EXISTS,
            Self::WrongState { .. } => codes::WRONG_STATE,
            Self::ResidencyRefused { .. } => codes::RESIDENCY_REFUSED,
            Self::QuiesceFailed { .. } => codes::QUIESCE_FAILED,
            Self::TransferFailed { .. } => codes::TRANSFER_FAILED,
            Self::ChunkCorrupt { .. } => codes::CHUNK_CORRUPT,
            Self::StoreFailed { .. } => codes::STORE_FAILED,
            Self::FingerprintFailed { .. } => codes::FINGERPRINT_FAILED,
            Self::FingerprintMismatch { .. } => codes::FINGERPRINT_MISMATCH,
            Self::CutOverFailed { .. } => codes::CUTOVER_FAILED,
            Self::AbortTooLate { .. } => codes::ABORT_TOO_LATE,
            Self::StepFailed { .. } => codes::STEP_FAILED,
        }
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
