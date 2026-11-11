//! Stable error codes for file operations, matching the `files.*` family of the native API
//! error registry (ADR-055).

/// A file-store failure with a stable machine-readable code.
///
/// The codes never change: callers match on [`FileError::code`], not on the message.
/// [`FileError::Io`] wraps the source error so it can reach logs and tracing while the API
/// envelope stays generic.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// The key does not exist in the store. Code `files.not_found`.
    #[error("file not found: {key}")]
    NotFound {
        /// The missing key, for the message only.
        key: String,
    },
    /// A `put` with `overwrite = false` hit an existing key; nothing changed. Code
    /// `files.already_exists`.
    #[error("file already exists: {key}")]
    AlreadyExists {
        /// The key that already holds content.
        key: String,
    },
    /// The streamed body exceeded `PutOptions::max_bytes`. Nothing was stored. Code
    /// `files.too_large`.
    #[error("file exceeds the maximum size of {max_bytes} bytes")]
    TooLarge {
        /// The limit that was exceeded.
        max_bytes: u64,
    },
    /// A key failed validation. Code `files.invalid_key`.
    #[error("invalid file key: {reason}")]
    InvalidKey {
        /// Plain-language reason.
        reason: &'static str,
    },
    /// A backend failure (disk, poisoned lock, corrupt metadata). Code `files.io`. Wraps the
    /// source error.
    #[error("file store io error: {source}")]
    Io {
        /// The underlying error, kept for logs and tracing.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl FileError {
    /// The stable code for this error, for the API error envelope.
    pub fn code(&self) -> &'static str {
        match self {
            FileError::NotFound { .. } => "files.not_found",
            FileError::AlreadyExists { .. } => "files.already_exists",
            FileError::TooLarge { .. } => "files.too_large",
            FileError::InvalidKey { .. } => "files.invalid_key",
            FileError::Io { .. } => "files.io",
        }
    }

    /// Wraps a message as [`FileError::Io`] (an `io::Error` of the `other` kind).
    pub fn io(message: impl Into<String>) -> Self {
        FileError::Io {
            source: Box::new(std::io::Error::other(message.into())),
        }
    }
}

/// Filesystem failures become [`FileError::Io`].
impl From<std::io::Error> for FileError {
    fn from(source: std::io::Error) -> Self {
        FileError::Io {
            source: Box::new(source),
        }
    }
}
