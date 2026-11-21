//! The tenant journal key as the tail chunk sees it: a [`KeyProvider`] that wraps the data key of
//! each entry and derives the MAC key (`tail-mac`). The relay never reads key material from the
//! environment and never holds a key beyond one call.

use std::fmt;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use spot_secrets::{EnvKey, KeyProvider, KeyRef, SecretBox, SecretError};

/// The HKDF purpose of the MAC key (ADR-061 A.2.2).
pub const MAC_PURPOSE: &[u8] = b"tail-mac";
const MAC_KEY_LEN: usize = 32;
/// How many key versions a reader tries, newest first, before it gives up.
const MAX_VERSIONS_TRIED: u32 = 64;

/// The domain of the public test keys of [`TailKeys::for_tests`].
const TEST_KEY_DOMAIN: &[u8] = b"spot.journal.tail.test-keys.v1\0";

type HmacSha256 = Hmac<Sha256>;

/// Standard base64 with padding (the form `EnvKey::from_base64` reads), for the test keys only.
fn base64_std(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let word = (b0 << 16) | (b1 << 8) | b2;
        let pick = |shift: u32| char::from(ALPHABET[((word >> shift) & 0x3f) as usize]);
        out.push(pick(18));
        out.push(pick(12));
        out.push(if chunk.len() > 1 { pick(6) } else { '=' });
        out.push(if chunk.len() > 2 { pick(0) } else { '=' });
    }
    out
}

/// The tenant's journal key, shared by the relay and every reader of its tail chunks.
#[derive(Clone)]
pub struct TailKeys {
    provider: Arc<dyn KeyProvider>,
}

impl fmt::Debug for TailKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TailKeys(..)")
    }
}

impl TailKeys {
    /// Keys backed by the tenant's journal key provider.
    #[must_use]
    pub fn new(provider: Arc<dyn KeyProvider>) -> Self {
        Self { provider }
    }

    /// Fixed, **public** keys for the tests of this crate and of its callers (relay, commit,
    /// protect, e2e): the tenant journal key is a KEK derived from the tenant id and a constant
    /// string, so the same tenant always gets the same keys, in every process, and two tenants
    /// never share one. They protect nothing: the derivation is in the source. Production code
    /// builds [`TailKeys::new`] over the tenant's real journal key provider, and
    /// [`crate::RelayConfig::new`] has no default keys (a test pins that).
    ///
    /// # Panics
    /// Never in practice: the derived key is always 32 bytes, which the provider accepts.
    #[doc(hidden)]
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn for_tests(tenant: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(TEST_KEY_DOMAIN);
        hasher.update(tenant.as_bytes());
        let kek: [u8; 32] = hasher.finalize().into();
        let provider = EnvKey::from_base64(&base64_std(&kek)).expect("a 32 byte test key");
        Self::new(Arc::new(provider))
    }

    /// The seal/open box over the provider (data keys wrapped by the tenant journal key).
    pub(crate) fn secret_box(&self) -> SecretBox<Arc<dyn KeyProvider>> {
        SecretBox::new(Arc::clone(&self.provider))
    }

    /// The MAC of `parts` (concatenated) under the key derived from the current key version.
    pub(crate) async fn mac_current(&self, parts: &[&[u8]]) -> Result<[u8; 32], SecretError> {
        let current = self.provider.current();
        let key = self
            .provider
            .derive_key(&current, MAC_PURPOSE, MAC_KEY_LEN)
            .await?;
        let mut mac = HmacSha256::new_from_slice(key.expose())
            .map_err(|_| SecretError::DeriveKey("the derived key has an unusable length"))?;
        for part in parts {
            mac.update(part);
        }
        Ok(mac.finalize().into_bytes().into())
    }

    /// Whether `expected` is the MAC of `parts` under the key of the current version or of an
    /// older one (the layout carries no key version, so a reader tries them newest first). The
    /// comparison is constant time. A version the provider no longer has is skipped.
    pub(crate) async fn verify_mac(&self, parts: &[&[u8]], expected: &[u8]) -> bool {
        let current = self.provider.current();
        let lowest = current
            .version
            .saturating_sub(MAX_VERSIONS_TRIED.saturating_sub(1))
            .max(1);
        let mut version = current.version;
        while version >= lowest {
            let candidate = KeyRef {
                id: current.id.clone(),
                version,
            };
            if let Ok(key) = self
                .provider
                .derive_key(&candidate, MAC_PURPOSE, MAC_KEY_LEN)
                .await
                && let Ok(mut mac) = HmacSha256::new_from_slice(key.expose())
            {
                for part in parts {
                    mac.update(part);
                }
                if mac.verify_slice(expected).is_ok() {
                    return true;
                }
            }
            version -= 1;
        }
        false
    }
}
