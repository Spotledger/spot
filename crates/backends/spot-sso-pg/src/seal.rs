//! Keyed digests and sealing for the pending-authorisation store.
//!
//! A pending record holds the `state`, the `nonce` and the PKCE verifier, which the callback
//! needs back in the clear but which must never sit in the database in the clear. The store
//! therefore keeps the record **sealed** (authenticated encryption bound to the tenant and the
//! row) and finds it by a **keyed digest** of the browser binding, never the binding itself.
//!
//! Nothing here reads entropy: the nonce source is injected ([`AesGcmSealer::new`]), so a test
//! states it and the production caller supplies its CSPRNG (or the secrets service).

use std::fmt;
use std::sync::Arc;

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::hmac;

/// The length in bytes of the keys this module takes.
pub const KEY_LEN: usize = 32;

/// Seals and opens a record. `aad` is authenticated, not encrypted: the store passes the tenant
/// and the row key, so a sealed value copied to another row or tenant does not open.
///
/// A failure is `None` and carries nothing: the store treats it as "cannot vouch" and refuses.
pub trait Sealer: Send + Sync {
    /// Seals `plaintext`; `None` when it cannot.
    fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Option<Vec<u8>>;
    /// Opens a value [`Sealer::seal`] produced for the same `aad`; `None` when it does not open.
    fn open(&self, aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>>;
}

/// A source of one fresh, never-repeating 96-bit nonce per call. The caller guarantees
/// uniqueness per key (a random draw from a CSPRNG is the usual way).
pub type NonceSource = Arc<dyn Fn() -> [u8; NONCE_LEN] + Send + Sync>;

/// AES-256-GCM sealing with the nonce prepended: `nonce || ciphertext || tag`.
pub struct AesGcmSealer {
    key: LessSafeKey,
    nonces: NonceSource,
}

impl fmt::Debug for AesGcmSealer {
    /// Never shows the key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AesGcmSealer").finish_non_exhaustive()
    }
}

impl AesGcmSealer {
    /// A sealer with a 32-byte `key` (held by `spot-secrets`) and a nonce source; `None` when the
    /// key is not 32 bytes.
    #[must_use]
    pub fn new(key: &[u8], nonces: NonceSource) -> Option<Self> {
        let key = UnboundKey::new(&AES_256_GCM, key).ok()?;
        Some(Self {
            key: LessSafeKey::new(key),
            nonces,
        })
    }
}

impl Sealer for AesGcmSealer {
    fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Option<Vec<u8>> {
        let nonce_bytes = (self.nonces)();
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut buffer = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(nonce, Aad::from(aad), &mut buffer)
            .ok()?;
        let mut sealed = Vec::with_capacity(NONCE_LEN + buffer.len());
        sealed.extend_from_slice(&nonce_bytes);
        sealed.extend_from_slice(&buffer);
        Some(sealed)
    }

    fn open(&self, aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < NONCE_LEN {
            return None;
        }
        let (nonce_bytes, rest) = sealed.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce_bytes).ok()?;
        let mut buffer = rest.to_vec();
        let plaintext = self
            .key
            .open_in_place(nonce, Aad::from(aad), &mut buffer)
            .ok()?;
        Some(plaintext.to_vec())
    }
}

/// The 8-byte big-endian length of `text`, the prefix that keeps parts from running together.
fn len_prefix(text: &str) -> [u8; 8] {
    u64::try_from(text.len()).unwrap_or(u64::MAX).to_be_bytes()
}

/// The keyed digest of a browser binding: HMAC-SHA256 over the length-prefixed tenant and
/// binding, so no choice of either can collide with another pair and a database dump alone
/// cannot be used to guess a binding.
pub(crate) fn binding_digest(key: &hmac::Key, tenant: &str, binding: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(tenant.len() + binding.len() + 16);
    message.extend_from_slice(&len_prefix(tenant));
    message.extend_from_slice(tenant.as_bytes());
    message.extend_from_slice(&len_prefix(binding));
    message.extend_from_slice(binding.as_bytes());
    hmac::sign(key, &message).as_ref().to_vec()
}

/// The authenticated data of a sealed record: the tenant and the row's digest.
pub(crate) fn aad(tenant: &str, digest: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(tenant.len() + digest.len() + 8);
    data.extend_from_slice(&len_prefix(tenant));
    data.extend_from_slice(tenant.as_bytes());
    data.extend_from_slice(digest);
    data
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn sealer() -> AesGcmSealer {
        let counter = AtomicU64::new(1);
        let nonces: NonceSource = Arc::new(move || {
            let mut nonce = [0_u8; NONCE_LEN];
            nonce[..8].copy_from_slice(&counter.fetch_add(1, Ordering::SeqCst).to_be_bytes());
            nonce
        });
        match AesGcmSealer::new(&[7_u8; KEY_LEN], nonces) {
            Some(sealer) => sealer,
            None => panic!("a 32-byte key builds a sealer"),
        }
    }

    #[test]
    fn a_sealed_value_opens_only_with_its_aad_and_hides_the_text() {
        let sealer = sealer();
        let sealed = sealer.seal(b"row-1", b"state-secret-value");
        let Some(sealed) = sealed else {
            panic!("sealing works");
        };
        assert!(!String::from_utf8_lossy(&sealed).contains("state-secret-value"));
        assert_eq!(
            sealer.open(b"row-1", &sealed).as_deref(),
            Some(&b"state-secret-value"[..])
        );
        assert_eq!(sealer.open(b"row-2", &sealed), None, "another row");
        let mut tampered = sealed.clone();
        if let Some(last) = tampered.last_mut() {
            *last ^= 1;
        }
        assert_eq!(sealer.open(b"row-1", &tampered), None, "tampered");
        assert_eq!(sealer.open(b"row-1", &sealed[..5]), None, "truncated");
    }

    #[test]
    fn a_short_key_builds_nothing() {
        let nonces: NonceSource = Arc::new(|| [0_u8; NONCE_LEN]);
        assert!(AesGcmSealer::new(&[1_u8; 16], nonces).is_none());
    }

    #[test]
    fn the_digest_depends_on_key_tenant_and_binding() {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &[1_u8; KEY_LEN]);
        let other = hmac::Key::new(hmac::HMAC_SHA256, &[2_u8; KEY_LEN]);
        let base = binding_digest(&key, "a", "bc");
        assert_eq!(base.len(), 32);
        assert_ne!(base, binding_digest(&key, "ab", "c"));
        assert_ne!(base, binding_digest(&key, "a", "bd"));
        assert_ne!(base, binding_digest(&other, "a", "bc"));
        assert_eq!(base, binding_digest(&key, "a", "bc"));
    }
}
