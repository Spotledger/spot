//! Sealed persistence: node-bound, tamper-evident, and never a lower epoch.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_bundle_signing::PublicKey;
use spot_offline_identity::{
    LeaseRecord, NodeClock, NodeId, Persisted, Persistence, SnapshotStore, TenantId, TrustAnchor,
};
use spot_offline_identity_adapters::{
    AdapterError, MemoryStateStore, SealedPersistence, SealedStateStore, Slot,
};
use spot_protected_keys::{LockedKey, TenantId as KeyTenantId};

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn key() -> LockedKey {
    LockedKey::from_bytes(&[7u8; 32]).unwrap()
}

fn key_tenant() -> KeyTenantId {
    KeyTenantId::new("acme").unwrap()
}

/// A state at `epoch` with a saved lease: what the store writes between two snapshots.
fn state(epoch: u64) -> Persisted {
    Persisted {
        signed: None,
        highest_epoch: epoch,
        snapshot_hash: None,
        delegation_version: 1,
        last_attested: 1_000,
        deadlines: None,
        lease: LeaseRecord {
            remaining_ms: [900_000, 7_200_000, 14_400_000],
            saved_at_mono_ms: 500,
            boot_id: 1,
            nonce: 0,
        },
        certificate: None,
    }
}

fn sealed_for(name: &str, store: MemoryStateStore) -> SealedPersistence<MemoryStateStore> {
    SealedPersistence::new(store, key(), key_tenant(), &node(name))
}

fn sealed(store: MemoryStateStore) -> SealedPersistence<MemoryStateStore> {
    sealed_for("branch-1", store)
}

/// The Ed25519 base point: a valid public key for a store that never verifies a signature.
fn public_key() -> PublicKey {
    let mut bytes = [0x66u8; 32];
    bytes[0] = 0x58;
    PublicKey::from_bytes(&bytes).unwrap()
}

fn anchor_for(name: &str) -> TrustAnchor {
    TrustAnchor {
        tenant: TenantId::new("acme").unwrap(),
        node: node(name),
        root: public_key(),
    }
}

fn clock(mono_ms: u64) -> NodeClock {
    NodeClock {
        mono_ms,
        boot_id: 1,
    }
}

#[test]
fn a_sealed_state_round_trips_and_its_lease_is_exactly_what_was_saved() {
    let mut p = sealed(MemoryStateStore::default());
    assert_eq!(p.fence(), 0);
    p.save(&state(3)).unwrap();
    assert_eq!(p.fence(), 3, "the fence follows the highest epoch");

    let loaded = p.load().unwrap().unwrap();
    assert_eq!(loaded, state(3));
    assert_eq!(
        loaded.lease,
        state(3).lease,
        "the remaining lease is restored as saved, never extended"
    );

    // A new process over the same store reads the same state and the same fence.
    let reopened = sealed(p.into_store());
    assert_eq!(reopened.load_state().unwrap(), Some(state(3)));
    assert_eq!(reopened.fence(), 3);
}

#[test]
fn the_state_is_ciphertext_and_never_the_plain_state() {
    let mut p = sealed(MemoryStateStore::default());
    p.save(&state(3)).unwrap();
    let store = p.into_store();
    let raw = store.state.unwrap();
    assert!(!raw.is_empty());
    assert!(
        !raw.windows(13).any(|w| w == b"highest_epoch"),
        "the stored bytes are sealed, not JSON"
    );
}

#[test]
fn a_tampered_state_is_refused_and_the_store_reports_damage() {
    let mut p = sealed(MemoryStateStore::default());
    p.save(&state(3)).unwrap();
    let mut store = p.into_store();
    let sealed_state = store.state.as_mut().unwrap();
    let last = sealed_state.len() - 1;
    sealed_state[last] ^= 0x01;

    let p = sealed(store);
    assert_eq!(p.load_state().unwrap_err(), AdapterError::StateTampered);
    assert_eq!(p.load().unwrap_err(), spot_offline_identity::PersistError);

    // The store the node opens reports the damage and holds no snapshot: fail closed.
    let store = p.into_store();
    let open = SnapshotStore::open(anchor_for("branch-1"), sealed(store), clock(0));
    assert!(open.recovered_from_damage());
    assert!(open.snapshot().is_none());
    assert_eq!(open.highest_epoch(), 0);
    assert_eq!(
        open.authorise_offline("clerk", spot_offline_identity::Tier::Three, clock(0)),
        spot_offline_identity::Decision::Deny(spot_offline_identity::DenyReason::NoSnapshot),
        "a damaged state fails closed at every tier"
    );
}

