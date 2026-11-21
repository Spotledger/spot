//! The canonical digest: SHA-256 over a fixed listing of the bundle's content.
//!
//! The listing is independent of ZIP ordering, timestamps, compression level and extra
//! fields, so two archives with the same content have the same digest and one changed byte
//! gives a different one. The digest is the bundle's content address; the `digest` entry in
//! the archive carries it and [`Bundle::open`](crate::Bundle::open) recomputes it.

use std::fmt;
use std::str::FromStr;

/// The SHA-256 over a bundle's canonical listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Wraps raw digest bytes.
    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The digest as lowercase hex, for logs and the archive's `digest` entry.
    pub fn to_hex(&self) -> String {
        let mut hex = String::with_capacity(64);
        for byte in self.0 {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Why a string is not a digest: it must be exactly 64 hexadecimal characters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a digest is 64 hex characters (256 bits), got {found}")]
pub struct DigestFromStrError {
    /// How many characters the string had.
    pub found: usize,
}

impl FromStr for Digest {
    type Err = DigestFromStrError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.len() != 64 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(DigestFromStrError { found: text.len() });
        }
        let mut bytes = [0_u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            let hi = hex_value(text.as_bytes()[i * 2]);
            let lo = hex_value(text.as_bytes()[i * 2 + 1]);
            *byte = (hi << 4) | lo;
        }
        Ok(Digest(bytes))
    }
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        // `FromStr` has checked the range; `A`..=`F` maps the same way.
        _ => byte - b'A' + 10,
    }
}

/// The domain prefix for the canonical digest input: `spotledger-custom-bundle-v1\0`.
pub const DIGEST_PREFIX: &[u8] = b"spotledger-custom-bundle-v1\0";

/// The canonical listing one digest covers, in byte-wise sorted name order:
/// `prefix || len(name) | name | len(content) | content` per entry.
///
/// Names and lengths are fixed-width little-endian, so concatenations can never coalesce
/// ambiguously, and the sorted order makes the digest independent of ZIP ordering.
pub fn canonical_input(parts: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(DIGEST_PREFIX);
    let mut sorted: Vec<&(String, Vec<u8>)> = parts.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    for (name, content) in sorted {
        out.extend_from_slice(&(name.len() as u64).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(content.len() as u64).to_le_bytes());
        out.extend_from_slice(content);
    }
    out
}

/// SHA-256 over `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// The SHA-256 of `bytes` as lowercase hex.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in sha256(bytes) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The digest over a whole entry listing (used both for the bundle and for one section).
pub(crate) fn digest_of(parts: &[(String, Vec<u8>)]) -> Digest {
    Digest::from_bytes(sha256(&canonical_input(parts)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let digest = Digest::from_bytes([0u8; 32]);
        assert_eq!(digest.to_hex().len(), 64);
        assert_eq!(digest.to_hex(), "0".repeat(64));
        assert_eq!(digest.to_hex().parse::<Digest>(), Ok(digest));
        let bytes = [255_u8; 32];
        assert_eq!(
            Digest::from_bytes(bytes).to_hex().parse::<Digest>(),
            Ok(Digest::from_bytes(bytes))
        );
        // Mixed case parses; canonical hex is lowercase.
        assert!("F".repeat(64).parse::<Digest>().is_ok());
    }

    #[test]
    fn bad_hex_rejects() {
        assert!(Digest::from_str("").is_err());
        assert!(Digest::from_str("00").is_err());
        assert!(Digest::from_str(&"g".repeat(64)).is_err());
        assert!(Digest::from_str(&"0".repeat(65)).is_err());
        assert!(Digest::from_str(&"0".repeat(63)).is_err());
    }

    #[test]
    fn canonical_input_is_order_independent() {
        let first = vec![
            ("z.txt".to_owned(), b"last".to_vec()),
            ("a.txt".to_owned(), b"first".to_vec()),
        ];
        let second = vec![
            ("a.txt".to_owned(), b"first".to_vec()),
            ("z.txt".to_owned(), b"last".to_vec()),
        ];
        assert_eq!(canonical_input(&first), canonical_input(&second));
        assert_eq!(digest_of(&first), digest_of(&second));
    }

    #[test]
    fn digest_changes_with_content() {
        let first = vec![("a.txt".to_owned(), b"x".to_vec())];
        let second = vec![("a.txt".to_owned(), b"y".to_vec())];
        assert_ne!(digest_of(&first), digest_of(&second));
    }
}
