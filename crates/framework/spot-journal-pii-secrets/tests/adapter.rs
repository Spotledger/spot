//! The adapter against the real `spot-secrets` keyring and the in-memory ports.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use serde_json::json;
use spot_journal_pii::{
    CounterNonces, FieldSpec, KeyHandle, KeyStoreError, PersonalFields, PiiError, SubjectKeys,
    erase, protect, reveal,
};
use spot_journal_pii_secrets::testing::{
    CounterRng, FixedClock, MemoryAudit, MemoryWrappedKeyStore,
};
use spot_journal_pii_secrets::{
    AdapterConfig, ProviderSubjectKeys, SUBJECT_KEY_DESTROYED, SecretsPiiError, StoredKey,
    WipeObserver,
};
use spot_secrets::{KeyProvider, KeyRef, MemoryKeyring, ProviderFuture, SecretBytes, WrappedKey};

/// A provider that can be switched into an outage.
struct Flaky {
    inner: Arc<MemoryKeyring>,
    down: AtomicBool,
}

impl KeyProvider for Flaky {
    fn current(&self) -> KeyRef {
        self.inner.current()
    }
    fn wrap<'a>(&'a self, data_key: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey> {
        if self.down.load(Ordering::SeqCst) {
            return Box::pin(async { Err(spot_secrets::SecretError::Provider("down".into())) });
        }
        self.inner.wrap(data_key, aad)
    }
    fn unwrap<'a>(
        &'a self,
        key: &'a KeyRef,
        wrapped: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes> {
        if self.down.load(Ordering::SeqCst) {
            return Box::pin(async { Err(spot_secrets::SecretError::Provider("down".into())) });
        }
        self.inner.unwrap(key, wrapped, aad)
    }
}

type Keys = ProviderSubjectKeys<Arc<Flaky>, Arc<MemoryWrappedKeyStore>, Arc<MemoryAudit>>;

struct Rig {
    keys: Keys,
    provider: Arc<Flaky>,
    store: Arc<MemoryWrappedKeyStore>,
    audit: Arc<MemoryAudit>,
    rng: Arc<CounterRng>,
}

fn keyring(id: &str) -> Arc<Flaky> {
    Arc::new(Flaky {
        inner: Arc::new(MemoryKeyring::new(id).expect("keyring")),
        down: AtomicBool::new(false),
    })
}

fn node(
    tenant: &str,
    provider: Arc<Flaky>,
    store: Arc<MemoryWrappedKeyStore>,
    audit: Arc<MemoryAudit>,
    seed: u64,
    max: usize,
) -> Rig {
    let rng = Arc::new(CounterRng::new(seed));
    let mut config = AdapterConfig::new(tenant, b"tenant-salt".to_vec());
    config.max_cached = max;
    let keys = ProviderSubjectKeys::new(
        config,
        provider.clone(),
        store.clone(),
        audit.clone(),
        Box::new(rng.clone()),
        Box::new(FixedClock(1_700_000_000_000)),
    )
    .expect("adapter");
    Rig {
        keys,
        provider,
        store,
        audit,
        rng,
    }
}

fn rig() -> Rig {
    node(
        "acme",
        keyring("acme-kek"),
        Arc::new(MemoryWrappedKeyStore::new()),
        Arc::new(MemoryAudit::new()),
        1,
        1_024,
    )
}

fn bytes(key: &KeyHandle) -> [u8; 32] {
    *key.expose()
}

#[test]
fn create_reuse_and_destroy() {
    let r = rig();
    let first = r.keys.key_for("alice").unwrap().expect("created");
    let again = r.keys.key_for("alice").unwrap().expect("reused");
    assert_eq!(bytes(&first), bytes(&again));
    let other = r.keys.key_for("bob").unwrap().expect("bob");
    assert_ne!(bytes(&first), bytes(&other));
    assert_eq!(r.rng.calls(1), 2, "one fill per subject");
    assert_eq!(r.store.live_count(), 2);

    r.keys.destroy("alice").unwrap();
    assert!(r.keys.key_for("alice").unwrap().is_none());
    assert!(r.keys.key_for("bob").unwrap().is_some());
    assert_eq!(r.store.live_count(), 1);

    // A fresh node over the same store cannot resurrect the subject either.
    let other_node = node(
        "acme",
        r.provider.clone(),
        r.store.clone(),
        r.audit.clone(),
        100,
        8,
    );
    assert!(other_node.keys.key_for("alice").unwrap().is_none());
}

