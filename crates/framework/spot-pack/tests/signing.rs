//! The signing suite: the happy-path sign → verify round trip over a really packed bundle,
//! then every tamper the brief lists, each refused with its exact `signing.*` code. Keys are
//! generated in-test from fixed seeds, live in test memory and temp files (never an
//! environment variable, never printed), and the certificate, anchor, revocation list and
//! policy are exactly what the installer passes.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_pack::KeyProvider as _;

use common::{
    APP_ID, NOW, SIGNED_AT, VERSION, anchor, certificate, certificate_for_other,
    certificate_window, chain, empty_list, good_dir, list_revoking, publisher, root, signed_by,
    workdir,
};

// --- the round trip -----------------------------------------------------------------------------

/// The happy path: pack → sign with the certificate's key → verify with the installer's
/// inputs. The proof carries the publisher, the key id, the digest and the signing time.
#[test]
fn sign_then_verify_round_trip() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    let certificate = certificate();
    let signature = signed_by(&bundle, &publisher(), &certificate);
    let proof = spot_pack::verify_packed(
        &bundle,
        &signature,
        &chain(&certificate),
        &anchor(),
        &empty_list(1),
        NOW,
        &spot_pack::install_policy(),
    )
    .expect("the signature verifies");
    assert_eq!(proof.publisher_id, "acme");
    assert_eq!(proof.key_id, publisher().public().key_id());
    assert_eq!(proof.signed_at, SIGNED_AT);
    assert_eq!(proof.digest.to_hex(), packed.digest.to_hex());
    assert_eq!(proof.revocation_generation, 1);
}

/// The key-file provider reads exactly the file `write_key_file` writes (32 base64 seed
/// bytes) and reproduces the same key id. Its `Debug` shows the path, never the bytes.
#[test]
fn file_key_provider_round_trips_the_key_file() {
    let (_keep, root_dir) = workdir();
    let key_path = root_dir.join("publisher.key");
    spot_pack::write_key_file(&publisher(), &key_path).expect("the key file is written");
    let provider = spot_pack::FileKeyProvider::new(&key_path);
    let pair = provider.key_pair().expect("the file holds a usable key");
    assert_eq!(pair.public().key_id(), publisher().public().key_id());
    let debug = format!("{provider:?}");
    assert!(
        debug.contains("publisher.key") && !debug.contains('='),
        "the Debug shows the file, never key material: {debug}"
    );
}

/// A key file holding anything but 32 base64 bytes is refused as unreadable — the `pack.sign`
/// code, and the message never echoes the file's contents.
#[test]
fn a_bad_key_file_is_key_unreadable() {
    let (_keep, root_dir) = workdir();
    let key_path = root_dir.join("bad.key");
    std::fs::write(&key_path, b"not base64 at all!\n").expect("the bad key file exists");
    let provider = spot_pack::FileKeyProvider::new(&key_path);
    let err = provider
        .key_pair()
        .expect_err("the file holds no usable key");
    assert!(matches!(err, spot_pack::PackError::KeyUnreadable));
    assert_eq!(err.code(), "pack.sign");
}

/// The signature names the app identity the manifest carries: the installer matches the
/// signature file against the bundle's manifest before the digest check.
#[test]
fn the_signature_names_the_app_identity() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    let certificate = certificate();
    let signature = signed_by(&bundle, &publisher(), &certificate);
    assert_eq!(signature.app_id, APP_ID);
    assert_eq!(signature.app_version, VERSION);
    assert_eq!(signature.publisher_id, "acme");
    assert_eq!(signature.signed_at, SIGNED_AT);
}

// --- the tamper suite ---------------------------------------------------------------------------

