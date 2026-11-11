//! Update signing keys: key ids and public keys.
//!
//! A key id is the first 16 bytes of the SHA-256 of the Ed25519 public key, as lower-case hex
//! (32 characters): the same derivation the store's and the licence's keys use, so one key id
//! means one key everywhere. The update keys are their own set, never the store's and never the
//! licence's. This crate reads no randomness: the caller supplies the key bytes, and only the
//! control plane holds the secret.

use std::fmt;

use ed25519_dalek::{Verifier, VerifyingKey};
use sha2::{Digest as _, Sha256};

use crate::error::UpdaterError;

/// Bytes of an Ed25519 public key.
pub const PUBLIC_KEY_BYTES: usize = 32;
/// Bytes of an Ed25519 signature.
pub const SIGNATURE_BYTES: usize = 64;

/// The id of a signing key: 32 lower-case hex characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyId(String);

impl KeyId {
    /// The id of `key`.
    #[must_use]
    pub fn of(key: &PublicKey) -> Self {
        let digest = Sha256::digest(key.as_bytes());
        let mut hex = String::with_capacity(32);
        for byte in &digest[..16] {
            hex.push_str(&format!("{byte:02x}"));
        }
        Self(hex)
    }

    /// Parses an id from its text form.
    ///
    /// # Errors
    /// [`UpdaterError::KeyMalformed`] unless the text is exactly 32 lower-case hex characters.
    pub fn parse(text: &str) -> Result<Self, UpdaterError> {
        let valid = text.len() == 32
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(key_malformed(
                "a key id is 32 lower-case hex characters".to_owned(),
            ))
        }
    }

    /// The id's text form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An Ed25519 public key. Its bytes were checked to be a curve point when it was made.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; PUBLIC_KEY_BYTES]);

impl PublicKey {
    /// Builds a public key from its 32 bytes.
    ///
    /// # Errors
    /// [`UpdaterError::KeyMalformed`] for the wrong length or bytes that are not a curve point.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, UpdaterError> {
        let array = <[u8; PUBLIC_KEY_BYTES]>::try_from(bytes)
            .map_err(|_| key_malformed(format!("a public key is {PUBLIC_KEY_BYTES} bytes")))?;
        VerifyingKey::from_bytes(&array)
            .map_err(|_| key_malformed("the bytes are not an Ed25519 point".to_owned()))?;
        Ok(Self(array))
    }

    /// The 32 bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; PUBLIC_KEY_BYTES] {
        &self.0
    }

    /// The key's id.
    #[must_use]
    pub fn key_id(&self) -> KeyId {
        KeyId::of(self)
    }

    /// Checks `signature` over `message`.
    ///
    /// # Errors
    /// [`UpdaterError::SignatureInvalid`] when the signature does not verify,
    /// [`UpdaterError::KeyMalformed`] if the stored bytes are no longer a point (not reachable
    /// through this type's constructors).
    pub fn verify(
        &self,
        message: &[u8],
        signature: &[u8; SIGNATURE_BYTES],
    ) -> Result<(), UpdaterError> {
        let key = VerifyingKey::from_bytes(&self.0)
            .map_err(|_| key_malformed("the bytes are not an Ed25519 point".to_owned()))?;
        let signature = ed25519_dalek::Signature::from_bytes(signature);
        key.verify(message, &signature)
            .map_err(|_| UpdaterError::SignatureInvalid)
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.key_id())
    }
}

/// The key-malformed error.
fn key_malformed(detail: String) -> UpdaterError {
    UpdaterError::KeyMalformed { detail }
}
