//! Identifier grammars for a [`Node`](crate::Node), a [`Rule`](crate::Rule) and a
//! process name.
//!
//! One grammar serves every name-shaped string: a lower-case letter first, then
//! lower-case letters, digits and underscores, at most 64 characters. It is the
//! same shape `spot-dag` uses, so a node id here is a valid DAG node id there.
//! [`NodeId`] bakes the check into construction; [`is_name`] is the free
//! predicate a process name or a rule id is checked against.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ModelError;

/// Maximum length of any name-shaped string (64 characters).
pub const MAX_NAME_LEN: usize = 64;

/// Whether `raw` matches `[a-z][a-z0-9_]{0,63}`.
#[must_use]
pub fn is_name(raw: &str) -> bool {
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() || raw.len() > MAX_NAME_LEN {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Maximum length of a field name (128 characters), the same bound ADR-058's
/// link field names use.
pub const MAX_FIELD_NAME_LEN: usize = 128;

/// Whether `raw` is a field name: one or more dotted segments of
/// `[a-z][a-z0-9_]*`, at most 128 characters.
///
/// It is the grammar a form step's fields and a contract message's fields use,
/// so a field that a process names is a field a tenant link can carry.
#[must_use]
pub fn is_field_name(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > MAX_FIELD_NAME_LEN {
        return false;
    }
    raw.split('.').all(|segment| {
        let mut chars = segment.chars();
        chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    })
}

/// The identifier of a node or a rule, with the grammar baked in.
///
/// Valid ids match `[a-z][a-z0-9_]{0,63}`. Build one with [`NodeId::new`];
/// deserialising validates too, so a bad id in text is an error before it ever
/// becomes a node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    /// Validates the grammar and builds the id.
    ///
    /// # Errors
    /// Returns [`ModelError::InvalidName`] when `raw` does not match
    /// `[a-z][a-z0-9_]{0,63}`.
    pub fn new(raw: &str) -> Result<Self, ModelError> {
        if is_name(raw) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(ModelError::InvalidName {
                raw: raw.to_owned(),
            })
        }
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for NodeId {
    type Error = ModelError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<NodeId> for String {
    fn from(id: NodeId) -> Self {
        id.0
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        for good in ["a", "start_here", "n0", "order_to_cash", "a_b_c_1"] {
            assert!(NodeId::new(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn rejects_bad_names() {
        for bad in ["", "A", "1abc", "-a", "a b", "a.b", "aB", "ä"] {
            assert!(NodeId::new(bad).is_err(), "{bad}");
            assert!(!is_name(bad), "{bad}");
        }
    }

    #[test]
    fn rejects_overlong_names() {
        let long = "a".repeat(MAX_NAME_LEN + 1);
        assert!(NodeId::new(&long).is_err());
    }

    #[test]
    fn field_names_are_dotted_lower_case_segments() {
        for good in ["amount", "order.total", "items.unit_price", "a.b.c"] {
            assert!(is_field_name(good), "{good}");
        }
        for bad in [
            "",
            "Amount",
            "1amount",
            "order.",
            ".order",
            "order..total",
            "a-b",
            "ä",
        ] {
            assert!(!is_field_name(bad), "{bad}");
        }
        let long = "a".repeat(MAX_FIELD_NAME_LEN + 1);
        assert!(!is_field_name(&long));
    }
}
