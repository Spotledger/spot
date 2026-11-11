//! Protecting and revealing an ordinary image.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;

use super::{at, blank, canonical, image, keys, personal_paths, protected, spec, strip};
use crate::{
    CounterNonces, FieldSpec, NONCE_LEN, NonceSource, PersonalFields, PiiError, protect, reveal,
};

#[test]
fn round_trip_restores_the_image() {
    let spec = spec();
    let keys = keys();
    let image = image("c-1", "alice", "bob", 1);

    let protected = protected(&image, &spec, &keys);
    assert_ne!(protected["email"], image["email"]);
    assert_eq!(reveal(&protected, &spec, &keys).expect("reveal"), image);
}

#[test]
fn protect_replaces_only_personal_values() {
    let spec = spec();
    let keys = keys();
    let image = image("c-1", "alice", "bob", 1);

    let protected = protected(&image, &spec, &keys);

    // Every envelope becomes the same marker and every other byte is compared: keys, links,
    // child rows, their order and their count, the subject columns and the version.
    assert_eq!(
        canonical(&strip(&protected)),
        canonical(&blank(&image, &personal_paths()))
    );
    assert_eq!(at(&protected, "items").as_array().expect("rows").len(), 2);
    assert_eq!(protected["version"], json!(1));
    assert_eq!(protected["link"], image["link"]);
}

#[test]
fn envelope_shape_is_the_documented_one() {
    let keys = keys();
    let protected = protected(&image("c-1", "alice", "bob", 1), &spec(), &keys);
    let envelope = at(&protected, "email");
    let object = envelope.as_object().expect("an envelope is an object");

    assert_eq!(object.len(), 5);
    assert_eq!(envelope["$pii"], json!(1));
    assert_eq!(envelope["subject"], json!("alice"));
    let nonce = URL_SAFE_NO_PAD
        .decode(envelope["nonce"].as_str().expect("nonce text"))
        .expect("nonce base64");
    assert_eq!(nonce.len(), NONCE_LEN);
    let ciphertext = URL_SAFE_NO_PAD
        .decode(envelope["ct"].as_str().expect("ciphertext text"))
        .expect("ciphertext base64");
    assert!(ciphertext.len() > 16, "ciphertext carries a Poly1305 tag");
    let aad_hash = URL_SAFE_NO_PAD
        .decode(envelope["aad_hash"].as_str().expect("hash text"))
        .expect("hash base64");
    assert_eq!(aad_hash.len(), 32);
}

#[test]
fn two_subjects_in_one_image_use_their_own_keys() {
    let spec = spec();
    let store = keys();
    let image = image("c-1", "alice", "bob", 1);

    let protected = protected(&image, &spec, &store);
    assert_eq!(at(&protected, "email")["subject"], json!("alice"));
    assert_eq!(at(&protected, "phone")["subject"], json!("alice"));
    assert_eq!(at(&protected, "items.0.note")["subject"], json!("alice"));
    assert_eq!(at(&protected, "guardian_email")["subject"], json!("bob"));

    // The two subjects' fields do not open under each other's key.
    let swapped = keys();
    swapped.insert("alice", crate::KeyHandle::new([8u8; 32]));
    assert_eq!(
        reveal(&protected, &spec, &swapped)
            .expect_err("another key")
            .code(),
        "journal_pii.envelope_invalid"
    );
}

#[test]
fn absent_field_is_skipped() {
    let spec = spec();
    let keys = keys();
    let mut image = image("c-1", "alice", "bob", 1);
    image.as_object_mut().expect("object").remove("items");

    let protected = protected(&image, &spec, &keys);
    assert!(protected.get("items").is_none());
    assert_eq!(reveal(&protected, &spec, &keys).expect("reveal"), image);
}

