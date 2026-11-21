//! The rebuild-side wrapper: reveal before the inner sink, fail closed on an outage.

use std::sync::Arc;

use serde_json::{Value, json};
use spot_image_upcast::Image;
use spot_journal_pii::SubjectKeys;
use spot_journal_rebuild::{
    AppliedAudit, AppliedDelete, AppliedImage, ApplySink, Call, RecordingSink,
};
use spot_journal_snapshot::ImageRecord;

use super::block_on;
use super::support::{
    FlakyKeys, contact_body, fields, nonces, parts, position, shared, tenant_keys, uuid,
};
use crate::RevealSink;

/// A protected `contact` image for record `n`, as the executor would hand it to a sink.
fn applied(
    person: &str,
    tag: &str,
    n: u64,
    version: u64,
    keys: &Arc<spot_journal_pii::MemoryKeys>,
) -> AppliedImage {
    let record = parts(keys, &nonces())
        .protect_record(ImageRecord {
            entity: "contact".to_owned(),
            id: uuid(n),
            version,
            entity_schema: 1,
            image: contact_body(person, tag),
        })
        .unwrap();
    to_applied(&record, n)
}

fn to_applied(record: &ImageRecord, n: u64) -> AppliedImage {
    let Value::Object(body) = record.image.clone() else {
        panic!("not an object");
    };
    AppliedImage {
        entity: record.entity.clone(),
        id: uuid(n),
        version: record.version,
        position: position(n),
        image: Image::new("contact", 1, body),
    }
}

fn body_of(sink: &RecordingSink) -> Value {
    match sink.calls().last() {
        Some(Call::Image(image)) => Value::Object(image.image.body().clone()),
        other => panic!("no image applied: {other:?}"),
    }
}

#[test]
fn the_inner_sink_sees_the_revealed_image_with_its_version_and_position() {
    let keys = tenant_keys(10, &["p1"]);
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), shared(&keys));
    block_on(sink.apply_image(applied("p1", "ada", 1, 4, &keys))).unwrap();
    assert_eq!(body_of(sink.inner()), contact_body("p1", "ada"));
    match sink.inner().calls().last() {
        Some(Call::Image(image)) => {
            assert_eq!(image.version, 4);
            assert_eq!(image.position, position(1));
            assert_eq!(image.image.entity_schema(), 1);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_destroyed_key_yields_the_placeholder_and_nothing_else_changes() {
    let keys = tenant_keys(10, &["p1", "p2"]);
    let stored = applied("p1", "ada", 1, 2, &keys);
    keys.destroy("p1").unwrap();
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), shared(&keys));
    block_on(sink.apply_image(stored)).unwrap();
    let body = body_of(sink.inner());
    let placeholder = json!({"$erased": 1, "purpose": "notifications", "erased_at": null});
    assert_eq!(body["email"], placeholder);
    assert_eq!(body["phones"][0]["number"], placeholder);
    assert_eq!(body["phones"][1]["number"], placeholder);
    let clear = contact_body("p1", "ada");
    for key in ["person", "name", "city", "api_token", "tax_id"] {
        assert_eq!(body[key], clear[key], "{key}");
    }
    assert_eq!(body["phones"][0]["label"], "home");
}

#[test]
fn a_secret_field_stays_in_its_stored_form_and_is_never_revealed() {
    let keys = tenant_keys(10, &["p1"]);
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), shared(&keys));
    let stored = applied("p1", "ada", 1, 1, &keys);
    assert_eq!(stored.image.body()["api_token"], "sealed:v1:ada");
    block_on(sink.apply_image(stored)).unwrap();
    assert_eq!(body_of(sink.inner())["api_token"], "sealed:v1:ada");
}

#[test]
fn a_key_store_outage_stops_the_rebuild_and_does_not_placeholder() {
    let keys = tenant_keys(10, &["p1"]);
    let stored = applied("p1", "ada", 1, 1, &keys);
    let flaky = FlakyKeys::new(Arc::clone(&keys));
    flaky.set_down(true);
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), flaky);
    let error = block_on(sink.apply_image(stored)).unwrap_err();
    assert!(
        error.reason.contains("journal_protect.reveal_failed"),
        "{}",
        error.reason
    );
    assert!(
        error.reason.contains("journal_pii.key_store"),
        "{}",
        error.reason
    );
    assert!(
        sink.inner().calls().is_empty(),
        "nothing reached the inner sink"
    );
}

