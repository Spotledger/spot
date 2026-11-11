//! Tests of signing, verification and key rotation.

use ed25519_dalek::{SigningKey, VerifyingKey};

use crate::{Manifest, ManifestError, SignedManifest, VerifyingKeys, sign, verify};

use super::{hash, journal, key, key_id, keys, manifest, tiling, verified};

/// The verification error code of `signed` under `keys`.
fn refusal(signed: &SignedManifest, keys: &VerifyingKeys) -> &'static str {
    verify(signed, keys)
        .expect_err("the signature must not verify")
        .code()
}

#[test]
fn a_signed_manifest_verifies() {
    let base = journal(10, 5);
    let signed = sign(&base, &key(7), &key_id(7));
    assert_eq!(signed.manifest, base);
    assert_eq!(signed.key_id, key_id(7));
    assert_eq!(signed.signature.len(), 64);
    assert_eq!(signed.generation(), 1);
    let verified = verify(&signed, &keys(7, 1, 10)).expect("the signature holds");
    assert_eq!(verified.manifest(), &base);
    assert_eq!(verified.source(), "site:demo");
    assert_eq!(verified.generation(), 1);
    assert_eq!(verified.key_id(), &key_id(7));
    assert_eq!(verified.segments().len(), 2);
    assert!(verified.forks().is_empty());
    assert_eq!(verified.last_seq(), 10);
    assert_eq!(verified.issued_at_millis(), base.issued_at_millis);
}

