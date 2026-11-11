//! Batch checksums: SHA-256 over a canonical, serde-independent encoding.
//!
//! Integrity is guarantee G3 of ADR-062: a record is applied with its dependencies, atomically,
//! or not at all. A batch that does not match its checksum is refused before anything in it is
//! looked at, and the sender re-requests it.
//!
//! The encoding never depends on a serde implementation or a map's iteration order. It is a
//! sequence of length-prefixed fields: a one-byte tag, then big-endian integers, then text as a
//! `u64` byte length followed by its UTF-8 bytes. The batch's own domain bytes prefix the whole
//! thing, so a batch checksum can never be mistaken for the checksum of another structure that
//! happens to encode the same way.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Domain separator for a batch's canonical encoding. Hyphens, not dots: it is not an error code.
pub(crate) const BATCH_DOMAIN: &[u8] = b"spot-sync-batch-v1";

/// A small builder for the canonical encoding. Field boundaries are explicit, so nothing is
/// ambiguous: `("a", "bc")` and `("ab", "c")` encode differently.
#[derive(Debug, Default)]
pub(crate) struct Encoder {
    out: Vec<u8>,
}

impl Encoder {
    /// An empty encoder.
    pub(crate) fn new() -> Self {
        Self { out: Vec::new() }
    }

    /// A one-byte field tag.
    pub(crate) fn tag(&mut self, tag: u8) {
        self.out.push(tag);
    }

    /// A big-endian `u64`.
    pub(crate) fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    /// Bytes, preceded by their length.
    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        self.out.extend_from_slice(bytes);
    }

    /// Text, preceded by its byte length.
    pub(crate) fn text(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    /// The encoded bytes.
    pub(crate) fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// The SHA-256 of a canonical encoding: 32 bytes, carried on the wire as lower-case hex.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Checksum([u8; 32]);

impl Checksum {
    /// The checksum of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Self(out)
    }

    /// The 32 raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The checksum as 64 lower-case hex characters.
    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        out
    }

    /// Parses 64 hex characters (either case).
    pub fn from_hex(hex: &str) -> Result<Self> {
        if hex.len() != 64 {
            return Err(Error::invalid_argument("a checksum is 64 hex characters"));
        }
        let mut bytes = [0u8; 32];
        let raw = hex.as_bytes();
        let (pairs, _rest) = raw.as_chunks::<2>();
        for (index, pair) in pairs.iter().enumerate() {
            let high = nibble(pair[0])?;
            let low = nibble(pair[1])?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

fn nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::invalid_argument(
            "a checksum holds only hex characters",
        )),
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Checksum {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Checksum {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let hex = String::deserialize(deserializer)?;
        Self::from_hex(&hex).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn hex_round_trips() {
        let checksum = Checksum::of(b"hello");
        assert_eq!(checksum.to_hex().len(), 64);
        assert_eq!(Checksum::from_hex(&checksum.to_hex()).unwrap(), checksum);
        assert!(Checksum::from_hex("nope").is_err());
        assert!(Checksum::from_hex(&"z".repeat(64)).is_err());
    }

    #[test]
    fn encoding_is_unambiguous() {
        let mut a = Encoder::new();
        a.text("a");
        a.text("bc");
        let mut b = Encoder::new();
        b.text("ab");
        b.text("c");
        assert_ne!(a.finish(), b.finish());
    }
}
