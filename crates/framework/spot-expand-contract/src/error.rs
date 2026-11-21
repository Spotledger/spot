//! Typed errors with stable `expand_contract.*` codes
//! (catalogue: `expand_contract.toml`).
//!
//! The gate itself does not error on a refused release: it returns
//! [`Refusal`](crate::check::Refusal) values, like `spot-release-meta`'s
//! [`Problem`](spot_release_meta::Problem). These errors are for input that cannot be read at
//! all — an object name, a release label or a manifest — so the caller can say "this input is
//! malformed" instead of guessing.

use crate::codes;

/// Anything the expand/contract reader refuses to accept.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An object name is not `table` or `table.column`.
    #[error("{}: `{name}`: {reason}", codes::BAD_OBJECT)]
    BadObject {
        /// The offending name, as written.
        name: String,
        /// Why it is not usable, in a sentence.
        reason: String,
    },
    /// A usage manifest could not be read.
    #[error("{}: {message}", codes::BAD_MANIFEST)]
    BadManifest {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// A release label is empty or unusable.
    #[error("{}: {message}", codes::BAD_RELEASE)]
    BadRelease {
        /// What is wrong, in a sentence.
        message: String,
    },
}

impl Error {
    /// The stable code, for callers that match on it rather than the message.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadObject { .. } => codes::BAD_OBJECT,
            Self::BadManifest { .. } => codes::BAD_MANIFEST,
            Self::BadRelease { .. } => codes::BAD_RELEASE,
        }
    }
}
