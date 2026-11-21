//! The one error type: a stable `scaffold.*` code and a message a developer can act on.

use std::fmt;

use crate::codes;

/// What a scaffold operation refused, or could not do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScaffoldError {
    /// The stable machine-readable code.
    pub code: &'static str,
    /// What went wrong, phrased for the app author.
    pub message: String,
}

impl ScaffoldError {
    /// An error with a code and a message.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// A name that is not a storage name.
    #[must_use]
    pub fn invalid_name(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_NAME, message)
    }

    /// A reserved word or platform prefix.
    #[must_use]
    pub fn reserved_name(message: impl Into<String>) -> Self {
        Self::new(codes::RESERVED_NAME, message)
    }

    /// A name that already exists.
    #[must_use]
    pub fn duplicate(message: impl Into<String>) -> Self {
        Self::new(codes::DUPLICATE, message)
    }

    /// An I/O failure.
    #[must_use]
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(codes::IO, message)
    }

    /// A path that would escape the output root.
    #[must_use]
    pub fn path_escape(message: impl Into<String>) -> Self {
        Self::new(codes::PATH_ESCAPE, message)
    }

    /// A symlink on the write path.
    #[must_use]
    pub fn symlink(message: impl Into<String>) -> Self {
        Self::new(codes::SYMLINK, message)
    }

    /// An overwrite that needs `--force`.
    #[must_use]
    pub fn exists(message: impl Into<String>) -> Self {
        Self::new(codes::EXISTS, message)
    }

    /// The exit code the command line returns for this error: 3, a refusal.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        3
    }
}

impl fmt::Display for ScaffoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ScaffoldError {}

/// The result of a scaffold operation.
pub type Result<T> = std::result::Result<T, ScaffoldError>;
