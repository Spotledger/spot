//! The `SubjectKeys` adapter over the `spot-secrets` key provider (WP-JRN-08.T3, ADR-061 s6 and
//! s9).
//!
//! `spot-journal-pii` seals personal fields under a per-subject key and defines the
//! [`spot_journal_pii::SubjectKeys`] trait. This crate implements it for real: erasing a
//! subject destroys a real key.
//!
//! ```text
//! ProviderSubjectKeys::new(config, provider, store, audit, rng, clock)
//!   key_for(subject)   -> the subject's key (made on first use), or None once destroyed
//!   destroy(subject)   -> deletes the wrapped key, wipes the cache, emits one audit event
//!   retained_key()     -> the tenant journal key for fields that are not erasable
//! ```
//!
//! * Keys are random bytes from the injected [`Rng`], stored in the [`WrappedKeyStore`] wrapped
//!   by the tenant's `KeyProvider` with associated data that names the tenant and the entry.
//! * A destroyed subject keeps a tombstone, so it never gets a second key.
//! * A destroy anywhere reaches every node: after a destroy anywhere, this node serves the key
//!   for at most `cache_ttl_millis` ([`DEFAULT_CACHE_TTL_MILLIS`], 60 seconds), or until
//!   [`ProviderSubjectKeys::forget_destroyed`] is called with the destroy event's `subject_hash`.
//!   Past the ttl a cached key is checked against the store again; a tombstone wipes it, and a
//!   store that cannot answer means an error, never the stale key.
//! * Provider, store, audit and random-source failures are errors, never "no key".
//! * No type that holds key bytes prints them; the crate has no clock, no random source, no
//!   database and no `unsafe`.
//!
//! Out of scope (later tasks): the persistent wrapped-key table, key backup, journal-key rotation,
//! moving the destroy event between nodes.

mod adapter;
mod block;
mod cache;
mod error;
mod ports;
pub mod testing;

pub use adapter::{
    AdapterConfig, DEFAULT_CACHE_TTL_MILLIS, DEFAULT_MAX_CACHED, ProviderSubjectKeys,
};
pub use error::SecretsPiiError;
pub use ports::{
    AuditPort, Clock, PortError, Rng, SUBJECT_KEY_DESTROYED, StoredKey, SubjectKeyDestroyed,
    WipeObserver, WrappedKeyStore,
};

#[cfg(test)]
mod unit {
    use crate::cache::{CachedKey, KeyCache};

    #[test]
    fn cache_is_bounded_and_evicts_least_recent() {
        let mut cache = KeyCache::new(2);
        cache.insert("a", CachedKey::new([1; 32], None, 0, None));
        cache.insert("b", CachedKey::new([2; 32], None, 0, None));
        assert!(cache.get("a").is_some());
        cache.insert("c", CachedKey::new([3; 32], None, 0, None));
        assert_eq!(cache.len(), 2);
        assert!(cache.get("b").is_none());
        assert!(cache.get("a").is_some());
    }
}
