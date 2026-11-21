//! The only place that touches the AEAD primitive, the digest and the manifest
//! authenticator.
//!
//! Every chunk is encrypted with XChaCha20-Poly1305 under one per-backup **data key**.
//! The data key never leaves the customer's machine except wrapped under the tenant
//! recovery key (and, when the customer explicitly opts in, an escrow key). The
//! associated data binds a ciphertext to its backup and chunk index, so a chunk copied
//! from another backup — or swapped with its neighbour — fails to open.
//!
//! The manifest authenticator is HMAC-SHA256 under a key derived from the data key with
//! a domain-separation label. HMAC is built on `sha2` here because the workspace has no
//! HMAC crate and the construction is small, fixed and fully covered by tests.

use std::fmt;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::TryRngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::error::{CloudBackupError, Result};

/// Length of a data key, a recovery key and a derived manifest key, in bytes.
pub const KEY_LEN: usize = 32;
/// Length of the XChaCha20 nonce, in bytes.
pub const NONCE_LEN: usize = 24;
/// Length of the Poly1305 authentication tag, in bytes.
pub const TAG_LEN: usize = 16;
/// Domain-separation label for the manifest authenticator key.
const MANIFEST_KEY_LABEL: &[u8] = b"spot-cloud-backup/manifest-key/v1";
/// SHA-256 block size, for the HMAC construction.
const SHA256_BLOCK: usize = 64;

/// A per-backup data key. Redacted in `Debug` and zeroed on drop.
pub struct DataKey(Zeroizing<[u8; KEY_LEN]>);

impl DataKey {
    /// A fresh random data key from the operating system RNG.
    ///
    /// # Errors
    /// [`CloudBackupError::Store`] when the OS RNG is unavailable (never in practice,
    /// and never by reading the clock).
    pub fn generate() -> Result<Self> {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        OsRng
            .try_fill_bytes(key.as_mut())
            .map_err(|_| CloudBackupError::Store {
                reason: "the operating system RNG is unavailable".to_owned(),
            })?;
        Ok(Self(key))
    }

    /// Wraps raw key bytes (used when a key travels wrapped and has just been unwrapped).
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The raw key bytes. Callers must not log or return them.
    pub fn expose(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl fmt::Debug for DataKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DataKey([redacted])")
    }
}

/// Fills `buffer` from the operating system RNG.
///
/// # Errors
/// [`CloudBackupError::Store`] when the OS RNG is unavailable.
pub fn random_bytes(buffer: &mut [u8]) -> Result<()> {
    OsRng
        .try_fill_bytes(buffer)
        .map_err(|_| CloudBackupError::Store {
            reason: "the operating system RNG is unavailable".to_owned(),
        })
}

/// A random lowercase hex string of `bytes` random bytes.
///
/// # Errors
/// [`CloudBackupError::Store`] when the OS RNG is unavailable.
pub fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0u8; bytes];
    random_bytes(&mut buffer)?;
    Ok(hex(&buffer))
}

/// Encrypts `plain` under `key`, returning `nonce || ciphertext || tag`.
///
/// Returns `None` only when the AEAD refuses the input, which it does not for the
/// lengths this crate passes; callers map that to their own stage error.
pub fn encrypt(key: &[u8; KEY_LEN], aad: &[u8], plain: &[u8]) -> Option<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    random_bytes(&mut nonce).ok()?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let body = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
        .ok()?;
    let mut out = Vec::with_capacity(NONCE_LEN + body.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&body);
    Some(out)
}

/// Decrypts `nonce || ciphertext || tag` under `key`, binding `aad`.
///
/// Returns `None` when the tag does not verify: the ciphertext, the key or the
/// associated data was altered. Nothing about the failure leaks, by construction.
pub fn decrypt(key: &[u8; KEY_LEN], aad: &[u8], sealed: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return None;
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: body, aad })
        .ok()
        .map(Zeroizing::new)
}

/// The associated data of one chunk: the backup id and the chunk index, framed so that
/// no two (backup, index) pairs share a byte string.
pub fn chunk_aad(backup_id: &str, index: u32) -> Vec<u8> {
    let mut aad = Vec::with_capacity(28 + backup_id.len());
    aad.extend_from_slice(b"spot-cloud-backup/chunk/v1\0");
    aad.extend_from_slice(backup_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(&index.to_be_bytes());
    aad
}

/// The associated data of a key wrap: the tenant, the backup id and the wrap label.
pub fn key_aad(tenant: &str, backup_id: &str, label: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(36 + tenant.len() + backup_id.len() + label.len());
    aad.extend_from_slice(b"spot-cloud-backup/key/v1\0");
    aad.extend_from_slice(tenant.as_bytes());
    aad.push(0);
    aad.extend_from_slice(backup_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(label.as_bytes());
    aad
}

/// The SHA-256 digest of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// The lowercase hex SHA-256 digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&sha256(bytes))
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// Parses exactly 64 lowercase hex digits into a SHA-256 digest.
pub fn parse_sha256(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return None;
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *slot = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// Compares two byte strings without an early exit on the first difference.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

/// The manifest authenticator key: `SHA-256(label || data_key)`.
pub fn manifest_key(data_key: &DataKey) -> Zeroizing<[u8; KEY_LEN]> {
    let mut hasher = Sha256::new();
    hasher.update(MANIFEST_KEY_LABEL);
    hasher.update(data_key.expose());
    Zeroizing::new(hasher.finalize().into())
}

/// HMAC-SHA256 of `message` under `key`, as in RFC 2104.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; SHA256_BLOCK];
    if key.len() > SHA256_BLOCK {
        block[..KEY_LEN].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; SHA256_BLOCK];
    let mut outer_pad = [0x5cu8; SHA256_BLOCK];
    for index in 0..SHA256_BLOCK {
        inner_pad[index] ^= block[index];
        outer_pad[index] ^= block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_1() {
        // RFC 4231 test case 1: 20 bytes of 0x0b, "Hi There".
        let key = [0x0bu8; 20];
        let tag = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex(&tag),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_hashes_keys_longer_than_a_block() {
        // RFC 4231 test case 6: a 131-byte key.
        let key = [0xaau8; 131];
        let tag = hmac_sha256(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&tag),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn digest_round_trips_through_hex() {
        let digest = sha256(b"spot");
        assert_eq!(parse_sha256(&hex(&digest)), Some(digest));
        assert_eq!(parse_sha256(&hex(&digest).to_uppercase()), None);
        assert_eq!(parse_sha256("ab"), None);
    }

    #[test]
    fn aad_frames_backup_and_index() {
        assert_ne!(chunk_aad("bk-1", 1), chunk_aad("bk-1", 2));
        assert_ne!(chunk_aad("bk-1", 1), chunk_aad("bk-1x", 1));
        assert_ne!(
            key_aad("acme", "bk-1", "recovery"),
            key_aad("acme", "bk-1", "escrow")
        );
    }

    #[test]
    fn wrong_key_and_tampering_both_fail_cleanly() {
        let key = DataKey::generate().expect("rng");
        let other = DataKey::generate().expect("rng");
        let sealed = encrypt(key.expose(), b"aad", b"plaintext").expect("encrypt");
        assert!(decrypt(key.expose(), b"aad", &sealed).is_some());
        assert!(decrypt(other.expose(), b"aad", &sealed).is_none());
        assert!(decrypt(key.expose(), b"other", &sealed).is_none());
        let mut flipped = sealed.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0x01;
        assert!(decrypt(key.expose(), b"aad", &flipped).is_none());
        assert!(decrypt(key.expose(), b"aad", &sealed[..4]).is_none());
    }
}
