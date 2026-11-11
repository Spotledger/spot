//! A destroy on another node reaches this node's key cache: the ttl and `forget_destroyed`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use spot_journal_pii::{KeyStoreError, SubjectKeys};
use spot_journal_pii_secrets::testing::{
    CounterRng, ManualClock, MemoryAudit, MemoryWrappedKeyStore,
};
use spot_journal_pii_secrets::{
    AdapterConfig, DEFAULT_CACHE_TTL_MILLIS, ProviderSubjectKeys, StoredKey, WipeObserver,
};
use spot_secrets::MemoryKeyring;

type Keys = ProviderSubjectKeys<Arc<MemoryKeyring>, Arc<MemoryWrappedKeyStore>, Arc<MemoryAudit>>;

#[derive(Default)]
struct Wipes(Mutex<usize>);

impl WipeObserver for Wipes {
    fn wiped(&self, after_wipe: &[u8; 32]) {
        assert_eq!(after_wipe, &[0u8; 32], "wiped means zeroed");
        *self.0.lock().unwrap() += 1;
    }
}

impl Wipes {
    fn count(&self) -> usize {
        *self.0.lock().unwrap()
    }
}

struct Node {
    keys: Keys,
    clock: ManualClock,
    rng: Arc<CounterRng>,
    wipes: Arc<Wipes>,
    audit: Arc<MemoryAudit>,
}

struct Shared {
    provider: Arc<MemoryKeyring>,
    store: Arc<MemoryWrappedKeyStore>,
}

fn shared() -> Shared {
    Shared {
        provider: Arc::new(MemoryKeyring::new("acme-kek").expect("keyring")),
        store: Arc::new(MemoryWrappedKeyStore::new()),
    }
}

fn node(s: &Shared, seed: u64, ttl: Option<u64>) -> Node {
    let clock = ManualClock::new(1_000_000);
    let rng = Arc::new(CounterRng::new(seed));
    let wipes = Arc::new(Wipes::default());
    let audit = Arc::new(MemoryAudit::new());
    let mut config = AdapterConfig::new("acme", b"tenant-salt".to_vec());
    if let Some(ttl) = ttl {
        config.cache_ttl_millis = ttl;
    }
    let keys = ProviderSubjectKeys::new(
        config,
        s.provider.clone(),
        s.store.clone(),
        audit.clone(),
        Box::new(rng.clone()),
        Box::new(clock.clone()),
    )
    .expect("adapter")
    .with_wipe_observer(wipes.clone());
    Node {
        keys,
        clock,
        rng,
        wipes,
        audit,
    }
}

fn key(n: &Node, subject: &str) -> Option<[u8; 32]> {
    n.keys.key_for(subject).unwrap().map(|k| *k.expose())
}

#[test]
fn default_ttl_is_sixty_seconds() {
    assert_eq!(DEFAULT_CACHE_TTL_MILLIS, 60_000);
    assert_eq!(
        AdapterConfig::new("t", vec![1]).cache_ttl_millis,
        DEFAULT_CACHE_TTL_MILLIS
    );
}

#[test]
fn remote_destroy_is_served_inside_the_ttl_and_gone_after_it() {
    let s = shared();
    let a = node(&s, 1, None);
    let b = node(&s, 1_000, None);
    let original = key(&a, "alice").expect("created");
    assert_eq!(key(&b, "alice"), Some(original));

    a.keys.destroy("alice").unwrap();
    // Inside the ttl B still serves from memory.
    b.clock.advance(59_999);
    assert_eq!(key(&b, "alice"), Some(original));
    assert_eq!(b.wipes.count(), 0);

    // Past the ttl B reads the tombstone, answers None and wipes.
    b.clock.advance(1);
    assert_eq!(key(&b, "alice"), None);
    assert_eq!(b.wipes.count(), 1);
    assert_eq!(key(&b, "alice"), None);
    assert_eq!(b.rng.calls(1_000), 0, "B never made a second key");
    assert_eq!(s.store.live_count(), 0);
    assert_eq!(
        s.store.peek(&entry(&b, "alice")),
        Some(StoredKey::Destroyed)
    );
    assert!(b.audit.events().is_empty(), "B audits nothing");
}

fn entry(n: &Node, subject: &str) -> String {
    format!("acme/subject/{}", n.keys.subject_hash(subject))
}

#[test]
fn forget_destroyed_closes_the_window_at_once() {
    let s = shared();
    let a = node(&s, 1, None);
    let b = node(&s, 1_000, None);
    let original = key(&a, "alice").unwrap();
    assert_eq!(key(&b, "alice"), Some(original));
    a.keys.destroy("alice").unwrap();

    let hash = a.audit.events()[0].subject_hash.clone();
    b.keys.forget_destroyed(&hash);
    assert_eq!(b.wipes.count(), 1, "the cached copy was wiped");
    assert_eq!(key(&b, "alice"), None);
    assert_eq!(b.rng.calls(1_000), 0);
    // Writes nothing and audits nothing.
    assert!(b.audit.events().is_empty());
    assert_eq!(a.audit.events().len(), 1);
}

