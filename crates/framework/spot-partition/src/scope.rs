//! Scope keys: the opaque `(key type, value)` pairs routing and ownership work on.
//!
//! A scope key is a pair an app defines and the platform never interprets (ADR-062 section 1,
//! ADR-063 section 3): `(warehouse, W7)`, `(company, C1)`, `(region, north)`. Apps register the
//! key types; a node holds a set of scope keys, and the keys decide what it owns and what it
//! receives. Nothing here knows what a "warehouse" is.

use std::fmt;

use crate::error::PartitionError;

/// An opaque `(key type, value)` pair (ADR-062 vocabulary).
///
/// Both parts are validated on construction: 1 to [`ScopeKey::MAX_LEN`] printable bytes with no
/// control characters. The platform compares keys for equality and never parses them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScopeKey {
    key_type: String,
    value: String,
}

impl ScopeKey {
    /// Longest allowed key type or value, in bytes.
    pub const MAX_LEN: usize = 128;

    /// Validates and wraps a scope key.
    ///
    /// # Errors
    ///
    /// [`PartitionError::InvalidScope`] when either part is empty, longer than
    /// [`ScopeKey::MAX_LEN`] bytes, or holds a control character.
    pub fn new(key_type: &str, value: &str) -> Result<Self, PartitionError> {
        validate_part("key type", key_type)?;
        validate_part("value", value)?;
        Ok(Self {
            key_type: key_type.to_owned(),
            value: value.to_owned(),
        })
    }

    /// The registered key type, for example `warehouse`.
    #[must_use]
    pub fn key_type(&self) -> &str {
        &self.key_type
    }

    /// The value within the key type, for example `W7`.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

fn validate_part(what: &str, part: &str) -> Result<(), PartitionError> {
    let reason = if part.is_empty() {
        Some("it is empty")
    } else if part.len() > ScopeKey::MAX_LEN {
        Some("it is longer than the 128-byte limit")
    } else if part.chars().any(char::is_control) {
        Some("it holds a control character")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(PartitionError::InvalidScope {
            reason: format!("the scope key {what} is invalid: {reason}"),
        }),
        None => Ok(()),
    }
}

impl fmt::Display for ScopeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.key_type, self.value)
    }
}
