//! `KmsKeyProvider`: a `spot_secrets::KeyProvider` over a [`KmsClient`].

use crate::cache::{DataKeyCache, cache_key};
use crate::client::{KeyInfo, KmsCiphertext, KmsClient};
use crate::derive;
use crate::error::KmsError;
use crate::seed::{self, Entropy, Seed};
use spot_core::Clock;
use spot_secrets::{KeyProvider, KeyRef, ProviderFuture, SecretBytes, SecretError, WrappedKey};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The longest data key `wrap` accepts, in bytes.
const MAX_DATA_KEY: usize = 1024;
/// The largest cache a provider may be configured with.
const MAX_CACHE_ENTRIES: usize = 100_000;

/// Configuration of a [`KmsKeyProvider`].
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// The file holding the wrapped derivation seed (created at first start, mode `0600`).
    pub seed_file: PathBuf,
    /// How often the key's versions are re-read from the KMS (default 60 s).
    pub refresh_interval: Duration,
    /// How long an unwrapped data key may be served from memory (default 300 s). This is the
    /// documented revocation latency.
    pub cache_ttl: Duration,
    /// The most data keys held in memory (default 1024).
    pub cache_max_entries: usize,
}

impl ProviderConfig {
    /// The defaults, with the seed in `seed_file`.
    pub fn new(seed_file: impl Into<PathBuf>) -> Self {
        Self {
            seed_file: seed_file.into(),
            refresh_interval: Duration::from_secs(60),
            cache_ttl: Duration::from_secs(300),
            cache_max_entries: 1024,
        }
    }
}

struct State {
    info: KeyInfo,
    /// When the last refresh was attempted, on the injected monotonic clock.
    attempted_at: Duration,
}

/// A key provider whose key-encryption key lives in the customer's KMS.
///
/// Revoking the key (or its token) stops the site from unsealing anything once the cached data
/// keys age out: the revocation latency is `cache_ttl`. Every refusal is a
/// `SecretError::Provider` whose text names the key and the `secrets_kms.*` code, or
/// `Tampered` when the KMS reports an authentication failure.
pub struct KmsKeyProvider {
    client: Arc<dyn KmsClient>,
    clock: Arc<dyn Clock>,
    key_id: String,
    key_name: String,
    seed_file: PathBuf,
    refresh_interval: Duration,
    state: RwLock<State>,
    cache: DataKeyCache,
    seed: Seed,
}

