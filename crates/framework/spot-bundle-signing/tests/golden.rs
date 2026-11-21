//! Golden vectors: the canonical bytes and the signatures of fixed keys and a fixed digest, byte
//! for byte. A change to the format or the encoding fails here, loudly.
//!
//! The values were captured from this implementation at format version 1 (the first release of
//! the signing layer). They are pinned on purpose: changing any of them changes what existing
//! signatures mean, so it needs a new domain prefix and format version, not an edit here.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bundle_signing::canonical::b64;

// CAPTURED FROM THIS IMPLEMENTATION, not from an independent reference: these values prove the
// format has not changed since v1, not that another implementation produces them.
#[test]
fn the_key_ids_are_pinned() {
    assert_eq!(
        publisher().public().key_id().as_str(),
        "6a3803d5f059902a1c6dafbc9ba47292"
    );
    assert_eq!(
        root().public().key_id().as_str(),
        "34750f98bd59fcfc946da45aaabe933b"
    );
}

// Captured from this implementation (not an independent reference).
#[test]
fn the_certificate_bytes_and_signature_are_pinned() {
    let cert = certificate();
    assert_eq!(
        String::from_utf8(cert.body.signed_bytes()).unwrap(),
        "SPOT-PUBLISHER-CERT-V1\n{\"display_name\":\"Acme Software\",\"issued_by\":\"34750f98bd59fcfc946da45aaabe933b\",\"public_key\":\"gTl3Dqh9F19Wo1Rmw0x+zMuNipG07jeiXfYPW4/Js5Q=\",\"publisher_id\":\"acme\",\"serial\":7,\"valid_from\":100,\"valid_to\":1000}"
    );
    assert_eq!(
        b64(cert.signature()),
        "XIPygnOwGOVGfDnWRgRz8l5WdJl3UkRd4vYaFFe+QAM7dcVGUJzXp8NLQRadsu7C56p9LDSkYysuOGJ7XQTxDw=="
    );
}

// Captured from this implementation (not an independent reference).
#[test]
fn the_bundle_digest_and_signature_are_pinned() {
    let bundle = good();
    assert_eq!(
        bundle.digest().to_hex(),
        "744bc3f6bfdada33a4c07127fb03314f7f153145cbd4795e8fa6b28f9e44ca82"
    );
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    assert_eq!(
        b64(sig.signature()),
        "Ie5SF/NY7lGQ4C8QmkcWaMS8t09q8l+MEDLilNGYUBwJGMakkjD9d1KSYy4kDtsDTSxM9Fo2ZeKWfAA3zPQQCg=="
    );
    assert_eq!(
        sig.to_json(),
        "{\n  \"format\": \"spot-bundle-signature\",\n  \"version\": 1,\n  \"app_id\": \"test_app\",\n  \"app_version\": \"1.0.0\",\n  \"publisher_id\": \"acme\",\n  \"key_id\": \"6a3803d5f059902a1c6dafbc9ba47292\",\n  \"signed_at\": 150,\n  \"signature\": \"Ie5SF/NY7lGQ4C8QmkcWaMS8t09q8l+MEDLilNGYUBwJGMakkjD9d1KSYy4kDtsDTSxM9Fo2ZeKWfAA3zPQQCg==\"\n}"
    );
}

// Captured from this implementation (not an independent reference).
#[test]
fn the_revocation_list_signature_is_pinned() {
    assert_eq!(
        b64(empty_list(1).signature()),
        "WZpVMcAus0UNh4ACzrcasnyXothTBhbd8RtZ+JpY2EwNMJj8QeEcMIep/wfUvr7ThC1+OD0kPVCcnYLcCrriBw=="
    );
}
