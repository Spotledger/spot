//! Typed errors for `spot-edge-sync`: stable `edge_sync.*` codes.
//!
//! The pure functions ([`crate::edges_of`], [`crate::diff`], [`crate::check_invariant`]) never
//! fail: a document that names an unknown DocType or a bad target yields fewer edges, not an
//! error, exactly as [`spot_graph_model`](spot_graph_model) reports a problem instead of
//! panicking. [`EdgeSyncError`] is for the boundary that *must* fail: turning a document's
//! DocType into a storage [`Name`](spot_storage_contract::Name) and a writer refusing a change.
//!
//! Callers match on [`EdgeSyncError::code`], never on the message: the codes are this crate's
//! API contract and every one is catalogued in
//! `crates/framework/spot-errors/catalogue/edge_sync.toml`.

use spot_app_api::error::{AppError, ErrorClass};

/// What can go wrong writing an edge set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EdgeSyncError {
    /// A document's DocType is not a valid storage name. Metadata that compiled always passes;
    /// a hand-built document can still carry one that does not.
    #[error("`{name}` is not a valid storage name for an edge end")]
    InvalidName {
        /// The name that failed.
        name: String,
    },

    /// A document's DocType is not in the effective metadata the schema was derived from.
    #[error("the metadata has no DocType `{doctype}`")]
    UnknownDocType {
        /// The DocType the document named.
        doctype: String,
    },

    /// A writer refused a change (the backend failed, or the edge table was unreachable).
    #[error("the edge writer failed: {message}")]
    WriteFailed {
        /// What the writer reported, without a raw SQL string or a secret.
        message: String,
    },

    /// The engine reported a write for a site other than the one this participant serves. The
    /// edge tables of one site are never written for another (tenancy), so the write fails.
    #[error("the edge participant serves another site than the write's")]
    WrongSite,

    /// The engine's write record lacked the document the edge set is derived from (a create or
    /// update without the written document, a delete without the loaded one). Nothing is
    /// guessed: the operation fails and rolls back.
    #[error("the write record carries no document to derive edges from (operation `{operation}`)")]
    MissingDocument {
        /// The operation the write belonged to.
        operation: String,
    },
}

impl EdgeSyncError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidName { .. } => "edge_sync.invalid_name",
            Self::UnknownDocType { .. } => "edge_sync.unknown_doctype",
            // The last two are platform defects the writer refuses: they report under the writer's code
            // (catalogued, with help) and say what was refused in the message.
            Self::WriteFailed { .. } | Self::WrongSite | Self::MissingDocument { .. } => {
                "edge_sync.write_failed"
            }
        }
    }

    /// The error as an [`AppError`], so a lifecycle hook that fails an edge write fails the
    /// document operation with a code the engine and the catalogue know.
    #[must_use]
    pub fn into_app_error(self) -> AppError {
        let code = self.code();
        AppError::new(ErrorClass::Internal, code, self.to_string())
    }
}

impl From<EdgeSyncError> for AppError {
    fn from(error: EdgeSyncError) -> Self {
        error.into_app_error()
    }
}

/// The result of a fallible edge-sync step.
pub type EdgeResult<T> = Result<T, EdgeSyncError>;
