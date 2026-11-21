//! A SHA-256 digest: 32 bytes, with a lower-case hex text form.
//!
//! The manifest names one digest for the payload archive and one for each file inside it. The
//! verifier recomputes them from the bytes on disk with [`Digest::of`] and compares, so a
//! flipped byte anywhere is located.

use std::fmt;

use sha2::{Digest as _, Sha256};

use crate::error::UpdaterError;

/// A SHA-256 digest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest([u8; Self::BYTES]);

impl Digest {
    /// The number of bytes in a digest.
    pub const BYTES: usize = 32;
    /// The number of hex characters in a digest's text form.
    pub const HEX_LEN: usize = Self::BYTES * 2;

    /// The SHA-256 of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// A digest from its 32 raw bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; Self::BYTES]) -> Self {
        Self(bytes)
    }

    /// The 32 raw bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; Self::BYTES] {
        &self.0
    }

    /// Parses the lower-case (or upper-case) hex text form.
    ///
    /// # Errors
    /// [`UpdaterError::ManifestMalformed`] unless `text` is exactly 64 hex characters.
    pub fn from_hex(text: &str) -> Result<Self, UpdaterError> {
        if text.len() != Self::HEX_LEN {
            return Err(malformed(format!(
                "a digest is {} hex characters, not {}",
                Self::HEX_LEN,
                text.len()
            )));
        }
        let mut out = [0u8; Self::BYTES];
        for (slot, pair) in out.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
            *slot = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
        }
        Ok(Self(out))
    }

    /// The lower-case hex text form.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(Self::HEX_LEN);
        for byte in &self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

/// The value of one hex character.
fn hex_value(byte: u8) -> Result<u8, UpdaterError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(malformed(format!(
            "`{}` is not a hex character",
            byte as char
        ))),
    }
}

/// The manifest-malformed error for a bad digest.
fn malformed(detail: String) -> UpdaterError {
    UpdaterError::ManifestMalformed { detail }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.to_hex())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}