impl std::fmt::Debug for KmsKeyProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KmsKeyProvider")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl KmsKeyProvider {
    /// Opens the provider: reads the key from the KMS, then loads the derivation seed (creating
    /// and storing it on first start).
    ///
    /// # Errors
    /// A refusal when the configuration is out of bounds, the KMS cannot be reached, the key is
    /// disabled, or the seed file is corrupt, foreign or readable by others.
    pub async fn open(
        client: Arc<dyn KmsClient>,
        config: ProviderConfig,
        clock: Arc<dyn Clock>,
        entropy: Arc<dyn Entropy>,
    ) -> Result<Self, SecretError> {
        let key_name = client.key_name().to_owned();
        let refuse = |e: KmsError| e.into_secret(&key_name);
        if config.refresh_interval.is_zero()
            || config.cache_ttl.is_zero()
            || config.cache_ttl > Duration::from_secs(24 * 3600)
            || config.cache_max_entries == 0
            || config.cache_max_entries > MAX_CACHE_ENTRIES
        {
            return Err(refuse(KmsError::ConfigInvalid(
                "a refresh interval, cache lifetime or cache size is out of range",
            )));
        }
        let info = client.key_info().await.map_err(refuse)?;
        if !info.enabled {
            return Err(refuse(KmsError::KeyDisabled));
        }
        let key_id = format!("kms:{key_name}");
        let seed = seed::load_or_create(
            &config.seed_file,
            &key_id,
            client.as_ref(),
            entropy.as_ref(),
        )
        .await
        .map_err(refuse)?;
        let now = clock.monotonic();
        Ok(Self {
            cache: DataKeyCache::new(config.cache_ttl, config.cache_max_entries),
            state: RwLock::new(State {
                info,
                attempted_at: now,
            }),
            client,
            clock,
            key_id,
            key_name,
            seed_file: config.seed_file,
            refresh_interval: config.refresh_interval,
            seed,
        })
    }

    /// Reads the key's versions from the KMS now.
    pub async fn refresh(&self) -> Result<(), SecretError> {
        self.refresh_inner()
            .await
            .map_err(|e| e.into_secret(&self.key_name))
    }

    async fn refresh_inner(&self) -> Result<(), KmsError> {
        let attempted_at = self.clock.monotonic();
        let result = self.client.key_info().await;
        let mut state = self.state_write();
        // The last known versions stay in force on a failure; the next attempt waits a full
        // interval.
        state.attempted_at = attempted_at;
        state.info = result?;
        Ok(())
    }

    /// Refreshes the key's versions when the interval has passed; a failure keeps the last
    /// known state.
    async fn refresh_if_due(&self) {
        let due = {
            let state = self.state_read();
            self.clock.monotonic().saturating_sub(state.attempted_at) >= self.refresh_interval
        };
        if due {
            let _ = self.refresh_inner().await;
        }
    }

    fn state_read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn state_write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn known(&self) -> KeyInfo {
        self.state_read().info
    }

    /// Drops every cached data key, so the next unwrap asks the KMS.
    pub fn flush_cache(&self) {
        self.cache.clear();
    }

    /// How many data keys are cached.
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Moves `wrapped` under the KMS key's latest version: unwraps it, then wraps the data key
    /// again. The host's re-wrap sweep (`rotate-kek`) calls this for each stored data key.
    ///
    /// The data key passes through this process's memory (zeroised); it is re-encrypted by the
    /// KMS's encrypt call, not by its rewrap call.
    pub async fn rewrap(
        &self,
        wrapped: &WrappedKey,
        aad: &[u8],
    ) -> Result<WrappedKey, SecretError> {
        let plain = self.unwrap_inner(&wrapped.key, wrapped, aad).await?;
        self.wrap_inner(plain.expose(), aad).await
    }

    /// Moves the derivation seed's wrapping to the latest key version (needed before the old
    /// version is retired in the KMS). Returns the version it is now wrapped under.
    pub async fn rewrap_seed(&self) -> Result<u32, SecretError> {
        seed::rewrap(&self.seed_file, &self.key_id, self.client.as_ref())
            .await
            .map_err(|e| e.into_secret(&self.key_name))
    }

    async fn wrap_inner(&self, data_key: &[u8], aad: &[u8]) -> Result<WrappedKey, SecretError> {
        let refuse = |e: KmsError| e.into_secret(&self.key_name);
        if data_key.is_empty() || data_key.len() > MAX_DATA_KEY {
            return Err(refuse(KmsError::InputInvalid(
                "the data key length is out of range",
            )));
        }
        self.refresh_if_due().await;
        if !self.known().enabled {
            return Err(refuse(KmsError::KeyDisabled));
        }
        let ciphertext = self.client.encrypt(aad, data_key).await.map_err(refuse)?;
        {
            let mut state = self.state_write();
            if ciphertext.key_version > state.info.latest_version {
                state.info.latest_version = ciphertext.key_version;
            }
        }
        Ok(WrappedKey {
            key: KeyRef {
                id: self.key_id.clone(),
                version: ciphertext.key_version,
            },
            bytes: ciphertext.bytes,
        })
    }

    async fn unwrap_inner(
        &self,
        key: &KeyRef,
        wrapped: &WrappedKey,
        aad: &[u8],
    ) -> Result<SecretBytes, SecretError> {
        let refuse = |e: KmsError| e.into_secret(&self.key_name);
        if key.id != self.key_id || wrapped.key != *key {
            return Err(SecretError::UnknownKey {
                id: key.id.clone(),
                version: key.version,
            });
        }
        self.refresh_if_due().await;
        if key.version < self.known().min_decryption_version {
            return Err(refuse(KmsError::VersionRetired {
                version: key.version,
            }));
        }
        let cache_id = cache_key(key.version, aad, &wrapped.bytes);
        if let Some(hit) = self.cache.get(&cache_id, self.clock.monotonic()) {
            return Ok(SecretBytes::new(hit));
        }
        let ciphertext = KmsCiphertext {
            key_version: key.version,
            bytes: wrapped.bytes.clone(),
        };
        match self.client.decrypt(aad, &ciphertext).await {
            Ok(plain) => {
                self.cache
                    .put(cache_id, plain.expose(), self.clock.monotonic());
                Ok(plain)
            }
            Err(KmsError::Rejected { status: 400 }) => {
                // An authentication failure (wrong aad, altered ciphertext) or a version the KMS
                // has just retired: re-read the key to tell which.
                let _ = self.refresh_inner().await;
                if key.version < self.known().min_decryption_version {
                    Err(refuse(KmsError::VersionRetired {
                        version: key.version,
                    }))
                } else {
                    Err(SecretError::Tampered)
                }
            }
            Err(e) => Err(refuse(e)),
        }
    }

    fn check_key(&self, key: &KeyRef) -> Result<(), SecretError> {
        let latest = self.known().latest_version;
        if key.id != self.key_id {
            return Err(SecretError::UnknownKey {
                id: key.id.clone(),
                version: key.version,
            });
        }
        if key.version == 0 || key.version > latest {
            return Err(SecretError::UnknownKeyVersion {
                version: key.version,
            });
        }
        Ok(())
    }
}

impl KeyProvider for KmsKeyProvider {
    fn current(&self) -> KeyRef {
        KeyRef {
            id: self.key_id.clone(),
            version: self.known().latest_version,
        }
    }

    fn wrap<'a>(&'a self, data_key: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey> {
        Box::pin(self.wrap_inner(data_key, aad))
    }

    fn unwrap<'a>(
        &'a self,
        key: &'a KeyRef,
        wrapped: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(self.unwrap_inner(key, wrapped, aad))
    }

    /// The blind-index key: HKDF over the seed. It is the same for every transit version of one
    /// key id (a transit rotation must not change blind indexes); the version only has to exist.
    fn index_key<'a>(&'a self, key: &'a KeyRef) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move {
            self.check_key(key)?;
            derive::index_key(&self.seed, &self.key_id)
        })
    }

    fn derive_key<'a>(
        &'a self,
        key: &'a KeyRef,
        info: &'a [u8],
        len: usize,
    ) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move {
            self.check_key(key)?;
            derive::derive_key(&self.seed, &self.key_id, info, len)
        })
    }
}
