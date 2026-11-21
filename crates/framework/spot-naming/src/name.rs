//! A generated, validated record name.

use std::fmt;

use crate::error::NamingError;

/// A legal record name: the human-readable identifier stored in a document's title field
/// (ADR-33 — the record's own id is always a separate UUIDv7).
///
/// A name is 1 to 140 bytes of printable text: no control characters and no leading or
/// trailing whitespace. Nothing is ever trimmed or case-folded; an invalid value is refused,
/// so a caller never silently stores a different name than it built.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordName(String);

impl RecordName {
    /// Longest allowed name in bytes.
    pub const MAX_LEN: usize = 140;

    /// Validates and wraps a name.
    ///
    /// # Errors
    ///
    /// [`NamingError::InvalidName`] when the value is empty, too long, holds a control
    /// character, or starts or ends with whitespace.
    pub fn new(value: &str) -> Result<Self, NamingError> {
        let reason = if value.is_empty() {
            Some("it is empty")
        } else if value.len() > Self::MAX_LEN {
            Some("it is longer than 140 bytes")
        } else if value.chars().any(char::is_control) {
            Some("it contains a control character")
        } else if value.trim() != value {
            Some("it starts or ends with whitespace")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(NamingError::InvalidName {
                value: value.to_owned(),
                reason,
            }),
            None => Ok(Self(value.to_owned())),
        }
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RecordName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RecordName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RecordName({:?})", self.0)
    }
}

impl AsRef<str> for RecordName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
