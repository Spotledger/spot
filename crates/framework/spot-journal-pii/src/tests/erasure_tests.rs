//! Erasure: destroying a subject's key, what that makes unreadable, and what it must not touch.

use serde_json::{Value, json};

use super::{at, image, keys, protected, set_at, spec};
use crate::{
    CounterNonces, FieldSpec, KeyHandle, MemoryKeys, PersonalFields, SubjectIndex, erase, protect,
    reveal,
};

/// The placeholder a destroyed subject's field becomes.
fn placeholder(purpose: &str) -> Value {
    json!({"$erased": 1, "purpose": purpose, "erased_at": null})
}

/// A `Contact` description with one legal-retention field.
fn spec_with_retained() -> PersonalFields {
    PersonalFields::new("Contact")
        .expect("entity")
        .with_field(FieldSpec::new("email", "owner", "notifications", "contact_5y").expect("email"))
        .with_field(
            FieldSpec::new("tax_id", "owner", "accounting", "tax_7y")
                .expect("tax_id")
                .with_erasable(false),
        )
}

#[test]
fn erasing_one_subject_leaves_every_other_byte_unchanged() {
    let spec = spec();
    let keys = keys();
    let image = image("c-1", "alice", "bob", 1);
    let protected = protected(&image, &spec, &keys);

    let receipt = erase("alice", &keys, None).expect("erase");
    assert_eq!(receipt.subject(), "alice");
    assert!(receipt.detail().is_none(), "no index, no detail");
    assert_eq!(keys.subjects(), vec!["bob".to_owned()]);

    let revealed = reveal(&protected, &spec, &keys).expect("reveal still succeeds");

    // Alice's fields are placeholders; the guardian's field is still hers to read.
    assert_eq!(at(&revealed, "email"), &placeholder("notifications"));
    assert_eq!(at(&revealed, "phone"), &placeholder("support"));
    assert_eq!(at(&revealed, "items.0.note"), &placeholder("order_notes"));
    assert_eq!(at(&revealed, "items.1.note"), &placeholder("order_notes"));
    assert_eq!(
        at(&revealed, "guardian_email"),
        &json!("guardian@example.test")
    );

    // Everything else is the image itself, byte for byte: one whole-image comparison covers
    // every untouched key, link, child row, row count and the version.
    let mut expected = image.clone();
    set_at(&mut expected, "email", placeholder("notifications"));
    set_at(&mut expected, "phone", placeholder("support"));
    set_at(&mut expected, "items.0.note", placeholder("order_notes"));
    set_at(&mut expected, "items.1.note", placeholder("order_notes"));
    assert_eq!(revealed, expected);
    assert_eq!(at(&revealed, "items").as_array().expect("rows").len(), 2);
    assert_eq!(at(&revealed, "items.0.qty"), &json!(2));
    assert_eq!(revealed["totals"], image["totals"]);
}

#[test]
fn a_key_destroyed_in_one_image_is_destroyed_in_every_copy() {
    let spec = spec();
    let keys = keys();

    // Three snapshots of the same subject's record, each with its own nonce sequence.
    let snapshots: Vec<Value> = [1u64, 2, 3]
        .into_iter()
        .map(|version| {
            let image = image("c-1", "alice", "bob", version);
            let mut nonces = CounterNonces::new();
            let protected = protect(&image, &spec, &keys, &mut nonces).expect("protect");
            assert_eq!(reveal(&protected, &spec, &keys).expect("reveal"), image);
            protected
        })
        .collect();

    erase("alice", &keys, None).expect("erase");

    for protected in &snapshots {
        let revealed = reveal(protected, &spec, &keys).expect("reveal after erasure");
        assert_eq!(at(&revealed, "email"), &placeholder("notifications"));
        assert_eq!(at(&revealed, "phone"), &placeholder("support"));
        assert_eq!(
            at(&revealed, "guardian_email"),
            &json!("guardian@example.test")
        );
    }
}

#[test]
fn a_legal_retention_field_survives_erasure() {
    let spec = spec_with_retained();
    let keys = keys();
    let mut image = image("c-1", "alice", "bob", 1);
    set_at(&mut image, "tax_id", json!("GB-123-456"));

    let protected = protected(&image, &spec, &keys);
    assert_eq!(at(&protected, "tax_id")["subject"], json!("alice"));
    assert_eq!(reveal(&protected, &spec, &keys).expect("reveal"), image);

    erase("alice", &keys, None).expect("erase");

    let revealed = reveal(&protected, &spec, &keys).expect("reveal after erasure");
    assert_eq!(at(&revealed, "tax_id"), &json!("GB-123-456"));
    assert_eq!(at(&revealed, "email"), &placeholder("notifications"));
}

