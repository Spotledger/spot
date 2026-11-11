use crate::crypto::{self, KEY_LEN};
use crate::error::SecretError;
use crate::keyring::KeyState;
use crate::secret::SecretBytes;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use zeroize::Zeroizing;

/// Boxed `Send` future returned by [`KeyProvider`] methods, so the trait stays object-safe.
pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SecretError>> + Send + 'a>>;

/// Identifies one version of a key-encryption key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyRef {
    /// Key id (a keyring name, a KMS key id).
    pub id: String,
    /// Key version, starting at 1.
    pub version: u32,
}

/// A data key wrapped under a KEK, with the KEK version that wrapped it.
#[derive(Clone, PartialEq, Eq)]
pub struct WrappedKey {
    /// The KEK that wrapped the data key.
    pub key: KeyRef,
    /// Provider-defined wrapped bytes.
    pub bytes: Vec<u8>,
}

impl fmt::Debug for WrappedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WrappedKey")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Source of key-encryption keys. It never hands out the KEK itself, so a cloud KMS or HSM
/// provider can implement it later.
pub trait KeyProvider: Send + Sync {
    /// The KEK version new values are wrapped under.
    fn current(&self) -> KeyRef;

    /// Wraps `data_key` under the current KEK, binding `aad`.
    fn wrap<'a>(&'a self, data_key: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey>;

    /// Unwraps `wrapped`, which was wrapped under `key`, checking `aad`.
    fn unwrap<'a>(
        &'a self,
        key: &'a KeyRef,
        wrapped: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes>;

    /// The blind-index key of `key`'s version: `HKDF-SHA256(kek, info = "spot blind index v1")`
    /// with an empty salt ([`crate::kdf::INDEX_KEY_INFO`]).
    ///
    /// It is derived from the **KEK**, never from a data key or a sealed value, so one leaked
    /// row does not leak the index key, and every version of the KEK gives a different index
    /// (the version byte of a [`crate::BlindIndex`] says which). The result is key material:
    /// it is zeroed on drop and must never be stored, logged or returned to a caller outside
    /// this crate.
    ///
    /// The default implementation refuses, so a provider must opt in rather than silently
    /// derive from something else.
    fn index_key<'a>(&'a self, key: &'a KeyRef) -> ProviderFuture<'a, SecretBytes> {
        let _ = key;
        Box::pin(async {
            Err(SecretError::IndexKey(
                "this key provider does not derive blind-index keys".into(),
            ))
        })
    }

    /// A key derived from `key`'s version of the KEK:
    /// `HKDF-SHA256(kek, salt = empty, info = DERIVE_PREFIX || info)`, `len` bytes
    /// ([`crate::kdf::DERIVE_PREFIX`] is `spot.derive.v1` and a zero byte, so no caller's `info`
    /// can collide with [`crate::kdf::INDEX_KEY_INFO`] or any other label of this crate).
    ///
    /// This is the one audited way to get a per-purpose key (a throttle salt, a token-binding
    /// key, a search pepper) without the KEK leaving the provider. `len` is 16 to 64 bytes and
    /// `info` is 1 to 256 bytes; anything else, or an unknown or retired key version, is an
    /// error. Neither `info` nor any key byte ever appears in an error or a `Debug`. The result
    /// is key material: zeroed on drop, never to be stored, logged or returned outside the
    /// caller that asked for it.
    ///
    /// The default implementation refuses, so a provider must opt in rather than silently
    /// derive from something else.
    fn derive_key<'a>(
        &'a self,
        key: &'a KeyRef,
        info: &'a [u8],
        len: usize,
    ) -> ProviderFuture<'a, SecretBytes> {
        let _ = (key, info, len);
        Box::pin(async {
            Err(crate::kdf::refused(
                "this key provider does not derive keys",
            ))
        })
    }
}

/// A single KEK read from an environment variable (base64, 32 bytes), for containers.
/// Its id is `env` and its only version is 1.
pub struct EnvKey(RwLock<KeyState>);

impl EnvKey {
    /// Reads the KEK from the environment variable `var`.
    pub fn from_env(var: &str) -> Result<Self, SecretError> {
        let value = std::env::var(var)
            .map(Zeroizing::new)
            .map_err(|_| SecretError::Provider(format!("environment variable {var} is not set")))?;
        Self::from_base64(&value)
    }

    /// Builds the KEK from a base64 string holding 32 bytes.
    pub fn from_base64(value: &str) -> Result<Self, SecretError> {
        let bytes = STANDARD
            .decode(value.trim())
            .map(Zeroizing::new)
            .map_err(|_| SecretError::Provider("env key is not valid base64".into()))?;
        let key: [u8; KEY_LEN] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| SecretError::Provider("env key must be 32 bytes".into()))?;
        let key = Zeroizing::new(key);
        Ok(Self(RwLock::new(KeyState::single("env", 1, key))))
    }
}

impl KeyProvider for EnvKey {
    fn current(&self) -> KeyRef {
        crate::keyring::read(&self.0).current_ref()
    }

    fn wrap<'a>(&'a self, data_key: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey> {
        Box::pin(async move { crate::keyring::read(&self.0).wrap(data_key, aad) })
    }

    fn unwrap<'a>(
        &'a self,
        key: &'a KeyRef,
        wrapped: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move { crate::keyring::read(&self.0).unwrap(key, wrapped, aad) })
    }

    fn index_key<'a>(&'a self, key: &'a KeyRef) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move { crate::keyring::read(&self.0).index_key(key) })
    }

    fn derive_key<'a>(
        &'a self,
        key: &'a KeyRef,
        info: &'a [u8],
        len: usize,
    ) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move { crate::keyring::read(&self.0).derive_key(key, info, len) })
    }
}

/// Used by `keyring` to build a fresh random key.
pub(crate) fn fresh_key() -> Result<Zeroizing<[u8; KEY_LEN]>, SecretError> {
    crypto::random_key()
}

/// Forwards [`KeyProvider`] through a smart pointer, so `Arc<P>` and `Box<dyn KeyProvider>`
/// can back a `SecretBox`.
macro_rules! forward_provider {
    ($ptr:ident) => {
        impl<T: KeyProvider + ?Sized> KeyProvider for $ptr<T> {
            fn current(&self) -> KeyRef {
                (**self).current()
            }

            fn wrap<'a>(
                &'a self,
                data_key: &'a [u8],
                aad: &'a [u8],
            ) -> ProviderFuture<'a, WrappedKey> {
                (**self).wrap(data_key, aad)
            }

            fn unwrap<'a>(
                &'a self,
                key: &'a KeyRef,
                wrapped: &'a WrappedKey,
                aad: &'a [u8],
            ) -> ProviderFuture<'a, SecretBytes> {
                (**self).unwrap(key, wrapped, aad)
            }

            fn index_key<'a>(&'a self, key: &'a KeyRef) -> ProviderFuture<'a, SecretBytes> {
                (**self).index_key(key)
            }

            fn derive_key<'a>(
                &'a self,
                key: &'a KeyRef,
                info: &'a [u8],
                len: usize,
            ) -> ProviderFuture<'a, SecretBytes> {
                (**self).derive_key(key, info, len)
            }
        }
    };
}

forward_provider!(Arc);
forward_provider!(Box);
