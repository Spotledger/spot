//! `ProviderSubjectKeys`: the `SubjectKeys` implementation over a `spot-secrets` key provider.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use sha2::{Digest, Sha256};
use spot_journal_pii::{KEY_LEN, KeyHandle, KeyStoreError, SubjectKeys};
use spot_secrets::{KeyProvider, WrappedKey};
use zeroize::Zeroizing;

use crate::block::block_on;
use crate::cache::{CachedKey, Hit, KeyCache};
use crate::error::SecretsPiiError;
use crate::ports::{
    AuditPort, Clock, Rng, StoredKey, SubjectKeyDestroyed, WipeObserver, WrappedKeyStore,
};

/// The default bound on cached unwrapped keys.
pub const DEFAULT_MAX_CACHED: usize = 1_024;

/// The default time a cached key is served without asking the store again: 60 seconds. It is the
/// longest an erased subject's fields stay readable on a node that missed the destroy.
pub const DEFAULT_CACHE_TTL_MILLIS: u64 = 60_000;

/// The entry id of the tenant journal key (subject entries are `<tenant>/subject/<hash>`).
fn journal_entry(tenant: &str) -> String {
    format!("{tenant}/journal")
}

/// What one adapter needs to know about its tenant.
pub struct AdapterConfig {
    /// The tenant. It is part of every entry id and of the associated data every key is wrapped
    /// under, so a wrapped key copied to another tenant's entry does not open.
    pub tenant: String,
    /// The tenant salt the subject id is hashed with for the audit event and the entry id. Keep
    /// it stable: changing it loses every subject's key.
    pub salt: Vec<u8>,
    /// At most this many unwrapped keys are cached ([`DEFAULT_MAX_CACHED`]).
    pub max_cached: usize,
    /// A cached key older than this many milliseconds (by the injected clock) is checked against
    /// the store before it is served again ([`DEFAULT_CACHE_TTL_MILLIS`]). Zero means check on
    /// every use. This bounds how long a destroy on another node goes unseen.
    pub cache_ttl_millis: u64,
}

impl AdapterConfig {
    /// A configuration with the default cache bound.
    #[must_use]
    pub fn new(tenant: impl Into<String>, salt: Vec<u8>) -> Self {
        Self {
            tenant: tenant.into(),
            salt,
            max_cached: DEFAULT_MAX_CACHED,
            cache_ttl_millis: DEFAULT_CACHE_TTL_MILLIS,
        }
    }
}

impl fmt::Debug for AdapterConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdapterConfig")
            .field("tenant", &self.tenant)
            .field("salt", &"<redacted>")
            .field("max_cached", &self.max_cached)
            .field("cache_ttl_millis", &self.cache_ttl_millis)
            .finish()
    }
}

/// Subject keys held wrapped in a [`WrappedKeyStore`] by the tenant's [`KeyProvider`].
///
/// * `key_for` creates a subject's key on first use (32 bytes from the injected [`Rng`]) and
///   answers `None` once it was destroyed.
/// * `destroy` deletes the wrapped key, wipes the cached copy and emits one audit event.
/// * `retained_key` is the tenant journal key, created on first use like a subject key and
///   never destroyable here.
/// * Any failure of the provider, the store, the audit port or the random source is an error,
///   never "no key": otherwise an outage would read as an erasure and a reveal would replace an
///   image with its placeholder.
///
/// One lock is held across each call, so two threads asking for one subject's first key get
/// one key. Two nodes racing is settled by [`WrappedKeyStore::put_if_absent`]: the loser adopts
/// the winner's key.
///
/// After a destroy anywhere, this node serves the key for at most `cache_ttl_millis`, or until
/// [`Self::forget_destroyed`] is called. Past the ttl a cached key is read from the store again:
/// a tombstone wipes the cached copy and the answer is `None`; an unchanged live entry has its
/// age refreshed; a store that cannot answer is an error, never the stale key.
pub struct ProviderSubjectKeys<P, W, A> {
    tenant: String,
    salt: Zeroizing<Vec<u8>>,
    provider: P,
    store: W,
    audit: A,
    rng: Box<dyn Rng>,
    clock: Box<dyn Clock>,
    ttl_millis: u64,
    observer: Option<Arc<dyn WipeObserver>>,
    cache: Mutex<KeyCache>,
}

