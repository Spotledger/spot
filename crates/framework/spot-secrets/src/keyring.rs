//! Key state shared by the in-memory, file and environment providers.

use crate::crypto::{self, KEY_LEN};
use crate::error::SecretError;
use crate::provider::{KeyProvider, KeyRef, ProviderFuture, WrappedKey, fresh_key};
use crate::secret::SecretBytes;
use std::collections::BTreeMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use zeroize::Zeroizing;

/// `None` marks a retired version: its key material is gone.
pub(crate) type Entry = Option<Zeroizing<[u8; KEY_LEN]>>;

#[derive(Clone)]
pub(crate) struct KeyState {
    pub(crate) id: String,
    pub(crate) current: u32,
    pub(crate) entries: BTreeMap<u32, Entry>,
}

pub(crate) fn read(lock: &RwLock<KeyState>) -> RwLockReadGuard<'_, KeyState> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn write(lock: &RwLock<KeyState>) -> RwLockWriteGuard<'_, KeyState> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

impl KeyState {
    pub(crate) fn single(id: &str, version: u32, key: Zeroizing<[u8; KEY_LEN]>) -> Self {
        Self {
            id: id.to_owned(),
            current: version,
            entries: BTreeMap::from([(version, Some(key))]),
        }
    }

    pub(crate) fn current_ref(&self) -> KeyRef {
        KeyRef {
            id: self.id.clone(),
            version: self.current,
        }
    }

    /// Associated data for the KEK layer: binds the key identity and the caller's `aad`.
    fn kek_aad(id: &str, version: u32, aad: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + id.len() + aad.len());
        out.extend_from_slice(b"spot-secrets/wrap/v1\0");
        out.extend_from_slice(id.as_bytes());
        out.push(0);
        out.extend_from_slice(&version.to_be_bytes());
        out.extend_from_slice(aad);
        out
    }

    pub(crate) fn wrap(&self, data_key: &[u8], aad: &[u8]) -> Result<WrappedKey, SecretError> {
        let Some(Some(kek)) = self.entries.get(&self.current) else {
            return Err(SecretError::Provider(
                "current key has no key material".into(),
            ));
        };
        let bytes = crypto::encrypt(kek, &Self::kek_aad(&self.id, self.current, aad), data_key)?;
        Ok(WrappedKey {
            key: self.current_ref(),
            bytes,
        })
    }

    pub(crate) fn unwrap(
        &self,
        key: &KeyRef,
        wrapped: &WrappedKey,
        aad: &[u8],
    ) -> Result<SecretBytes, SecretError> {
        let unknown = || SecretError::UnknownKey {
            id: key.id.clone(),
            version: key.version,
        };
        if key.id != self.id {
            return Err(unknown());
        }
        match self.entries.get(&key.version) {
            None => Err(unknown()),
            Some(None) => Err(SecretError::RetiredKey {
                id: key.id.clone(),
                version: key.version,
            }),
            Some(Some(kek)) => {
                let plain = crypto::decrypt(
                    kek,
                    &Self::kek_aad(&self.id, key.version, aad),
                    &wrapped.bytes,
                )?;
                Ok(SecretBytes::new(plain.to_vec()))
            }
        }
    }

    /// The KEK of `key`'s version, for a derivation that never lets it leave this module.
    fn kek_of(&self, key: &KeyRef) -> Result<&Zeroizing<[u8; KEY_LEN]>, SecretError> {
        let unknown = || SecretError::UnknownKey {
            id: key.id.clone(),
            version: key.version,
        };
        if key.id != self.id {
            return Err(unknown());
        }
        match self.entries.get(&key.version) {
            None => Err(unknown()),
            Some(None) => Err(SecretError::RetiredKey {
                id: key.id.clone(),
                version: key.version,
            }),
            Some(Some(kek)) => Ok(kek),
        }
    }

    /// The blind-index key of `key`'s version, derived from that version's KEK
    /// ([`crate::kdf::INDEX_KEY_INFO`]). The KEK itself never leaves this function.
    pub(crate) fn index_key(&self, key: &KeyRef) -> Result<SecretBytes, SecretError> {
        let kek = self.kek_of(key)?;
        let derived = crate::kdf::hkdf_sha256(kek.as_slice(), crate::kdf::INDEX_KEY_INFO)
            .map_err(|e| SecretError::IndexKey(e.to_string()))?;
        Ok(SecretBytes::new(derived.to_vec()))
    }

    /// A key derived from `key`'s version of the KEK with `info` and `len`
    /// ([`crate::kdf::derive_key`]). The KEK itself never leaves this function.
    pub(crate) fn derive_key(
        &self,
        key: &KeyRef,
        info: &[u8],
        len: usize,
    ) -> Result<SecretBytes, SecretError> {
        crate::kdf::derive_key(self.kek_of(key)?.as_slice(), info, len)
    }

    /// The state after adding version n+1 and making it current.
    pub(crate) fn rotated(&self) -> Result<Self, SecretError> {
        let next = self
            .entries
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        let mut state = self.clone();
        state.entries.insert(next, Some(fresh_key()?));
        state.current = next;
        Ok(state)
    }

    /// The state after retiring `version`. Refuses while values still use it.
    pub(crate) fn retired(
        &self,
        version: u32,
        values_still_use: bool,
    ) -> Result<Self, SecretError> {
        if version == self.current {
            return Err(SecretError::Provider(
                "cannot retire the current key version".into(),
            ));
        }
        if !self.entries.contains_key(&version) {
            return Err(SecretError::UnknownKey {
                id: self.id.clone(),
                version,
            });
        }
        if values_still_use {
            return Err(SecretError::Provider(format!(
                "key version {version} is still in use; rewrap its values first"
            )));
        }
        let mut state = self.clone();
        state.entries.insert(version, None);
        Ok(state)
    }
}

/// In-memory keyring for tests. Keys vanish when it is dropped.
pub struct MemoryKeyring(RwLock<KeyState>);

impl MemoryKeyring {
    /// A keyring named `id` with a random version 1 key.
    pub fn new(id: &str) -> Result<Self, SecretError> {
        Ok(Self(RwLock::new(KeyState::single(id, 1, fresh_key()?))))
    }

    /// Adds version n+1 and makes it current; returns the new version.
    pub fn rotate(&self) -> Result<u32, SecretError> {
        let mut state = write(&self.0);
        *state = state.rotated()?;
        Ok(state.current)
    }

    /// Drops the key material of `version`. Refuses while `values_still_use` is true, and
    /// always for the current version.
    pub fn retire(&self, version: u32, values_still_use: bool) -> Result<(), SecretError> {
        let mut state = write(&self.0);
        *state = state.retired(version, values_still_use)?;
        Ok(())
    }
}

impl KeyProvider for MemoryKeyring {
    fn current(&self) -> KeyRef {
        read(&self.0).current_ref()
    }

    fn wrap<'a>(&'a self, data_key: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey> {
        Box::pin(async move { read(&self.0).wrap(data_key, aad) })
    }

    fn unwrap<'a>(
        &'a self,
        key: &'a KeyRef,
        wrapped: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move { read(&self.0).unwrap(key, wrapped, aad) })
    }

    fn index_key<'a>(&'a self, key: &'a KeyRef) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move { read(&self.0).index_key(key) })
    }

    fn derive_key<'a>(
        &'a self,
        key: &'a KeyRef,
        info: &'a [u8],
        len: usize,
    ) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move { read(&self.0).derive_key(key, info, len) })
    }
}
