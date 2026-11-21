//! In-memory doubles for the two traits the crate asks the caller for.
//!
//! They exist so that a caller can write a test without a key provider or an operating-system
//! RNG, and so that this crate's own suite exercises the public API. They are not a key store:
//! nothing here is durable, nothing is encrypted at rest, and [`CounterNonces`] is
//! deliberately not random.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::key::{KeyHandle, KeyStoreError, SubjectKeys};
use crate::nonce::{NONCE_LEN, NonceSource};

/// One process's worth of subject keys and a tenant journal key, held in memory.
#[derive(Debug)]
pub struct MemoryKeys {
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    keys: BTreeMap<String, KeyHandle>,
    retained: Option<KeyHandle>,
}

impl MemoryKeys {
    /// A store whose tenant journal key (the one legal-retention fields use) is `retained`, and
    /// which holds no subject keys.
    #[must_use]
    pub fn new(retained: KeyHandle) -> Self {
        Self {
            state: Mutex::new(State {
                keys: BTreeMap::new(),
                retained: Some(retained),
            }),
        }
    }

    /// A store with no retained key at all, so a legal-retention field cannot be protected or
    /// revealed: the shape of a tenant whose journal key has not been provisioned.
    #[must_use]
    pub fn without_retained() -> Self {
        Self {
            state: Mutex::new(State {
                keys: BTreeMap::new(),
                retained: None,
            }),
        }
    }

    /// Gives `subject` the key `key`. Replaces a key the subject already had.
    pub fn insert(&self, subject: &str, key: KeyHandle) {
        self.lock().keys.insert(subject.to_owned(), key);
    }

    /// Replaces the tenant journal key.
    pub fn set_retained(&self, key: KeyHandle) {
        self.lock().retained = Some(key);
    }

    /// The subjects that still have a key, sorted.
    #[must_use]
    pub fn subjects(&self) -> Vec<String> {
        self.lock().keys.keys().cloned().collect()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SubjectKeys for MemoryKeys {
    fn key_for(&self, subject: &str) -> Result<Option<KeyHandle>, KeyStoreError> {
        Ok(self.lock().keys.get(subject).cloned())
    }

    fn destroy(&self, subject: &str) -> Result<(), KeyStoreError> {
        self.lock().keys.remove(subject);
        Ok(())
    }

    fn retained_key(&self) -> Result<KeyHandle, KeyStoreError> {
        self.lock()
            .retained
            .clone()
            .ok_or_else(|| KeyStoreError::Unavailable("no retained key is configured".to_owned()))
    }
}

/// A nonce source that counts, for tests: `0`, `1`, `2`, ... in the first eight bytes.
///
/// Deterministic on purpose, so a test can prove that two protections of one plaintext under
/// one key still differ (the counter is different, so the ciphertext is). Never use it in
/// production: a counter that restarts at zero reuses nonces.
#[derive(Debug, Default, Clone)]
pub struct CounterNonces {
    next: u64,
}

impl CounterNonces {
    /// A counter starting at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many nonces have been handed out.
    #[must_use]
    pub fn issued(&self) -> u64 {
        self.next
    }
}

impl NonceSource for CounterNonces {
    fn next_nonce(&mut self) -> Option<[u8; NONCE_LEN]> {
        let mut nonce = [0u8; NONCE_LEN];
        nonce[..8].copy_from_slice(&self.next.to_be_bytes());
        self.next = self.next.wrapping_add(1);
        Some(nonce)
    }
}