#[test]
fn a_state_sealed_for_another_node_is_refused() {
    let mut a = sealed_for("branch-1", MemoryStateStore::default());
    a.save(&state(2)).unwrap();
    let store_a = a.into_store();

    // The same bytes and the same fence, presented to another node.
    let other = MemoryStateStore {
        state: store_a.state.clone(),
        fence: store_a.fence.clone(),
        fail_writes: false,
    };
    let b = sealed_for("branch-2", other);
    assert_eq!(
        b.load_state().unwrap_err(),
        AdapterError::StateTampered,
        "the associated data names the node, so the state does not open elsewhere"
    );

    let open = SnapshotStore::open(anchor_for("branch-2"), b, clock(0));
    assert!(
        open.recovered_from_damage(),
        "a state from another node is damage, not a snapshot"
    );
    assert!(open.snapshot().is_none());
}

#[test]
fn a_rolled_back_epoch_is_refused() {
    // An epoch-1 state, kept as a backup or an older disk image would keep it.
    let mut backup = sealed(MemoryStateStore::default());
    backup.save(&state(1)).unwrap();
    let older = backup.into_store().state.unwrap();

    // The node goes on to accept epoch 2; the fence records it.
    let mut p = sealed(MemoryStateStore::default());
    p.save(&state(1)).unwrap();
    p.save(&state(2)).unwrap();
    let mut store = p.into_store();
    store.state = Some(older);

    let p = sealed(store);
    assert_eq!(
        p.load_state().unwrap_err(),
        AdapterError::StateRollback { stored: 1, seen: 2 },
        "the highest epoch never goes down"
    );
    assert_eq!(
        p.fence(),
        2,
        "a refused load still remembers the authenticated fence"
    );
    // The damaged node cannot be made to write the older epoch back, and the fence on disk
    // does not go down.
    let mut p = p;
    assert_eq!(
        p.save_state(&state(1)).unwrap_err(),
        AdapterError::StateRollback { stored: 1, seen: 2 }
    );
    assert_eq!(p.fence(), 2);
    let open = SnapshotStore::open(anchor_for("branch-1"), p, clock(0));
    assert!(open.recovered_from_damage());
}

#[test]
fn a_save_never_writes_a_lower_epoch_than_the_node_has_seen() {
    let mut p = sealed(MemoryStateStore::default());
    p.save(&state(2)).unwrap();
    let mut p = sealed(p.into_store());
    assert!(p.load_state().unwrap().is_some());
    assert_eq!(p.fence(), 2);
    assert_eq!(
        p.save_state(&state(1)).unwrap_err(),
        AdapterError::StateRollback { stored: 1, seen: 2 }
    );
    assert_eq!(
        p.load_state().unwrap(),
        Some(state(2)),
        "the newer state is untouched"
    );
}

#[test]
fn a_fence_without_a_state_is_refused() {
    let mut p = sealed(MemoryStateStore::default());
    p.save(&state(4)).unwrap();
    let mut store = p.into_store();
    store.state = None;
    assert_eq!(
        sealed(store).load_state().unwrap_err(),
        AdapterError::StateRead
    );
}

#[test]
fn a_store_that_cannot_write_fails_the_save() {
    let mut p = sealed(MemoryStateStore {
        state: None,
        fence: None,
        fail_writes: true,
    });
    assert_eq!(
        p.save_state(&state(1)).unwrap_err(),
        AdapterError::StateWrite
    );
    assert_eq!(p.load().unwrap(), None, "nothing was written");
}

#[test]
fn a_good_state_keeps_its_counters_even_without_a_snapshot() {
    let mut p = sealed(MemoryStateStore::default());
    p.save(&state(5)).unwrap();
    let open = SnapshotStore::open(anchor_for("branch-1"), p, clock(0));
    assert!(!open.recovered_from_damage());
    assert_eq!(
        open.highest_epoch(),
        5,
        "the epoch counters are read back, so an old snapshot cannot be replayed"
    );
    assert!(
        open.snapshot().is_none(),
        "there was no signed snapshot in this state"
    );
}

#[test]
fn the_two_slots_are_distinct_pieces_of_ciphertext() {
    let mut p = sealed(MemoryStateStore::default());
    p.save(&state(6)).unwrap();
    let store = p.into_store();
    assert!(store.state.is_some());
    assert!(store.fence.is_some());
    assert_ne!(store.state, store.fence);
}

