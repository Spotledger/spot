//! The relay-side transform on single lines and batches.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use spot_journal_pii::NonceSource;
use spot_journal_relay::Batch;

use super::support::{
    audit_line, clear_values, contact_body, domain_line, image_of, nonces, position, shared,
    state_line, tenant_keys, transform,
};
use crate::{
    PersonalFieldsSource, ProtectError, ProtectTransform, SharedFields, SourceError, StaticFields,
};

fn contact_line(person: &str, tag: &str, seq: u64) -> String {
    state_line("contact", 1, "insert", 1, &contact_body(person, tag), seq)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn a_state_line_leaves_no_personal_value_in_clear() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    let out = t
        .transform_line(contact_line("p1", "ada", 1).as_bytes())
        .unwrap();
    let out = text(&out);
    for value in clear_values("ada") {
        assert!(!out.contains(&value), "a personal value is still in clear");
    }
    assert!(!out.contains('\n'));
    let image = image_of(out.as_bytes());
    assert_eq!(image["email"]["$pii"], 1);
    assert_eq!(image["phones"][0]["number"]["$pii"], 1);
    assert_eq!(image["phones"][1]["number"]["$pii"], 1);
    assert_eq!(image["tax_id"]["$pii"], 1);
}

#[test]
fn everything_that_is_not_a_personal_field_is_unchanged() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    let before = contact_body("p1", "ada");
    let out = t
        .transform_line(contact_line("p1", "ada", 1).as_bytes())
        .unwrap();
    let after = image_of(&out);
    for key in ["person", "name", "city"] {
        assert_eq!(after[key], before[key], "{key}");
    }
    assert_eq!(after["phones"][0]["label"], "home");
    assert_eq!(after["phones"][1]["label"], "work");
    assert_eq!(after["phones"].as_array().unwrap().len(), 2);
    // The image never grew the identity members the transform binds for the call.
    assert!(after.get("id").is_none());
    assert!(after.get("version").is_none());
    let line: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(line["subject_version"], 1);
    assert_eq!(line["topic"], "doc_state");
    assert_eq!(line["data"]["op"], "insert");
}

#[test]
fn a_secret_field_is_left_exactly_as_it_is() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    let out = t
        .transform_line(contact_line("p1", "ada", 1).as_bytes())
        .unwrap();
    assert_eq!(image_of(&out)["api_token"], "sealed:v1:ada");
}

#[test]
fn the_same_value_protected_twice_gives_different_ciphertexts() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    let line = contact_line("p1", "ada", 1);
    let a = image_of(&t.transform_line(line.as_bytes()).unwrap());
    let b = image_of(&t.transform_line(line.as_bytes()).unwrap());
    assert_ne!(a["email"], b["email"]);
}

#[test]
fn a_domain_event_with_a_personal_looking_value_is_left_as_the_same_bytes() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    let line = domain_line(1, &json!({"email": "ada@mail.example", "person": "p1"}), 4);
    assert_eq!(t.transform_line(line.as_bytes()).unwrap(), line.as_bytes());
}

#[test]
fn an_audit_event_is_left_as_the_same_bytes() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    let line = audit_line(6);
    assert_eq!(t.transform_line(line.as_bytes()).unwrap(), line.as_bytes());
}

#[test]
fn an_entity_without_personal_fields_is_left_as_the_same_bytes() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    let line = state_line("ledger", 9, "insert", 1, &json!({"amount": "10.00"}), 7);
    assert_eq!(t.transform_line(line.as_bytes()).unwrap(), line.as_bytes());
}

#[test]
fn a_subject_without_a_key_is_refused_and_the_error_names_no_value() {
    let keys = tenant_keys(10, &[]);
    let t = transform(&keys, &nonces());
    let error = t
        .transform_line(contact_line("p1", "ada", 1).as_bytes())
        .unwrap_err();
    assert_eq!(error.code(), "journal_protect.protect_failed");
    assert_eq!(error.pii_code(), Some("journal_pii.key_missing"));
    let shown = format!("{error} {error:?}");
    for value in clear_values("ada") {
        assert!(!shown.contains(&value));
    }
}

#[test]
fn a_key_store_outage_on_the_way_in_is_a_refusal() {
    let keys = tenant_keys(10, &["p1"]);
    let flaky = super::support::FlakyKeys::new(Arc::clone(&keys));
    flaky.set_down(true);
    let t = ProtectTransform::new(super::support::fields(), flaky, nonces());
    let error = t
        .transform_line(contact_line("p1", "ada", 1).as_bytes())
        .unwrap_err();
    assert_eq!(error.pii_code(), Some("journal_pii.key_store"));
}

struct NoNonces;

impl NonceSource for NoNonces {
    fn next_nonce(&mut self) -> Option<[u8; spot_journal_pii::NONCE_LEN]> {
        None
    }
}

