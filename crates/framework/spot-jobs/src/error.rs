//! Job engine errors.

use spot_storage_contract::error::StorageError;
use thiserror::Error;

/// Result type for job engine operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors from the job engine.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Error {
    /// Job name is invalid (not in `app:job` format).
    #[error("job.invalid_name")]
    InvalidName,

    /// Queue name is not recognized.
    #[error("job.unknown_queue")]
    UnknownQueue,

    /// A claim is no longer valid (fencing); the job may be claimed by another worker.
    #[error("job.claim_lost")]
    ClaimLost,

    /// The worker was lost and the lease expired.
    #[error("job.worker_lost")]
    WorkerLost,

    /// The job handler panicked.
    #[error("job.handler_panicked")]
    HandlerPanicked,

    /// The job exceeded its timeout.
    #[error("job.timeout")]
    Timeout,

    /// A merged job context would exceed the size or key limit; nothing was written. The
    /// error carries none of the content.
    #[error("job.context_too_large")]
    ContextTooLarge,

    /// The intake needs to resync with the outbox feed.
    #[error("job.intake_resync")]
    IntakeResync,

    /// A generic error message.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Create an error with a message.
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }
}

impl From<StorageError> for Error {
    fn from(err: StorageError) -> Self {
        if err.kind == spot_storage_contract::error::ErrorKind::ResyncRequired {
            Error::IntakeResync
        } else {
            Error::Other(err.explanation)
        }
    }
}