#[test]
fn a_legal_retention_field_is_not_under_the_subject_key() {
    let spec = spec_with_retained();
    let mut image = image("c-1", "alice", "bob", 1);
    set_at(&mut image, "tax_id", json!("GB-123-456"));

    let tenant = MemoryKeys::new(KeyHandle::new([9u8; 32]));
    tenant.insert("alice", KeyHandle::new([7u8; 32]));
    let protected = protected(&image, &spec, &tenant);

    // Same subject key, another tenant journal key: the erasable field opens, the retained one
    // does not, so the retained field is sealed under the tenant key and not under hers.
    let other_tenant = MemoryKeys::new(KeyHandle::new([5u8; 32]));
    other_tenant.insert("alice", KeyHandle::new([7u8; 32]));
    let error = reveal(&protected, &spec, &other_tenant).expect_err("another tenant key");
    assert_eq!(error.code(), "journal_pii.envelope_invalid");

    // And a tenant with no journal key at all cannot protect the field in the first place.
    let no_retained = MemoryKeys::without_retained();
    no_retained.insert("alice", KeyHandle::new([7u8; 32]));
    let error = protect(&image, &spec, &no_retained, &mut CounterNonces::new())
        .expect_err("no retained key");
    assert_eq!(error.code(), "journal_pii.key_store");
}

#[test]
fn the_receipt_lists_what_an_index_recorded() {
    let keys = keys();
    let mut index = SubjectIndex::new();
    index.record("Contact", "email", true);
    index.record("Contact", "email", true);
    index.record("Contact", "phone", true);
    index.record("Contact", "items.*.note", true);
    // A legal-retention field stays readable, so the receipt must not claim it was erased.
    index.record("Contact", "tax_id", false);
    assert_eq!(index.entries().len(), 4);

    let receipt = erase("alice", &keys, Some(&index)).expect("erase");
    let detail = receipt.detail().expect("an index gives detail");
    assert_eq!(detail.total(), 4);
    assert_eq!(detail.entities().len(), 1);

    let entity = &detail.entities()[0];
    assert_eq!(entity.entity(), "Contact");
    assert_eq!(entity.count(), 4);
    let fields: Vec<(&str, u64)> = entity
        .fields()
        .iter()
        .map(|field| (field.path(), field.count()))
        .collect();
    assert_eq!(
        fields,
        vec![("email", 2), ("items.*.note", 1), ("phone", 1)]
    );
}

#[test]
fn an_index_with_only_retained_fields_produces_an_empty_detail() {
    let keys = keys();
    let mut index = SubjectIndex::new();
    index.record("Contact", "tax_id", false);

    let receipt = erase("alice", &keys, Some(&index)).expect("erase");
    let detail = receipt.detail().expect("an index gives detail");
    assert_eq!(detail.total(), 0);
    assert!(detail.entities().is_empty());
}

#[test]
fn erasing_twice_is_idempotent() {
    let keys = keys();
    let first = erase("alice", &keys, None).expect("first");
    let second = erase("alice", &keys, None).expect("second");
    assert_eq!(first, second);
}

#[test]
fn revealing_an_erased_image_twice_changes_nothing() {
    let spec = spec();
    let keys = keys();
    let protected = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);
    erase("alice", &keys, None).expect("erase");

    let once = reveal(&protected, &spec, &keys).expect("first reveal");
    let twice = reveal(&once, &spec, &keys).expect("second reveal");
    assert_eq!(once, twice);
}

#[test]
fn an_empty_subject_is_refused() {
    let keys = keys();
    let error = erase("", &keys, None).expect_err("no subject");
    assert_eq!(error.code(), "journal_pii.subject_invalid");
}

#[test]
fn another_subjects_key_never_opens_this_subjects_field() {
    let spec = spec();
    let keys = keys();
    let protected = protected(&image("c-1", "alice", "bob", 1), &spec, &keys);

    // Bob's key is not Alice's key, and his field is not hers: revealing under a store where
    // only bob's key survives leaves alice's fields erased and bob's readable.
    erase("alice", &keys, None).expect("erase alice");
    let revealed = reveal(&protected, &spec, &keys).expect("reveal");
    assert_eq!(
        at(&revealed, "guardian_email"),
        &json!("guardian@example.test")
    );
    assert_eq!(at(&revealed, "email"), &placeholder("notifications"));
}
