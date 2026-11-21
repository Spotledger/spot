//! The plan hash: SHA-256 over a plan's canonical form (brief §1, like `spot-dag`'s content
//! hash).
//!
//! The canonical form is compact JSON produced from the plan's data (see
//! [`plan::canonical_bytes`](crate::plan)); `serde_json`'s `Value` maps sort their keys, so
//! maps serialise sorted, and ordered structures (the field list) serialise in declaration
//! order. The hash deliberately excludes the metadata version: two builds of the same
//! metadata content are provably identical, whatever version stamp they carry, so a plan hash
//! equal to the old one means "keep the old Arc" in [`PlanCache::refresh`](crate::PlanCache).

use std::fmt;

use sha2::{Digest, Sha256};

/// SHA-256 hash of a plan's canonical form.
///
/// Display is 64 lowercase hex characters. Two builds of the same metadata content have the
/// same hash; any real change (a field, a rule, a constraint, a target) changes it. The
/// metadata version is not part of the hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanHash(String);

impl PlanHash {
    /// A placeholder hash for a plan still under construction; real plans overwrite it with
    /// [`PlanHash::of_bytes`] before anything reads it.
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self("0000000000000000000000000000000000000000000000000000000000000000".to_owned())
    }

    /// Hashes a canonical byte form.
    #[must_use]
    pub(crate) fn of_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hex(&hasher.finalize()))
    }

    /// The 64 lowercase hex characters.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlanHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lowercase hex of a digest or of raw bytes, written without float or fancy code.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(*byte >> 4)] as char);
        out.push(HEX[usize::from(*byte & 0x0f)] as char);
    }
    out
}
