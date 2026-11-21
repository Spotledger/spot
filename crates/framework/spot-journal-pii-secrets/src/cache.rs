//! A bounded LRU of unwrapped keys. Every key is overwritten when it leaves.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use spot_secrets::WrappedKey;

use crate::ports::WipeObserver;

/// 32 key bytes that wipe themselves on drop.
pub(crate) struct CachedKey {
    bytes: [u8; 32],
    observer: Option<Arc<dyn WipeObserver>>,
    /// When (injected clock, ms) the key was loaded or last found live in the store.
    loaded_at: i64,
    /// The wrapped form the key came from (ciphertext, not secret); a revalidation compares it.
    wrapped: Option<WrappedKey>,
}

/// What a cache hit tells the adapter.
pub(crate) struct Hit {
    pub(crate) bytes: [u8; 32],
    pub(crate) loaded_at: i64,
    pub(crate) wrapped: Option<WrappedKey>,
}

impl CachedKey {
    pub(crate) fn new(
        bytes: [u8; 32],
        observer: Option<Arc<dyn WipeObserver>>,
        loaded_at: i64,
        wrapped: Option<WrappedKey>,
    ) -> Self {
        Self {
            bytes,
            observer,
            loaded_at,
            wrapped,
        }
    }

    pub(crate) fn bytes(&self) -> [u8; 32] {
        self.bytes
    }
}

impl Drop for CachedKey {
    fn drop(&mut self) {
        self.bytes.fill(0);
        // Keep the wipe from being optimised away as a dead store.
        std::hint::black_box(&self.bytes);
        if let Some(observer) = &self.observer {
            observer.wiped(&self.bytes);
        }
    }
}

impl fmt::Debug for CachedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CachedKey(<redacted>)")
    }
}

#[derive(Debug)]
pub(crate) struct KeyCache {
    max: usize,
    tick: u64,
    entries: HashMap<String, (CachedKey, u64)>,
}

impl KeyCache {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            max,
            tick: 0,
            entries: HashMap::new(),
        }
    }

    pub(crate) fn get(&mut self, entry: &str) -> Option<Hit> {
        self.tick += 1;
        let tick = self.tick;
        let (key, last) = self.entries.get_mut(entry)?;
        *last = tick;
        Some(Hit {
            bytes: key.bytes(),
            loaded_at: key.loaded_at,
            wrapped: key.wrapped.clone(),
        })
    }

    /// Marks the entry as found live in the store at `now`.
    pub(crate) fn refresh(&mut self, entry: &str, now: i64) {
        if let Some((key, _)) = self.entries.get_mut(entry) {
            key.loaded_at = now;
        }
    }

    /// Inserts `key`, evicting (and so wiping) the least recently used entry when full.
    pub(crate) fn insert(&mut self, entry: &str, key: CachedKey) {
        self.tick += 1;
        if !self.entries.contains_key(entry) && self.entries.len() >= self.max {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (_, last))| *last)
                .map(|(name, _)| name.clone());
            if let Some(name) = oldest {
                self.entries.remove(&name);
            }
        }
        self.entries.insert(entry.to_owned(), (key, self.tick));
    }

    /// Drops (and so wipes) the entry.
    pub(crate) fn remove(&mut self, entry: &str) {
        self.entries.remove(entry);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}
