//! Tests of the crate.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod erasure_tests;
mod leak_tests;
mod protect_tests;
mod subject_tests;
mod tamper_tests;

use serde_json::{Value, json};

use crate::{CounterNonces, FieldSpec, KeyHandle, MemoryKeys, PersonalFields, protect};

/// The key of the first data subject.
pub(crate) const ALICE: u8 = 7;
/// The key of the second data subject.
pub(crate) const BOB: u8 = 8;
/// The tenant journal key (legal-retention fields).
pub(crate) const RETAINED: u8 = 9;

/// A key store holding both subjects' keys and the tenant journal key.
pub(crate) fn keys() -> MemoryKeys {
    let keys = MemoryKeys::new(KeyHandle::new([RETAINED; 32]));
    keys.insert("alice", KeyHandle::new([ALICE; 32]));
    keys.insert("bob", KeyHandle::new([BOB; 32]));
    keys
}

/// The description of a `Contact` image: three fields of one subject, one of a second (the
/// guardian), and one inside a child row.
pub(crate) fn spec() -> PersonalFields {
    PersonalFields::new("Contact")
        .expect("entity")
        .with_field(FieldSpec::new("email", "owner", "notifications", "contact_5y").expect("email"))
        .with_field(FieldSpec::new("phone", "owner", "support", "contact_5y").expect("phone"))
        .with_field(
            FieldSpec::new("items.*.note", "owner", "order_notes", "order_7y").expect("note"),
        )
        .with_field(
            FieldSpec::new("guardian_email", "guardian", "safeguarding", "contact_5y")
                .expect("guardian_email"),
        )
}

/// A contact image with id `id`, owned by `owner` and guarded by `guardian`, at `version`.
pub(crate) fn image(id: &str, owner: &str, guardian: &str, version: u64) -> Value {
    json!({
        "id": id,
        "version": version,
        "owner": owner,
        "guardian": guardian,
        "email": "ada@example.test",
        "phone": "+44 113 000 0000",
        "guardian_email": "guardian@example.test",
        "city": "Leeds",
        "link": {"doctype": "Customer", "name": "C-9"},
        "items": [
            {"row": 1, "note": "leave at the door", "qty": 2},
            {"row": 2, "note": "ring twice", "qty": 1}
        ],
        "totals": {"qty": 3}
    })
}

/// The paths of every personal value in [`image`].
pub(crate) fn personal_paths() -> Vec<&'static str> {
    vec![
        "email",
        "phone",
        "items.0.note",
        "items.1.note",
        "guardian_email",
    ]
}

/// Protects `image` with the crate's default description and a fresh counter, for the tests
/// that only care about the result.
pub(crate) fn protected(image: &Value, spec: &PersonalFields, keys: &MemoryKeys) -> Value {
    protect(image, spec, keys, &mut CounterNonces::new()).expect("protect")
}

/// Canonical JSON of `value` (serde_json's object map is ordered, so this is stable).
pub(crate) fn canonical(value: &Value) -> String {
    serde_json::to_string(value).expect("serialise")
}

/// The value at a dotted path, with array positions written as indices.
pub(crate) fn at<'a>(value: &'a Value, path: &str) -> &'a Value {
    let mut node = value;
    for segment in path.split('.') {
        let next = match node {
            Value::Object(map) => map.get(segment),
            Value::Array(items) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get(index)),
            _ => None,
        };
        node = next.unwrap_or_else(|| panic!("no value at {path}"));
    }
    node
}

/// Replaces the value at a dotted path, for building an expected image.
pub(crate) fn set_at(node: &mut Value, path: &str, replacement: Value) {
    let mut segments: Vec<&str> = path.split('.').collect();
    let last = segments.pop().expect("a path has a last segment");
    let mut node = node;
    for segment in segments {
        node = match node {
            Value::Object(map) => map.get_mut(segment).expect("path in the image"),
            Value::Array(items) => {
                let index: usize = segment.parse().expect("array index");
                items.get_mut(index).expect("row in the image")
            }
            _ => panic!("not a container"),
        };
    }
    match node {
        Value::Object(map) => {
            map.insert(last.to_owned(), replacement);
        }
        _ => panic!("not an object"),
    }
}

/// `value` with every envelope replaced by the marker `<pii>`, recursively.
///
/// Comparing this with the image whose personal values were replaced by the same marker proves
/// that nothing else moved: `serde_json`'s object map is ordered, so the two serialised forms
/// are equal only when every other key, value, child row and row count is identical.
pub(crate) fn strip(value: &Value) -> Value {
    match value {
        Value::Object(map) if map.contains_key("$pii") => Value::String("<pii>".to_owned()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, child)| (key.clone(), strip(child)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip).collect()),
        other => other.clone(),
    }
}

/// The image with every personal value replaced by `<pii>`, for comparison with [`strip`].
pub(crate) fn blank(image: &Value, paths: &[&str]) -> Value {
    let mut out = image.clone();
    for path in paths {
        set_at(&mut out, path, Value::String("<pii>".to_owned()));
    }
    out
}
