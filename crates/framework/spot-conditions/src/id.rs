//! The stable id of a condition.

use std::borrow::Borrow;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::ConditionsError;

/// A condition's stable id, in the upper snake case ADR-067 section 1 spells its catalogue in
/// (`MISSING_DEP`, `LAG_HIGH`, `NODE_TOO_OLD`).
///
/// The shape is fixed and checked once, at [`ConditionId::parse`], so every later use (a
/// registry key, an audit event, a log line) carries an id that is already known-good: no
/// whitespace, no lower case, no empty segment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ConditionId(String);

impl ConditionId {
    /// The longest id accepted, in bytes.
    pub const MAX_LEN: usize = 64;

    /// Parse an id.
    ///
    /// Accepts `[A-Z][A-Z0-9]*(_[A-Z0-9]+)*` of at most [`ConditionId::MAX_LEN`] bytes. Refuses
    /// anything else as [`ConditionsError::UnknownId`]: such an id was never a registered
    /// condition, and accepting it would put a name into the audit record that nothing can
    /// explain.
    pub fn parse(text: &str) -> Result<Self, ConditionsError> {
        if is_upper_snake(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(ConditionsError::UnknownId {
                id: text.to_owned(),
            })
        }
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for ConditionId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConditionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ConditionId {
    /// Reads an id through [`ConditionId::parse`], so a deserialised id is a valid one: the
    /// invariant holds for data that arrives from a file as much as for one built in code.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Whether `text` is upper snake case: upper-case letters and digits, single underscores
/// between non-empty segments, no leading or trailing underscore.
fn is_upper_snake(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > ConditionId::MAX_LEN {
        return false;
    }
    if !bytes[0].is_ascii_uppercase() {
        return false;
    }
    let mut previous_underscore = false;
    for &byte in bytes {
        if !(byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_') {
            return false;
        }
        if byte == b'_' && previous_underscore {
            return false;
        }
        previous_underscore = byte == b'_';
    }
    !previous_underscore
}
