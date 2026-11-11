#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! Hygiene tests: no key material reaches `Debug`, `Display`, an error, serde, a log or the
//! backup-shaped key store (WP-SEC-10.T1, item 6 of the brief).
//!
//! The canary is a fixed 32-byte key value. A [`RecordingLock`] proves the canary really is
//! loaded into a [`LockedKey`] (the positive control); every other assertion proves it never
//! appears in a printable, serialisable or logged form.

use std::sync::{Arc, Mutex};

use spot_protected_keys::{
    FieldAd, FieldCipher, GroupName, Hub, KeyEpoch, LockedKey, MemoryLock, MemoryRootKeyProvider,
    MonoTime, NodeId, NodeKeyStore, NodeWrapKey, ProtectedKeyError, RootKeyProvider, TenantId,
    Tier, TierTable,
};
use spot_secrets::{KeyRef, WrappedKey};

/// The canary key value: 32 ASCII bytes, so it is visible in any text encoding.
const CANARY: &[u8; 32] = b"CANARY-KEY-MATERIAL-DO-NOT-LEAK!";
/// The same canary as text.
const CANARY_TEXT: &str = "CANARY-KEY-MATERIAL-DO-NOT-LEAK!";

fn canary_hex() -> String {
    CANARY.iter().map(|b| format!("{b:02x}")).collect()
}

fn assert_no_canary(label: &str, text: &str) {
    assert!(
        !text.contains(CANARY_TEXT),
        "{label} leaked the canary as text: {text}"
    );
    assert!(
        !text.contains(&canary_hex()),
        "{label} leaked the canary as hex: {text}"
    );
}

fn tenant() -> TenantId {
    TenantId::new("acme").expect("tenant")
}

fn group(name: &str) -> GroupName {
    GroupName::new(name).expect("group")
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).expect("node")
}

fn canary_key() -> LockedKey {
    LockedKey::from_bytes(CANARY).expect("canary key")
}

/// A [`MemoryLock`] that records the bytes it is asked to pin, as a positive control.
#[derive(Clone, Default)]
struct RecordingLock {
    seen: Arc<Mutex<Vec<u8>>>,
}

impl RecordingLock {
    fn seen(&self) -> Vec<u8> {
        self.seen.lock().expect("lock").clone()
    }
}

impl MemoryLock for RecordingLock {
    fn lock(&self, region: &[u8]) -> bool {
        self.seen.lock().expect("lock").extend_from_slice(region);
        true
    }

    fn unlock(&self, _region: &[u8]) {}

    fn core_dumps_excluded(&self) -> bool {
        true
    }
}

#[test]
fn the_canary_is_really_loaded_into_locked_memory() {
    // Positive control: the canary exists, and the `MemoryLock` seam receives exactly it.
    let key = canary_key();
    let lock = RecordingLock::default();
    assert!(key.pin(&lock));
    assert_eq!(lock.seen(), CANARY.as_slice());
    assert!(lock.core_dumps_excluded());
    assert_eq!(key.len(), 32);
    assert!(!key.is_empty());
}

#[test]
fn the_public_types_never_print_key_material() {
    let provider = MemoryRootKeyProvider::from_key(tenant(), &canary_key()).expect("provider");
    let mut hub = Hub::new(provider);
    let g = group("bank_details");
    hub.create_group(&g, MonoTime::ZERO).expect("create");
    let n = node("branch-1");
    let wrap = NodeWrapKey::from_key(n.clone(), canary_key());
    let node_wrap = wrap.duplicate().expect("duplicate");
    hub.register_node(&n, wrap).expect("register");
    let mut tiers = TierTable::new();
    tiers.grant(n.clone(), g.clone(), Tier::two());
    let release = hub
        .release(&n, &g, MonoTime::ZERO, &tiers)
        .expect("release");

    assert_no_canary("KeyRelease Debug", &format!("{release:?}"));
    assert_no_canary("NodeWrapKey Debug", &format!("{node_wrap:?}"));

    let mut store = NodeKeyStore::new(n.clone(), node_wrap);
    store.accept(&release, MonoTime::ZERO).expect("accept");
    // A held key has no `Debug`/`Display` at all; only its place can be printed.
    assert_eq!(store.node(), &n);
    assert_eq!(store.held().len(), 1);

    let backup = hub.backup();
    assert_no_canary("KeyStoreBackup Debug", &format!("{backup:?}"));
    for wrapped in &backup.groups {
        assert_no_canary("WrappedGroupKey Debug", &format!("{wrapped:?}"));
    }
}

#[test]
fn serde_output_and_the_backup_hold_only_wrapped_keys() {
    let provider = MemoryRootKeyProvider::from_key(tenant(), &canary_key()).expect("provider");
    let mut hub = Hub::new(provider);
    let g = group("bank_details");
    hub.create_group(&g, MonoTime::ZERO).expect("create");
    hub.rotate_group(&g, MonoTime::from_millis(1))
        .expect("rotate");

    let backup = hub.backup();
    let json = serde_json::to_string(&backup).expect("json");
    assert_no_canary("backup JSON", &json);
    // The wrapped bytes are ciphertext, never the root key the backup is wrapped under.
    for wrapped in &backup.groups {
        assert_ne!(wrapped.bytes.as_slice(), CANARY.as_slice());
    }

    let n = node("branch-1");
    hub.register_node(&n, NodeWrapKey::from_key(n.clone(), canary_key()))
        .expect("register");
    let mut tiers = TierTable::new();
    tiers.grant(n.clone(), g.clone(), Tier::two());
    let release = hub
        .release(&n, &g, MonoTime::ZERO, &tiers)
        .expect("release");
    let release_json = serde_json::to_string(&release).expect("json");
    assert_no_canary("KeyRelease JSON", &release_json);
    assert!(release_json.contains("wrapped_key"));
}

#[test]
fn errors_carry_no_key_material() {
    // A tampered ciphertext reports authentication failure, never the key or the bytes.
    let key = canary_key();
    let ad = FieldAd::new(tenant(), "Bank Account", "iban", "BA-1", KeyEpoch::FIRST);
    let mut sealed = FieldCipher::encrypt(&key, &ad, b"DE89").expect("encrypt");
    sealed[0] ^= 0xff;
    let err = FieldCipher::decrypt(&key, &ad, &sealed).expect_err("tampered");
    assert!(matches!(err, ProtectedKeyError::Tampered));
    assert_no_canary("Tampered error", &err.to_string());
    assert_no_canary("Tampered Debug", &format!("{err:?}"));

    // A wrong root key reference names the reference, never any material.
    let provider = MemoryRootKeyProvider::from_key(tenant(), &canary_key()).expect("provider");
    let wrong = WrappedKey {
        key: KeyRef {
            id: "someone-elses-kms".to_owned(),
            version: 9,
        },
        bytes: vec![0u8; 48],
    };
    let err = provider.unwrap(&wrong, b"aad").expect_err("wrong root key");
    assert_no_canary("RootKey error", &err.to_string());
}
