//! The hub side of a sealed field: the private sealing key, and the only `unseal` in the
//! platform.
//!
//! This module exists **only in a hub build**. `lib.rs` gates it on `not(feature = "node")`,
//! so a node build (`--features node`) compiles it out and has no way to
//! open a sealed value: the type that could, and the function that does, are not in the
//! binary. A node holds a [`SealingKey`](crate::sealbox::SealingKey) (the public half) and nothing
//! more.
//!
//! [`HubKeys`] is the seam a real deployment implements over a KMS or an HSM: the private key
//! never has to enter the process. [`LocalHubKeys`] is the in-process reference used by tests
//! and a single-node hub.

use std::sync::Arc;

use spot_protected_keys::{GroupName, TenantId};
use spot_secrets::SecretBytes;

use crate::bind::SealedAd;
use crate::error::SealedFieldsError;
use crate::sealbox::{BoxSecretKey, HubPublicKey, Sealed, SealedWriter, SealingKey, open};

/// The hub's sealed-field key: a public half nodes seal to, and an unseal call.
pub trait HubKeys: Send + Sync {
    /// The tenant these keys serve.
    fn tenant(&self) -> &TenantId;

    /// The public key nodes seal values to.
    fn public_key(&self) -> HubPublicKey;

    /// The sealing key a node stores.
    fn sealing_key(&self) -> SealingKey {
        SealingKey::new(self.public_key())
    }

    /// Opens a value a node sealed, checking its place.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::Tampered`] for a value this key did not produce or a place it does
    /// not belong to.
    fn unseal(&self, sealed: &Sealed, ad: &SealedAd) -> Result<SecretBytes, SealedFieldsError>;
}

/// The in-process [`HubKeys`]: the private key in this process's memory.
pub struct LocalHubKeys {
    tenant: TenantId,
    secret: BoxSecretKey,
}

impl LocalHubKeys {
    /// A fresh hub key for `tenant`.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::Malformed`] when the operating system RNG is unavailable.
    pub fn generate(tenant: TenantId) -> Result<Self, SealedFieldsError> {
        Ok(Self {
            tenant,
            secret: BoxSecretKey::generate()?,
        })
    }

    /// A hub key from caller-supplied private material (a test seam).
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::Malformed`] when the material is not 32 bytes.
    pub fn from_secret(tenant: TenantId, bytes: &[u8]) -> Result<Self, SealedFieldsError> {
        Ok(Self {
            tenant,
            secret: BoxSecretKey::from_bytes(bytes)?,
        })
    }
}

impl HubKeys for LocalHubKeys {
    fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    fn public_key(&self) -> HubPublicKey {
        self.secret.public_key()
    }

    fn unseal(&self, sealed: &Sealed, ad: &SealedAd) -> Result<SecretBytes, SealedFieldsError> {
        open(sealed, &self.secret, ad)
    }
}

/// A hub that can write and read sealed values, for a test or a single-process hub.
pub struct LocalHub {
    keys: Arc<dyn HubKeys>,
}

impl LocalHub {
    /// A hub over `keys`.
    #[must_use]
    pub fn new(keys: Arc<dyn HubKeys>) -> Self {
        Self { keys }
    }

    /// A hub with a fresh local key for `tenant`.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::Malformed`] when the operating system RNG is unavailable.
    pub fn generate(tenant: TenantId) -> Result<Self, SealedFieldsError> {
        Ok(Self::new(Arc::new(LocalHubKeys::generate(tenant)?)))
    }

    /// The keys.
    #[must_use]
    pub fn keys(&self) -> &Arc<dyn HubKeys> {
        &self.keys
    }

    /// The public key a node seals to.
    #[must_use]
    pub fn public_key(&self) -> HubPublicKey {
        self.keys.public_key()
    }

    /// The writer a node uses (it needs only the public key).
    #[must_use]
    pub fn writer(&self) -> SealedWriter {
        SealedWriter::new(self.keys.sealing_key())
    }

    /// Opens a sealed value.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::Tampered`] for a value this hub did not receive or a moved place.
    pub fn unseal(&self, sealed: &Sealed, ad: &SealedAd) -> Result<SecretBytes, SealedFieldsError> {
        self.keys.unseal(sealed, ad)
    }

    /// The protection group a sealed value belongs to, as recorded next to it, for a log line.
    /// It carries no value.
    #[must_use]
    pub fn describe(&self, group: &GroupName, field: &str) -> String {
        format!("tenant={} group={group} field={field}", self.keys.tenant())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn tenant() -> TenantId {
        TenantId::new("acme").expect("tenant")
    }

    #[test]
    fn a_hub_unseal_round_trips() {
        let hub = LocalHub::generate(tenant()).expect("hub");
        let writer = hub.writer();
        let ad = SealedAd::of_field(tenant(), "Bank Account", "iban", "BA-1");
        let sealed = writer.seal(b"DE89", &ad).expect("seal");
        let opened = hub.unseal(&sealed, &ad).expect("unseal");
        assert_eq!(opened.expose(), b"DE89");
    }

    #[test]
    fn a_writer_needs_only_the_public_key() {
        let hub = LocalHub::generate(tenant()).expect("hub");
        let public = hub.public_key();
        let writer = SealedWriter::from_public_key(public);
        let sealed = writer.seal(b"x", &SealedAd::unbound()).expect("seal");
        assert!(hub.unseal(&sealed, &SealedAd::unbound()).is_ok());
    }

    #[test]
    fn another_hub_cannot_open_it() {
        let a = LocalHub::generate(tenant()).expect("hub");
        let b = LocalHub::generate(tenant()).expect("hub");
        let sealed = a.writer().seal(b"x", &SealedAd::unbound()).expect("seal");
        assert_eq!(
            b.unseal(&sealed, &SealedAd::unbound())
                .expect_err("wrong hub")
                .code(),
            crate::error::TAMPERED
        );
    }

    #[test]
    fn the_key_ring_prints_no_material() {
        let hub = LocalHub::generate(tenant()).expect("hub");
        let text = hub.describe(&GroupName::new("bank_details").expect("group"), "iban");
        assert!(text.contains("bank_details"));
        assert!(text.contains("acme"));
    }
}
