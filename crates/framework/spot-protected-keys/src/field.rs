//! Field encryption and the blind index.
//!
//! [`FieldCipher`] is randomised authenticated encryption bound to the field's place
//! `(tenant, doctype, field, document, epoch)`, so a ciphertext copied to another field,
//! document or tenant fails to open. There is no deterministic mode; the only deterministic
//! function is the separately named [`BlindIndexKey`], an HMAC under its own key, for equality
//! lookups on fields such as an IBAN.

use crate::bind;
use crate::crypto;
use crate::error::ProtectedKeyError;
use crate::ids::{KeyEpoch, TenantId};
use crate::locked::LockedKey;
use serde::{Deserialize, Serialize};
use spot_secrets::SecretBytes;
use std::fmt;

/// Where a protected value lives: the associated data that binds its ciphertext.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldAd {
    tenant: TenantId,
    doctype: String,
    field: String,
    document: String,
    epoch: KeyEpoch,
}

impl FieldAd {
    /// The place of one protected value.
    #[must_use]
    pub fn new(
        tenant: TenantId,
        doctype: impl Into<String>,
        field: impl Into<String>,
        document: impl Into<String>,
        epoch: KeyEpoch,
    ) -> Self {
        Self {
            tenant,
            doctype: doctype.into(),
            field: field.into(),
            document: document.into(),
            epoch,
        }
    }

    /// The tenant.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The DocType.
    #[must_use]
    pub fn doctype(&self) -> &str {
        &self.doctype
    }

    /// The field.
    #[must_use]
    pub fn field(&self) -> &str {
        &self.field
    }

    /// The document id.
    #[must_use]
    pub fn document(&self) -> &str {
        &self.document
    }

    /// The epoch.
    #[must_use]
    pub fn epoch(&self) -> KeyEpoch {
        self.epoch
    }

    /// A copy of this place with a different epoch (for re-encryption).
    #[must_use]
    pub fn with_epoch(&self, epoch: KeyEpoch) -> Self {
        let mut out = self.clone();
        out.epoch = epoch;
        out
    }
}

/// Randomised AEAD for one protected field.
pub struct FieldCipher;

impl FieldCipher {
    /// Encrypts `plaintext` under `key`, binding `ad`.
    pub fn encrypt(
        key: &LockedKey,
        ad: &FieldAd,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, ProtectedKeyError> {
        crypto::seal(
            key.expose(),
            &bind::field_ad(&ad.tenant, &ad.doctype, &ad.field, &ad.document, ad.epoch),
            plaintext,
        )
    }

    /// Decrypts `sealed` under `key`, checking `ad`. A moved ciphertext fails.
    pub fn decrypt(
        key: &LockedKey,
        ad: &FieldAd,
        sealed: &[u8],
    ) -> Result<SecretBytes, ProtectedKeyError> {
        crypto::open(
            key.expose(),
            &bind::field_ad(&ad.tenant, &ad.doctype, &ad.field, &ad.document, ad.epoch),
            sealed,
        )
    }
}

/// The key of the blind index: a separate key from the data key, never the same material.
pub struct BlindIndexKey(LockedKey);

impl BlindIndexKey {
    /// A fresh random blind-index key.
    pub fn generate() -> Result<Self, ProtectedKeyError> {
        Ok(Self(LockedKey::random()?))
    }

    /// A blind-index key from caller-supplied material (a test seam).
    #[must_use]
    pub fn from_key(key: LockedKey) -> Self {
        Self(key)
    }

    /// The index of `value`: deterministic, so an equality lookup can find it. It is a keyed
    /// hash, not a cipher, and it deliberately leaks equality only.
    #[must_use]
    pub fn index(&self, value: &[u8]) -> BlindIndex {
        match crypto::blind_index(self.0.expose(), value) {
            Ok(bytes) => BlindIndex(bytes),
            // HMAC accepts any key length, so this is unreachable; the fallback is an index
            // that matches nothing rather than a panic.
            Err(_) => BlindIndex([0u8; crypto::INDEX_LEN]),
        }
    }
}

/// A blind index: the keyed hash of a lookup value, safe to store and to index.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlindIndex([u8; crypto::INDEX_LEN]);

impl BlindIndex {
    /// The raw bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; crypto::INDEX_LEN] {
        self.0
    }

    /// Constant-time equality, for a lookup.
    #[must_use]
    pub fn matches(&self, other: &BlindIndex) -> bool {
        crypto::blind_index_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for BlindIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlindIndex({self})")
    }
}

impl fmt::Display for BlindIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