#[test]
fn secret_field_is_passed_through_both_ways() {
    let spec = PersonalFields::new("Contact")
        .expect("entity")
        .with_field(FieldSpec::new("email", "owner", "notifications", "contact_5y").expect("email"))
        .with_field(
            FieldSpec::new("api_token", "owner", "integration", "contract_7y")
                .expect("secret")
                .with_secret(true),
        );
    let keys = keys();
    let sealed = "spsec:v1:AAAA";
    let mut image = image("c-1", "alice", "bob", 1);
    super::set_at(&mut image, "api_token", json!(sealed));

    let protected = protect(&image, &spec, &keys, &mut CounterNonces::new()).expect("protect");
    assert_eq!(protected["api_token"], json!(sealed));
    assert_ne!(protected["email"], image["email"]);
    let revealed = reveal(&protected, &spec, &keys).expect("reveal");
    assert_eq!(revealed["api_token"], json!(sealed));
    assert_eq!(revealed, image);
}

#[test]
fn reveal_passes_through_unprotected_values() {
    let spec = spec();
    let keys = keys();
    let image = image("c-1", "alice", "bob", 1);

    // A record written before the field was declared personal stays readable, and revealing is
    // idempotent once every value is back.
    assert_eq!(reveal(&image, &spec, &keys).expect("reveal"), image);
}

#[test]
fn same_plaintext_twice_encrypts_differently() {
    let spec = spec();
    let keys = keys();
    let image = image("c-1", "alice", "bob", 1);
    let mut nonces = CounterNonces::new();

    let first = protect(&image, &spec, &keys, &mut nonces).expect("first");
    let second = protect(&image, &spec, &keys, &mut nonces).expect("second");

    // Equal ciphertexts would leak that two records hold the same value, so the nonce must
    // differ even though the key and the plaintext are the same.
    assert_ne!(at(&first, "email")["ct"], at(&second, "email")["ct"]);
    assert_ne!(at(&first, "email")["nonce"], at(&second, "email")["nonce"]);
    assert_eq!(reveal(&first, &spec, &keys).expect("reveal"), image);
    assert_eq!(reveal(&second, &spec, &keys).expect("reveal"), image);
}

#[test]
fn one_nonce_per_envelope() {
    let mut nonces = CounterNonces::new();
    let _ = protect(
        &image("c-1", "alice", "bob", 1),
        &spec(),
        &keys(),
        &mut nonces,
    )
    .expect("protect");
    // email, phone, two child notes, guardian_email.
    assert_eq!(nonces.issued(), 5);
}

#[test]
fn image_without_id_or_version_is_refused() {
    let spec = spec();
    let keys = keys();
    let mut nonces = CounterNonces::new();

    let without_id =
        json!({"version": 1, "owner": "alice", "guardian": "bob", "email": "a@b.test"});
    assert_eq!(
        protect(&without_id, &spec, &keys, &mut nonces)
            .expect_err("no id")
            .code(),
        "journal_pii.image_invalid"
    );

    let without_version =
        json!({"id": "c-1", "owner": "alice", "guardian": "bob", "email": "a@b.test"});
    assert_eq!(
        protect(&without_version, &spec, &keys, &mut nonces)
            .expect_err("no version")
            .code(),
        "journal_pii.image_invalid"
    );

    assert_eq!(
        protect(&json!([1, 2]), &spec, &keys, &mut nonces)
            .expect_err("not an object")
            .code(),
        "journal_pii.image_invalid"
    );
}

#[test]
fn missing_subject_is_refused() {
    let spec = spec();
    let keys = keys();
    let mut nonces = CounterNonces::new();

    let mut no_subject = image("c-1", "alice", "bob", 1);
    no_subject.as_object_mut().expect("object").remove("owner");
    let error = protect(&no_subject, &spec, &keys, &mut nonces).expect_err("no subject");
    assert_eq!(error.code(), "journal_pii.subject_invalid");
    assert_eq!(
        error.to_string(),
        "the data-subject id is invalid: the subject path does not reach a value"
    );

    let mut blank_subject = image("c-1", "alice", "bob", 1);
    super::set_at(&mut blank_subject, "owner", json!(""));
    assert_eq!(
        protect(&blank_subject, &spec, &keys, &mut nonces)
            .expect_err("empty subject")
            .code(),
        "journal_pii.subject_invalid"
    );
}

