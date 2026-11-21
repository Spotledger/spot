//! Record keys, document versions and storage identifiers.

use std::fmt;

use crate::error::{ErrorKind, StorageError, StorageResult};

/// Immutable, time-ordered key of every record: a UUIDv7 stored as 16 bytes (ADR-33).
///
/// Links, graph edges and line references always point at a `RecordId`. It is never a
/// customer-facing identifier; the human-readable document number is an ordinary unique field.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordId([u8; 16]);

impl RecordId {
    /// A new UUIDv7 from the current time. Ids created later sort after earlier ones.
    pub fn new_v7() -> Self {
        Self(*uuid::Uuid::now_v7().as_bytes())
    }

    /// Wraps 16 bytes read from storage. Fails unless they are a UUIDv7.
    pub fn from_bytes(bytes: [u8; 16]) -> StorageResult<Self> {
        if uuid::Uuid::from_bytes(bytes).get_version_num() != 7 {
            return Err(StorageError::new(
                ErrorKind::InvalidValue,
                "record ids must be UUIDv7 values (ADR-33)",
            ));
        }
        Ok(Self(bytes))
    }

    /// The 16 bytes as stored.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for RecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RecordId({self})")
    }
}

impl fmt::Display for RecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        uuid::Uuid::from_bytes(self.0).hyphenated().fmt(f)
    }
}

/// Version of a stored document: 1 after insert, incremented by every update of the parent or
/// its children. Used for optimistic concurrency and as the sync unit (ADR-23).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u64);

impl Version {
    /// The version every document has right after insert.
    pub const FIRST: Version = Version(1);

    /// The version after one more write.
    pub fn next(self) -> Version {
        Version(self.0 + 1)
    }
}

/// A storage identifier: entity, field, index, topic or lock namespace name.
///
/// Always `[a-z][a-z0-9_]*`, at most 63 bytes, so a backend can use it as a quoted identifier
/// without escaping. Mapping DocType labels such as "Sales Invoice" to names such as
/// `sales_invoice` is the metadata layer's job, not the storage's.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(String);

impl Name {
    /// Longest allowed name in bytes (the PostgreSQL identifier limit).
    pub const MAX_LEN: usize = 63;

    /// Validates and wraps a name.
    pub fn new(s: &str) -> StorageResult<Self> {
        let mut chars = s.chars();
        let valid = s.len() <= Self::MAX_LEN
            && chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !valid {
            return Err(StorageError::new(
                ErrorKind::InvalidName,
                format!(
                    "`{s}` is not a valid storage name: use lowercase letters, digits and \
                     underscores, starting with a letter, at most {} bytes",
                    Self::MAX_LEN
                ),
            ));
        }
        Ok(Self(s.to_owned()))
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_validated() {
        assert!(Name::new("sales_invoice").is_ok());
        assert!(Name::new("a1").is_ok());
        for bad in [
            "",
            "Sales",
            "1abc",
            "_x",
            "a-b",
            "a b",
            "tab\"x",
            &"a".repeat(64),
        ] {
            let err = Name::new(bad).err();
            assert_eq!(err.map(|e| e.kind), Some(ErrorKind::InvalidName), "{bad:?}");
        }
    }

    #[test]
    fn ids_are_v7_and_time_ordered() {
        let a = RecordId::new_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = RecordId::new_v7();
        assert!(a < b);
        assert_eq!(RecordId::from_bytes(*a.as_bytes()).ok(), Some(a));
        assert!(RecordId::from_bytes(*uuid::Uuid::nil().as_bytes()).is_err());
    }
}
