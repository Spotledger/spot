//! The blind index: a keyed MAC a `secret` field can be looked up by, without reversibility.
//!
//! A sealed value cannot be searched: randomised encryption gives the same plaintext a
//! different ciphertext every time. A field that must be found by value therefore carries one
//! more column holding a **blind index**: `HMAC-SHA256(index_key, scope || 0x00 || value)`,
//! prefixed with the byte of the key version that produced it. The index key is HKDF-SHA256 of
//! the provider's key-encryption key ([`crate::kdf`]), never the data key, so an attacker who
//! reads one sealed row learns nothing about the index key of the rest.
//!
//! **Why equality on the 33 bytes is safe.** The index is itself a keyed MAC, so it is a
//! pseudorandom 256-bit string to anyone without the index key: a timing difference in the
//! database's `=` comparison reveals only how many leading bytes of an unknowable value match,
//! which says nothing about the plaintext. The value is compared whole (all 33 bytes: version
//! byte and MAC), never by prefix, and no code here compares a secret to anything at all.

use crate::error::SecretError;
use crate::kdf;
use std::fmt;

/// Length of a blind index in bytes: one key-version byte, then the 32-byte MAC.
pub const INDEX_LEN: usize = 1 + kdf::DERIVED_LEN;

/// Separates the scope from the value in the MAC input, so a value can never be read as a
/// longer scope or the other way round.
const SCOPE_SEPARATOR: u8 = 0x00;

/// Longest storage name the metadata layer accepts, in bytes (`Name::MAX_LEN`).
const MAX_NAME_BYTES: usize = 63;

/// The engine-managed name infix. No field may contain it, so a scope never names one.
pub const RESERVED_INFIX: &str = "__";

/// A blind index of one value in one field of one DocType.
///
/// It is safe to store: it is a keyed MAC, not a hash of a guessable input, and it cannot be
/// reversed without the index key. It is **not** safe to compare with anything but whole-value
/// equality (see the module comment).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlindIndex {
    version: u8,
    mac: [u8; kdf::DERIVED_LEN],
}

impl BlindIndex {
    /// The stored form: the key-version byte followed by the MAC.
    pub fn to_bytes(self) -> [u8; INDEX_LEN] {
        let mut out = [0u8; INDEX_LEN];
        out[0] = self.version;
        out[1..].copy_from_slice(&self.mac);
        out
    }

    /// Reads the stored form.
    ///
    /// Key versions start at 1, so a zero prefix byte was not written by this crate: a wrong
    /// column, a truncated value or a value from another system. It is refused rather than
    /// treated as version 1, because that would silently make an index unmatchable.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SecretError> {
        let bytes: [u8; INDEX_LEN] = bytes
            .try_into()
            .map_err(|_| SecretError::Malformed("blind index is not 33 bytes"))?;
        let version = bytes[0];
        if version == 0 {
            return Err(SecretError::UnknownKeyVersion { version: 0 });
        }
        let mut mac = [0u8; kdf::DERIVED_LEN];
        mac.copy_from_slice(&bytes[1..]);
        Ok(Self { version, mac })
    }

    /// The key version byte the index was derived under.
    pub fn version(self) -> u8 {
        self.version
    }

    /// Derives the index of `value` in `scope` under `index_key` and `version`.
    ///
    /// `scope` is `"<doctype>.<field>"`, checked by [`check_scope`]. The MAC input is
    /// `scope || 0x00 || value`, so equal values in two fields, or a value that itself starts
    /// with another scope's text, never collide.
    pub(crate) fn compute(
        index_key: &[u8],
        version: u8,
        scope: &str,
        value: &[u8],
    ) -> Result<Self, SecretError> {
        let mac = kdf::hmac_sha256(index_key, &[scope.as_bytes(), &[SCOPE_SEPARATOR], value])?;
        Ok(Self { version, mac })
    }
}

impl fmt::Debug for BlindIndex {
    /// Names the version only: the MAC is a lookup key for a secret, so it never reaches a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlindIndex")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// Checks that `scope` is `"<doctype>.<field>"` in storage-name characters, with no
/// engine-managed `__`.
///
/// The scope names a column (`<field>__bidx`) and it keys the MAC, so a loose scope would let
/// two fields share an index. The parts are storage names, so the DocType and field names the
/// metadata layer accepts are exactly the ones accepted here.
pub fn check_scope(scope: &str) -> Result<(), SecretError> {
    let parts: Vec<&str> = scope.split('.').collect();
    if parts.len() != 2 {
        return Err(SecretError::Malformed(
            "blind index scope must be `<doctype>.<field>`",
        ));
    }
    for part in parts {
        if part.contains(RESERVED_INFIX) {
            return Err(SecretError::ReservedName(scope.to_owned()));
        }
        if !is_storage_name(part) {
            return Err(SecretError::Malformed(
                "blind index scope parts must be storage names",
            ));
        }
    }
    Ok(())
}

/// Whether `s` is a storage name: 1 to 63 bytes of `[a-z0-9_]`, not starting with a digit.
fn is_storage_name(s: &str) -> bool {
    let bytes = s.as_bytes();
    match bytes.first() {
        Some(b) if b.is_ascii_lowercase() => {}
        _ => return false,
    }
    bytes.len() <= MAX_NAME_BYTES
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}
