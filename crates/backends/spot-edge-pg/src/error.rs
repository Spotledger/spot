//! Typed errors for `spot-edge-pg`: stable `edge_pg.*` codes.
//!
//! Every code is catalogued in
//! `crates/framework/spot-errors/catalogue/edge_pg.toml`; callers match on [`EdgePgError::code`],
//! never on the message. The [`EdgeWriter`](spot_edge_sync::EdgeWriter) implementation maps a
//! failure onto [`EdgeSyncError::WriteFailed`] so the document operation that shares its
//! transaction fails with a code the engine and the catalogue already know.

use spot_app_api::{AppError, ErrorClass};
use spot_edge_sync::EdgeSyncError;

/// What can go wrong running the edge store on PostgreSQL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EdgePgError {
    /// The pool or the database is unreachable.
    #[error("the edge store is unavailable: {message}")]
    Unavailable {
        /// What the driver reported, without a raw SQL string or a secret.
        message: String,
    },

    /// A statement against the edge tables failed.
    #[error("the edge store could not apply a change: {message}")]
    WriteFailed {
        /// What the driver reported, without a raw SQL string or a secret.
        message: String,
    },

    /// A stored row could not be turned back into an [`Edge`](spot_edge_sync::Edge).
    #[error("a stored edge could not be decoded: {message}")]
    DecodeFailed {
        /// What could not be read.
        message: String,
    },

    /// The edge tables could not be created.
    #[error("the edge tables could not be created: {message}")]
    MigrationFailed {
        /// What the driver reported.
        message: String,
    },

    /// The drift report a repair was asked for is not there.
    #[error("the drift report `{report_key}` does not exist")]
    ReportNotFound {
        /// The report that was asked for.
        report_key: String,
    },
}

impl EdgePgError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "edge_pg.unavailable",
            Self::WriteFailed { .. } => "edge_pg.write_failed",
            Self::DecodeFailed { .. } => "edge_pg.decode_failed",
            Self::MigrationFailed { .. } => "edge_pg.migration_failed",
            Self::ReportNotFound { .. } => "edge_pg.report_not_found",
        }
    }

    /// The [`ErrorClass`] the native API reports this error as.
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Unavailable { .. } => ErrorClass::Unavailable,
            Self::ReportNotFound { .. } => ErrorClass::NotFound,
            Self::WriteFailed { .. } | Self::DecodeFailed { .. } | Self::MigrationFailed { .. } => {
                ErrorClass::Internal
            }
        }
    }

    /// The error as an [`AppError`].
    #[must_use]
    pub fn into_app_error(self) -> AppError {
        let class = self.class();
        AppError::new(class, self.code(), self.to_string())
    }

    /// The error as an [`EdgeSyncError`], so a failing edge write fails the document operation.
    #[must_use]
    pub fn into_edge_sync(self) -> EdgeSyncError {
        EdgeSyncError::WriteFailed {
            message: self.to_string(),
        }
    }

    /// A write failure from what a driver reported.
    #[must_use]
    pub fn write(message: impl Into<String>) -> Self {
        Self::WriteFailed {
            message: message.into(),
        }
    }

    /// A decode failure.
    #[must_use]
    pub fn decode(message: impl Into<String>) -> Self {
        Self::DecodeFailed {
            message: message.into(),
        }
    }
}

impl From<EdgePgError> for AppError {
    fn from(error: EdgePgError) -> Self {
        error.into_app_error()
    }
}

/// The result of a fallible edge-store operation.
pub type Result<T> = std::result::Result<T, EdgePgError>;
