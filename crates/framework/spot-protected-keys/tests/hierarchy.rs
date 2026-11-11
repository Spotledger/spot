#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! End-to-end tests of the protected-group key hierarchy (WP-SEC-10.T1).
//!
//! They walk the whole path the brief describes: the hub creates and rotates group keys, a
//! node receives a key only wrapped to its own node key and only under a lease, a revoked
//! node is cut off and its groups rotate, and a protected field round-trips under the
//! associated-data binding.

use spot_protected_keys::{
    BlindIndexKey, FieldAd, FieldCipher, GroupName, Hub, KeyEpoch, KeyRelease, LockedKey,
    MemoryRootKeyProvider, MonoTime, NodeId, NodeKeyStore, NodeStatus, NodeWrapKey,
    ProtectedKeyError, TenantId, Tier, TierTable,
};

fn tenant() -> TenantId {
    TenantId::new("acme").expect("tenant")
}

fn group(name: &str) -> GroupName {
    GroupName::new(name).expect("group")
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).expect("node")
}

fn empty_hub() -> Hub<MemoryRootKeyProvider> {
    Hub::new(MemoryRootKeyProvider::new(tenant()).expect("root provider"))
}

fn grant(tiers: &mut TierTable, node: &NodeId, group: &GroupName) {
    tiers.grant(node.clone(), group.clone(), Tier::two());
}

#[test]
fn a_node_receives_the_group_key_wrapped_and_reads_a_field() {
    let mut hub = empty_hub();
    let g = group("bank_details");
    let epoch = hub.create_group(&g, MonoTime::ZERO).expect("create group");
    assert_eq!(epoch, KeyEpoch::FIRST);

    let n = node("branch-1");
    let hub_copy = NodeWrapKey::generate(n.clone()).expect("wrap key");
    let node_copy = hub_copy.duplicate().expect("duplicate");
    hub.register_node(&n, hub_copy).expect("register node");

    let mut tiers = TierTable::new();
    grant(&mut tiers, &n, &g);

    let now = MonoTime::from_millis(1_000);
    let release = hub.release(&n, &g, now, &tiers).expect("release");
    assert_eq!(release.epoch, epoch);
    assert_eq!(release.group, g);
    assert_eq!(release.node, n);
    assert!(!release.is_expired(now));

    let mut store = NodeKeyStore::new(n.clone(), node_copy);
    store.accept(&release, now).expect("accept");
    assert!(store.is_held(&g, epoch));

    let ad = FieldAd::new(tenant(), "Bank Account", "iban", "BA-1", epoch);
    let secret = b"DE89370400440532013000";
    let ciphertext = store
        .with_key(&g, epoch, now, |key| FieldCipher::encrypt(key, &ad, secret))
        .expect("held key")
        .expect("encrypt");
    assert_ne!(ciphertext.as_slice(), secret.as_slice());

    let opened = store
        .with_key(&g, epoch, now, |key| {
            FieldCipher::decrypt(key, &ad, &ciphertext)
        })
        .expect("held key")
        .expect("decrypt");
    assert_eq!(opened.expose(), secret.as_slice());
}

#[test]
fn release_is_refused_without_a_permission_tier() {
    let mut hub = empty_hub();
    let g = group("bank_details");
    hub.create_group(&g, MonoTime::ZERO).expect("create group");
    let n = node("branch-1");
    hub.register_node(&n, NodeWrapKey::generate(n.clone()).expect("wrap key"))
        .expect("register");
    let tiers = TierTable::new();
    let err = hub
        .release(&n, &g, MonoTime::ZERO, &tiers)
        .expect_err("no tier");
    assert!(matches!(err, ProtectedKeyError::NoPermission { .. }));
}

#[test]
fn a_suspended_node_takes_no_release_and_a_revoked_one_is_final() {
    let mut hub = empty_hub();
    let g = group("bank_details");
    hub.create_group(&g, MonoTime::ZERO).expect("create group");
    let n = node("branch-1");
    hub.register_node(&n, NodeWrapKey::generate(n.clone()).expect("wrap key"))
        .expect("register");
    let mut tiers = TierTable::new();
    grant(&mut tiers, &n, &g);

    hub.set_status(&n, NodeStatus::Suspended).expect("suspend");
    let err = hub
        .release(&n, &g, MonoTime::ZERO, &tiers)
        .expect_err("suspended");
    assert!(matches!(err, ProtectedKeyError::NodeNotActive { .. }));

    hub.set_status(&n, NodeStatus::Active).expect("reactivate");
    assert!(hub.release(&n, &g, MonoTime::ZERO, &tiers).is_ok());

    hub.revoke_node(&n, MonoTime::ZERO).expect("revoke");
    assert_eq!(hub.status(&n), Some(NodeStatus::Revoked));
    assert!(matches!(
        hub.set_status(&n, NodeStatus::Active),
        Err(ProtectedKeyError::NodeRevoked { .. })
    ));
    assert!(matches!(
        hub.release(&n, &g, MonoTime::ZERO, &tiers),
        Err(ProtectedKeyError::NodeRevoked { .. })
    ));
}

