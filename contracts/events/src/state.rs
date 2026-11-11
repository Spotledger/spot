//! The payload schema of the State event `doc_state` (WP-JRN-01.T1, events 0.7.3; ADR-061
//! section 3 and its 2026-10-31 amendment).
//!
//! A `doc_state` event is the whole committed image of one document, written by the capture
//! step in the same transaction as the document (WP-JRN-02.T2), read by the rebuild executor,
//! the snapshot writer and the sync bundle code. This module fixes the shape once.
//!
//! # Where each fact lives
//!
//! | Fact | Where |
//! | --- | --- |
//! | document identity (DocType storage name, record id) | the envelope's `subject` |
//! | the document's version | the envelope's `subject_version` (required) |
//! | what happened | `data.op`: `insert`, `update` or `delete` |
//! | the catalogue version the image was written under | `data.entity_schema`, an integer of at least 1 |
//! | the document | `data.image`, an object |
//!
//! `data` carries exactly those three members; any other member is refused. `image` is the
//! whole document in the native value encoding (ADR-055): every declared field and every child
//! row, decimals as strings. For `delete` it is the last committed version. The contract checks
//! that it is an object and nothing more: its fields belong to the DocType, and the engine
//! validates an image against its DocType.
//!
//! # Personal fields
//!
//! `spot-journal-protect` replaces each personal field of an image with a sealed envelope
//! `{"$pii": 1, "subject", "ct", "nonce", "aad_hash"}` before the image reaches the journal, and
//! erasure turns it into the placeholder `{"$erased": 1, "purpose", "erased_at": null}`. Either
//! form is a valid value anywhere inside `image` (a field, a child-row field, an element of a
//! list). So the shape cannot be mistaken, an object that carries either marker must be
//! complete: a half-written envelope or placeholder is refused. Nothing else inside `image`
//! is looked at, so the string bound and the secret-key scan of the `sys_*` topics do not apply:
//! a document's text fields may be long and are the DocType's own.

use std::collections::BTreeSet;

use serde_json::{Map, Value as Json};

use crate::envelope::EventEnvelope;
use crate::names::Topic;
use crate::schema::{EventSchema, PayloadType, SchemaError};
use crate::system::{Fields, closed, err, req};

/// The topic of a document's committed image.
pub const DOC_STATE_TOPIC: &str = "doc_state";

/// The operations a state event records (a closed set: a new one is a new major version).
pub const STATE_OPS: [&str; 3] = ["insert", "update", "delete"];

/// The marker member of a sealed personal-field envelope (`spot-journal-pii`).
pub const PII_MARKER: &str = "$pii";
/// The marker member of an erasure placeholder.
pub const ERASED_MARKER: &str = "$erased";

/// The deepest nesting of `image` the personal-field check walks. Deeper values are refused,
/// which also bounds the recursion.
pub const MAX_IMAGE_DEPTH: usize = 64;

/// The v1 payload schema of `doc_state` (revision 0).
pub fn schema() -> EventSchema {
    let mut f = Fields::new();
    req(&mut f, "op", closed(&STATE_OPS));
    // The catalogue version of the DocType the image was written under; at least 1 (checked by
    // `validate`).
    req(&mut f, "entity_schema", PayloadType::Int);
    // An object; checked by `validate`.
    req(&mut f, "image", PayloadType::Json);
    let topic = match Topic::any(DOC_STATE_TOPIC) {
        Ok(t) => t,
        Err(e) => unreachable!("doc_state is a valid topic: {e}"),
    };
    EventSchema {
        topic,
        major: 1,
        revision: 0,
        fields: f,
        retired: BTreeSet::new(),
    }
}