#[test]
fn stored_key_is_wrapped_and_round_trips() {
    let r = rig();
    let key = r.keys.key_for("alice").unwrap().unwrap();
    let entry = format!("acme/subject/{}", r.keys.subject_hash("alice"));
    let Some(StoredKey::Live(wrapped)) = r.store.peek(&entry) else {
        panic!("a live wrapped key");
    };
    assert!(
        !wrapped
            .bytes
            .windows(32)
            .any(|w| w == bytes(&key).as_slice()),
        "the plain key is not in the stored bytes"
    );
    // A fresh node unwraps the same key from the store.
    let n2 = node(
        "acme",
        r.provider.clone(),
        r.store.clone(),
        r.audit.clone(),
        500,
        8,
    );
    assert_eq!(
        bytes(&n2.keys.key_for("alice").unwrap().unwrap()),
        bytes(&key)
    );
    assert_eq!(n2.rng.calls(500), 0, "nothing new was made");
}

#[test]
fn another_tenants_key_cannot_unwrap() {
    let a = rig();
    a.keys.key_for("alice").unwrap().unwrap();
    let a_entry = format!("acme/subject/{}", a.keys.subject_hash("alice"));
    let Some(copied) = a.store.peek(&a_entry) else {
        panic!("entry");
    };

    // Tenant B has its own key provider and store; A's wrapped key is planted at B's entry id.
    let b = node(
        "globex",
        keyring("globex-kek"),
        Arc::new(MemoryWrappedKeyStore::new()),
        Arc::new(MemoryAudit::new()),
        9,
        8,
    );
    let b_entry = format!("globex/subject/{}", b.keys.subject_hash("alice"));
    b.store.poke(&b_entry, copied.clone());
    let err = b.keys.key_for("alice").unwrap_err();
    assert!(matches!(err, KeyStoreError::Unavailable(_)), "{err:?}");

    // Same provider, different tenant name: the associated data still differs.
    let c = node(
        "initech",
        a.provider.clone(),
        Arc::new(MemoryWrappedKeyStore::new()),
        Arc::new(MemoryAudit::new()),
        9,
        8,
    );
    let c_entry = format!("initech/subject/{}", c.keys.subject_hash("alice"));
    c.store.poke(&c_entry, copied);
    assert!(c.keys.key_for("alice").is_err());
}

#[test]
fn journal_keys_differ_per_tenant_and_are_never_destroyed() {
    let a = rig();
    let b = node(
        "globex",
        keyring("globex-kek"),
        Arc::new(MemoryWrappedKeyStore::new()),
        Arc::new(MemoryAudit::new()),
        50,
        8,
    );
    let ja = a.keys.retained_key().unwrap();
    assert_eq!(bytes(&ja), bytes(&a.keys.retained_key().unwrap()));
    assert_ne!(bytes(&ja), bytes(&b.keys.retained_key().unwrap()));

    let err = a.keys.destroy_journal_key().unwrap_err();
    assert_eq!(err.code(), "journal_pii_secrets.refuse_journal_key");
    assert!(a.audit.events().is_empty());

    // Erasing a subject leaves the journal key alone.
    a.keys.key_for("alice").unwrap();
    a.keys.destroy("alice").unwrap();
    assert_eq!(bytes(&a.keys.retained_key().unwrap()), bytes(&ja));
}

#[test]
fn destroy_is_idempotent_and_audits_each_time() {
    let r = rig();
    r.keys.key_for("alice").unwrap();
    r.keys.destroy("alice").unwrap();
    r.keys.destroy("alice").unwrap();
    r.keys.destroy("never-seen").unwrap();
    let events = r.audit.events();
    assert_eq!(events.len(), 3);
    assert!(!events[0].already_absent);
    assert!(events[1].already_absent);
    assert!(events[2].already_absent);
    // A subject destroyed before it ever had a key stays keyless.
    assert!(r.keys.key_for("never-seen").unwrap().is_none());
}

#[test]
fn audit_event_has_no_raw_subject_and_no_key_bytes() {
    let r = rig();
    let key = r.keys.key_for("alice@example.test").unwrap().unwrap();
    r.keys.destroy("alice@example.test").unwrap();
    let events = r.audit.events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.at, 1_700_000_000_000);
    assert_eq!(
        event.subject_hash,
        r.keys.subject_hash("alice@example.test")
    );
    assert_eq!(event.subject_hash.len(), 64);
    let text = format!("{event:?}");
    assert!(!text.contains("alice"));
    assert!(!text.contains("example.test"));
    for window in bytes(&key).windows(4) {
        let needle = format!("{window:?}");
        assert!(!text.contains(&needle));
    }
    assert_eq!(SUBJECT_KEY_DESTROYED, "journal_pii.subject_key_destroyed");
    // The hash depends on the tenant salt.
    let mut config = AdapterConfig::new("acme", b"another-salt".to_vec());
    config.max_cached = 4;
    let salted = ProviderSubjectKeys::new(
        config,
        r.provider.clone(),
        r.store.clone(),
        r.audit.clone(),
        Box::new(CounterRng::new(1)),
        Box::new(FixedClock(0)),
    )
    .unwrap();
    assert_ne!(
        salted.subject_hash("alice@example.test"),
        event.subject_hash
    );
}