#[test]
fn rotation_creates_a_new_epoch_and_keeps_the_old_one_readable() {
    let mut hub = empty_hub();
    let g = group("bank_details");
    hub.create_group(&g, MonoTime::ZERO).expect("create group");
    let n = node("branch-1");
    hub.register_node(&n, NodeWrapKey::generate(n.clone()).expect("wrap key"))
        .expect("register");
    let mut tiers = TierTable::new();
    grant(&mut tiers, &n, &g);

    let now = MonoTime::from_millis(10);
    let first = hub
        .release_epoch(&n, &g, KeyEpoch::FIRST, now, &tiers)
        .expect("epoch 1");
    let second_epoch = hub
        .rotate_group(&g, MonoTime::from_millis(20))
        .expect("rotate");
    assert_eq!(second_epoch, KeyEpoch::new(2));
    assert_eq!(hub.group_epoch(&g), Some(second_epoch));

    // The new epoch is what a fresh release hands out.
    let second = hub.release(&n, &g, now, &tiers).expect("release");
    assert_eq!(second.epoch, second_epoch);
    // The old epoch is still readable for old data.
    let old = hub
        .release_epoch(&n, &g, KeyEpoch::FIRST, now, &tiers)
        .expect("old epoch");
    assert_eq!(old.epoch, first.epoch);
    // An epoch that was never created is refused.
    assert!(matches!(
        hub.release_epoch(&n, &g, KeyEpoch::new(99), now, &tiers),
        Err(ProtectedKeyError::UnknownEpoch { .. })
    ));
}

#[test]
fn the_lease_bound_comes_from_the_tier_and_the_key_is_discarded_after_it() {
    let mut hub = empty_hub();
    let g = group("bank_details");
    hub.create_group(&g, MonoTime::ZERO).expect("create group");
    let n = node("branch-1");
    let wrap = NodeWrapKey::generate(n.clone()).expect("wrap key");
    let node_copy = wrap.duplicate().expect("duplicate");
    hub.register_node(&n, wrap).expect("register");

    let mut tiers = TierTable::new();
    tiers.grant(n.clone(), g.clone(), Tier::one());
    let now = MonoTime::from_millis(5_000);
    let release = hub.release(&n, &g, now, &tiers).expect("release");
    assert_eq!(release.lease_until, now.plus(Tier::one().bound()));

    let mut store = NodeKeyStore::new(n.clone(), node_copy);
    store.accept(&release, now).expect("accept");
    assert_eq!(store.discard_expired(release.lease_until), 1);
    assert!(!store.is_held(&g, KeyEpoch::FIRST));
    assert!(store.key(&g, KeyEpoch::FIRST, now).is_err());
}

#[test]
fn revoking_a_node_rotates_every_group_it_held_and_stops_release() {
    let mut hub = empty_hub();
    let bank = group("bank_details");
    let payroll = group("payroll");
    hub.create_group(&bank, MonoTime::ZERO).expect("bank");
    hub.create_group(&payroll, MonoTime::ZERO).expect("payroll");

    let n = node("branch-1");
    hub.register_node(&n, NodeWrapKey::generate(n.clone()).expect("wrap key"))
        .expect("register");
    let mut tiers = TierTable::new();
    grant(&mut tiers, &n, &bank);
    grant(&mut tiers, &n, &payroll);

    let now = MonoTime::from_millis(1);
    hub.release(&n, &bank, now, &tiers).expect("release bank");
    hub.release(&n, &payroll, now, &tiers)
        .expect("release payroll");

    let report = hub.revoke_node(&n, now).expect("revoke");
    assert_eq!(report.node, n);
    assert_eq!(report.revoked_at, now);
    assert_eq!(report.rotated.len(), 2);
    for rotated in &report.rotated {
        assert_eq!(rotated.from, KeyEpoch::FIRST);
        assert_eq!(rotated.to, KeyEpoch::new(2));
    }
    assert_eq!(hub.group_epoch(&bank), Some(KeyEpoch::new(2)));
    assert_eq!(hub.revocation_epoch(&n, &bank), Some(KeyEpoch::FIRST));
    assert_eq!(hub.revoked_at(&n), Some(now));
    assert!(matches!(
        hub.release(&n, &bank, now, &tiers),
        Err(ProtectedKeyError::NodeRevoked { .. })
    ));
}