/// One flipped byte, ANYWHERE in the packed bytes: for every single position the flip either
/// damages the container (`Bundle::open` refuses) or moves the content the canonical digest
/// covers (the old signature then no longer verifies, `signing.signature_invalid`). The
/// digest covers the entries' CONTENT — manifest, component, assets — not the ZIP's
/// structural padding, so a flip that changes nothing the digest reads is a no-op by
/// construction, and the signature rightly still verifies. What this pins: no flip can
/// smuggle changed content past the signature.
#[test]
fn one_flipped_byte_is_never_accepted() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    let certificate = certificate();
    let signature = signed_by(&bundle, &publisher(), &certificate);

    let len = packed.bytes.len();
    let mut refused_at_open = 0_usize;
    let mut verify_refused = 0_usize;
    let mut content_unchanged = 0_usize;
    for position in 0..len {
        let mut tampered_bytes = packed.bytes.clone();
        tampered_bytes[position] ^= 0x01;
        match spot_bundle::Bundle::open(&tampered_bytes, &options.limits, &options.host) {
            Err(_) => refused_at_open += 1,
            Ok(tampered) => {
                if tampered.digest() != bundle.digest() {
                    let outcome = spot_pack::verify_packed(
                        &tampered,
                        &signature,
                        &chain(&certificate),
                        &anchor(),
                        &empty_list(1),
                        NOW,
                        &spot_pack::install_policy(),
                    );
                    assert!(
                        outcome.is_err(),
                        "a content flip at byte {position} must be refused"
                    );
                    verify_refused += 1;
                } else {
                    content_unchanged += 1;
                }
            }
        }
    }
    // Every position landed in exactly one bucket, and every content flip was refused.
    assert_eq!(
        refused_at_open + verify_refused + content_unchanged,
        len,
        "every byte position is accounted for"
    );
    assert!(
        refused_at_open + verify_refused > 0,
        "the tamper paths exist in this bundle"
    );
}

/// The attacker edits the manifest and keeps the old signature file: the bundle re-packs to a
/// different digest, so the signature over the old digest is refused.
#[test]
fn signed_bytes_edited_afterwards_are_refused() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let certificate = certificate();
    let opened = packed.open(&options).expect("the packed bytes open");
    let signature = signed_by(&opened, &publisher(), &certificate);

    let edited_dir = good_dir(
        &root_dir,
        &[(
            "manifest.toml",
            format!(
                "[app]\n\
                 name = \"{}\"\n\
                 version = \"{}\"\n\
                 \n\
                 [[events]]\n\
                 doctype = \"customer\"\n\
                 event = \"before_save\"\n\
                 step = \"{}:on_save\"\n\
                 \n\
                 [[assets]]\n\
                 path = \"assets/logo.svg\"\n\
                 # tampered after signing\n",
                APP_ID, VERSION, APP_ID
            )
            .into_bytes(),
        )],
    );
    let (_r2, packed2) = spot_pack::pack_app(&edited_dir, &options).expect("the edited app packs");
    assert_ne!(
        packed.digest.to_hex(),
        packed2.digest.to_hex(),
        "the digest moves with the manifest"
    );
    let tampered = packed2.open(&options).expect("the edited bundle opens");
    let refusal = spot_pack::verify_packed(
        &tampered,
        &signature,
        &chain(&certificate),
        &anchor(),
        &empty_list(1),
        NOW,
        &spot_pack::install_policy(),
    )
    .expect_err("the old signature must not verify over the edited bundle");
    assert_eq!(refusal.code(), "signing.signature_invalid");
}

/// A signature made with a key the certificate does not name: refused at signing time
/// (`pack.sign`) — the packer never writes a signature its own certificate contradicts.
#[test]
fn signing_with_a_foreign_key_is_refused_at_signing_time() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    let mis_sign = spot_pack::sign_packed(
        &bundle,
        APP_ID,
        VERSION,
        &certificate(),
        &spot_bundle_signing::generate_keypair(&common::other_publisher().to_seed_bytes()),
        SIGNED_AT,
    );
    let err = mis_sign.expect_err("the other key is not the certificate's key");
    assert!(matches!(err, spot_pack::PackError::KeyMismatch));
    assert_eq!(err.code(), "pack.sign");
}