#[derive(Default)]
struct Wipes {
    all_zero: Mutex<Vec<bool>>,
    count: AtomicUsize,
}

impl WipeObserver for Wipes {
    fn wiped(&self, after_wipe: &[u8; 32]) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.all_zero
            .lock()
            .unwrap()
            .push(after_wipe.iter().all(|b| *b == 0));
    }
}

#[test]
fn eviction_and_destroy_zeroize_the_cached_key() {
    let wipes = Arc::new(Wipes::default());
    let r = node(
        "acme",
        keyring("acme-kek"),
        Arc::new(MemoryWrappedKeyStore::new()),
        Arc::new(MemoryAudit::new()),
        1,
        2,
    );
    let keys = r.keys.with_wipe_observer(wipes.clone());
    keys.key_for("a").unwrap();
    keys.key_for("b").unwrap();
    assert_eq!(wipes.count.load(Ordering::SeqCst), 0);
    keys.key_for("c").unwrap(); // evicts "a"
    assert_eq!(wipes.count.load(Ordering::SeqCst), 1);
    keys.destroy("c").unwrap(); // wipes the cached copy
    assert_eq!(wipes.count.load(Ordering::SeqCst), 2);
    assert!(wipes.all_zero.lock().unwrap().iter().all(|z| *z));
    // The evicted key is still served, now from the store.
    assert!(keys.key_for("a").unwrap().is_some());
}

#[test]
fn a_destroyed_subject_is_never_served_from_the_cache() {
    let r = rig();
    r.keys.key_for("alice").unwrap().unwrap(); // now cached
    r.keys.destroy("alice").unwrap();
    assert!(r.keys.key_for("alice").unwrap().is_none());
}

#[test]
fn concurrent_first_use_yields_one_key() {
    let r = Arc::new(rig());
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let r = r.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                bytes(&r.keys.key_for("alice").unwrap().unwrap())
            })
        })
        .collect();
    let got: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(got[0], got[1]);
    assert_eq!(r.store.live_count(), 1);
    assert_eq!(r.rng.calls(1), 1);
}

#[test]
fn two_nodes_racing_adopt_the_winners_key() {
    for round in 0..20 {
        let provider = keyring("acme-kek");
        let store = Arc::new(MemoryWrappedKeyStore::new());
        let audit = Arc::new(MemoryAudit::new());
        let n1 = Arc::new(node(
            "acme",
            provider.clone(),
            store.clone(),
            audit.clone(),
            1,
            8,
        ));
        let n2 = Arc::new(node("acme", provider, store.clone(), audit, 1_000, 8));
        let barrier = Arc::new(Barrier::new(2));
        let spawn = |n: Arc<Rig>, b: Arc<Barrier>| {
            std::thread::spawn(move || {
                b.wait();
                bytes(&n.keys.key_for("alice").unwrap().unwrap())
            })
        };
        let h1 = spawn(n1, barrier.clone());
        let h2 = spawn(n2, barrier);
        assert_eq!(h1.join().unwrap(), h2.join().unwrap(), "round {round}");
        assert_eq!(store.live_count(), 1);
    }
}

fn spec() -> PersonalFields {
    PersonalFields::new("Contact")
        .unwrap()
        .with_field(FieldSpec::new("email", "owner", "notifications", "contact_5y").unwrap())
        .with_field(
            FieldSpec::new("tax_ref", "owner", "legal", "tax_7y")
                .unwrap()
                .with_erasable(false),
        )
}

fn image() -> serde_json::Value {
    json!({"id": "c-1", "version": 1, "owner": "alice",
           "email": "ada@example.test", "tax_ref": "TX-1"})
}

#[test]
fn store_outage_on_reveal_is_an_error_not_a_placeholder() {
    let r = rig();
    let sealed = protect(&image(), &spec(), &r.keys, &mut CounterNonces::new()).unwrap();

    // A cold node (nothing cached) while the store is down.
    let cold = node(
        "acme",
        r.provider.clone(),
        r.store.clone(),
        r.audit.clone(),
        700,
        8,
    );
    r.store.set_down(true);
    let err = reveal(&sealed, &spec(), &cold.keys).unwrap_err();
    assert!(matches!(err, PiiError::KeyStore(_)), "{err:?}");
    assert_eq!(err.code(), "journal_pii.key_store");

    // The image was not replaced by anything; once the store is back it reveals.
    r.store.set_down(false);
    assert_eq!(reveal(&sealed, &spec(), &cold.keys).unwrap(), image());
}

