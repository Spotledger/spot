//! One record in a batch: the unit an apply path stores.
//!
//! Sync's apply is **idempotent keyed by record name and version** (ADR-023, ADR-062 G4): the
//! same `(name, version)` applied twice changes nothing the second time. This crate never
//! interprets a record's [`payload`](Record::payload); it carries an already-encoded record
//! image the sender produced and the apply path understands. The payload is opaque here, which
//! keeps the protocol independent of any document or replication format (WP-EDG-06 owns apply).
//!
//! A record also carries its `seq`, the position it occupies in its source's stream. Sequences
//! are monotonic per source and contiguous inside a [`Batch`](crate::Batch); they are what gap
//! detection and checkpoints count.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::checksum::Encoder;
use crate::error::{Error, Result};

/// The longest record payload this crate will carry, in bytes.
pub const MAX_PAYLOAD_BYTES: usize = 1 << 20;

/// The name part of an apply key: the record a batch entry is about.
///
/// It is opaque to this crate. The apply path (WP-EDG-06) decides what a name is, for example a
/// document's number or `entity:id`; sync only requires that `(name, version)` identifies one
/// applied record. Non-empty, at most 256 characters, with no control characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RecordName(String);

impl RecordName {
    /// Validates and wraps a record name.
    pub fn new(name: &str) -> Result<Self> {
        if name.is_empty() {
            return Err(Error::invalid_argument("a record name is not empty"));
        }
        if name.chars().count() > 256 {
            return Err(Error::invalid_argument(
                "a record name is at most 256 characters",
            ));
        }
        if name.chars().any(char::is_control) {
            return Err(Error::invalid_argument(
                "a record name holds no control characters",
            ));
        }
        Ok(Self(name.to_owned()))
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

impl TryFrom<String> for RecordName {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(&value)
    }
}

impl From<RecordName> for String {
    fn from(value: RecordName) -> Self {
        value.0
    }
}

/// The idempotency key of an applied record: its name and version.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecordKey {
    /// The record name.
    pub name: RecordName,
    /// The record version.
    pub version: u64,
}

impl RecordKey {
    /// A key from its parts.
    pub fn new(name: RecordName, version: u64) -> Self {
        Self { name, version }
    }

    /// The key of `record`.
    pub fn of(record: &Record) -> Self {
        Self {
            name: record.name.clone(),
            version: record.version,
        }
    }
}

impl fmt::Display for RecordKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// One record version to apply, at a stream sequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Record {
    /// The apply key's name part.
    pub name: RecordName,
    /// The apply key's version part; at least 1.
    pub version: u64,
    /// The record's position in its source's stream; at least 1.
    pub seq: u64,
    /// The opaque, already-encoded record image.
    pub payload: String,
}

impl Record {
    /// A record; fails on an empty name, a zero version or sequence, or an oversize payload.
    pub fn new(
        name: RecordName,
        version: u64,
        seq: u64,
        payload: impl Into<String>,
    ) -> Result<Self> {
        let payload = payload.into();
        if version == 0 {
            return Err(Error::invalid_argument(
                "a record version is at least 1 (ADR-053)",
            ));
        }
        if seq == 0 {
            return Err(Error::invalid_argument("a stream sequence is at least 1"));
        }
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(Error::invalid_argument("a record payload is at most 1 MiB"));
        }
        Ok(Self {
            name,
            version,
            seq,
            payload,
        })
    }

    /// The record's apply key.
    pub fn key(&self) -> RecordKey {
        RecordKey::of(self)
    }

    /// Appends this record to a canonical encoding.
    pub(crate) fn encode(&self, encoder: &mut Encoder) {
        const RECORD_TAG: u8 = 0x02;
        encoder.tag(RECORD_TAG);
        encoder.text(self.name.as_str());
        encoder.u64(self.version);
        encoder.u64(self.seq);
        encoder.text(&self.payload);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn records_validate_their_parts() {
        let name = RecordName::new("sales_invoice:1").unwrap();
        assert!(Record::new(name.clone(), 1, 1, "{}").is_ok());
        assert!(Record::new(name.clone(), 0, 1, "{}").is_err());
        assert!(Record::new(name.clone(), 1, 0, "{}").is_err());
        assert!(RecordName::new("").is_err());
        assert!(RecordName::new("has\ncontrol").is_err());
    }

    #[test]
    fn key_is_name_and_version() {
        let record = Record::new(RecordName::new("d").unwrap(), 3, 7, "x").unwrap();
        assert_eq!(record.key().to_string(), "d@3");
    }
}
