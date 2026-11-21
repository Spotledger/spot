//! The tenant root key and the provider that holds it.
//!
//! The root key is a key-encryption key in head-office key management (ADR-070 rule 12.13 e).
//! The hub wraps a group data key under it, so the group key's at-rest form is a wrapped key
//! and never the key itself. This crate ships the [`RootKeyProvider`] trait and a local
//! [`MemoryRootKeyProvider`] for tests; a KMS, an HSM or a key file adapter lives elsewhere.

use crate::bind;
use crate::crypto;
use crate::error::ProtectedKeyError;
use crate::ids::{GroupName, KeyEpoch, TenantId};
use crate::locked::LockedKey;
use spot_secrets::{KeyRef, SecretBytes, WrappedKey};
use zeroize::Zeroizing;

/// The tenant root key: an opaque handle (tenant plus the KEK reference in use). It holds no
/// material; the provider does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantRootKey {
    tenant: TenantId,
    key: KeyRef,
}

impl TenantRootKey {
    /// The tenant the root key serves.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The key-encryption-key reference new group keys are wrapped under.
    #[must_use]
    pub fn key_ref(&self) -> &KeyRef {
        &self.key
    }

    /// The KEK version.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.key.version
    }
}

/// Source of the tenant root key. It never hands out the root key itself; it only wraps and
/// unwraps group data keys, so a cloud KMS or HSM can implement it later.
pub trait RootKeyProvider: Send + Sync {
    /// The tenant this provider serves.
    fn tenant(&self) -> &TenantId;

    /// The root key new group data keys are wrapped under.
    fn root(&self) -> TenantRootKey;

    /// Wraps `data_key` under the root key, binding `aad`.
    fn wrap(&self, data_key: &LockedKey, aad: &[u8]) -> Result<WrappedKey, ProtectedKeyError>;

    /// Unwraps `wrapped`, checking `aad`.
    fn unwrap(&self, wrapped: &WrappedKey, aad: &[u8]) -> Result<SecretBytes, ProtectedKeyError>;
}

/// The local test provider: one random root key in this process's memory.
///
/// It is for tests and single-process use only; a real deployment uses a KMS or HSM adapter
/// that keeps the root key outside the application.
pub struct MemoryRootKeyProvider {
    tenant: TenantId,
    key: Zeroizing<[u8; crate::locked::KEY_LEN]>,
    id: String,
    version: u32,
}

impl MemoryRootKeyProvider {
    /// A provider for `tenant` with a fresh random root key.
    pub fn new(tenant: TenantId) -> Result<Self, ProtectedKeyError> {
        let key = LockedKey::random()?;
        Self::from_key(tenant, &key)
    }

    /// A provider for `tenant` with a caller-supplied root key (a test seam).
    pub fn from_key(tenant: TenantId, key: &LockedKey) -> Result<Self, ProtectedKeyError> {
        let mut copy = Zeroizing::new([0u8; crate::locked::KEY_LEN]);
        copy.copy_from_slice(key.expose());
        Ok(Self {
            tenant,
            key: copy,
            id: "memory-root".to_owned(),
            version: 1,
        })
    }

    /// The key-encryption-key id (always `memory-root`).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    fn matching(&self, key: &KeyRef) -> bool {
        key.id == self.id && key.version == self.version
    }
}

impl RootKeyProvider for MemoryRootKeyProvider {
    fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    fn root(&self) -> TenantRootKey {
        TenantRootKey {
            tenant: self.tenant.clone(),
            key: KeyRef {
                id: self.id.clone(),
                version: self.version,
            },
        }
    }

    fn wrap(&self, data_key: &LockedKey, aad: &[u8]) -> Result<WrappedKey, ProtectedKeyError> {
        let bytes = crypto::seal(&self.key, aad, data_key.expose())?;
        Ok(WrappedKey {
            key: self.root().key,
            bytes,
        })
    }

    fn unwrap(&self, wrapped: &WrappedKey, aad: &[u8]) -> Result<SecretBytes, ProtectedKeyError> {
        if !self.matching(&wrapped.key) {
            return Err(ProtectedKeyError::RootKey(format!(
                "no root key {} version {}",
                wrapped.key.id, wrapped.key.version
            )));
        }
        crypto::open(&self.key, aad, &wrapped.bytes)
    }
}

/// The associated data the hub binds a group data key to at the root layer.
#[must_use]
pub(crate) fn root_aad(tenant: &TenantId, group: &GroupName, epoch: KeyEpoch) -> Vec<u8> {
    bind::root_ad(tenant, group, epoch)
}
