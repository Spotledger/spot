//! The canonical digest: SHA-256 over a fixed listing of the bundle's content (see
//! [`crate`] for the exact construction).

use std::fmt;
use std::str::FromStr;

/// The SHA-256 over a bundle's canonical listing.
///
/// Two archives with the same content packed differently — other entry order, other
/// timestamps, other compression levels or extra fields — have the same digest; one changed
/// byte gives a different one. Signing (WP-STR-02) and the installer (WP-STR-04) compare
/// this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Wraps raw digest bytes.
    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The digest as lowercase hex, for logs and the registry (WP-STR-03).
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

/// Why a string is not a digest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a digest is 64 hex characters (256 bits), got {found}")]
pub struct DigestFromStrError {
    /// How many characters the string had, when it failed the length check.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let bytes = [0u8; 32];
        let digest = Digest::from_bytes(bytes);
        let hex = digest.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, "0".repeat(64));
        assert_eq!(hex.parse::<Digest>(), Ok(digest));
        let bytes = [255_u8; 32];
        let hex = Digest::from_bytes(bytes).to_hex();
        assert_eq!(hex.parse::<Digest>(), Ok(Digest::from_bytes(bytes)));
        // Mixed case parses; canonical hex is lowercase.
        assert!("F".repeat(64).parse::<Digest>().is_ok());
    }

    #[test]
    fn bad_hex_rejects() {
        assert!(Digest::from_str("").is_err());
        assert!(Digest::from_str("00").is_err());
        assert!(Digest::from_str("g".repeat(64).as_str()).is_err());
        // 65 characters is one too many.
        assert!(Digest::from_str("0".repeat(65).as_str()).is_err());
        // A 63-character string is one short.
        assert!(Digest::from_str("0".repeat(63).as_str()).is_err());
    }
}
