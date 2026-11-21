//! Scope keys and document-type names: the routing vocabulary (ADR-063).
//!
//! A **scope key** is an opaque `(key type, value)` pair that a node holds, for example
//! `(company, C1)` or `(warehouse, W7)`. Apps register key types; the platform never interprets
//! them (ADR-062 section 1, ADR-063 section 3). A **document-type name** is a storage name
//! (`[a-z][a-z0-9_]*`), the identifier a change's entity is routed by.
//!
//! Both types validate only what safety needs: non-empty, bounded, no control characters. Nothing
//! here decides what a key *means*, and no key type is special.

use std::fmt;

use crate::error::{Error, Result};

/// The longest opaque text this crate carries in a scope key or a payload.
pub const MAX_OPAQUE_BYTES: usize = 256;

/// One scope key: an opaque `(key type, value)` pair a node holds and a change requires.
///
/// The pair is the whole meaning. The platform compares keys for equality and subset, never for
/// order of importance, and never reads a key type by name (ADR-058 rule 2: no ERP concepts in
/// platform code).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScopeKey {
    key_type: String,
    value: String,
}

impl ScopeKey {
    /// A scope key; fails when either part is empty, too long, or holds a control character.
    pub fn new(key_type: &str, value: &str) -> Result<Self> {
        validate_opaque("a scope key's type", key_type)?;
        validate_opaque("a scope key's value", value)?;
        Ok(Self {
            key_type: key_type.to_owned(),
            value: value.to_owned(),
        })
    }

    /// The key type (for example `company`).
    pub fn key_type(&self) -> &str {
        &self.key_type
    }

    /// The key value (for example `C1`).
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for ScopeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.key_type, self.value)
    }
}

/// A document-type name: the storage name a change's entity is routed and stored by.
///
/// The name is `[a-z][a-z0-9_]*`, at most 63 bytes, the same shape ADR-056 fixes for a topic and
/// the storage contract fixes for a record name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Doctype(String);

impl Doctype {
    /// A document-type name; fails unless it is a storage name.
    pub fn new(name: &str) -> Result<Self> {
        let mut chars = name.chars();
        let valid = name.len() <= 63
            && chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !valid {
            return Err(Error::invalid_argument(
                "a document-type name is a storage name: [a-z][a-z0-9_]*, at most 63 bytes",
            ));
        }
        Ok(Self(name.to_owned()))
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Doctype {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `value` is opaque text this crate will carry: non-empty, at most
/// [`MAX_OPAQUE_BYTES`] bytes, with no control characters.
fn validate_opaque(what: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::invalid_argument(format!("{what} is not empty")));
    }
    if value.len() > MAX_OPAQUE_BYTES {
        return Err(Error::invalid_argument(format!(
            "{what} is at most {MAX_OPAQUE_BYTES} bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(Error::invalid_argument(format!(
            "{what} holds no control characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn scope_keys_are_opaque_but_bounded() {
        let key = ScopeKey::new("company", "C1").unwrap();
        assert_eq!(key.key_type(), "company");
        assert_eq!(key.value(), "C1");
        assert_eq!(key.to_string(), "company(C1)");
        assert!(ScopeKey::new("", "C1").is_err());
        assert!(ScopeKey::new("company", "").is_err());
        assert!(ScopeKey::new("company", "has\ncontrol").is_err());
        assert!(ScopeKey::new("company", &"x".repeat(MAX_OPAQUE_BYTES + 1)).is_err());
    }

    #[test]
    fn doctypes_are_storage_names() {
        assert!(Doctype::new("price_list").is_ok());
        for bad in ["", "Price", "1price", "price-list", "price list"] {
            assert!(Doctype::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
