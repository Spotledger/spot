#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! Recorded security finding: the node write is bound to the tenant, the DocType and the key version, and a node
//! holding the wrong tenant key cannot produce an envelope the hub accepts. The hub-side tests
//! need the private key and are compiled out of a `--features node` build; the key-version
//! test runs under both.

mod common;

use common::*;
#[cfg(not(feature = "node"))]
use spot_protected_keys::TenantId;
use spot_protected_keys::{FieldAd, MonoTime};
use spot_sealed_fields::EncryptedStore;
#[cfg(not(feature = "node"))]
use spot_sealed_fields::{NodeFieldStore, Sealed, SealedAd};

// --------------------------------------------------------------- hub side (not in a node build)

#[cfg(not(feature = "node"))]
mod hub_side {
    use super::*;
    use spot_sealed_fields::LocalHub;

    #[test]
    fn hub_unseal_errors_never_carry_the_value() {
        let hub = LocalHub::generate(tenant()).expect("hub");
        let other = LocalHub::generate(tenant()).expect("other hub");
        let mut store = node_store(hub.public_key());
        let sealed = write_marker(&mut store, &iban_ad()).expect("write");

        // The hub really opens it, so the failures below are about the marker's own blob.
        let opened = hub.unseal(&sealed, &iban_ad()).expect("unseal");
        assert_eq!(opened.expose(), marker());
        assert_clean("secret_debug", &format!("{opened:?}"));

        let mut shown = Vec::new();
        let moved = SealedAd::of_field(tenant(), DOCTYPE, "iban", "BA-2");
        shown.push(show(&hub.unseal(&sealed, &moved).expect_err("moved")));
        shown.push(show(
            &hub.unseal(&sealed, &SealedAd::unbound())
                .expect_err("unbound"),
        ));
        shown.push(show(
            &other.unseal(&sealed, &iban_ad()).expect_err("other hub"),
        ));
        let mut bytes = sealed.to_bytes();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        let corrupted = Sealed::from_bytes(&bytes).expect("shape is fine");
        shown.push(show(
            &hub.unseal(&corrupted, &iban_ad()).expect_err("corrupted"),
        ));
        for text in shown {
            assert_clean("error", &text);
        }
        // The hub's log description names ids only.
        assert_clean("log", &hub.describe(&group("bank_details"), "iban"));
    }

    #[test]
    fn a_node_holding_the_wrong_tenant_key_cannot_produce_an_envelope_the_hub_accepts() {
        let acme_hub = LocalHub::generate(tenant()).expect("acme hub");
        let other_tenant = TenantId::new("other").expect("tenant");
        let other_hub = LocalHub::generate(other_tenant.clone()).expect("other hub");

        // The node was provisioned with the other tenant's key and bound to the other tenant.
        let mut wrong = NodeFieldStore::new(
            node("branch-1"),
            other_tenant.clone(),
            DOCTYPE,
            other_hub.public_key(),
        );
        // 1. It cannot even build an envelope for acme: the refusal is on the node, before
        //    anything is sealed, stored or synced.
        let for_acme = write_marker(&mut wrong, &iban_ad()).expect_err("acme ad on other store");
        assert_eq!(for_acme.code(), spot_sealed_fields::MALFORMED);
        assert!(wrong.is_empty());
        assert!(wrong.blob("BA-1", "iban").is_none());

        // 2. What it can build is for the other tenant, and the acme hub refuses it under
        //    either binding.
        let other_ad = SealedAd::of_field(other_tenant, DOCTYPE, "iban", "BA-1");
        let envelope = write_marker(&mut wrong, &other_ad).expect("its own tenant");
        for ad in [&other_ad, &iban_ad()] {
            assert_eq!(
                acme_hub
                    .unseal(&envelope, ad)
                    .expect_err("not acme's")
                    .code(),
                spot_sealed_fields::TAMPERED
            );
        }
        assert_eq!(
            other_hub
                .unseal(&envelope, &other_ad)
                .expect("its own hub")
                .expose(),
            marker()
        );

        // 3. A node of acme holding the other tenant's hub key seals to the wrong hub; the
        //    store accepts the acme binding (it is its own), but acme's hub does not open it.
        let mut mixed =
            NodeFieldStore::new(node("branch-1"), tenant(), DOCTYPE, other_hub.public_key());
        let envelope = write_marker(&mut mixed, &iban_ad()).expect("acme binding, wrong key");
        assert_eq!(
            acme_hub
                .unseal(&envelope, &iban_ad())
                .expect_err("wrong key")
                .code(),
            spot_sealed_fields::TAMPERED
        );

        // 4. Bypassing the store with the raw writer is still refused, at the hub.
        let raw = spot_sealed_fields::SealedWriter::from_public_key(other_hub.public_key())
            .seal(marker(), &iban_ad())
            .expect("raw seal");
        assert_eq!(
            acme_hub.unseal(&raw, &iban_ad()).expect_err("raw").code(),
            spot_sealed_fields::TAMPERED
        );

        // 5. Control: the right tenant, key and binding is accepted.
        let mut right =
            NodeFieldStore::new(node("branch-1"), tenant(), DOCTYPE, acme_hub.public_key());
        let good = write_marker(&mut right, &iban_ad()).expect("right");
        assert_eq!(
            acme_hub
                .unseal(&good, &iban_ad())
                .expect("accepted")
                .expose(),
            marker()
        );
    }
}

// ----------------------------------------------------------------- key version, both feature sets

#[test]
fn an_encrypted_write_whose_ad_names_another_key_version_is_refused() {
    let now = MonoTime::from_millis(1_000);
    let (mut keys, g, epoch) = leased_node(now);
    let mut store = EncryptedStore::new(&mut keys);
    let right = FieldAd::new(tenant(), DOCTYPE, "iban", "BA-1", epoch);
    let wrong = FieldAd::new(tenant(), DOCTYPE, "iban", "BA-1", epoch.next());

    // Epoch 2 in the ad, epoch 1 key in the call: refused with a stable code, before the
    // cipher runs.
    let error = store
        .write(&g, epoch, &wrong, marker(), now)
        .expect_err("another key version");
    assert_eq!(error.code(), spot_sealed_fields::MALFORMED);
    // The reverse (epoch 1 in the ad, epoch 2 named in the call) is refused as well.
    let error = store
        .write(&g, epoch.next(), &right, marker(), now)
        .expect_err("another key version");
    assert_eq!(error.code(), spot_sealed_fields::MALFORMED);
    // The matching write is stored and reads back.
    let field = store
        .write(&g, epoch, &right, marker(), now)
        .expect("match");
    assert_eq!(field.epoch, epoch);
    assert_eq!(
        store.read(&field, &right, now).expect("read").expose(),
        marker()
    );
}
