//! The only place that touches the AEAD primitive.

use crate::error::SecretError;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::TryRngCore;
use rand::rngs::OsRng;
use zeroize::Zeroizing;

pub(crate) const KEY_LEN: usize = 32;
pub(crate) const NONCE_LEN: usize = 24;

/// Fills `buf` from the operating system RNG.
pub(crate) fn random(buf: &mut [u8]) -> Result<(), SecretError> {
    OsRng
        .try_fill_bytes(buf)
        .map_err(|_| SecretError::Provider("operating system RNG unavailable".into()))
}

/// A fresh random 256-bit key.
pub(crate) fn random_key() -> Result<Zeroizing<[u8; KEY_LEN]>, SecretError> {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    random(key.as_mut())?;
    Ok(key)
}

/// Encrypts `plain`; returns `nonce || ciphertext`.
pub(crate) fn encrypt(
    key: &[u8; KEY_LEN],
    aad: &[u8],
    plain: &[u8],
) -> Result<Vec<u8>, SecretError> {
    let mut nonce = [0u8; NONCE_LEN];
    random(&mut nonce)?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let body = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
        .map_err(|_| SecretError::Tampered)?;
    let mut out = Vec::with_capacity(NONCE_LEN + body.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decrypts `nonce || ciphertext`.
pub(crate) fn decrypt(
    key: &[u8; KEY_LEN],
    aad: &[u8],
    sealed: &[u8],
) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    if sealed.len() < NONCE_LEN {
        return Err(SecretError::Malformed("ciphertext shorter than nonce"));
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: body, aad })
        .map(Zeroizing::new)
        .map_err(|_| SecretError::Tampered)
}
