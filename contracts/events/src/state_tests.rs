//! Tests of the `doc_state` payload schema (WP-JRN-01.T1).

use std::collections::BTreeSet;

use serde_json::{Value as Json, json};

use crate::envelope::EventEnvelope;
use crate::registry::{Class, Retention, lookup};
use crate::state::{self, DOC_STATE_TOPIC};

/// One example by its kind (`insert`, `update`, ...): the file is `doc_state.<kind>.json`.
fn example(kind: &str) -> Json {
    let name = format!("doc_state.{kind}.json");
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/doc")
        .join(&name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn with(data: &Json, field: &str, value: Option<Json>) -> Json {
    let mut d = data.clone();
    if let Some(map) = d.as_object_mut() {
        match value {
            Some(v) => map.insert(field.to_owned(), v),
            None => map.remove(field),
        };
    }
    d
}

fn refused(data: &Json) {
    assert!(state::validate(data).is_err(), "accepted {data}");
}

#[test]
fn the_schema_is_well_formed_and_the_entry_is_unchanged() {
    let s = state::schema();
    assert_eq!(s.topic.as_str(), DOC_STATE_TOPIC);
    assert_eq!((s.major, s.revision), (1, 0));
    assert_eq!(s.check_definition(), Ok(()));
    assert_eq!(crate::check(&s, &s), Ok(()));
    let e = lookup(DOC_STATE_TOPIC);
    assert!(e.is_some_and(|e| e.class == Class::State
        && e.retention == Retention::Outbox
        && e.since == "0.7.1"
        && e.owner == "spot-journal-rebuild"
        && !e.retired));
}

#[test]
fn every_doc_example_validates_and_all_three_ops_are_covered() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/doc");
    let mut ops = BTreeSet::new();
    let mut n = 0;
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{e}")) {
        let name = entry
            .unwrap_or_else(|e| panic!("{e}"))
            .file_name()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with("doc_state."), "{name}");
        let kind = name
            .strip_prefix("doc_state.")
            .and_then(|rest| rest.strip_suffix(".json"))
            .unwrap_or_else(|| panic!("{name}"));
        let data = example(kind);
        assert_eq!(state::validate(&data), Ok(()), "{name}");
        ops.insert(data["op"].as_str().unwrap_or_default().to_owned());
        n += 1;
    }
    assert_eq!(n, 5);
    let want: BTreeSet<String> = state::STATE_OPS.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(ops, want);
}

#[test]
fn invalid_shapes_are_refused() {
    let ok = example("insert");
    refused(&with(&ok, "extra", Some(json!(1))));
    for op in [json!("upsert"), json!("INSERT"), json!(1), json!(null)] {
        refused(&with(&ok, "op", Some(op)));
    }
    for field in ["op", "entity_schema", "image"] {
        refused(&with(&ok, field, None));
    }
    for v in [json!(0), json!(-1), json!(1.5), json!("2"), json!(null)] {
        refused(&with(&ok, "entity_schema", Some(v)));
    }
    for v in [json!([]), json!("x"), json!(7), json!(null), json!([{}])] {
        refused(&with(&ok, "image", Some(v)));
    }
    refused(&json!([]));
    // The smallest valid image is the empty object; entity_schema 1 is the lowest.
    assert_eq!(
        state::validate(&json!({"op": "delete", "entity_schema": 1, "image": {}})),
        Ok(())
    );
}

#[test]
fn the_image_is_opaque_apart_from_the_two_personal_field_forms() {
    let ok = example("insert");
    // Long text and a field called like a secret belong to the DocType, not to the contract.
    let image = json!({"body": "x".repeat(10_000), "password_hint": "none", "n": {"a": [1, 2]}});
    assert_eq!(state::validate(&with(&ok, "image", Some(image))), Ok(()));
    // Either form is valid anywhere: a field, a child row, a list element.
    let sealed = example("protected_field")["image"]["phone"].clone();
    let erased = example("protected_field")["image"]["email"].clone();
    let image =
        json!({"a": sealed, "rows": [{"b": erased.clone()}, sealed.clone()], "c": [[erased]]});
    assert_eq!(state::validate(&with(&ok, "image", Some(image))), Ok(()));
    // A half-written form is refused.
    let bad_forms = [
        json!({"$pii": 1}),
        json!({"$pii": 2, "subject": "s", "ct": "a", "nonce": "b", "aad_hash": "c"}),
        json!({"$pii": 1, "subject": "s", "ct": "a b", "nonce": "b", "aad_hash": "c"}),
        json!({"$pii": 1, "subject": "s", "ct": "a", "nonce": "b", "aad_hash": "c", "x": 1}),
        json!({"$erased": 1}),
        json!({"$erased": 2, "purpose": "p"}),
        json!({"$erased": 1, "purpose": 3}),
        json!({"$erased": 1, "purpose": "p", "erased_at": "yesterday"}),
        json!({"$erased": 1, "purpose": "p", "other": 1}),
    ];
    for bad in bad_forms {
        refused(&with(&ok, "image", Some(json!({"f": bad.clone()}))));
        refused(&with(&ok, "image", Some(json!({"rows": [{"f": bad}]}))));
    }
    // Absurd nesting is refused instead of recursing without bound.
    let mut deep = json!({});
    for _ in 0..(state::MAX_IMAGE_DEPTH + 2) {
        deep = json!({"d": deep});
    }
    refused(&with(&ok, "image", Some(deep)));
}

#[test]
fn a_state_envelope_without_subject_version_is_invalid() {
    let make = |version: Option<u64>| {
        let mut env = json!({
            "event_id": "0192f1c0-0000-7000-8000-0000000000e1",
            "topic": "doc_state",
            "schema_version": 1,
            "source_id": "localhost",
            "position": {"generation": 1, "seq": 1},
            "transaction": "0192f1c0-0000-7000-8000-0000000000e2",
            "ordinal": 0,
            "subject": {"entity": "sales_invoice", "id": "0192f1c0-0000-7000-8000-000000000001"},
            "meta": {
                "occurred_at": "2026-11-02T10:00:00Z",
                "actor": {"kind": "system", "component": "capture"},
                "correlation_id": "0192f1c0-0000-7000-8000-0000000000e3"
            },
            "data": example("insert")
        });
        if let (Some(v), Some(map)) = (version, env.as_object_mut()) {
            map.insert("subject_version".to_owned(), json!(v));
        }
        EventEnvelope::from_json(&env.to_string()).unwrap_or_else(|e| panic!("{e}"))
    };
    assert_eq!(state::validate_envelope(&make(Some(1))), Ok(()));
    assert!(state::validate_envelope(&make(None)).is_err());
    let mut other = make(Some(1));
    other.data = json!({});
    assert!(state::validate_envelope(&other).is_err());
}