#[test]
fn a_nonce_source_that_gives_nothing_is_a_refusal_not_a_reused_nonce() {
    let keys = tenant_keys(10, &["p1"]);
    let t = ProtectTransform::new(
        super::support::fields(),
        shared(&keys),
        Arc::new(Mutex::new(NoNonces)),
    );
    let error = t
        .transform_line(contact_line("p1", "ada", 1).as_bytes())
        .unwrap_err();
    assert_eq!(error.pii_code(), Some("journal_pii.nonce_unavailable"));
}

struct BrokenSource;

impl PersonalFieldsSource for BrokenSource {
    fn fields_for(
        &self,
        _entity: &str,
    ) -> Result<Option<Arc<spot_journal_pii::PersonalFields>>, SourceError> {
        Err(SourceError::new("the metadata source is down"))
    }
}

#[test]
fn a_description_source_that_fails_is_a_refusal_not_no_personal_fields() {
    let keys = tenant_keys(10, &["p1"]);
    let source: SharedFields = Arc::new(BrokenSource);
    let t = ProtectTransform::new(source, shared(&keys), nonces());
    let error = t
        .transform_line(contact_line("p1", "ada", 1).as_bytes())
        .unwrap_err();
    assert_eq!(error.code(), "journal_protect.spec_unavailable");
}

#[test]
fn a_line_that_is_not_an_envelope_is_refused() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    for bad in [&b"not json"[..], b"{\"topic\":\"doc_state\"}", b"\xff\xfe"] {
        let error = t.transform_line(bad).unwrap_err();
        assert_eq!(error.code(), "journal_protect.entry_invalid");
    }
}

#[test]
fn a_state_entry_without_a_version_or_with_odd_data_is_refused() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    let body = contact_body("p1", "ada");
    let no_version = super::support::envelope(
        "doc_state",
        "contact",
        1,
        None,
        json!({"op": "insert", "entity_schema": 1, "image": body}),
        1,
    );
    assert!(matches!(
        t.transform_line(no_version.as_bytes()),
        Err(ProtectError::EntryInvalid(_))
    ));
    let odd = super::support::envelope("doc_state", "contact", 1, Some(1), json!([1, 2]), 1);
    assert!(matches!(
        t.transform_line(odd.as_bytes()),
        Err(ProtectError::EntryInvalid(_))
    ));
    let not_object = super::support::envelope(
        "doc_state",
        "contact",
        1,
        Some(1),
        json!({"op": "insert", "entity_schema": 1, "image": "text"}),
        1,
    );
    assert!(matches!(
        t.transform_line(not_object.as_bytes()),
        Err(ProtectError::EntryInvalid(_))
    ));
}

#[test]
fn an_image_that_names_another_record_than_its_entry_is_refused() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    let mut body = contact_body("p1", "ada");
    body["id"] = json!(super::support::uuid(2));
    let line = state_line("contact", 1, "insert", 1, &body, 1);
    let error = t.transform_line(line.as_bytes()).unwrap_err();
    assert_eq!(error.code(), "journal_protect.identity_mismatch");
    let mut body = contact_body("p1", "ada");
    body["version"] = json!(7);
    let line = state_line("contact", 1, "insert", 1, &body, 1);
    assert_eq!(
        t.transform_line(line.as_bytes()).unwrap_err().code(),
        "journal_protect.identity_mismatch"
    );
}

#[test]
fn a_tombstone_without_an_image_or_personal_fields_passes_through() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    let gone = super::support::envelope(
        "doc_state",
        "contact",
        1,
        Some(3),
        json!({"op": "delete", "entity_schema": 1, "image": {"gone": true}}),
        5,
    );
    assert_eq!(t.transform_line(gone.as_bytes()).unwrap(), gone.as_bytes());
    let bare = super::support::envelope(
        "doc_state",
        "contact",
        1,
        Some(3),
        json!({"op": "delete", "entity_schema": 1}),
        5,
    );
    assert_eq!(t.transform_line(bare.as_bytes()).unwrap(), bare.as_bytes());
}

#[test]
fn a_batch_is_protected_whole_or_not_at_all() {
    let keys = tenant_keys(10, &["p1"]);
    let t = transform(&keys, &nonces());
    let good = contact_line("p1", "ada", 1);
    let bad = contact_line("p9", "eve", 2);
    let batch = Batch {
        batch_id: 1,
        entries: vec![
            (position(1), good.clone().into_bytes()),
            (position(2), bad.into_bytes()),
        ],
    };
    assert!(t.transform_batch(batch).is_err());

    let batch = Batch {
        batch_id: 1,
        entries: vec![
            (position(1), good.into_bytes()),
            (position(2), audit_line(2).into_bytes()),
        ],
    };
    let out = t.transform_batch(batch).unwrap();
    assert_eq!(out.batch_id, 1);
    assert_eq!(out.entries.len(), 2);
    assert_eq!(out.entries[0].0, position(1));
    assert_eq!(out.entries[1].1, audit_line(2).into_bytes());
}

#[test]
fn a_static_map_of_descriptions_answers_per_entity() {
    let source = StaticFields::new().with(super::support::contact_fields());
    assert!(source.fields_for("contact").unwrap().is_some());
    assert!(source.fields_for("ledger").unwrap().is_none());
}
