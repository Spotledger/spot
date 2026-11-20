//! Assertions over what the engine answered. A failing assertion panics with the refusal's code,
//! class and field paths and codes: never a field value.

use spot_app_api::ErrorClass;
use spot_app_api::document::{ChildRow, Document};
use spot_app_api::meta::FieldKind;
use spot_app_api::prelude::{RecordId, Value};
use spot_storage_contract::{ChangeFeed, FeedPosition};

use crate::actor::DocResult;
use crate::harness::AppHarness;

/// The operation succeeded.
///
/// # Panics
///
/// When the engine refused it.
#[track_caller]
pub fn assert_ok(result: &DocResult) {
    assert!(
        result.is_ok(),
        "expected success, but it was {}",
        result.diagnostic()
    );
}

/// The engine refused the operation with exactly `code`.
///
/// # Panics
///
/// On success, or another code.
#[track_caller]
pub fn assert_error(result: &DocResult, code: &str) {
    assert_eq!(
        result.error_code(),
        Some(code),
        "expected `{code}`, but it was {}",
        result.diagnostic()
    );
}

/// The refusal carries a field error `code` at `field`.
///
/// # Panics
///
/// When no such field error exists.
#[track_caller]
pub fn assert_field_error(result: &DocResult, field: &str, code: &str) {
    let found = result
        .field_errors()
        .iter()
        .any(|(path, c)| path == field && c == code);
    assert!(
        found,
        "expected field error `{code}` at `{field}`, but it was {}",
        result.diagnostic()
    );
}

/// The refusal is a permission refusal (the class), not a validation or a missing document.
///
/// # Panics
///
/// When the operation succeeded or was refused for another reason.
#[track_caller]
pub fn assert_denied(result: &DocResult) {
    let denied = result
        .error()
        .is_some_and(|e| e.class == ErrorClass::PermissionDenied);
    assert!(
        denied,
        "expected a permission refusal, but it was {}",
        result.diagnostic()
    );
}

/// The document the actor read does not carry `field` (the engine's read path hides it).
///
/// # Panics
///
/// When the read failed or the field is present with a value.
#[track_caller]
pub fn assert_field_hidden(result: &DocResult, field: &str) {
    assert_ok(result);
    let visible = result.doc().is_some_and(|d| {
        d.fields()
            .iter()
            .any(|(k, v)| k.as_str() == field && !v.is_null())
    });
    assert!(!visible, "the field `{field}` is readable by this actor");
}

fn rows_equal(a: &[ChildRow], b: &[ChildRow]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.fields == y.fields)
}

/// Two documents have the same DocType, the same field values and the same child rows in the
/// same order. Field order does not matter; ids, versions and row ids are not compared.
///
/// # Panics
///
/// When they differ; the message names the fields that differ, never their values.
#[track_caller]
pub fn assert_document_eq(actual: &Document, expected: &Document) {
    assert_eq!(actual.doctype(), expected.doctype(), "the DocTypes differ");
    let mut differing: Vec<String> = Vec::new();
    let names: std::collections::BTreeSet<_> = actual
        .fields()
        .keys()
        .chain(expected.fields().keys())
        .collect();
    for name in names {
        let a = actual.fields().get(name).unwrap_or(&Value::Null);
        let b = expected.fields().get(name).unwrap_or(&Value::Null);
        if a != b {
            differing.push(name.to_string());
        }
    }
    let tables: std::collections::BTreeSet<_> = actual
        .tables()
        .keys()
        .chain(expected.tables().keys())
        .collect();
    for table in tables {
        let a = actual.rows(table.as_str());
        let b = expected.rows(table.as_str());
        if !rows_equal(a, b) {
            differing.push(format!("{table}[]"));
        }
    }
    assert!(
        differing.is_empty(),
        "the documents differ in: {}",
        differing.join(", ")
    );
}

/// An event with `topic` is in the outbox.
///
/// # Panics
///
/// When none is.
pub async fn assert_event_published(harness: &AppHarness, topic: &str) {
    let events = harness
        .engine()
        .backend()
        .read_outbox(FeedPosition::START, 10_000)
        .await
        .unwrap_or_default();
    let found = events
        .iter()
        .any(|record| record.event.topic.as_str() == topic);
    assert!(
        found,
        "no `{topic}` event was published ({} events in the outbox)",
        events.len()
    );
}

/// Every link of every document the actors created points at a document that exists.
///
/// # Panics
///
/// When a link is orphaned; the message names the DocType and field.
pub async fn assert_no_orphan_links(harness: &AppHarness) {
    let created: Vec<(String, RecordId)> = harness
        .created
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let meta = harness.meta();
    for (doctype, id) in created {
        let Ok(Some(doc)) = harness.engine().peek(&doctype, id).await else {
            continue; // deleted since: nothing to check
        };
        let Some(def) = meta.doctype(&doctype) else {
            continue;
        };
        for field in &def.fields {
            let FieldKind::Link { doctype: target } = &field.kind else {
                continue;
            };
            if let Value::Id(link) = doc.get(field.name.as_str()) {
                let exists = harness
                    .engine()
                    .peek(target.as_str(), *link)
                    .await
                    .is_ok_and(|d| d.is_some());
                assert!(
                    exists,
                    "`{doctype}.{}` links to a missing `{target}`",
                    field.name
                );
            }
        }
    }
}
