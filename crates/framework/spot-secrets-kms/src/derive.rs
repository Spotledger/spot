//! Blind-index and derived keys from the seed: HKDF-SHA256 with a salt, the same `info`
//! conventions as `spot-secrets`.

use crate::seed::Seed;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use spot_secrets::{
    DERIVE_PREFIX, INDEX_KEY_INFO, MAX_DERIVE_INFO_LEN, MAX_DERIVED_LEN, MIN_DERIVED_LEN,
    SecretBytes, SecretError,
};
use zeroize::Zeroizing;

type HmacSha256 = Hmac<Sha256>;

/// The logical key version the seed's derivations are salted with. A transit rotation changes
/// the wrapping key, not the seed, so this stays 1 for the life of the seed: a blind index must
/// not change when the KMS key rotates.
const LOGICAL_VERSION: u32 = 1;

/// The length of a blind-index key, in bytes.
const INDEX_KEY_LEN: usize = 32;

fn salt(key_id: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"spot_kms_derive_salt_v1");
    hash.update([0]);
    hash.update(key_id.as_bytes());
    hash.update([0]);
    hash.update(LOGICAL_VERSION.to_be_bytes());
    hash.finalize().into()
}

fn mac(key: &[u8], parts: &[&[u8]]) -> Result<[u8; 32], SecretError> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| SecretError::DeriveKey("the MAC rejected the key length"))?;
    for part in parts {
        mac.update(part);
    }
    Ok(mac.finalize().into_bytes().into())
}

/// HKDF-SHA256 (RFC 5869) over `seed` with the salt of `key_id`: extract once, then expand
/// `ceil(len / 32)` blocks.
fn hkdf(seed: &Seed, key_id: &str, info: &[u8], len: usize) -> Result<SecretBytes, SecretError> {
    let prk = Zeroizing::new(mac(&salt(key_id), &[seed.expose()])?);
    let mut out: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::with_capacity(len + 32));
    let mut previous: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::new());
    let mut counter: u8 = 1;
    while out.len() < len {
        let block = Zeroizing::new(mac(
            prk.as_slice(),
            &[previous.as_slice(), info, &[counter]],
        )?);
        out.extend_from_slice(block.as_slice());
        previous = Zeroizing::new(block.to_vec());
        counter = counter.checked_add(1).ok_or(SecretError::DeriveKey(
            "the requested length needs too many blocks",
        ))?;
    }
    out.truncate(len);
    Ok(SecretBytes::new(out.to_vec()))
}

/// The blind-index key of `key_id`.
pub(crate) fn index_key(seed: &Seed, key_id: &str) -> Result<SecretBytes, SecretError> {
    hkdf(seed, key_id, INDEX_KEY_INFO, INDEX_KEY_LEN)
}

/// A derived key: `info` is 1 to 256 bytes and `len` 16 to 64, as in `spot-secrets`.
pub(crate) fn derive_key(
    seed: &Seed,
    key_id: &str,
    info: &[u8],
    len: usize,
) -> Result<SecretBytes, SecretError> {
    if info.is_empty() {
        return Err(SecretError::DeriveKey("the info label is empty"));
    }
    if info.len() > MAX_DERIVE_INFO_LEN {
        return Err(SecretError::DeriveKey("the info label is too long"));
    }
    if !(MIN_DERIVED_LEN..=MAX_DERIVED_LEN).contains(&len) {
        return Err(SecretError::DeriveKey(
            "the requested length is outside 16 to 64 bytes",
        ));
    }
    let mut full = Zeroizing::new(Vec::with_capacity(DERIVE_PREFIX.len() + info.len()));
    full.extend_from_slice(DERIVE_PREFIX);
    full.extend_from_slice(info);
    hkdf(seed, key_id, full.as_slice(), len)
}
