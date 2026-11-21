//! Errors of the contract text form.
//!
//! They are library-internal: a host turns them into an app error with its own
//! code. The messages name the key or the index that is wrong, never a value
//! from a payload.

/// Why a contract text document could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    /// The text is not well-formed TOML.
    #[error("the contract text is not valid TOML: {message}")]
    Toml {
        /// The parser's message.
        message: String,
    },
    /// The text, or the contract in it, is larger than the bounds of
    /// [`limits`](crate::limits).
    #[error("the contract is too large: {message}")]
    TooLarge {
        /// Which bound was passed.
        message: String,
    },
    /// The text is well-formed but not a contract document.
    #[error("the contract text has the wrong shape: {message}")]
    Shape {
        /// What is wrong.
        message: String,
    },
}

impl TextError {
    /// Builds a shape error.
    #[must_use]
    pub fn shape(message: impl Into<String>) -> Self {
        Self::Shape {
            message: message.into(),
        }
    }
}