/// Validates the `data` of a `doc_state` event: strictly against [`schema`], then
/// `entity_schema >= 1`, `image` an object, and every personal-field envelope or erasure
/// placeholder inside it well formed.
pub fn validate(data: &Json) -> Result<(), Vec<SchemaError>> {
    let mut errors = schema().validate(data).err().unwrap_or_default();
    if errors.is_empty()
        && let Some(obj) = data.as_object()
    {
        if obj.get("entity_schema").and_then(Json::as_i64) < Some(1) {
            errors.push(err("entity_schema", "at least 1"));
        }
        match obj.get("image") {
            Some(image) if image.is_object() => scan_image(image, "image", 0, &mut errors),
            _ => errors.push(err("image", "must be a JSON object")),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validates a whole `doc_state` envelope: the topic, the `subject_version` the capture
/// invariant compares by, and the payload.
pub fn validate_envelope(envelope: &EventEnvelope) -> Result<(), Vec<SchemaError>> {
    let mut errors = Vec::new();
    if envelope.topic.as_str() != DOC_STATE_TOPIC {
        errors.push(err("topic", "not a doc_state event"));
    }
    if envelope.subject_version.is_none() {
        errors.push(err(
            "subject_version",
            "a doc_state event carries the document's version",
        ));
    }
    if let Err(e) = validate(&envelope.data) {
        errors.extend(e);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn scan_image(value: &Json, path: &str, depth: usize, errors: &mut Vec<SchemaError>) {
    if depth > MAX_IMAGE_DEPTH {
        errors.push(err(path, "nested too deeply"));
        return;
    }
    match value {
        Json::Object(map) => {
            if map.contains_key(PII_MARKER) {
                check_sealed(map, path, errors);
            } else if map.contains_key(ERASED_MARKER) {
                check_erased(map, path, errors);
            } else {
                for (k, v) in map {
                    scan_image(v, &format!("{path}.{k}"), depth + 1, errors);
                }
            }
        }
        Json::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                scan_image(v, &format!("{path}[{i}]"), depth + 1, errors);
            }
        }
        _ => {}
    }
}

fn is_b64url(v: Option<&Json>) -> bool {
    v.and_then(Json::as_str).is_some_and(|s| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

fn check_sealed(map: &Map<String, Json>, path: &str, errors: &mut Vec<SchemaError>) {
    let known = [PII_MARKER, "subject", "ct", "nonce", "aad_hash"];
    if map.keys().any(|k| !known.contains(&k.as_str())) || map.len() != known.len() {
        errors.push(err(
            path,
            "a personal-field envelope has exactly $pii, subject, ct, nonce and aad_hash",
        ));
        return;
    }
    if map.get(PII_MARKER).and_then(Json::as_u64) != Some(1) {
        errors.push(err(path, "unknown personal-field envelope version"));
    }
    if map
        .get("subject")
        .and_then(Json::as_str)
        .is_none_or(str::is_empty)
    {
        errors.push(err(
            path,
            "the envelope's subject must be a non-empty string",
        ));
    }
    for k in ["ct", "nonce", "aad_hash"] {
        if !is_b64url(map.get(k)) {
            errors.push(err(path, format!("{k} must be base64url text")));
        }
    }
}

fn check_erased(map: &Map<String, Json>, path: &str, errors: &mut Vec<SchemaError>) {
    let known = [ERASED_MARKER, "purpose", "erased_at"];
    if map.keys().any(|k| !known.contains(&k.as_str())) || !map.contains_key("purpose") {
        errors.push(err(
            path,
            "an erasure placeholder has $erased, purpose and optionally erased_at",
        ));
        return;
    }
    if map.get(ERASED_MARKER).and_then(Json::as_u64) != Some(1) {
        errors.push(err(path, "unknown erasure placeholder version"));
    }
    if !map.get("purpose").is_some_and(Json::is_string) {
        errors.push(err(path, "purpose must be a string"));
    }
    if !matches!(
        map.get("erased_at"),
        None | Some(Json::Null) | Some(Json::Number(_))
    ) {
        errors.push(err(path, "erased_at is null or an integer"));
    }
}