impl<P, W, A> ProviderSubjectKeys<P, W, A>
where
    P: KeyProvider,
    W: WrappedKeyStore,
    A: AuditPort,
{
    /// Builds the adapter, or refuses an unusable configuration.
    pub fn new(
        config: AdapterConfig,
        provider: P,
        store: W,
        audit: A,
        rng: Box<dyn Rng>,
        clock: Box<dyn Clock>,
    ) -> Result<Self, SecretsPiiError> {
        if config.tenant.is_empty() {
            return Err(SecretsPiiError::ConfigInvalid("the tenant is empty"));
        }
        if config.salt.is_empty() {
            return Err(SecretsPiiError::ConfigInvalid("the tenant salt is empty"));
        }
        if config.max_cached == 0 {
            return Err(SecretsPiiError::ConfigInvalid("max_cached is zero"));
        }
        Ok(Self {
            tenant: config.tenant,
            salt: Zeroizing::new(config.salt),
            provider,
            store,
            audit,
            rng,
            clock,
            ttl_millis: config.cache_ttl_millis,
            observer: None,
            cache: Mutex::new(KeyCache::new(config.max_cached)),
        })
    }

    /// Tells `observer` each time a cached key is wiped (a test hook).
    #[must_use]
    pub fn with_wipe_observer(mut self, observer: Arc<dyn WipeObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// The tenant-salted hash of `subject` as the audit event carries it: lower-case hex
    /// SHA-256 over the salt length, the salt and the subject.
    #[must_use]
    pub fn subject_hash(&self, subject: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update((self.salt.len() as u64).to_be_bytes());
        hasher.update(self.salt.as_slice());
        hasher.update(subject.as_bytes());
        let digest = hasher.finalize();
        let mut out = String::with_capacity(64);
        for byte in digest {
            out.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
            out.push(char::from(b"0123456789abcdef"[usize::from(byte & 0x0f)]));
        }
        out
    }

    /// Always refuses: the tenant journal key protects legal-retention fields and is never
    /// destroyed here. Nothing is deleted and nothing is audited.
    pub fn destroy_journal_key(&self) -> Result<(), SecretsPiiError> {
        Err(SecretsPiiError::RefuseJournalKey)
    }

    fn subject_entry(&self, subject: &str) -> String {
        format!("{}/subject/{}", self.tenant, self.subject_hash(subject))
    }

    fn aad(&self, entry: &str) -> Vec<u8> {
        format!(
            "spot.journal-pii-secrets.v1;tenant={};entry={entry}",
            self.tenant
        )
        .into_bytes()
    }

    fn lock(&self) -> MutexGuard<'_, KeyCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn unwrap_key(
        &self,
        entry: &str,
        wrapped: &WrappedKey,
    ) -> Result<[u8; KEY_LEN], SecretsPiiError> {
        let aad = self.aad(entry);
        let plain = block_on(self.provider.unwrap(&wrapped.key, wrapped, &aad))
            .map_err(|e| SecretsPiiError::Provider(e.code()))?;
        <[u8; KEY_LEN]>::try_from(plain.expose())
            .map_err(|_| SecretsPiiError::EntryInvalid("the unwrapped key has the wrong length"))
    }

    #[allow(clippy::type_complexity)]
    fn create_key(
        &self,
        entry: &str,
    ) -> Result<([u8; KEY_LEN], (bool, WrappedKey)), SecretsPiiError> {
        let mut fresh = Zeroizing::new([0u8; KEY_LEN]);
        self.rng
            .fill(fresh.as_mut_slice())
            .map_err(|e| SecretsPiiError::Rng(e.0))?;
        let aad = self.aad(entry);
        let wrapped = block_on(self.provider.wrap(fresh.as_slice(), &aad))
            .map_err(|e| SecretsPiiError::Provider(e.code()))?;
        let stored = self
            .store
            .put_if_absent(entry, wrapped.clone())
            .map_err(|e| SecretsPiiError::Store(e.0))?;
        Ok((*fresh, (stored, wrapped)))
    }

    /// Whether a key loaded at `loaded_at` may be served at `now` without asking the store. A
    /// clock that went backwards is not fresh.
    fn fresh(&self, loaded_at: i64, now: i64) -> bool {
        let age = i128::from(now) - i128::from(loaded_at);
        age >= 0 && age < i128::from(self.ttl_millis)
    }

    /// Reads the store again for a cached key past its ttl. `Ok(Some)` serves it, `Ok(None)` is a
    /// tombstone (the cached copy is gone). A failure wipes the cached copy too: fail closed.
    fn revalidate(
        &self,
        cache: &mut KeyCache,
        entry: &str,
        hit: Hit,
        now: i64,
    ) -> Result<Option<[u8; KEY_LEN]>, SecretsPiiError> {
        let found = match self.store.get(entry) {
            Ok(found) => found,
            Err(e) => {
                cache.remove(entry);
                return Err(SecretsPiiError::Store(e.0));
            }
        };
        match found {
            Some(StoredKey::Destroyed) => {
                cache.remove(entry);
                Ok(None)
            }
            Some(StoredKey::Live(wrapped)) => {
                if hit.wrapped.as_ref() == Some(&wrapped) {
                    cache.refresh(entry, now);
                    return Ok(Some(hit.bytes));
                }
                // The entry was rewritten: drop the old copy and serve what the store holds.
                cache.remove(entry);
                let bytes = self.unwrap_key(entry, &wrapped)?;
                cache.insert(
                    entry,
                    CachedKey::new(bytes, self.observer.clone(), now, Some(wrapped)),
                );
                Ok(Some(bytes))
            }
            None => {
                // The store forgot an entry it held: never mint a second key over it.
                cache.remove(entry);
                Err(SecretsPiiError::EntryInvalid(
                    "the store no longer holds a key this node had cached",
                ))
            }
        }
    }

    /// The key under `entry`: cached, stored or newly made. `None` only for a tombstone.
    fn resolve(&self, entry: &str) -> Result<Option<[u8; KEY_LEN]>, SecretsPiiError> {
        let mut cache = self.lock();
        let now = self.clock.now_unix_ms();
        if let Some(hit) = cache.get(entry) {
            if self.fresh(hit.loaded_at, now) {
                return Ok(Some(hit.bytes));
            }
            return self.revalidate(&mut cache, entry, hit, now);
        }
        let found = self
            .store
            .get(entry)
            .map_err(|e| SecretsPiiError::Store(e.0))?;
        let (bytes, wrapped) = match found {
            Some(StoredKey::Destroyed) => return Ok(None),
            Some(StoredKey::Live(wrapped)) => (self.unwrap_key(entry, &wrapped)?, wrapped),
            None => {
                let (mine, stored) = self.create_key(entry)?;
                if stored.0 {
                    (mine, stored.1)
                } else {
                    // Another node won the race: adopt its key, drop ours.
                    match self
                        .store
                        .get(entry)
                        .map_err(|e| SecretsPiiError::Store(e.0))?
                    {
                        Some(StoredKey::Live(wrapped)) => {
                            (self.unwrap_key(entry, &wrapped)?, wrapped)
                        }
                        Some(StoredKey::Destroyed) => return Ok(None),
                        None => {
                            return Err(SecretsPiiError::EntryInvalid(
                                "the store refused a key then reported no entry",
                            ));
                        }
                    }
                }
            }
        };
        cache.insert(
            entry,
            CachedKey::new(bytes, self.observer.clone(), now, Some(wrapped)),
        );
        Ok(Some(bytes))
    }

    /// Another node destroyed `subject_hash`'s key (the `subject_hash` its
    /// `journal_pii.subject_key_destroyed` event carries, never the subject): wipes this node's
    /// cached copy at once, so the next use reads the store and finds the tombstone. Idempotent;
    /// an unknown hash does nothing. It never fails, writes nothing to the store and emits no
    /// audit event.
    pub fn forget_destroyed(&self, subject_hash: &str) {
        let entry = format!("{}/subject/{subject_hash}", self.tenant);
        self.lock().remove(&entry);
    }

    fn destroy_subject(&self, subject: &str) -> Result<(), SecretsPiiError> {
        let entry = self.subject_entry(subject);
        let mut cache = self.lock();
        // Wipe the cached copy first: even if the store then fails, this node serves no more.
        cache.remove(&entry);
        let removed = self
            .store
            .destroy(&entry)
            .map_err(|e| SecretsPiiError::Store(e.0))?;
        let event = SubjectKeyDestroyed {
            subject_hash: self.subject_hash(subject),
            at: self.clock.now_unix_ms(),
            already_absent: !removed,
        };
        // A failed audit after a successful delete is an error: the caller retries, the retry
        // finds the tombstone and audits with `already_absent = true`.
        self.audit
            .record(&event)
            .map_err(|e| SecretsPiiError::Audit(e.0))
    }
}

impl<P, W, A> SubjectKeys for ProviderSubjectKeys<P, W, A>
where
    P: KeyProvider,
    W: WrappedKeyStore,
    A: AuditPort,
{
    fn key_for(&self, subject: &str) -> Result<Option<KeyHandle>, KeyStoreError> {
        let entry = self.subject_entry(subject);
        Ok(self.resolve(&entry)?.map(KeyHandle::new))
    }

    fn destroy(&self, subject: &str) -> Result<(), KeyStoreError> {
        Ok(self.destroy_subject(subject)?)
    }

    fn retained_key(&self) -> Result<KeyHandle, KeyStoreError> {
        let entry = journal_entry(&self.tenant);
        match self.resolve(&entry)? {
            Some(bytes) => Ok(KeyHandle::new(bytes)),
            None => Err(
                SecretsPiiError::EntryInvalid("the journal key entry is marked destroyed").into(),
            ),
        }
    }
}

impl<P, W, A> fmt::Debug for ProviderSubjectKeys<P, W, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderSubjectKeys")
            .field("tenant", &self.tenant)
            .field("keys", &"<redacted>")
            .finish_non_exhaustive()
    }
}
