//! `reveal` checks the subject the image names against the subject the envelope names, before
//! any key lookup (WP-JRN-08.T2).

use serde_json::{Value, json};

use super::{at, image, keys, protected, set_at, spec};
use crate::{CounterNonces, erase, protect, reveal};

fn assert_mismatch(image: &Value) {
    let spec = spec();
    let error = reveal(image, &spec, &keys()).expect_err("mismatch");
    assert_eq!(error.code(), "journal_pii.subject_mismatch");
}

#[test]
fn subject_absent_from_the_image_is_a_mismatch() {
    let mut sealed = protected(&image("c-1", "alice", "bob", 1), &spec(), &keys());
    sealed.as_object_mut().expect("object").remove("owner");
    assert_mismatch(&sealed);
}

#[test]
fn subject_of_the_wrong_type_is_a_mismatch() {
    let mut sealed = protected(&image("c-1", "alice", "bob", 1), &spec(), &keys());
    set_at(&mut sealed, "owner", json!(7));
    assert_mismatch(&sealed);
}

#[test]
fn a_different_subject_is_a_mismatch() {
    let mut sealed = protected(&image("c-1", "alice", "bob", 1), &spec(), &keys());
    set_at(&mut sealed, "owner", json!("bob"));
    assert_mismatch(&sealed);
}

#[test]
fn an_empty_subject_is_a_mismatch() {
    let mut sealed = protected(&image("c-1", "alice", "bob", 1), &spec(), &keys());
    set_at(&mut sealed, "owner", json!(""));
    assert_mismatch(&sealed);
}

#[test]
fn a_mismatch_is_reported_even_when_the_key_is_gone() {
    let keys = keys();
    let mut sealed = protected(&image("c-1", "alice", "bob", 1), &spec(), &keys);
    erase("alice", &keys, None).expect("erase");
    set_at(&mut sealed, "owner", json!("bob"));
    let error = reveal(&sealed, &spec(), &keys).expect_err("mismatch under a dead key");
    assert_eq!(error.code(), "journal_pii.subject_mismatch");
}

#[test]
fn an_envelope_moved_to_another_subjects_field_fails() {
    // alice's email envelope pasted into the guardian (bob) field.
    let spec = spec();
    let keys = keys();
    let mut sealed = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);
    let email = at(&sealed, "email").clone();
    set_at(&mut sealed, "guardian_email", email);
    let error = reveal(&sealed, &spec, &keys).expect_err("moved to bob's field");
    assert_eq!(error.code(), "journal_pii.subject_mismatch");
}

#[test]
fn an_envelope_moved_to_another_record_of_another_subject_fails() {
    let spec = spec();
    let keys = keys();
    let first = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);
    let mut second = image("c-1", "bob", "alice", 1);
    set_at(&mut second, "email", at(&first, "email").clone());
    let error = reveal(&second, &spec, &keys).expect_err("moved to bob's record");
    assert_eq!(error.code(), "journal_pii.subject_mismatch");
}

#[test]
fn an_envelope_moved_to_another_field_of_the_same_subject_still_fails_on_position() {
    let spec = spec();
    let keys = keys();
    let mut sealed = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);
    let email = at(&sealed, "email").clone();
    set_at(&mut sealed, "phone", email);
    let error = reveal(&sealed, &spec, &keys).expect_err("moved field");
    assert_eq!(error.code(), "journal_pii.envelope_invalid");
}

#[test]
fn erased_subject_still_yields_the_placeholder_when_the_subject_matches() {
    let spec = spec();
    let keys = keys();
    let sealed = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);
    erase("alice", &keys, None).expect("erase");
    let revealed = reveal(&sealed, &spec, &keys).expect("reveal");
    assert_eq!(at(&revealed, "email")["$erased"], json!(1));
    assert_eq!(at(&revealed, "guardian_email"), "guardian@example.test");
}

#[test]
fn the_associated_data_digest_is_pinned() {
    // Golden: version 1 binds entity, id, path, subject and version. Changing the construction
    // breaks every envelope already in a journal, so it must be a new envelope version.
    let sealed = protected(&image("c-1", "alice", "bob", 1), &spec(), &keys());
    assert_eq!(at(&sealed, "email")["$pii"], json!(1));
    assert_eq!(
        at(&sealed, "email")["aad_hash"],
        json!("lCCkloo-UPbv1VAnDHFGrcuuh_8bJhdJmd_krInYZQo")
    );
}

#[test]
fn protect_then_reveal_is_the_identity_for_resolvable_subjects() {
    let spec = spec();
    let subjects = [
        "alice",
        "bob",
        "a",
        "a-very-long-subject-identifier-0123456789",
    ];
    let keys = keys();
    for (index, subject) in subjects.iter().enumerate() {
        let byte = u8::try_from(index).expect("small") + 20;
        keys.insert(subject, crate::KeyHandle::new([byte; 32]));
    }
    for (index, owner) in subjects.iter().enumerate() {
        for guardian in subjects {
            let version = u64::try_from(index).expect("small") + 1;
            let original = image(&format!("c-{index}"), owner, guardian, version);
            let sealed =
                protect(&original, &spec, &keys, &mut CounterNonces::new()).expect("protect");
            assert_eq!(reveal(&sealed, &spec, &keys).expect("reveal"), original);
        }
    }
}

#[test]
fn a_version_1_envelope_written_earlier_still_opens() {
    // An envelope as the first release wrote it (counter nonce 0), literal so a change to the
    // format cannot silently re-create it.
    let spec = spec();
    let mut stored = image("c-1", "alice", "bob", 1);
    set_at(
        &mut stored,
        "email",
        json!({"$pii":1,"aad_hash":"lCCkloo-UPbv1VAnDHFGrcuuh_8bJhdJmd_krInYZQo","ct":"EgGKozZIRlNgFstAMFGIMzj4wgyOHweS4_9VZew8VeWrGQ","nonce":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","subject":"alice"}),
    );
    let revealed = reveal(&stored, &spec, &keys()).expect("reveal");
    assert_eq!(at(&revealed, "email"), "ada@example.test");
}
