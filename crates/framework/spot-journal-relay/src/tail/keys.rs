//! The tenant journal key as the tail chunk sees it: a [`KeyProvider`] that wraps the data key of
//! each entry and derives the MAC key (`tail-mac`). The relay never reads key material from the
//! environment and never holds a key beyond one call.

use std::fmt;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use spot_secrets::{KeyProvider, KeyRef, SecretBox, SecretError};

/// The HKDF purpose of the MAC key (ADR-061 A.2.2).
pub const MAC_PURPOSE: &[u8] = b"tail-mac";
const MAC_KEY_LEN: usize = 32;
/// How many key versions a reader tries, newest first, before it gives up.
const MAX_VERSIONS_TRIED: u32 = 64;

type HmacSha256 = Hmac<Sha256>;

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