#[test]
fn re_registering_a_node_does_not_hide_the_groups_it_held_from_revocation() {
    let mut hub = empty_hub();
    let bank = group("bank_details");
    hub.create_group(&bank, MonoTime::ZERO).expect("bank");
    let n = node("branch-1");
    hub.register_node(&n, NodeWrapKey::generate(n.clone()).expect("wrap key"))
        .expect("register");
    let mut tiers = TierTable::new();
    grant(&mut tiers, &n, &bank);
    let now = MonoTime::from_millis(1);
    hub.release(&n, &bank, now, &tiers).expect("release");

    hub.register_node(&n, NodeWrapKey::generate(n.clone()).expect("new key"))
        .expect("re-register");
    let report = hub.revoke_node(&n, now).expect("revoke");
    assert_eq!(report.rotated.len(), 1);
    assert_eq!(hub.group_epoch(&bank), Some(KeyEpoch::new(2)));
}
#[test]
fn a_revoked_node_cannot_open_data_written_after_rotation() {
    let mut hub = empty_hub();
    let g = group("bank_details");
    hub.create_group(&g, MonoTime::ZERO).expect("create group");

    let a = node("branch-a");
    let a_hub = NodeWrapKey::generate(a.clone()).expect("wrap key");
    let a_node = a_hub.duplicate().expect("duplicate");
    hub.register_node(&a, a_hub).expect("register a");
    let mut tiers = TierTable::new();
    grant(&mut tiers, &a, &g);

    let now = MonoTime::from_millis(0);
    let first = hub.release(&a, &g, now, &tiers).expect("release a");
    let mut store_a = NodeKeyStore::new(a.clone(), a_node);
    store_a.accept(&first, now).expect("a accepts");

    // The node is revoked; its group rotates to epoch 2.
    let report = hub.revoke_node(&a, now).expect("revoke");
    assert_eq!(report.rotated[0].to, KeyEpoch::new(2));

    // The hub will not hand it a new key.
    assert!(matches!(
        hub.release(&a, &g, now, &tiers),
        Err(ProtectedKeyError::NodeRevoked { .. })
    ));

    // The node learns of the revocation at the epoch it held and forgets every key.
    store_a.record_revocation(KeyEpoch::FIRST);
    assert!(store_a.held().is_empty());

    // Even a release for the new epoch, addressed to it, is refused, so it can never decrypt
    // data written after the rotation.
    let b = node("branch-b");
    hub.register_node(&b, NodeWrapKey::generate(b.clone()).expect("wrap key"))
        .expect("register b");
    grant(&mut tiers, &b, &g);
    let second = hub.release(&b, &g, now, &tiers).expect("release b");
    assert_eq!(second.epoch, KeyEpoch::new(2));
    let forged_for_a = KeyRelease {
        node: a.clone(),
        ..second.clone()
    };
    assert!(matches!(
        store_a.accept(&forged_for_a, now),
        Err(ProtectedKeyError::NodeRevoked { .. })
    ));
    assert!(store_a.with_key(&g, KeyEpoch::new(2), now, |_| ()).is_err());

    // An authorised holder still reads the old epoch's data.
    assert!(
        hub.release_epoch(&b, &g, KeyEpoch::FIRST, now, &tiers)
            .is_ok()
    );
}

#[test]
fn retiring_an_epoch_removes_its_material_but_never_the_current_one() {
    let mut hub = empty_hub();
    let g = group("bank_details");
    hub.create_group(&g, MonoTime::ZERO).expect("create group");
    let n = node("branch-1");
    hub.register_node(&n, NodeWrapKey::generate(n.clone()).expect("wrap key"))
        .expect("register");
    let mut tiers = TierTable::new();
    grant(&mut tiers, &n, &g);

    hub.rotate_group(&g, MonoTime::from_millis(1))
        .expect("rotate");
    hub.retire_epoch(&g, KeyEpoch::FIRST).expect("retire");
    assert!(matches!(
        hub.release_epoch(&n, &g, KeyEpoch::FIRST, MonoTime::ZERO, &tiers),
        Err(ProtectedKeyError::EpochRetired { .. })
    ));
    assert!(matches!(
        hub.retire_epoch(&g, KeyEpoch::new(2)),
        Err(ProtectedKeyError::Malformed(_))
    ));
    assert!(matches!(
        hub.retire_epoch(&g, KeyEpoch::new(99)),
        Err(ProtectedKeyError::UnknownEpoch { .. })
    ));
}