#[test]
fn forget_destroyed_is_idempotent_and_ignores_unknown_hashes() {
    let s = shared();
    let a = node(&s, 1, None);
    let original = key(&a, "alice").unwrap();
    a.keys.forget_destroyed("not-a-known-hash");
    a.keys.forget_destroyed("");
    assert_eq!(a.wipes.count(), 0);
    assert_eq!(key(&a, "alice"), Some(original));

    // Forgetting a live subject only costs a reload: same key, from the store.
    let hash = a.keys.subject_hash("alice");
    a.keys.forget_destroyed(&hash);
    a.keys.forget_destroyed(&hash);
    assert_eq!(a.wipes.count(), 1);
    assert_eq!(key(&a, "alice"), Some(original));
    assert_eq!(a.rng.calls(1), 1, "no new key");
    assert_eq!(s.store.live_count(), 1);
}

#[test]
fn a_store_failure_after_the_ttl_is_an_error_never_the_stale_key() {
    let s = shared();
    let a = node(&s, 1, None);
    let b = node(&s, 1_000, None);
    let original = key(&a, "alice").unwrap();
    assert_eq!(key(&b, "alice"), Some(original));

    s.store.set_down(true);
    // Inside the ttl the cache still answers without the store.
    b.clock.advance(1_000);
    assert_eq!(key(&b, "alice"), Some(original));
    b.clock.advance(60_000);
    let err = b.keys.key_for("alice").unwrap_err();
    assert!(matches!(err, KeyStoreError::Unavailable(_)), "{err:?}");
    assert_eq!(b.wipes.count(), 1, "the stale copy was dropped");

    // When the store returns, the live key is loaded again; nothing new is made.
    s.store.set_down(false);
    assert_eq!(key(&b, "alice"), Some(original));
    assert_eq!(b.rng.calls(1_000), 0);
}

#[test]
fn ttl_zero_revalidates_on_every_use() {
    let s = shared();
    let a = node(&s, 1, Some(0));
    let b = node(&s, 1_000, Some(0));
    let original = key(&a, "alice").unwrap();
    assert_eq!(key(&b, "alice"), Some(original));
    assert_eq!(key(&b, "alice"), Some(original));

    // The very next use after a remote destroy sees it, with no time passing.
    a.keys.destroy("alice").unwrap();
    assert_eq!(key(&b, "alice"), None);
    assert_eq!(b.wipes.count(), 1);

    // And a store outage fails the very next use.
    let c = node(&s, 2_000, Some(0));
    assert!(key(&c, "bob").is_some());
    s.store.set_down(true);
    assert!(c.keys.key_for("bob").is_err());
}

#[test]
fn a_live_entry_revalidates_keeps_working_and_gets_a_fresh_age() {
    let s = shared();
    let b = node(&s, 1, None);
    let original = key(&b, "alice").unwrap();

    b.clock.advance(61_000);
    assert_eq!(key(&b, "alice"), Some(original), "revalidated, still live");
    assert_eq!(b.wipes.count(), 0, "nothing was wiped or reloaded");

    // The age was refreshed: with the store down, a use 30 s later is still inside the new ttl.
    s.store.set_down(true);
    b.clock.advance(30_000);
    assert_eq!(key(&b, "alice"), Some(original));
    b.clock.advance(30_000);
    assert!(b.keys.key_for("alice").is_err());
    assert_eq!(b.rng.calls(1), 1);
}

#[test]
fn the_journal_key_revalidates_too_and_survives() {
    let s = shared();
    let b = node(&s, 1, None);
    let j = *b.keys.retained_key().unwrap().expose();
    b.clock.advance(120_000);
    assert_eq!(*b.keys.retained_key().unwrap().expose(), j);
    assert_eq!(b.rng.calls(1), 1);
}

#[test]
fn a_clock_that_goes_backwards_is_not_fresh() {
    let s = shared();
    let b = node(&s, 1, None);
    let original = key(&b, "alice").unwrap();
    b.clock.advance(-500_000);
    s.store.set_down(true);
    assert!(
        b.keys.key_for("alice").is_err(),
        "no trust in a cached age from the future"
    );
    s.store.set_down(false);
    assert_eq!(key(&b, "alice"), Some(original));
}

#[test]
fn an_entry_the_store_forgot_is_an_error_not_a_second_key() {
    let s = shared();
    let b = node(&s, 1, None);
    key(&b, "alice").unwrap();
    // A store that lost the entry without a tombstone.
    s.store.remove_raw(&entry(&b, "alice"));
    b.clock.advance(61_000);
    assert!(b.keys.key_for("alice").is_err());
    assert_eq!(b.rng.calls(1), 1, "no second key was made");
    assert_eq!(b.wipes.count(), 1);
}

#[test]
fn a_rewritten_entry_is_reloaded_after_the_ttl() {
    let s = shared();
    let b = node(&s, 1_000, None);
    let first = key(&b, "alice").unwrap();
    // Another deployment of the same tenant made a different key for the same entry id.
    let elsewhere = Shared {
        provider: s.provider.clone(),
        store: Arc::new(MemoryWrappedKeyStore::new()),
    };
    let other = node(&elsewhere, 77, None);
    let second = key(&other, "alice").unwrap();
    assert_ne!(first, second);
    let e = entry(&b, "alice");
    s.store.poke(&e, elsewhere.store.peek(&e).expect("entry"));
    b.clock.advance(61_000);
    assert_eq!(key(&b, "alice"), Some(second));
    assert_eq!(b.wipes.count(), 1, "the old copy was wiped");
}
