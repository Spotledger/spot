//! The AEAD and the blind index: the only places that touch a primitive.
//!
//! The AEAD is XChaCha20-Poly1305, the choice `spot-secrets` (WP-SEC-03.T1) already made and
//! vetted; the sealing shape (`nonce || ciphertext`, 24-byte nonce, 16-byte tag) matches it.
//! The blind index is HMAC-SHA-256 under its own key.

use crate::error::ProtectedKeyError;
use crate::locked::{KEY_LEN, LockedKey};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use rand::TryRngCore;
use rand::rngs::OsRng;
use sha2::Sha256;
use spot_secrets::SecretBytes;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// The XChaCha20-Poly1305 nonce length.
pub(crate) const NONCE_LEN: usize = 24;
/// The Poly1305 tag length.
pub(crate) const TAG_LEN: usize = 16;
/// The blind index length (a full SHA-256 digest).
pub(crate) const INDEX_LEN: usize = 32;

/// Fills `buf` from the operating system CSPRNG.
pub(crate) fn random(buf: &mut [u8]) -> Result<(), ProtectedKeyError> {
    OsRng
        .try_fill_bytes(buf)
        .map_err(|_| ProtectedKeyError::RootKey("operating system RNG unavailable".to_owned()))
}

/// A fresh random 256-bit key.
pub(crate) fn random_key() -> Result<LockedKey, ProtectedKeyError> {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    random(key.as_mut())?;
    LockedKey::from_bytes(key.as_slice())
}

/// Encrypts `plain` under `key`, binding `aad`; returns `nonce || ciphertext`.
pub(crate) fn seal(
    key: &[u8; KEY_LEN],
    aad: &[u8],
    plain: &[u8],
) -> Result<Vec<u8>, ProtectedKeyError> {
    let mut nonce = [0u8; NONCE_LEN];
    random(&mut nonce)?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let body = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
        .map_err(|_| ProtectedKeyError::Tampered)?;
    let mut out = Vec::with_capacity(NONCE_LEN + body.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decrypts `nonce || ciphertext` under `key`, checking `aad`.
pub(crate) fn open(
    key: &[u8; KEY_LEN],
    aad: &[u8],
    sealed: &[u8],
) -> Result<SecretBytes, ProtectedKeyError> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(ProtectedKeyError::Malformed("ciphertext is too short"));
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: body, aad })
        .map(SecretBytes::new)
        .map_err(|_| ProtectedKeyError::Tampered)
}

/// HMAC-SHA-256 of `value` under `key`, for the blind index.
pub(crate) fn blind_index(
    key: &[u8; KEY_LEN],
    value: &[u8],
) -> Result<[u8; INDEX_LEN], ProtectedKeyError> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key)
        .map_err(|_| ProtectedKeyError::Malformed("bad blind index key"))?;
    mac.update(value);
    let out = mac.finalize().into_bytes();
    let mut index = [0u8; INDEX_LEN];
    index.copy_from_slice(&out);
    Ok(index)
}

/// Constant-time equality of two blind indexes.
pub(crate) fn blind_index_eq(a: &[u8; INDEX_LEN], b: &[u8; INDEX_LEN]) -> bool {
    a.as_slice().ct_eq(b.as_slice()).into()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn seal_round_trips_and_binds_aad() {
        let key = random_key().expect("key");
        let sealed = seal(key.expose(), b"one", b"secret").expect("seal");
        let opened = open(key.expose(), b"one", &sealed).expect("open");
        assert_eq!(opened.expose(), b"secret");
        assert!(open(key.expose(), b"two", &sealed).is_err());
    }

    #[test]
    fn blind_index_is_stable_and_keyed() {
        let key = LockedKey::random().expect("key");
        let other = LockedKey::random().expect("key");
        let a = blind_index(key.expose(), b"DE89").expect("index");
        let b = blind_index(key.expose(), b"DE89").expect("index");
        let c = blind_index(other.expose(), b"DE89").expect("index");
        assert!(blind_index_eq(&a, &b));
        assert!(!blind_index_eq(&a, &c));
    }
}
