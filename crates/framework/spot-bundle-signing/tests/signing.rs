//! The acceptance line, literal: a valid signature verifies, and each tampered, revoked, stale,
//! untrusted or expired case is refused with its own code.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bundle_signing::*;

fn check(
    bundle: &spot_bundle::Bundle,
    sig: &BundleSignature,
    certificate: SignedCertificate,
    list: &RevocationList,
    now: i64,
    policy: &VerifyPolicy,
    version: &str,
) -> Result<Verified, SigningError> {
    let digest = bundle.digest();
    verify(
        &facts(&digest, version),
        sig,
        &chain(certificate),
        &anchor(),
        list,
        now,
        policy,
    )
}

#[test]
fn a_valid_signature_verifies() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let verified = check(
        &bundle,
        &sig,
        certificate(),
        &empty_list(1),
        NOW,
        &install(),
        VERSION,
    )
    .unwrap();
    assert_eq!(verified.publisher_id, "acme");
    assert_eq!(verified.key_id, publisher().public().key_id());
    assert_eq!(verified.signed_at, SIGNED_AT);
    assert_eq!(verified.revocation_generation, 1);
}

#[test]
fn one_flipped_byte_in_the_wasm_is_refused() {
    let signed = good();
    let sig = sign(&signed, APP_ID, VERSION, SIGNED_AT);
    let tampered = open(&manifest('a', VERSION), b"b");
    assert_ne!(signed.digest(), tampered.digest());
    let code = refused_with(check(
        &tampered,
        &sig,
        certificate(),
        &empty_list(1),
        NOW,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.signature_invalid");
}

#[test]
fn one_flipped_byte_in_the_manifest_is_refused() {
    let signed = good();
    let sig = sign(&signed, APP_ID, VERSION, SIGNED_AT);
    let tampered = open(&manifest('b', VERSION), b"a");
    let code = refused_with(check(
        &tampered,
        &sig,
        certificate(),
        &empty_list(1),
        NOW,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.signature_invalid");
}

#[test]
fn a_signature_moved_to_another_app_id_is_refused() {
    let bundle = good();
    let moved = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    // The file now says another app, but the bytes it was made over still name this one.
    let moved = BundleSignature::from_parts(
        "other_app".to_owned(),
        moved.app_version.clone(),
        moved.publisher_id.clone(),
        moved.key_id.clone(),
        moved.signed_at,
        *moved.signature(),
    );
    let code = refused_with(check(
        &bundle,
        &moved,
        certificate(),
        &empty_list(1),
        NOW,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.signature_invalid");
}

#[test]
fn a_signature_made_for_another_app_is_refused_as_app_mismatch() {
    let bundle = good();
    let other = sign(&bundle, "other_app", VERSION, SIGNED_AT);
    let code = refused_with(check(
        &bundle,
        &other,
        certificate(),
        &empty_list(1),
        NOW,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.app_mismatch");
}

#[test]
fn a_signature_for_1_0_0_cannot_be_moved_to_the_1_1_0_bundle() {
    // The manifest, and so the digest, names the version: the 1.0.0 signature does not cover
    // the 1.1.0 bundle's bytes.
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let upgraded = open(&manifest('a', "1.1.0"), b"a");
    let code = refused_with(check(
        &upgraded,
        &sig,
        certificate(),
        &empty_list(1),
        NOW,
        &install(),
        "1.1.0",
    ));
    assert_eq!(code, "signing.signature_invalid");
}

#[test]
fn a_caller_presenting_1_1_0_for_the_signed_1_0_0_is_refused_as_version_mismatch() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let code = refused_with(check(
        &bundle,
        &sig,
        certificate(),
        &empty_list(1),
        NOW,
        &install(),
        "1.1.0",
    ));
    assert_eq!(code, "signing.version_mismatch");
}

#[test]
fn an_expired_certificate_is_refused_at_install() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let expired = certificate_under(&root(), 300);
    let code = refused_with(check(
        &bundle,
        &sig,
        expired,
        &empty_list(1),
        400,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.certificate_expired");
}

#[test]
fn an_expired_certificate_is_accepted_at_startup_only_when_policy_allows() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let startup = |allow| VerifyPolicy {
        mode: VerifyMode::Startup,
        allow_expired_certificate_at_startup: allow,
        held_revocation_generation: 0,
    };
    let expired = || certificate_under(&root(), 300);
    assert!(
        check(
            &bundle,
            &sig,
            expired(),
            &empty_list(1),
            400,
            &startup(true),
            VERSION
        )
        .is_ok()
    );
    let code = refused_with(check(
        &bundle,
        &sig,
        expired(),
        &empty_list(1),
        400,
        &startup(false),
        VERSION,
    ));
    assert_eq!(code, "signing.certificate_expired");
}

#[test]
fn a_certificate_not_yet_valid_is_refused() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let code = refused_with(check(
        &bundle,
        &sig,
        certificate(),
        &empty_list(1),
        50,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.certificate_not_yet_valid");
}

fn list_revoking_key(effective_from: i64, compromised: bool) -> RevocationList {
    issue_revocation_list(
        RevocationBody {
            generation: 2,
            issued_at: 0,
            issued_by: root().public().key_id(),
            keys: vec![RevokedKey {
                key_id: publisher().public().key_id(),
                effective_from,
                compromised,
                reason: "test".to_owned(),
            }],
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        &root(),
    )
}

#[test]
fn a_key_revoked_before_signing_is_refused() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let list = list_revoking_key(100, false);
    let code = refused_with(check(
        &bundle,
        &sig,
        certificate(),
        &list,
        NOW,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.key_revoked");
}

#[test]
fn a_key_revoked_after_signing_does_not_invalidate_the_bundle() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let list = list_revoking_key(180, false);
    assert!(
        check(
            &bundle,
            &sig,
            certificate(),
            &list,
            NOW,
            &install(),
            VERSION
        )
        .is_ok()
    );
}

#[test]
fn a_compromised_key_is_refused_even_when_revoked_after_signing() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let list = list_revoking_key(180, true);
    let code = refused_with(check(
        &bundle,
        &sig,
        certificate(),
        &list,
        NOW,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.key_compromised");
}

#[test]
fn a_revoked_bundle_digest_is_refused() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let list = issue_revocation_list(
        RevocationBody {
            generation: 2,
            issued_at: 0,
            issued_by: root().public().key_id(),
            keys: Vec::new(),
            publishers: Vec::new(),
            bundles: vec![RevokedBundle {
                digest_hex: bundle.digest().to_hex(),
                reason: "test".to_owned(),
            }],
        },
        &root(),
    );
    let code = refused_with(check(
        &bundle,
        &sig,
        certificate(),
        &list,
        NOW,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.bundle_revoked");
}

#[test]
fn a_revoked_publisher_is_refused() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let list = issue_revocation_list(
        RevocationBody {
            generation: 2,
            issued_at: 0,
            issued_by: root().public().key_id(),
            keys: Vec::new(),
            publishers: vec![RevokedPublisher {
                publisher_id: "acme".to_owned(),
                effective_from: 100,
                reason: "test".to_owned(),
            }],
            bundles: Vec::new(),
        },
        &root(),
    );
    let code = refused_with(check(
        &bundle,
        &sig,
        certificate(),
        &list,
        NOW,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.publisher_revoked");
}

#[test]
fn a_stale_revocation_list_is_refused() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let policy = VerifyPolicy {
        held_revocation_generation: 5,
        ..install()
    };
    let code = refused_with(check(
        &bundle,
        &sig,
        certificate(),
        &empty_list(3),
        NOW,
        &policy,
        VERSION,
    ));
    assert_eq!(code, "signing.stale_revocation_list");
}

#[test]
fn an_unknown_root_is_refused() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let stranger = other_key();
    let code = refused_with(check(
        &bundle,
        &sig,
        certificate_under(&stranger, 1000),
        &empty_list(1),
        NOW,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.unknown_root");
}

#[test]
fn a_publisher_revoked_after_signing_does_not_invalidate_the_bundle() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let list = issue_revocation_list(
        RevocationBody {
            generation: 2,
            issued_at: 0,
            issued_by: root().public().key_id(),
            keys: Vec::new(),
            publishers: vec![RevokedPublisher {
                publisher_id: "acme".to_owned(),
                effective_from: 180,
                reason: "test".to_owned(),
            }],
            bundles: Vec::new(),
        },
        &root(),
    );
    assert!(
        check(
            &bundle,
            &sig,
            certificate(),
            &list,
            NOW,
            &install(),
            VERSION
        )
        .is_ok()
    );
}

#[test]
fn a_root_outside_its_validity_window_is_refused() {
    // The certificate's root is closed; the revocation list is signed by a second, open root,
    // so only the certificate's own root can be what refuses.
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let closed = TrustAnchor::new(vec![
        TrustedRoot::new(root().public(), 0, Some(100)),
        TrustedRoot::new(other_key().public(), 0, None),
    ]);
    let list = issue_revocation_list(
        RevocationBody {
            generation: 1,
            issued_at: 0,
            issued_by: other_key().public().key_id(),
            keys: Vec::new(),
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        &other_key(),
    );
    let digest = bundle.digest();
    let result = verify(
        &facts(&digest, VERSION),
        &sig,
        &chain(certificate()),
        &closed,
        &list,
        NOW,
        &install(),
    );
    assert_eq!(refused_with(result), "signing.root_outside_window");
}

#[test]
fn a_certificate_with_a_forged_issuer_is_refused() {
    // The body names the trusted root, but another key signed it.
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let body = certificate().body;
    let forged = SignedCertificate::from_parts(body.clone(), *certificate().signature());
    let attacker = other_key();
    let forged_by_attacker = issue_certificate(body, &attacker);
    assert_eq!(forged_by_attacker.body.issued_by, forged.body.issued_by);
    let code = refused_with(check(
        &bundle,
        &sig,
        forged_by_attacker,
        &empty_list(1),
        NOW,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.certificate_signature_invalid");
}

#[test]
fn a_signature_by_a_key_the_certificate_does_not_name_is_refused_when_signing() {
    let bundle = good();
    let result = sign_bundle(
        &bundle.digest(),
        APP_ID,
        VERSION,
        &certificate(),
        &other_key(),
        SIGNED_AT,
    );
    assert_eq!(refused_with(result), "signing.key_mismatch");
}

#[test]
fn a_revocation_list_signed_by_an_untrusted_key_is_refused() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    let forged = issue_revocation_list(
        RevocationBody {
            generation: 9,
            issued_at: 0,
            issued_by: root().public().key_id(),
            keys: Vec::new(),
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        &other_key(),
    );
    let code = refused_with(check(
        &bundle,
        &sig,
        certificate(),
        &forged,
        NOW,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.revocation_signature_invalid");
}

#[test]
fn key_pair_debug_never_shows_the_secret() {
    let key = publisher();
    let text = format!("{key:?}");
    let seed_hex: String = key
        .to_seed_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(text.contains("<redacted>"));
    assert!(!text.contains(&seed_hex));
    assert!(text.contains(key.public().key_id().as_str()));
}

#[test]
fn a_signature_dated_before_the_certificate_window_is_refused() {
    // The certificate is valid over [100, 200); the signer claims 50.
    let bundle = good();
    let cert = certificate_window(100, 200);
    let sig = sign_at(&bundle, 50, &cert);
    let code = refused_with(check(
        &bundle,
        &sig,
        cert,
        &empty_list(1),
        150,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.signed_outside_certificate_window");
}

#[test]
fn a_signature_dated_after_the_certificate_window_is_refused() {
    // The certificate is valid over [100, 200), and still is at 150; the signer claims 250.
    let bundle = good();
    let cert = certificate_window(100, 200);
    let sig = sign_at(&bundle, 250, &cert);
    let code = refused_with(check(
        &bundle,
        &sig,
        cert,
        &empty_list(1),
        150,
        &install(),
        VERSION,
    ));
    assert_eq!(code, "signing.signed_outside_certificate_window");
}

#[test]
fn an_expired_certificate_at_startup_accepts_only_a_signature_made_while_valid() {
    let bundle = good();
    let startup = VerifyPolicy {
        mode: VerifyMode::Startup,
        allow_expired_certificate_at_startup: true,
        held_revocation_generation: 0,
    };
    // The certificate is valid over [100, 200) and expired at 300.
    let cert = certificate_window(100, 200);
    let back_dated = sign_at(&bundle, 250, &cert);
    let code = refused_with(check(
        &bundle,
        &back_dated,
        cert.clone(),
        &empty_list(1),
        300,
        &startup,
        VERSION,
    ));
    assert_eq!(code, "signing.signed_outside_certificate_window");
    let made_while_valid = sign_at(&bundle, 150, &cert);
    assert!(
        check(
            &bundle,
            &made_while_valid,
            cert,
            &empty_list(1),
            300,
            &startup,
            VERSION
        )
        .is_ok()
    );
}
