#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! Property tests for the three rules the brief names: a revoked node cannot unwrap a key of
//! an epoch at or after its revocation epoch, rotation only ever moves forward, and a
//! ciphertext opens only under the associated data it was sealed with (WP-SEC-10.T1, item 7).

use proptest::prelude::*;

use spot_protected_keys::{
    FieldAd, FieldCipher, GroupName, Hub, KeyEpoch, KeyRelease, LockedKey, MemoryRootKeyProvider,
    MonoTime, NodeId, NodeKeyStore, NodeWrapKey, ProtectedKeyError, TenantId, Tier, TierTable,
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

proptest! {
    #[test]
    fn rotation_is_strictly_monotonic(rotations in 1usize..12) {
        let mut hub = Hub::new(MemoryRootKeyProvider::new(tenant()).expect("provider"));
        let g = group("bank_details");
        let mut epoch = hub.create_group(&g, MonoTime::ZERO).expect("create");
        prop_assert_eq!(epoch, KeyEpoch::FIRST);
        for i in 0..rotations {
            let next = hub
                .rotate_group(&g, MonoTime::from_millis(i as u64))
                .expect("rotate");
            prop_assert!(next > epoch, "epoch {next} must follow {epoch}");
            prop_assert_eq!(next.get(), epoch.get() + 1);
            prop_assert_eq!(hub.group_epoch(&g), Some(next));
            epoch = next;
        }
    }

    #[test]
    fn a_revoked_node_cannot_unwrap_at_or_after_its_revocation_epoch(extra in 0u64..16) {
        let n = node("branch-1");
        let wrap = NodeWrapKey::generate(n.clone()).expect("wrap key");
        let mut store = NodeKeyStore::new(n.clone(), wrap);
        let revoked = KeyEpoch::new(5);
        store.record_revocation(revoked);

        let epoch = KeyEpoch::new(revoked.get() + extra);
        let release = KeyRelease {
            tenant: tenant(),
            group: group("bank_details"),
            epoch,
            node: n.clone(),
            // A well-formed-looking wrapping; the revocation check happens first.
            wrapped_key: vec![0u8; 64],
            lease_until: MonoTime::from_millis(1_000_000),
        };
        let result = store.accept(&release, MonoTime::ZERO);
        prop_assert!(
            matches!(result, Err(ProtectedKeyError::NodeRevoked { .. })),
            "epoch {epoch} must be refused after revocation at {revoked}"
        );
        prop_assert!(!store.is_held(&group("bank_details"), epoch));
    }

    #[test]
    fn a_ciphertext_opens_only_under_its_own_place(
        field in "[a-z_]{1,6}",
        document in "[0-9]{1,6}",
    ) {
        let key = LockedKey::from_bytes(&[9u8; 32]).expect("key");
        let here = FieldAd::new(tenant(), "Bank Account", &field, &document, KeyEpoch::FIRST);
        let sealed = FieldCipher::encrypt(&key, &here, b"DE89370400440532013000").expect("encrypt");

        // The place it was sealed under opens it.
        prop_assert!(FieldCipher::decrypt(&key, &here, &sealed).is_ok());

        // Another document, another epoch or another field does not.
        let other_document = FieldAd::new(tenant(), "Bank Account", &field, "OTHER", KeyEpoch::FIRST);
        prop_assert!(matches!(
            FieldCipher::decrypt(&key, &other_document, &sealed),
            Err(ProtectedKeyError::Tampered)
        ));
        let other_field = FieldAd::new(tenant(), "Bank Account", "other_field", &document, KeyEpoch::FIRST);
        prop_assert!(matches!(
            FieldCipher::decrypt(&key, &other_field, &sealed),
            Err(ProtectedKeyError::Tampered)
        ));
        let later_epoch = FieldAd::new(tenant(), "Bank Account", &field, &document, KeyEpoch::new(2));
        prop_assert!(matches!(
            FieldCipher::decrypt(&key, &later_epoch, &sealed),
            Err(ProtectedKeyError::Tampered)
        ));
    }

    #[test]
    fn a_held_key_never_outlives_its_lease(advance in 0u64..1_000_000) {
        let mut hub = Hub::new(MemoryRootKeyProvider::new(tenant()).expect("provider"));
        let g = group("bank_details");
        hub.create_group(&g, MonoTime::ZERO).expect("create");
        let n = node("branch-1");
        let wrap = NodeWrapKey::generate(n.clone()).expect("wrap key");
        let node_wrap = wrap.duplicate().expect("duplicate");
        hub.register_node(&n, wrap).expect("register");
        let mut tiers = TierTable::new();
        tiers.grant(n.clone(), g.clone(), Tier::one());

        let now = MonoTime::from_millis(0);
        let release = hub.release(&n, &g, now, &tiers).expect("release");
        let mut store = NodeKeyStore::new(n.clone(), node_wrap);
        store.accept(&release, now).expect("accept");

        let later = release.lease_until.plus(spot_protected_keys::Duration::from_millis(advance));
        store.discard_expired(later);
        prop_assert!(!store.is_held(&g, KeyEpoch::FIRST));
        prop_assert!(store.key(&g, KeyEpoch::FIRST, later).is_err());
    }
}
