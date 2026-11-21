//! Verification: a good manifest verifies; every tamper and trust failure is refused with its
//! own code.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_updater::{
    Digest, RevokedUpdateKey, UpdateSignature, verify_file, verify_payload, verify_update,
};

#[test]
fn a_good_manifest_verifies() {
    let manifest = manifest();
    let verified = verify_update(
        &manifest,
        Some(&sign(&manifest, SIGNER)),
        &anchor(),
        &empty_revocation(7),
        NOW,
        0,
    )
    .unwrap();
    assert_eq!(verified.key_id, key(SIGNER).public().key_id());
    assert_eq!(verified.revocation_generation, 7);
}

#[test]
fn an_unsigned_update_is_refused() {
    let code = refused_with(verify_update(
        &manifest(),
        None,
        &anchor(),
        &empty_revocation(1),
        NOW,
        0,
    ));
    assert_eq!(code, "updater.unsigned");
}

#[test]
fn an_unknown_key_is_refused() {
    let manifest = manifest();
    // Signed by REVOKER, which the anchor trusts only as the revocation issuer; but the signer
    // here is a key no anchor lists.
    let code = refused_with(verify_update(
        &manifest,
        Some(&sign(&manifest, 42)),
        &anchor(),
        &empty_revocation(1),
        NOW,
        0,
    ));
    assert_eq!(code, "updater.key_unknown");
}

#[test]
fn a_wrong_key_is_refused() {
    let manifest = manifest();
    // The signature names the trusted key but was made by another key.
    let forged = UpdateSignature::from_parts(
        key(SIGNER).public().key_id(),
        key(42).sign(&manifest.signing_message()),
    );
    let code = refused_with(verify_update(
        &manifest,
        Some(&forged),
        &anchor(),
        &empty_revocation(1),
        NOW,
        0,
    ));
    assert_eq!(code, "updater.signature");
}

#[test]
fn a_revoked_key_is_refused() {
    let manifest = manifest();
    let code = refused_with(verify_update(
        &manifest,
        Some(&sign(&manifest, SIGNER)),
        &anchor(),
        &revocation(2, vec![revoked_signer()]),
        NOW,
        0,
    ));
    assert_eq!(code, "updater.key_revoked");
}

#[test]
fn a_stale_revocation_list_is_refused() {
    let manifest = manifest();
    let code = refused_with(verify_update(
        &manifest,
        Some(&sign(&manifest, SIGNER)),
        &anchor(),
        &empty_revocation(3),
        NOW,
        4,
    ));
    assert_eq!(code, "updater.revocation_stale");
}

#[test]
fn a_bad_revocation_signature_is_refused() {
    let manifest = manifest();
    let revocation = spot_updater::UpdateRevocation::from_parts(
        empty_revocation(1).body,
        key(42).sign(&empty_revocation(1).body.signed_bytes()),
    );
    let code = refused_with(verify_update(
        &manifest,
        Some(&sign(&manifest, SIGNER)),
        &anchor(),
        &revocation,
        NOW,
        0,
    ));
    assert_eq!(code, "updater.revocation_signature");
}

#[test]
fn an_expired_anchor_window_is_refused() {
    let manifest = manifest();
    let code = refused_with(verify_update(
        &manifest,
        Some(&sign(&manifest, SIGNER)),
        &anchor_between((0, Some(NOW)), (0, None)),
        &empty_revocation(1),
        NOW,
        0,
    ));
    assert_eq!(code, "updater.key_expired");
}

#[test]
fn a_not_yet_valid_anchor_window_is_refused() {
    let manifest = manifest();
    let code = refused_with(verify_update(
        &manifest,
        Some(&sign(&manifest, SIGNER)),
        &anchor_between((NOW + 1, None), (0, None)),
        &empty_revocation(1),
        NOW,
        0,
    ));
    assert_eq!(code, "updater.key_not_yet_valid");
}

#[test]
fn a_tampered_manifest_is_refused() {
    let manifest = manifest();
    let signature = sign(&manifest, SIGNER);
    let mut tampered = manifest.clone();
    tampered.version = semver::Version::new(9, 9, 9);
    let code = refused_with(verify_update(
        &tampered,
        Some(&signature),
        &anchor(),
        &empty_revocation(1),
        NOW,
        0,
    ));
    assert_eq!(code, "updater.signature");
}

#[test]
fn a_flipped_byte_in_the_payload_is_refused() {
    let manifest = manifest();
    let mut bytes = payload_bytes();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    let code = refused_with(verify_payload(
        &manifest,
        bytes.len() as u64,
        Digest::of(&bytes),
    ));
    assert_eq!(code, "updater.payload_digest");
}

#[test]
fn a_wrong_payload_size_is_refused() {
    let manifest = manifest();
    let code = refused_with(verify_payload(&manifest, 1, manifest.payload.sha256));
    assert_eq!(code, "updater.payload_size");
}

#[test]
fn a_flipped_byte_in_a_file_is_refused() {
    let manifest = manifest();
    let mut bytes = file_bytes();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    let code = refused_with(verify_file(
        &manifest.files[0],
        bytes.len() as u64,
        Digest::of(&bytes),
    ));
    assert_eq!(code, "updater.file_digest");
}

#[test]
fn a_wrong_file_size_is_refused() {
    let manifest = manifest();
    let code = refused_with(verify_file(&manifest.files[0], 1, manifest.files[0].sha256));
    assert_eq!(code, "updater.file_size");
}

#[test]
fn the_matching_payload_and_file_verify() {
    let manifest = manifest();
    verify_payload(
        &manifest,
        payload_bytes().len() as u64,
        Digest::of(&payload_bytes()),
    )
    .unwrap();
    verify_file(
        &manifest.files[0],
        file_bytes().len() as u64,
        Digest::of(&file_bytes()),
    )
    .unwrap();
}

#[test]
fn a_revoked_key_on_the_list_still_verifies_when_named_differently() {
    let manifest = manifest();
    let other = RevokedUpdateKey {
        key_id: key(REVOKER).public().key_id(),
        reason: "retired".to_owned(),
    };
    verify_update(
        &manifest,
        Some(&sign(&manifest, SIGNER)),
        &anchor(),
        &revocation(1, vec![other]),
        NOW,
        0,
    )
    .unwrap();
}