#[test]
fn provider_and_rng_outages_fail_closed() {
    let r = rig();
    r.provider.down.store(true, Ordering::SeqCst);
    assert!(r.keys.key_for("alice").is_err());
    assert_eq!(r.store.live_count(), 0, "nothing stored on a failed wrap");
    r.provider.down.store(false, Ordering::SeqCst);

    r.rng.set_failing(true);
    assert!(r.keys.key_for("alice").is_err());
    assert_eq!(r.store.live_count(), 0);
    r.rng.set_failing(false);
    assert!(r.keys.key_for("alice").unwrap().is_some());

    // Provider down on a cold node when the key exists: an error, not "None".
    let cold = node(
        "acme",
        r.provider.clone(),
        r.store.clone(),
        r.audit.clone(),
        5,
        8,
    );
    r.provider.down.store(true, Ordering::SeqCst);
    assert!(cold.keys.key_for("alice").is_err());
    assert!(cold.keys.retained_key().is_err());
}

#[test]
fn destroy_failures_are_errors_and_retry_cleanly() {
    let r = rig();
    r.keys.key_for("alice").unwrap();

    r.store.set_down(true);
    assert!(r.keys.destroy("alice").is_err());
    assert!(r.audit.events().is_empty(), "no audit for a failed delete");
    r.store.set_down(false);

    r.audit.set_failing(true);
    let err = r.keys.destroy("alice").unwrap_err();
    assert!(matches!(err, KeyStoreError::Unavailable(m) if m.contains("audit_failed")));
    r.audit.set_failing(false);

    // The retry finds the tombstone and audits once, as already absent.
    r.keys.destroy("alice").unwrap();
    let events = r.audit.events();
    assert_eq!(events.len(), 1);
    assert!(events[0].already_absent);
}

#[test]
fn erase_end_to_end_leaves_retention_fields_readable() {
    let r = rig();
    let sealed = protect(&image(), &spec(), &r.keys, &mut CounterNonces::new()).unwrap();
    let receipt = erase("alice", &r.keys, None).unwrap();
    assert_eq!(receipt.subject(), "alice");
    let revealed = reveal(&sealed, &spec(), &r.keys).unwrap();
    assert_eq!(revealed["email"]["$erased"], json!(1));
    assert_eq!(revealed["tax_ref"], json!("TX-1"));
}

#[test]
fn debug_output_redacts_everything_that_holds_key_bytes() {
    let r = rig();
    let key = r.keys.key_for("alice").unwrap().unwrap();
    r.keys.retained_key().unwrap();
    assert_eq!(
        format!("{:?}", r.keys),
        "ProviderSubjectKeys { tenant: \"acme\", keys: \"<redacted>\", .. }"
    );
    assert_eq!(format!("{key:?}"), "KeyHandle(<redacted>)");
    let config = AdapterConfig::new("acme", b"tenant-salt".to_vec());
    let text = format!("{config:?}");
    assert!(text.contains("<redacted>") && !text.contains("tenant-salt"));
    for text in [format!("{:?}", r.keys), format!("{:?}", r.store)] {
        for window in bytes(&key).windows(4) {
            assert!(!text.contains(&format!("{window:?}")));
        }
    }
}

#[test]
fn configuration_is_validated_and_codes_are_stable() {
    let build = |config: AdapterConfig| {
        ProviderSubjectKeys::new(
            config,
            keyring("k"),
            Arc::new(MemoryWrappedKeyStore::new()),
            Arc::new(MemoryAudit::new()),
            Box::new(CounterRng::new(1)),
            Box::new(FixedClock(0)),
        )
        .err()
    };
    assert!(matches!(
        build(AdapterConfig::new("", b"s".to_vec())),
        Some(SecretsPiiError::ConfigInvalid(_))
    ));
    assert!(build(AdapterConfig::new("t", Vec::new())).is_some());
    let mut zero = AdapterConfig::new("t", b"s".to_vec());
    zero.max_cached = 0;
    assert!(build(zero).is_some());
    assert!(build(AdapterConfig::new("t", b"s".to_vec())).is_none());
    assert_eq!(
        SecretsPiiError::Provider("x").code(),
        "journal_pii_secrets.provider_failed"
    );
}