/// The other publisher, under its own certificate, signs a bundle and the installer holds the
/// FIRST publisher's certificate: every input the check needs is present but the identities
/// do not match the bundle — refused, whichever check fires (publisher, signature validity).
#[test]
fn cross_publisher_signature_is_refused() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    // The signature names publisher `other`; the presented certificate names `acme`, whose
    // key the signature was NOT made with.
    let signature = signed_by(
        &bundle,
        &common::other_publisher(),
        &certificate_for_other(),
    );
    let outcome = spot_pack::verify_packed(
        &bundle,
        &signature,
        &chain(&certificate()),
        &anchor(),
        &empty_list(1),
        NOW,
        &spot_pack::install_policy(),
    );
    assert!(
        outcome.is_err(),
        "the other publisher's signature must not verify"
    );
}

/// The signing key is on the revocation list with `effective_from` at or before the
/// signature's time: `signing.key_revoked`. A revocation list is state the installer holds
/// now; a key revoked at 100 refuses a signature made at 150.
#[test]
fn revoked_key_is_refused_as_key_revoked() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    let certificate = certificate();
    let signature = signed_by(&bundle, &publisher(), &certificate);
    let list = list_revoking(&publisher(), 100, false, 2);
    let refusal = spot_pack::verify_packed(
        &bundle,
        &signature,
        &chain(&certificate),
        &anchor(),
        &list,
        NOW,
        &spot_pack::install_policy(),
    )
    .expect_err("the revoked key must be refused");
    assert_eq!(refusal.code(), "signing.key_revoked");
}

/// A key revoked as compromised is refused for every signature, including one made before
/// the revocation took effect: `signing.key_compromised`.
#[test]
fn compromised_key_is_refused_even_for_earlier_signatures() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    let certificate = certificate();
    let signature = signed_by(&bundle, &publisher(), &certificate);
    let list = list_revoking(&publisher(), 300, true, 2);
    let refusal = spot_pack::verify_packed(
        &bundle,
        &signature,
        &chain(&certificate),
        &anchor(),
        &list,
        NOW,
        &spot_pack::install_policy(),
    )
    .expect_err("the compromised key's signatures are refused");
    assert_eq!(refusal.code(), "signing.key_compromised");
}

/// An expired certificate: verify refuses with `signing.certificate_expired` under the
/// install policy, where no expiry grace exists.
#[test]
fn expired_certificate_is_refused_at_install() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    let signature = signed_by(&bundle, &publisher(), &certificate());
    // Valid over [100, 200); the signature at 150 is inside, the check at 500 is past.
    let expired = certificate_window(100, 200);
    let refusal = spot_pack::verify_packed(
        &bundle,
        &signature,
        &chain(&expired),
        &anchor(),
        &empty_list(1),
        NOW,
        &spot_pack::install_policy(),
    )
    .expect_err("the expired certificate must be refused");
    assert_eq!(refusal.code(), "signing.certificate_expired");
}

/// A certificate whose window has not opened yet: `signing.certificate_not_yet_valid`.
#[test]
fn not_yet_valid_certificate_is_refused() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    let signature = signed_by(&bundle, &publisher(), &certificate());
    // Valid over [600, 1200); the check time 500 sits before the start.
    let future = certificate_window(600, 1200);
    let refusal = spot_pack::verify_packed(
        &bundle,
        &signature,
        &chain(&future),
        &anchor(),
        &empty_list(1),
        NOW,
        &spot_pack::install_policy(),
    )
    .expect_err("the future certificate must be refused");
    assert_eq!(refusal.code(), "signing.certificate_not_yet_valid");
}

