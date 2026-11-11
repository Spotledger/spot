use spot_journal_snapshot::{
    ImageRecord, SnapshotKeys, SnapshotParams, SnapshotReader, SnapshotSigner, SnapshotWriter,
    list_snapshots, mark_verified,
};
use spot_journal_store::MemoryStore;

use super::{block_on, keys, signer};

const MSG: &[u8] = b"the snapshot index bytes";

#[test]
fn rotation_keeps_old_generations_verifiable_and_nothing_else() {
    let set = keys(&[("a", 1, 1, 3), ("b", 2, 4, u64::MAX)]);
    let a = signer("a", 1);
    let b = signer("b", 2);
    let sig_a = a.sign(MSG);
    let sig_b = b.sign(MSG);
    for generation in 1..=3 {
        assert!(
            set.verify("a", generation, MSG, &sig_a),
            "a at {generation}"
        );
    }
    // A snapshot signed with A at generation 3 still verifies after the rotation to B.
    assert!(set.verify("b", 4, MSG, &sig_b));
    assert!(set.verify("b", 900, MSG, &sig_b));
    // A claimed for generation 4 does not.
    assert!(!set.verify("a", 4, MSG, &sig_a));
    // B claimed for an earlier generation does not.
    assert!(!set.verify("b", 3, MSG, &sig_b));
    // An unknown key does not.
    let stranger = signer("c", 3).sign(MSG);
    assert!(!set.verify("c", 2, MSG, &stranger));
    // A key id with another key's signature does not.
    assert!(!set.verify("a", 2, MSG, &sig_b));
}

#[test]
fn a_truncated_flipped_or_wrong_message_signature_is_refused() {
    let set = keys(&[("a", 1, 1, 10)]);
    let sig = signer("a", 1).sign(MSG);
    assert!(set.verify("a", 5, MSG, &sig));
    assert!(!set.verify("a", 5, MSG, &sig[..63]));
    assert!(!set.verify("a", 5, MSG, &[]));
    let mut longer = sig.clone();
    longer.push(0);
    assert!(!set.verify("a", 5, MSG, &longer));
    for index in [0, 31, 32, 63] {
        let mut flipped = sig.clone();
        flipped[index] ^= 0x01;
        assert!(!set.verify("a", 5, MSG, &flipped), "byte {index}");
    }
    assert!(!set.verify("a", 5, b"another message", &sig));
}

#[test]
fn signatures_are_deterministic_64_byte_ed25519() {
    let a = signer("a", 1);
    assert_eq!(a.key_id(), "a");
    assert_eq!(a.sign(MSG), a.sign(MSG));
    assert_eq!(a.sign(MSG).len(), 64);
    assert_ne!(a.sign(MSG), a.sign(b"other"));
}

#[test]
fn a_key_set_of_another_tenant_does_not_verify_this_tenants_signature() {
    // Tenant A and tenant B use the same key id, with different keys.
    let tenant_a = keys(&[("journal-1", 1, 1, 10)]);
    let tenant_b = keys(&[("journal-1", 9, 1, 10)]);
    let sig = signer("journal-1", 1).sign(MSG);
    assert!(tenant_a.verify("journal-1", 1, MSG, &sig));
    assert!(!tenant_b.verify("journal-1", 1, MSG, &sig));
}

#[test]
fn debug_output_carries_the_key_id_and_no_key_material() {
    let seed = 0x5a_u8;
    let signer = signer("journal-key-1", seed);
    let text = format!("{signer:?} {signer:#?}");
    assert!(text.contains("journal-key-1"));
    for needle in [
        format!("{seed}"),
        format!("{seed:02x}").repeat(4),
        format!("{seed:02X}").repeat(4),
        "signing_key".to_owned(),
        "SigningKey".to_owned(),
    ] {
        assert!(!text.contains(&needle), "Debug leaks {needle:?}: {text}");
    }
    // The signing key bytes themselves (all 0x5a) never print in any decimal form either.
    assert!(!text.contains("90, 90"));
    // The key set shows public material only; the point is it prints without panicking.
    let _ = format!("{:?}", keys(&[("a", 1, 1, 2)]));
}

