//! The in-memory cache of unwrapped data keys: bounded in count and in age, zeroised on
//! eviction, never written anywhere.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use zeroize::Zeroizing;

type CacheKey = [u8; 32];

struct Entry {
    value: Zeroizing<Vec<u8>>,
    stored_at: Duration,
}

pub(crate) struct DataKeyCache {
    ttl: Duration,
    max_entries: usize,
    entries: Mutex<HashMap<CacheKey, Entry>>,
}

/// The cache key: SHA-256 over the key version, the length-prefixed aad and the ciphertext, so
/// the map never holds the ciphertext itself and no two inputs can run together.
pub(crate) fn cache_key(version: u32, aad: &[u8], ciphertext: &[u8]) -> CacheKey {
    let mut hash = Sha256::new();
    hash.update(version.to_be_bytes());
    hash.update((aad.len() as u64).to_be_bytes());
    hash.update(aad);
    hash.update(ciphertext);
    hash.finalize().into()
}

impl DataKeyCache {
    pub(crate) fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<CacheKey, Entry>> {
        // A plain map: a panic elsewhere cannot leave it half-updated.
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The cached value when it is younger than the TTL; an older entry is dropped (zeroised).
    pub(crate) fn get(&self, key: &CacheKey, now: Duration) -> Option<Vec<u8>> {
        let mut map = self.lock();
        let fresh = now.saturating_sub(map.get(key)?.stored_at) < self.ttl;
        if fresh {
            map.get(key).map(|entry| entry.value.to_vec())
        } else {
            map.remove(key);
            None
        }
    }

    /// Stores `value`. When the cache is full, expired entries go first, then the oldest.
    pub(crate) fn put(&self, key: CacheKey, value: &[u8], now: Duration) {
        let mut map = self.lock();
        if !map.contains_key(&key) && map.len() >= self.max_entries {
            let ttl = self.ttl;
            map.retain(|_, entry| now.saturating_sub(entry.stored_at) < ttl);
            while map.len() >= self.max_entries {
                let oldest = map
                    .iter()
                    .min_by_key(|(_, entry)| entry.stored_at)
                    .map(|(k, _)| *k);
                match oldest {
                    Some(k) => {
                        map.remove(&k);
                    }
                    None => break,
                }
            }
        }
        map.insert(
            key,
            Entry {
                value: Zeroizing::new(value.to_vec()),
                stored_at: now,
            },
        );
    }

    /// Drops every entry (each is zeroised as it drops).
    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_and_count_bounds() {
        let cache = DataKeyCache::new(Duration::from_secs(10), 2);
        let (a, b, c) = (
            cache_key(1, b"a", b"x"),
            cache_key(1, b"b", b"x"),
            cache_key(1, b"c", b"x"),
        );
        cache.put(a, b"1", Duration::from_secs(0));
        cache.put(b, b"2", Duration::from_secs(1));
        cache.put(c, b"3", Duration::from_secs(2));
        assert_eq!(cache.len(), 2);
        assert!(
            cache.get(&a, Duration::from_secs(2)).is_none(),
            "oldest evicted"
        );
        assert_eq!(
            cache.get(&c, Duration::from_secs(9)).as_deref(),
            Some(&b"3"[..])
        );
        assert!(cache.get(&c, Duration::from_secs(12)).is_none(), "expired");
    }

    #[test]
    fn key_binds_version_aad_and_ciphertext_without_running_together() {
        assert_ne!(cache_key(1, b"ab", b"c"), cache_key(1, b"a", b"bc"));
        assert_ne!(cache_key(1, b"a", b"c"), cache_key(2, b"a", b"c"));
    }
}