/// One field change a test applies to a signed manifest before verifying it again.
type Mutation = (&'static str, Box<dyn Fn(&mut Manifest)>);

#[test]
fn every_field_of_the_manifest_is_covered_by_the_signature() {
    let keys = keys(7, 1, 10);
    let mutations: Vec<Mutation> = vec![
        ("source", Box::new(|m| m.source.push('x'))),
        ("generation", Box::new(|m| m.generation = 2)),
        (
            "first_seq",
            Box::new(|m| {
                m.segments[0].first_seq = 0;
            }),
        ),
        ("last_seq", Box::new(|m| m.segments[0].last_seq = 4)),
        ("entry_count", Box::new(|m| m.segments[0].entry_count = 4)),
        (
            "segment_hash",
            Box::new(|m| m.segments[0].segment_hash = hash(0x31)),
        ),
        (
            "previous_hash",
            Box::new(|m| m.segments[0].previous_hash = hash(0x32)),
        ),
        (
            "segments",
            Box::new(|m| {
                m.segments.pop();
            }),
        ),
        (
            "forks",
            Box::new(|m| m.forks.push(crate::Fork::new(2, 3, "extra", 0))),
        ),
        (
            "previous_manifest_hash",
            Box::new(|m| m.previous_manifest_hash = Some(hash(0x33))),
        ),
        ("issued_at_millis", Box::new(|m| m.issued_at_millis += 1)),
        (
            "issued_at_millis backwards",
            Box::new(|m| m.issued_at_millis -= 1),
        ),
    ];
    for (label, mutate) in mutations {
        let base = journal(10, 5);
        let mut tampered = sign(&base, &key(7), &key_id(7));
        mutate(&mut tampered.manifest);
        assert_eq!(
            refusal(&tampered, &keys),
            "journal_manifest.signature_invalid",
            "changing the {label} must break the signature"
        );
    }
}

#[test]
fn the_key_id_is_covered_by_the_signature() {
    let base = journal(10, 5);
    let mut signed = sign(&base, &key(7), &key_id(7));
    // The window set knows both ids; a signature cannot move to the other one.
    let mut rotation = keys(7, 1, 10);
    rotation
        .add_key(&key_id(8), key(8).verifying_key(), 1, 10)
        .expect("the second window is valid");
    signed.key_id = key_id(8);
    assert_eq!(
        refusal(&signed, &rotation),
        "journal_manifest.signature_invalid"
    );
}

#[test]
fn the_signature_bytes_are_covered_by_the_signature() {
    let keys = keys(7, 1, 10);
    let mut signed = sign(&journal(10, 5), &key(7), &key_id(7));
    signed.signature[0] ^= 0xff;
    assert_eq!(
        refusal(&signed, &keys),
        "journal_manifest.signature_invalid"
    );
    let mut truncated = sign(&journal(10, 5), &key(7), &key_id(7));
    truncated.signature.truncate(63);
    assert_eq!(
        refusal(&truncated, &keys),
        "journal_manifest.signature_invalid"
    );
}

#[test]
fn an_unknown_key_id_is_refused() {
    let signed = sign(&journal(10, 5), &key(9), &key_id(9));
    assert_eq!(
        refusal(&signed, &keys(7, 1, 10)),
        "journal_manifest.key_unknown"
    );
    let error = verify(&signed, &keys(7, 1, 10)).expect_err("the key is unknown");
    assert_eq!(error, ManifestError::KeyUnknown(key_id(9)));
    assert!(VerifyingKeys::new().is_empty());
    assert_eq!(VerifyingKeys::new().len(), 0);
}

#[test]
fn a_rotated_key_verifies_the_generations_it_signed() {
    // Key 7 signed generations 1 and 2; key 8 took over from generation 3.
    let mut rotation = keys(7, 1, 2);
    rotation
        .add_key(&key_id(8), key(8).verifying_key(), 3, 5)
        .expect("a rotation window is valid");
    assert_eq!(rotation.len(), 2);

    let first = journal(10, 5);
    assert!(verify(&sign(&first, &key(7), &key_id(7)), &rotation).is_ok());

    let third = manifest("site:demo", 3, tiling(3, 4, 4, 9));
    assert!(verify(&sign(&third, &key(8), &key_id(8)), &rotation).is_ok());

    // The old key no longer covers generation 3, and the new one never covered generation 1.
    assert_eq!(
        refusal(&sign(&third, &key(7), &key_id(7)), &rotation),
        "journal_manifest.key_not_valid_for_generation"
    );
    assert_eq!(
        refusal(&sign(&first, &key(8), &key_id(8)), &rotation),
        "journal_manifest.key_not_valid_for_generation"
    );
}

#[test]
fn a_signature_by_another_key_is_refused() {
    // The manifest names key 7, but key 8 signed it.
    let signed = sign(&journal(10, 5), &key(8), &key_id(7));
    assert_eq!(
        refusal(&signed, &keys(7, 1, 10)),
        "journal_manifest.signature_invalid"
    );
}

#[test]
fn a_key_window_must_not_overlap_itself() {
    let mut set = keys(7, 1, 5);
    let error = set
        .add_key(&key_id(7), key(7).verifying_key(), 5, 9)
        .expect_err("overlapping windows of one id are ambiguous");
    assert_eq!(error.code(), "journal_manifest.key_window_invalid");
    assert!(matches!(error, ManifestError::KeyWindowInvalid(_)));
    // A window that starts after the first one ends is disjoint and therefore allowed.
    set.add_key(&key_id(7), key(7).verifying_key(), 6, 9)
        .expect("a disjoint window of the same key is unambiguous");
}

#[test]
fn a_key_window_must_be_a_window() {
    let mut set = VerifyingKeys::new();
    assert!(matches!(
        set.add_key(&key_id(7), key(7).verifying_key(), 5, 4),
        Err(ManifestError::KeyWindowInvalid(_))
    ));
    assert!(matches!(
        set.add_key(&key_id(7), key(7).verifying_key(), 0, 4),
        Err(ManifestError::KeyWindowInvalid(_))
    ));
    assert!(matches!(
        set.add_key("  ", key(7).verifying_key(), 1, 4),
        Err(ManifestError::KeyWindowInvalid(_))
    ));
    assert!(set.is_empty());
}

#[test]
fn a_public_key_must_be_an_ed25519_key() {
    let mut set = VerifyingKeys::new();
    assert_eq!(
        set.add(&key_id(7), &[0_u8; 31], 1, 5)
            .expect_err("31 bytes are not a public key"),
        ManifestError::KeyInvalid
    );
    assert_eq!(
        ManifestError::KeyInvalid.code(),
        "journal_manifest.key_invalid"
    );
    let bytes = key(7).verifying_key().to_bytes();
    set.add(&key_id(7), &bytes, 1, 5).expect("the key is valid");
    assert_eq!(set.len(), 1);
}

#[test]
fn an_authentic_but_inconsistent_manifest_is_refused() {
    // Signing does not validate: verification does, after the signature holds.
    let mut holed = journal(10, 5);
    holed.segments.remove(0);
    let signed = sign(&holed, &key(7), &key_id(7));
    let error = verify(&signed, &keys(7, 1, 10)).expect_err("the manifest is inconsistent");
    assert_eq!(
        error,
        ManifestError::ManifestGap {
            first_seq: 1,
            last_seq: 5
        }
    );
    assert_eq!(error.code(), "journal_manifest.manifest_gap");
}

#[test]
fn a_verified_manifest_is_proof_enough_to_compare() {
    let verified = verified(&journal(10, 5));
    assert_eq!(verified.segments(), &journal(10, 5).segments[..]);
}

#[test]
fn the_signing_key_type_is_the_callers() {
    // The crate never holds a secret: the key is the caller's `ed25519_dalek::SigningKey`.
    let signing: SigningKey = key(7);
    let verifying: VerifyingKey = signing.verifying_key();
    let signed = sign(&journal(4, 4), &signing, "operator-key");
    let mut set = VerifyingKeys::new();
    set.add_key("operator-key", verifying, 1, 1)
        .expect("the window is valid");
    assert!(verify(&signed, &set).is_ok());
}

#[test]
fn verify_message_agrees_with_verify_on_the_manifests_own_vectors() {
    let base = journal(10, 5);
    let signed = sign(&base, &key(7), &key_id(7));
    let set = keys(7, 1, 10);
    let check = |signed: &SignedManifest, set: &VerifyingKeys| {
        set.verify_message(
            &signed.key_id,
            signed.generation(),
            &signed.canonical_bytes(),
            &signed.signature,
        )
        .err()
        .map(|e| e.code())
    };
    assert_eq!(check(&signed, &set), None);
    assert!(verify(&signed, &set).is_ok());

    let mut flipped = signed.clone();
    flipped.signature[0] ^= 1;
    assert_eq!(check(&flipped, &set), Some(refusal(&flipped, &set)));

    let mut short = signed.clone();
    short.signature.truncate(63);
    assert_eq!(check(&short, &set), Some(refusal(&short, &set)));

    let outside = keys(7, 2, 10);
    assert_eq!(check(&signed, &outside), Some(refusal(&signed, &outside)));

    let stranger = keys(8, 1, 10);
    assert_eq!(check(&signed, &stranger), Some(refusal(&signed, &stranger)));
}
