//! An envelope that is moved or altered must not open, and must say so with
//! `journal_pii.envelope_invalid`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;

use super::{at, image, keys, protected, set_at, spec};
use crate::{PersonalFields, PiiError, erase, reveal};

/// The message the wrong-position refusals carry: the same code, one fixed phrase.
fn assert_wrong_position(error: PiiError) {
    assert_eq!(error.code(), "journal_pii.envelope_invalid");
    assert_eq!(
        error.to_string(),
        "the personal-field envelope is invalid: the associated data does not match where the envelope sits"
    );
}

#[test]
fn envelope_moved_to_another_field_fails() {
    let spec = spec();
    let keys = keys();
    let image = image("c-1", "alice", "bob", 1);
    let protected = protected(&image, &spec, &keys);

    let mut moved = protected.clone();
    set_at(&mut moved, "phone", at(&protected, "email").clone());

    assert_wrong_position(reveal(&moved, &spec, &keys).expect_err("moved field"));
}

#[test]
fn envelope_moved_to_another_row_fails() {
    let spec = spec();
    let keys = keys();
    let protected = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);

    let mut moved = protected.clone();
    set_at(
        &mut moved,
        "items.1.note",
        at(&protected, "items.0.note").clone(),
    );

    assert_wrong_position(reveal(&moved, &spec, &keys).expect_err("moved row"));
}

#[test]
fn envelope_moved_to_another_record_fails() {
    let spec = spec();
    let keys = keys();
    let first = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);

    let mut second = image("c-2", "alice", "bob", 1);
    set_at(&mut second, "email", at(&first, "email").clone());

    assert_wrong_position(reveal(&second, &spec, &keys).expect_err("moved record"));
}

#[test]
fn envelope_moved_to_another_version_fails() {
    let spec = spec();
    let keys = keys();
    let protected = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);

    let mut later = protected.clone();
    set_at(&mut later, "version", json!(2));

    assert_wrong_position(reveal(&later, &spec, &keys).expect_err("moved version"));
}

#[test]
fn envelope_moved_to_another_entity_fails() {
    let keys = keys();
    let protected = protected(&image("c-1", "alice", "bob", 1), &spec(), &keys);

    // Same fields, same record, another entity: the associated data names the entity.
    let other = PersonalFields::new("Lead").expect("entity").with_field(
        crate::FieldSpec::new("email", "owner", "notifications", "contact_5y").expect("email"),
    );

    assert_wrong_position(reveal(&protected, &other, &keys).expect_err("moved entity"));
}

#[test]
fn envelope_relabelled_to_another_subject_fails() {
    let spec = spec();
    let keys = keys();
    let protected = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);

    let mut relabelled = protected.clone();
    let mut envelope = at(&protected, "email").clone();
    envelope
        .as_object_mut()
        .expect("envelope")
        .insert("subject".to_owned(), json!("bob"));
    set_at(&mut relabelled, "email", envelope);

    // The record still names alice, so the check that runs before any key lookup refuses it.
    let error = reveal(&relabelled, &spec, &keys).expect_err("relabelled");
    assert_eq!(error.code(), "journal_pii.subject_mismatch");
}

#[test]
fn a_tampered_ciphertext_fails() {
    let spec = spec();
    let keys = keys();
    let protected = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);

    let mut tampered = protected.clone();
    let mut envelope = at(&protected, "email").clone();
    let text = at(&protected, "email")["ct"].as_str().expect("ct");
    let mut bytes = URL_SAFE_NO_PAD.decode(text).expect("base64");
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    envelope["ct"] = json!(URL_SAFE_NO_PAD.encode(&bytes));
    set_at(&mut tampered, "email", envelope);

    let error = reveal(&tampered, &spec, &keys).expect_err("tampered");
    assert_eq!(error.code(), "journal_pii.envelope_invalid");
    assert_eq!(
        error.to_string(),
        "the personal-field envelope is invalid: the envelope does not open where it sits"
    );
}

#[test]
fn an_unknown_envelope_version_is_refused() {
    let spec = spec();
    let keys = keys();
    let protected = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);

    let mut future = protected.clone();
    let mut envelope = at(&protected, "email").clone();
    envelope
        .as_object_mut()
        .expect("envelope")
        .insert("$pii".to_owned(), json!(2));
    set_at(&mut future, "email", envelope);

    let error = reveal(&future, &spec, &keys).expect_err("version 2");
    assert_eq!(error.code(), "journal_pii.envelope_version");
    assert_eq!(
        error.to_string(),
        "the personal-field envelope at email has version 2, which this build does not read"
    );
    assert!(matches!(
        error,
        PiiError::EnvelopeVersion { version: 2, .. }
    ));
}

#[test]
fn a_broken_envelope_shape_is_refused() {
    let spec = spec();
    let keys = keys();
    let protected = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);

    let mut missing_nonce = protected.clone();
    let mut envelope = at(&protected, "email").clone();
    envelope.as_object_mut().expect("envelope").remove("nonce");
    set_at(&mut missing_nonce, "email", envelope);
    assert_eq!(
        reveal(&missing_nonce, &spec, &keys)
            .expect_err("no nonce")
            .code(),
        "journal_pii.envelope_invalid"
    );

    let mut bad_base64 = protected.clone();
    let mut envelope = at(&protected, "email").clone();
    envelope["nonce"] = json!("not base64!!");
    set_at(&mut bad_base64, "email", envelope);
    assert_eq!(
        reveal(&bad_base64, &spec, &keys)
            .expect_err("bad base64")
            .code(),
        "journal_pii.envelope_invalid"
    );

    let mut short_nonce = protected.clone();
    let mut envelope = at(&protected, "email").clone();
    envelope["nonce"] = json!(URL_SAFE_NO_PAD.encode([0u8; 12]));
    set_at(&mut short_nonce, "email", envelope);
    assert_eq!(
        reveal(&short_nonce, &spec, &keys)
            .expect_err("short nonce")
            .code(),
        "journal_pii.envelope_invalid"
    );

    let mut not_a_number = protected.clone();
    let mut envelope = at(&protected, "email").clone();
    envelope["$pii"] = json!("one");
    set_at(&mut not_a_number, "email", envelope);
    assert_eq!(
        reveal(&not_a_number, &spec, &keys)
            .expect_err("marker is a string")
            .code(),
        "journal_pii.envelope_invalid"
    );
}

#[test]
fn a_moved_envelope_under_a_destroyed_key_still_fails() {
    let spec = spec();
    let keys = keys();
    let first = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);

    let mut second = image("c-2", "alice", "bob", 1);
    set_at(&mut second, "email", at(&first, "email").clone());

    erase("alice", &keys, None).expect("erase");

    // The key is gone, so nothing can be decrypted: the stored associated-data digest is what
    // tells "erased where it belongs" from "moved here", and the moved envelope must not be
    // reported as an erasure.
    assert_wrong_position(reveal(&second, &spec, &keys).expect_err("moved under a dead key"));
}