/// A signature whose signing time sits outside the certificate's window:
/// `signing.signed_outside_certificate_window`.
#[test]
fn signed_outside_the_certificate_window_is_refused() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    // The signature claims 150; the certificate only opens at 300.
    let narrow = certificate_window(300, 1200);
    let signature = spot_pack::sign_packed(
        &bundle,
        APP_ID,
        VERSION,
        &narrow,
        &spot_bundle_signing::generate_keypair(&publisher().to_seed_bytes()),
        SIGNED_AT,
    )
    .expect("the key is the certificate's");
    let refusal = spot_pack::verify_packed(
        &bundle,
        &signature,
        &chain(&narrow),
        &anchor(),
        &empty_list(1),
        NOW,
        &spot_pack::install_policy(),
    )
    .expect_err("the signing time must sit inside the window");
    assert_eq!(refusal.code(), "signing.signed_outside_certificate_window");
}

/// A revocation list signed by a root the anchor does not trust:
/// `signing.revocation_signature_invalid`. The generation-3 list would replace the caller's
/// generation-1 empty list, which is exactly why a rogue list must never be accepted.
#[test]
fn revocation_list_from_an_unknown_root_is_refused() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    let certificate = certificate();
    let signature = signed_by(&bundle, &publisher(), &certificate);
    let rogue = spot_bundle_signing::generate_keypair(&[9u8; 32]);
    let rogue_list = spot_bundle_signing::issue_revocation_list(
        spot_bundle_signing::RevocationBody {
            generation: 3,
            issued_at: 0,
            issued_by: rogue.public().key_id(),
            keys: Vec::new(),
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        &rogue,
    );
    let refusal = spot_pack::verify_packed(
        &bundle,
        &signature,
        &chain(&certificate),
        &anchor(),
        &rogue_list,
        NOW,
        &spot_pack::install_policy(),
    )
    .expect_err("the rogue list must be refused");
    // The trust walk refuses the issuer before any list signature check: the rogue root is
    // not one the anchor holds (`signing.unknown_root`), the coarse code the installer sees.
    assert_eq!(refusal.code(), "signing.unknown_root");
}

/// The publisher (not just one key) is on the revocation list:
/// `signing.publisher_revoked`.
#[test]
fn revoked_publisher_is_refused_as_publisher_revoked() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    let certificate = certificate();
    let signature = signed_by(&bundle, &publisher(), &certificate);
    let list = spot_bundle_signing::issue_revocation_list(
        spot_bundle_signing::RevocationBody {
            generation: 2,
            issued_at: 0,
            issued_by: root().public().key_id(),
            keys: Vec::new(),
            publishers: vec![spot_bundle_signing::RevokedPublisher {
                publisher_id: "acme".to_owned(),
                effective_from: 100,
                reason: "test".to_owned(),
            }],
            bundles: Vec::new(),
        },
        &root(),
    );
    let refusal = spot_pack::verify_packed(
        &bundle,
        &signature,
        &chain(&certificate),
        &anchor(),
        &list,
        NOW,
        &spot_pack::install_policy(),
    )
    .expect_err("the revoked publisher must be refused");
    assert_eq!(refusal.code(), "signing.publisher_revoked");
}

/// The bundle digest itself is on the revocation list: `signing.bundle_revoked`.
#[test]
fn revoked_bundle_digest_is_refused_as_bundle_revoked() {
    let (_keep, root_dir) = workdir();
    let dir = good_dir(&root_dir, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let bundle = packed.open(&options).expect("the packed bytes open");
    let certificate = certificate();
    let signature = signed_by(&bundle, &publisher(), &certificate);
    let list = spot_bundle_signing::issue_revocation_list(
        spot_bundle_signing::RevocationBody {
            generation: 2,
            issued_at: 0,
            issued_by: root().public().key_id(),
            keys: Vec::new(),
            publishers: Vec::new(),
            bundles: vec![spot_bundle_signing::RevokedBundle {
                digest_hex: bundle.digest().to_hex(),
                reason: "test".to_owned(),
            }],
        },
        &root(),
    );
    let refusal = spot_pack::verify_packed(
        &bundle,
        &signature,
        &chain(&certificate),
        &anchor(),
        &list,
        NOW,
        &spot_pack::install_policy(),
    )
    .expect_err("the revoked bundle must be refused");
    assert_eq!(refusal.code(), "signing.bundle_revoked");
}