#[test]
fn a_wrapped_backup_restores_the_same_keys() {
    let root_key = LockedKey::from_bytes(&[7u8; 32]).expect("root key");
    let provider = MemoryRootKeyProvider::from_key(tenant(), &root_key).expect("provider");
    let mut hub = Hub::new(provider);
    let g = group("bank_details");
    hub.create_group(&g, MonoTime::ZERO).expect("create group");
    hub.rotate_group(&g, MonoTime::from_millis(1))
        .expect("rotate");

    let backup = hub.backup();
    assert_eq!(backup.groups.len(), 2);
    assert_eq!(backup.tenant, tenant());

    let provider2 = MemoryRootKeyProvider::from_key(tenant(), &root_key).expect("provider");
    let mut restored = Hub::restore(provider2, backup).expect("restore");
    assert_eq!(restored.group_epoch(&g), Some(KeyEpoch::new(2)));

    // The restored hub can still release a usable key.
    let n = node("branch-1");
    restored
        .register_node(&n, NodeWrapKey::generate(n.clone()).expect("wrap key"))
        .expect("register");
    let mut tiers = TierTable::new();
    grant(&mut tiers, &n, &g);
    let release = restored
        .release(&n, &g, MonoTime::ZERO, &tiers)
        .expect("release");
    assert_eq!(release.epoch, KeyEpoch::new(2));
}

#[test]
fn the_reencryption_plan_lists_the_older_epochs_oldest_first() {
    let mut hub = empty_hub();
    let g = group("bank_details");
    hub.create_group(&g, MonoTime::ZERO).expect("create group");
    hub.rotate_group(&g, MonoTime::from_millis(1))
        .expect("rotate");
    hub.rotate_group(&g, MonoTime::from_millis(2))
        .expect("rotate");

    let plan = hub
        .plan_reencrypt(&g, MonoTime::from_millis(3))
        .expect("plan");
    assert_eq!(plan.target, KeyEpoch::new(3));
    assert_eq!(plan.len(), 2);
    assert_eq!(plan.batches[0].from, KeyEpoch::FIRST);
    assert_eq!(plan.batches[1].from, KeyEpoch::new(2));
    assert!(!plan.is_complete(0));
    assert!(plan.is_complete(2));
    assert_eq!(plan.remaining(1), 1);
    assert_eq!(plan.resume(0).expect("first batch").to, KeyEpoch::new(3));
    assert!(plan.resume(2).is_none());
}

#[test]
fn a_release_addressed_to_another_node_is_refused() {
    let mut hub = empty_hub();
    let g = group("bank_details");
    hub.create_group(&g, MonoTime::ZERO).expect("create group");
    let a = node("branch-a");
    let b = node("branch-b");

    // Registering a wrapping key for the wrong node is a mismatch.
    assert!(matches!(
        hub.register_node(&b, NodeWrapKey::generate(a.clone()).expect("wrap key")),
        Err(ProtectedKeyError::WrongNode { .. })
    ));

    hub.register_node(&a, NodeWrapKey::generate(a.clone()).expect("wrap key"))
        .expect("register a");
    let mut tiers = TierTable::new();
    grant(&mut tiers, &a, &g);
    let release = hub
        .release(&a, &g, MonoTime::ZERO, &tiers)
        .expect("release");

    let mut store_b = NodeKeyStore::new(b, NodeWrapKey::generate(node("branch-b")).expect("wrap"));
    assert!(matches!(
        store_b.accept(&release, MonoTime::ZERO),
        Err(ProtectedKeyError::WrongNode { .. })
    ));
}

#[test]
fn a_ciphertext_cannot_be_moved_between_fields() {
    let key = LockedKey::from_bytes(&[3u8; 32]).expect("key");
    let here = FieldAd::new(tenant(), "Bank Account", "iban", "BA-1", KeyEpoch::FIRST);
    let elsewhere = FieldAd::new(tenant(), "Bank Account", "iban", "BA-2", KeyEpoch::FIRST);
    let sealed = FieldCipher::encrypt(&key, &here, b"DE89").expect("encrypt");
    assert!(FieldCipher::decrypt(&key, &here, &sealed).is_ok());
    assert!(matches!(
        FieldCipher::decrypt(&key, &elsewhere, &sealed),
        Err(ProtectedKeyError::Tampered)
    ));
}

#[test]
fn the_blind_index_is_deterministic_and_uses_its_own_key() {
    let key = BlindIndexKey::generate().expect("index key");
    let other = BlindIndexKey::generate().expect("index key");
    let a = key.index(b"DE89370400440532013000");
    let b = key.index(b"DE89370400440532013000");
    let c = key.index(b"GB33BUKB20201555555555");
    let d = other.index(b"DE89370400440532013000");
    assert!(a.matches(&b));
    assert!(!a.matches(&c));
    assert!(!a.matches(&d));
}