#[test]
fn an_envelope_moved_to_another_record_does_not_open() {
    let keys = tenant_keys(10, &["p1"]);
    let mut moved = applied("p1", "ada", 1, 1, &keys);
    moved.id = uuid(2);
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), shared(&keys));
    let error = block_on(sink.apply_image(moved)).unwrap_err();
    assert!(
        error.reason.contains("journal_pii.envelope_invalid"),
        "{}",
        error.reason
    );
    assert!(sink.inner().calls().is_empty());
}

#[test]
fn two_tenants_cannot_reveal_each_others_envelopes() {
    let acme = tenant_keys(10, &["p1"]);
    let rival = tenant_keys(50, &["p1"]);
    let stored = applied("p1", "ada", 1, 1, &acme);
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), shared(&rival));
    let error = block_on(sink.apply_image(stored)).unwrap_err();
    assert!(
        error.reason.contains("journal_pii.envelope_invalid"),
        "{}",
        error.reason
    );
    assert!(sink.inner().calls().is_empty());
}

#[test]
fn another_tenant_without_the_subject_key_cannot_read_the_record_either() {
    let acme = tenant_keys(10, &["p1"]);
    let rival = tenant_keys(50, &[]);
    let stored = applied("p1", "ada", 1, 1, &acme);
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), shared(&rival));
    // The email would be a placeholder, but the retained tax id is sealed under acme's journal key,
    // which rival does not hold, so the whole image is refused.
    assert!(block_on(sink.apply_image(stored)).is_err());
    assert!(sink.inner().calls().is_empty());
}

#[test]
fn an_erasable_false_field_survives_an_erasure_and_an_erased_subject_is_idempotent() {
    let keys = tenant_keys(10, &["p1"]);
    let stored = applied("p1", "ada", 1, 1, &keys);
    keys.destroy("p1").unwrap();
    keys.destroy("p1").unwrap();
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), shared(&keys));
    block_on(sink.apply_image(stored)).unwrap();
    assert_eq!(body_of(sink.inner())["tax_id"], "TAX-ada");
}

#[test]
fn an_image_of_an_entity_without_personal_fields_is_handed_over_as_it_is() {
    let keys = tenant_keys(10, &["p1"]);
    let image = AppliedImage {
        entity: "ledger".to_owned(),
        id: uuid(9),
        version: 1,
        position: position(9),
        image: Image::new(
            "ledger",
            1,
            json!({"amount": "10.00"}).as_object().unwrap().clone(),
        ),
    };
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), shared(&keys));
    block_on(sink.apply_image(image.clone())).unwrap();
    assert_eq!(sink.inner().calls(), &[Call::Image(image)]);
}

#[test]
fn tombstones_audit_and_the_lifecycle_calls_reach_the_inner_sink_untouched() {
    let keys = tenant_keys(10, &["p1"]);
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), shared(&keys));
    let delete = AppliedDelete {
        entity: "contact".to_owned(),
        id: uuid(1),
        version: 3,
        position: position(5),
    };
    let audit = AppliedAudit {
        event_id: uuid(77),
        topic: "sys_role_changed".to_owned(),
        position: position(6),
        entity: "role".to_owned(),
        id: uuid(8),
        data: json!({"role": "r1"}),
    };
    block_on(sink.apply_delete(delete.clone())).unwrap();
    block_on(sink.apply_audit(audit.clone())).unwrap();
    block_on(sink.commit(position(6))).unwrap();
    block_on(sink.abort("journal_rebuild.cancelled")).unwrap();
    let calls = sink.into_inner().calls().to_vec();
    assert_eq!(
        calls,
        vec![
            Call::Delete(delete),
            Call::Audit(audit),
            Call::Commit(position(6)),
            Call::Abort("journal_rebuild.cancelled".to_owned()),
        ]
    );
}