fn params(tenant: &str, generation: u64, position: u64) -> SnapshotParams {
    SnapshotParams {
        tenant: tenant.to_owned(),
        generation,
        position,
        manifest_hash: "ab".repeat(32),
        taken_at_millis: 1_700_000_000_000,
    }
}

fn image(n: u64) -> ImageRecord {
    ImageRecord {
        entity: "account".to_owned(),
        id: format!("{n:06}"),
        version: n,
        entity_schema: 1,
        image: serde_json::json!({ "n": n }),
    }
}

#[test]
fn snapshot_write_list_mark_verified_and_read_back_with_the_adapters() {
    let store = MemoryStore::new();
    let set = keys(&[("a", 1, 1, 3), ("b", 2, 4, u64::MAX)]);
    let a = signer("a", 1);
    block_on(async {
        let mut writer = SnapshotWriter::new(&store, params("acme", 3, 100), &a).unwrap();
        writer.write_part((0..50).map(image)).await.unwrap();
        writer.finish().await.unwrap();

        let listed = list_snapshots(&store, "acme", &set).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].verified);

        mark_verified(&store, "acme", 3, 100, &set, &a)
            .await
            .unwrap();
        let listed = list_snapshots(&store, "acme", &set).await.unwrap();
        assert!(listed[0].verified);

        let reader = SnapshotReader::open(&store, "acme", 3, 100, &set)
            .await
            .unwrap();
        let mut stream = reader.images();
        let mut count = 0;
        while stream.next().await.unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 50);
    });
}

#[test]
fn a_marker_signed_with_another_tenants_key_is_not_trusted() {
    let store = MemoryStore::new();
    let set = keys(&[("a", 1, 1, 10)]);
    let a = signer("a", 1);
    // The attacker signs a marker under the same key id with a different key.
    let forger = signer("a", 77);
    block_on(async {
        let mut writer = SnapshotWriter::new(&store, params("acme", 1, 10), &a).unwrap();
        writer.write_part((0..3).map(image)).await.unwrap();
        writer.finish().await.unwrap();

        // The forger's marker is written with a key set that trusts the forger.
        let forged = keys(&[("a", 77, 1, 10)]);
        // The index is signed by `a`, so the forger's key set cannot even open it.
        assert!(
            mark_verified(&store, "acme", 1, 10, &forged, &forger)
                .await
                .is_err()
        );
        // With the genuine key set, no marker exists, so the snapshot is not verified.
        let listed = list_snapshots(&store, "acme", &set).await.unwrap();
        assert!(!listed[0].verified);
        // A foreign-signed index is not listed by the genuine key set.
        let mut writer = SnapshotWriter::new(&store, params("acme", 1, 20), &forger).unwrap();
        writer.write_part((0..3).map(image)).await.unwrap();
        writer.finish().await.unwrap();
        let listed = list_snapshots(&store, "acme", &set).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].position, 10);
    });
}

#[test]
fn a_marker_by_the_forger_alone_does_not_verify_a_genuine_snapshot() {
    let store = MemoryStore::new();
    let set = keys(&[("a", 1, 1, 10)]);
    let a = signer("a", 1);
    let forger = signer("a", 77);
    block_on(async {
        let mut writer = SnapshotWriter::new(&store, params("acme", 1, 10), &a).unwrap();
        writer.write_part((0..3).map(image)).await.unwrap();
        writer.finish().await.unwrap();
        // The forger trusts both keys to get past the index, then marks with its own key.
        let mut both = spot_journal_manifest::VerifyingKeys::new();
        both.add_key("a", super::secret(1).verifying_key(), 1, 10)
            .unwrap();
        let lenient = crate::ManifestSnapshotKeys::new(both);
        mark_verified(&store, "acme", 1, 10, &lenient, &forger)
            .await
            .unwrap();
        // The genuine key set does not trust that marker.
        let listed = list_snapshots(&store, "acme", &set).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].verified);
    });
}