#[test]
fn already_protected_field_is_refused() {
    let spec = spec();
    let keys = keys();
    let image = image("c-1", "alice", "bob", 1);
    let once = protected(&image, &spec, &keys);

    let error = protect(&once, &spec, &keys, &mut CounterNonces::new()).expect_err("twice");
    assert_eq!(error.code(), "journal_pii.already_protected");
    assert_eq!(
        error.to_string(),
        "the personal field at email already holds an envelope"
    );
}

#[test]
fn missing_key_is_refused() {
    let spec = spec();
    let keys = crate::MemoryKeys::new(crate::KeyHandle::new([9u8; 32]));
    keys.insert("bob", crate::KeyHandle::new([8u8; 32]));
    let error = protect(
        &image("c-1", "alice", "bob", 1),
        &spec,
        &keys,
        &mut CounterNonces::new(),
    )
    .expect_err("alice has no key");
    assert_eq!(error.code(), "journal_pii.key_missing");
}

#[test]
fn exhausted_nonce_source_is_refused() {
    struct NoNonces;

    impl NonceSource for NoNonces {
        fn next_nonce(&mut self) -> Option<[u8; NONCE_LEN]> {
            None
        }
    }

    let error = protect(
        &image("c-1", "alice", "bob", 1),
        &spec(),
        &keys(),
        &mut NoNonces,
    )
    .expect_err("no nonce");
    assert_eq!(error.code(), "journal_pii.nonce_unavailable");
}

#[test]
fn duplicate_field_paths_are_refused() {
    let spec = PersonalFields::new("Contact")
        .expect("entity")
        .with_field(FieldSpec::new("email", "owner", "a", "b").expect("email"))
        .with_field(FieldSpec::new("email", "owner", "c", "d").expect("email again"));

    let error = protect(
        &image("c-1", "alice", "bob", 1),
        &spec,
        &keys(),
        &mut CounterNonces::new(),
    )
    .expect_err("duplicate");
    assert_eq!(error.code(), "journal_pii.spec_invalid");
    assert_eq!(
        error.to_string(),
        "the personal-field description is invalid: two personal fields share one path"
    );
}

#[test]
fn a_field_needs_a_purpose_and_a_retention_class() {
    assert_eq!(
        FieldSpec::new("email", "owner", "", "contact_5y")
            .expect_err("no purpose")
            .code(),
        "journal_pii.spec_invalid"
    );
    assert_eq!(
        FieldSpec::new("email", "owner", "notifications", "")
            .expect_err("no retention class")
            .code(),
        "journal_pii.spec_invalid"
    );
    assert_eq!(
        PersonalFields::new("").expect_err("no entity").code(),
        "journal_pii.spec_invalid"
    );
}

#[test]
fn a_path_is_dot_separated_and_star_walks_arrays() {
    assert!(FieldSpec::new("items.*.note", "owner", "p", "r").is_ok());
    assert!(FieldSpec::new("", "owner", "p", "r").is_err());
    assert!(FieldSpec::new("items..note", "owner", "p", "r").is_err());
    assert!(FieldSpec::new("items.", "owner", "p", "r").is_err());
    assert!(FieldSpec::new("items.n*te", "owner", "p", "r").is_err());
    // A subject is a record-level value: it never walks an array.
    let error = FieldSpec::new("items.*.note", "items.*.owner", "p", "r").expect_err("subject");
    assert_eq!(error.code(), "journal_pii.spec_invalid");
}

#[test]
fn an_error_carries_no_value() {
    let keys = crate::MemoryKeys::without_retained();
    let spec = PersonalFields::new("Contact").expect("entity").with_field(
        FieldSpec::new("email", "owner", "notifications", "contact_5y")
            .expect("email")
            .with_erasable(false),
    );
    let image = image("c-1", "alice", "bob", 1);
    let error = protect(&image, &spec, &keys, &mut CounterNonces::new()).expect_err("no key");
    assert_eq!(error.code(), "journal_pii.key_store");
    assert!(!error.to_string().contains("ada@example.test"));
    assert!(matches!(
        error,
        PiiError::KeyStore(crate::KeyStoreError::Unavailable(_))
    ));
}
