//! The snapshot-side transform on image records.

use serde_json::json;
use spot_journal_pii::SubjectKeys;
use spot_journal_snapshot::ImageRecord;

use super::support::{clear_values, contact_body, nonces, parts, tenant_keys, uuid};

fn record(person: &str, tag: &str, n: u64, version: u64) -> ImageRecord {
    ImageRecord {
        entity: "contact".to_owned(),
        id: uuid(n),
        version,
        entity_schema: 1,
        image: contact_body(person, tag),
    }
}

#[test]
fn a_record_is_protected_and_revealed_back_to_the_same_image() {
    let keys = tenant_keys(10, &["p1"]);
    let p = parts(&keys, &nonces());
    let original = record("p1", "ada", 1, 3);
    let sealed = p.protect_record(original.clone()).unwrap();
    let text = serde_json::to_string(&sealed).unwrap();
    for value in clear_values("ada") {
        assert!(!text.contains(&value));
    }
    assert_eq!(sealed.entity, original.entity);
    assert_eq!(sealed.id, original.id);
    assert_eq!(sealed.version, original.version);
    assert_eq!(sealed.entity_schema, original.entity_schema);
    assert_eq!(sealed.image["city"], original.image["city"]);
    assert_eq!(p.reveal_record(sealed).unwrap(), original);
}

#[test]
fn records_without_personal_fields_are_returned_as_they_are() {
    let keys = tenant_keys(10, &["p1"]);
    let p = parts(&keys, &nonces());
    let ledger = ImageRecord {
        entity: "ledger".to_owned(),
        id: uuid(9),
        version: 1,
        entity_schema: 1,
        image: json!({"amount": "10.00"}),
    };
    assert_eq!(p.protect_record(ledger.clone()).unwrap(), ledger);
    assert_eq!(p.reveal_record(ledger.clone()).unwrap(), ledger);
}

#[test]
fn a_batch_of_records_is_protected_whole_or_not_at_all() {
    let keys = tenant_keys(10, &["p1"]);
    let p = parts(&keys, &nonces());
    let out = p
        .protect_records([record("p1", "ada", 1, 1), record("p1", "bob", 2, 1)])
        .unwrap();
    assert_eq!(out.len(), 2);
    let error = p
        .protect_records([record("p1", "ada", 1, 1), record("p9", "eve", 2, 1)])
        .unwrap_err();
    assert_eq!(error.code(), "journal_protect.protect_failed");
}

#[test]
fn a_destroyed_subject_reveals_as_the_placeholder() {
    let keys = tenant_keys(10, &["p1"]);
    let p = parts(&keys, &nonces());
    let sealed = p.protect_record(record("p1", "ada", 1, 1)).unwrap();
    keys.destroy("p1").unwrap();
    let revealed = p.reveal_record(sealed).unwrap();
    assert_eq!(revealed.image["email"]["$erased"], 1);
    assert_eq!(revealed.image["tax_id"], "TAX-ada");
}

#[test]
fn a_record_whose_image_is_not_an_object_is_refused() {
    let keys = tenant_keys(10, &["p1"]);
    let p = parts(&keys, &nonces());
    let mut bad = record("p1", "ada", 1, 1);
    bad.image = json!("text");
    assert_eq!(
        p.protect_record(bad).unwrap_err().code(),
        "journal_protect.entry_invalid"
    );
}