/// A store whose fence write always fails, recording the order of the writes.
#[derive(Default)]
struct FenceFails {
    state: Option<Vec<u8>>,
    writes: Vec<Slot>,
}

impl SealedStateStore for FenceFails {
    fn read(&self, slot: Slot) -> Result<Option<Vec<u8>>, AdapterError> {
        Ok(match slot {
            Slot::State => self.state.clone(),
            Slot::Fence => None,
        })
    }

    fn write(&mut self, slot: Slot, bytes: &[u8]) -> Result<(), AdapterError> {
        self.writes.push(slot);
        match slot {
            Slot::State => {
                self.state = Some(bytes.to_vec());
                Ok(())
            }
            Slot::Fence => Err(AdapterError::StateWrite),
        }
    }
}

#[test]
fn the_state_slot_is_written_before_the_fence() {
    // A fence write that fails leaves a readable newer state rather than an unreadable old one.
    let mut p = SealedPersistence::new(
        FenceFails::default(),
        key(),
        key_tenant(),
        &node("branch-1"),
    );
    assert_eq!(
        p.save_state(&state(2)).unwrap_err(),
        AdapterError::StateWrite
    );
    let store = p.into_store();
    assert_eq!(store.writes, vec![Slot::State, Slot::Fence]);
    assert!(store.state.is_some());
}

/// A state at `epoch` whose lease rests on the hub reply with `nonce`.
fn state_with_nonce(epoch: u64, nonce: u64) -> Persisted {
    let mut s = state(epoch);
    s.lease.nonce = nonce;
    s
}

#[test]
fn the_fence_remembers_the_newest_hub_nonce_apart_from_the_state_slot() {
    let mut p = sealed(MemoryStateStore::default());
    p.save(&state_with_nonce(2, 1)).unwrap();
    let older_state = p.into_store();
    let older_state = older_state.state.unwrap();

    let mut p = sealed(MemoryStateStore::default());
    p.save(&state_with_nonce(2, 1)).unwrap();
    p.save(&state_with_nonce(2, 5)).unwrap();
    assert_eq!(p.nonce_fence(), 5);
    assert_eq!(p.nonce_floor(), 5);

    // The state slot alone is put back: same epoch, so the epoch fence cannot object, but the
    // nonce fence still knows a newer hub reply was seen.
    let mut store = p.into_store();
    store.state = Some(older_state);
    let p = sealed(store);
    let loaded = p.load().unwrap().unwrap();
    assert_eq!(loaded.lease.nonce, 1, "the image holds the older nonce");
    assert_eq!(p.nonce_floor(), 5, "the fence slot still holds the newest");
}

#[test]
fn a_node_that_reopens_remembers_the_nonce_floor_from_disk() {
    let mut p = sealed(MemoryStateStore::default());
    p.save(&state_with_nonce(1, 9)).unwrap();
    let reopened = sealed(p.into_store());
    assert_eq!(reopened.nonce_floor(), 0, "nothing is read before load");
    reopened.load().unwrap();
    assert_eq!(reopened.nonce_floor(), 9);
}

#[test]
fn a_fence_slot_written_before_the_nonce_existed_still_loads_with_nonce_zero() {
    use spot_protected_keys::{FieldAd, FieldCipher, KeyEpoch};
    let mut p = sealed(MemoryStateStore::default());
    p.save(&state(3)).unwrap();
    let mut store = p.into_store();
    // The earlier format: eight bytes, the epoch only.
    let ad = FieldAd::new(
        key_tenant(),
        concat!("offline_identity", ".state.v1"),
        "epoch_fence",
        "branch-1".to_owned(),
        KeyEpoch::FIRST,
    );
    store.fence = Some(FieldCipher::encrypt(&key(), &ad, &3u64.to_be_bytes()).unwrap());
    let p = sealed(store);
    assert_eq!(p.load_state().unwrap(), Some(state(3)));
    assert_eq!(p.fence(), 3);
    assert_eq!(p.nonce_floor(), 0);

    // A fence slot of any other length is malformed.
    let mut store = p.into_store();
    store.fence = Some(FieldCipher::encrypt(&key(), &ad, &[0u8; 9]).unwrap());
    assert_eq!(
        sealed(store).load_state().unwrap_err(),
        AdapterError::StateMalformed
    );
}
