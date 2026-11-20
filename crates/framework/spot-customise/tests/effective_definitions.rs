#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
//! WP-EXT-09.T3: `effective_definitions(base, &Records)` is the fold the service runs, as a
//! function: its output equals what a node serves after the service applied the same changes,
//! and a record that does not build is an error, never a panic.

mod common;
use common::*;
use spot_customise::{
    CustomiseError, DocTypeRecord, FieldRecord, RecordState, Records, codes, effective_definitions,
    effective_definitions_with,
};
use spot_meta::{BuildOptions, FieldSpec, FieldType};
use spot_meta_live::MetaHash;

fn text_field(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

fn field_names(definitions: &[spot_meta::Definition], doctype: &str) -> Vec<String> {
    definitions
        .iter()
        .find(|d| d.name == doctype)
        .unwrap_or_else(|| panic!("`{doctype}` is in the effective definitions"))
        .fields
        .iter()
        .map(|f| f.name.clone())
        .collect()
}

#[tokio::test]
async fn it_equals_what_the_service_serves_with_a_custom_field_a_setter_and_a_removed_field() {
    let rig = Rig::new().await;
    let who = admin();
    // A custom DocType, a custom field that stays, one that is removed again, and a property
    // setter: the three record kinds and the three states that matter.
    rig.service
        .create_doctypes(&who, &[TICKET, TICKET_NOTE])
        .await
        .unwrap();
    rig.service
        .add_custom_field(&who, "invoice", &text_field("custom_note"), None)
        .await
        .unwrap();
    rig.service
        .add_custom_field(&who, "invoice", &text_field("custom_gone"), None)
        .await
        .unwrap();
    rig.service
        .set_property(&who, "invoice", None, "label", "Bill")
        .await
        .unwrap();
    rig.service
        .remove_custom_field(&who, "invoice", "custom_gone")
        .await
        .unwrap();

    let records = rig.records.snapshot();
    let effective = effective_definitions(&base_definitions(), &records).unwrap();

    // The same content a node of the site serves, definition for definition.
    let served = rig.live.current();
    let by_name = |definitions: &[spot_meta::Definition]| {
        let mut sorted = definitions.to_vec();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        sorted
    };
    assert_eq!(
        by_name(&effective),
        by_name(served.meta().definitions()),
        "the function and the service build the same effective definitions"
    );
    assert_eq!(
        &MetaHash::of_definitions(&effective).unwrap(),
        served.hash(),
        "and the same content hash two nodes compare"
    );

    // And the pieces are what was changed.
    let invoice = field_names(&effective, "invoice");
    assert!(invoice.iter().any(|n| n == "custom_note"), "{invoice:?}");
    assert!(
        !invoice.iter().any(|n| n == "custom_gone"),
        "a removed field is not effective: {invoice:?}"
    );
    assert!(
        effective.iter().any(|d| d.name == "ticket"),
        "the custom DocType is effective"
    );
    let with_options =
        effective_definitions_with(&base_definitions(), &records, &BuildOptions::new()).unwrap();
    assert_eq!(with_options, effective, "default options are the same fold");
}

#[tokio::test]
async fn pending_and_failed_records_are_not_effective_and_the_input_is_not_changed() {
    let rig = Rig::new().await;
    rig.service
        .add_custom_field(&admin(), "invoice", &text_field("custom_note"), None)
        .await
        .unwrap();
    let mut records = rig.records.snapshot();
    let before = records.clone();
    let base = base_definitions();
    let applied = effective_definitions(&base, &records).unwrap();
    assert!(field_names(&applied, "invoice").contains(&"custom_note".to_owned()));

    for state in [RecordState::Pending, RecordState::Failed] {
        for field in &mut records.fields {
            field.state = state;
        }
        let effective = effective_definitions(&base, &records).unwrap();
        assert!(
            !field_names(&effective, "invoice").contains(&"custom_note".to_owned()),
            "a {state:?} field is not served"
        );
    }
    // The base is untouched: the base definitions alone are what an empty record set gives.
    let none = effective_definitions(&base, &Records::default()).unwrap();
    for definition in &base {
        assert!(none.iter().any(|d| d.name == definition.name));
    }
    assert_ne!(records, before, "only the test's copy was edited");
}

#[test]
fn an_invalid_record_is_an_error_with_every_issue_and_never_a_panic() {
    let base = base_definitions();
    let mut records = Records::default();
    records.fields.push(FieldRecord {
        doctype: "invoice".to_owned(),
        name: "custom_bad".to_owned(),
        spec: "this is [not toml".to_owned(),
        insert_after: None,
        state: RecordState::Applied,
        reason: None,
        intent: 1,
        actor: "test".to_owned(),
    });
    records.doctypes.push(DocTypeRecord {
        name: "ghost".to_owned(),
        module: "custom".to_owned(),
        definition: "name = ".to_owned(),
        state: RecordState::Applied,
        reason: None,
        intent: 2,
        actor: "test".to_owned(),
        prior: None,
    });
    let error = effective_definitions(&base, &records).unwrap_err();
    let CustomiseError::Invalid { issues } = &error else {
        panic!("expected the invalid-change error, got {error:?}");
    };
    assert!(
        issues.iter().all(|i| i.code == codes::BAD_DEFINITION),
        "{issues:?}"
    );
    let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
    assert!(paths.contains(&"invoice.custom_bad"), "{paths:?}");
    assert!(paths.contains(&"ghost"), "{paths:?}");
    assert_eq!(error.code(), "customise.invalid");
}

#[test]
fn a_field_that_collides_with_an_existing_one_is_an_error() {
    let base = base_definitions();
    let existing = base
        .iter()
        .find(|d| d.name == "invoice")
        .unwrap()
        .fields
        .first()
        .unwrap()
        .name
        .clone();
    let mut records = Records::default();
    records.fields.push(FieldRecord {
        doctype: "invoice".to_owned(),
        name: existing.clone(),
        spec: toml::to_string(&text_field(&existing)).unwrap(),
        insert_after: None,
        state: RecordState::Applied,
        reason: None,
        intent: 1,
        actor: "test".to_owned(),
    });
    let error = effective_definitions(&base, &records).unwrap_err();
    let CustomiseError::Invalid { issues } = &error else {
        panic!("expected the invalid-change error, got {error:?}");
    };
    assert!(
        issues.iter().any(|i| i.code == codes::FIELD_COLLISION),
        "{issues:?}"
    );
}
