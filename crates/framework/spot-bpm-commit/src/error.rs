//! Typed errors for `spot-bpm-commit`: one enum with stable `bpm_commit.*` codes.
//!
//! Callers match on [`CommitError::code`], never on the message. Every code has an entry in
//! `spot-errors`' `bpm_commit.toml`, all with `class = "none"`: these errors are raised on the
//! host-driven path of a request and never ride an `AppError`. No message holds an effect
//! payload, a secret or text a peer sent; a failure of the hold names the hold's stable code
//! only.

/// Result of every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, CommitError>;

/// The stable codes, one per variant.
pub mod codes {
    /// A Commit node's config is not in its documented form.
    pub const INVALID_CONFIG: &str = "bpm_commit.invalid_config";
    /// A declaration (node effects, request or node id) is outside its bounds.
    pub const INVALID_DECLARATION: &str = "bpm_commit.invalid_declaration";
    /// A commit was asked at a node the process does not mark as a commit point.
    pub const NOT_COMMIT_POINT: &str = "bpm_commit.not_commit_point";
    /// The request-scoped hold refused or failed.
    pub const HOLD_FAILED: &str = "bpm_commit.hold_failed";
}

/// One problem while holding, committing or discarding a request's external effects.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommitError {
    /// A Commit node's config is malformed.
    #[error("the commit point `{node}` has an invalid config: {detail}")]
    InvalidConfig {
        /// The node.
        node: String,
        /// What is wrong, in our own words (never the config's text).
        detail: String,
    },
    /// A declaration is outside its bounds.
    #[error("invalid declaration: {detail}")]
    InvalidDeclaration {
        /// What is wrong.
        detail: String,
    },
    /// The node is not a commit point of the process.
    #[error("node `{node}` is not a commit point of this process")]
    NotCommitPoint {
        /// The node.
        node: String,
    },
    /// The hold refused or failed; nothing about the effect is repeated here.
    #[error("the request hold failed with `{hold_code}`")]
    Hold {
        /// The hold's stable `effect_hold.*` code.
        hold_code: String,
    },
}

impl CommitError {
    /// The stable catalogue code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidConfig { .. } => codes::INVALID_CONFIG,
            Self::InvalidDeclaration { .. } => codes::INVALID_DECLARATION,
            Self::NotCommitPoint { .. } => codes::NOT_COMMIT_POINT,
            Self::Hold { .. } => codes::HOLD_FAILED,
        }
    }

    /// The hold's own code, when this error wraps one: lets a caller tell a dispatch failure
    /// (retry the commit point) from a state it cannot read.
    #[must_use]
    pub fn hold_code(&self) -> Option<&str> {
        match self {
            Self::Hold { hold_code } => Some(hold_code),
            _ => None,
        }
    }
}

impl From<spot_effect_hold::Error> for CommitError {
    fn from(error: spot_effect_hold::Error) -> Self {
        Self::Hold {
            hold_code: error.code().to_owned(),
        }
    }
}
