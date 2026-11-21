//! The field access trace: records for `trace` groups on every channel that releases a
//! value, refusals traced as unauthorised, no value in a record, fail-closed on a full
//! queue, and no cache between calls.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, RwLock};

use common::*;
use spot_activity::FieldChange;
use spot_app_api::prelude::{OffsetDateTime, Value};
use spot_field_guard::{Channel, DocProtection, ProtectionSource, Via};
use spot_permissions::{PermissionEngine, Snapshot};

fn redact(fx: &Fixture, who: &str, channel: Channel) -> spot_app_api::Document {
    let mut doc = invoice(&corpus());
    fx.guard
        .redact(channel, &principal(who), DOCTYPE, &doc_facts(), &mut doc)
        .unwrap();
    doc
}

/// `(field, authorised)` of the drained records, in order.
fn seen(fx: &Fixture) -> Vec<(String, bool)> {
    fx.trace
        .drain()
        .into_iter()
        .map(|r| (r.field, r.authorised))
        .collect()
}

#[test]
fn a_trace_group_read_emits_a_record_on_version_export_and_list_column() {
    for channel in [Channel::Version, Channel::Export, Channel::ListColumn] {
        let fx = fixture();
        redact(&fx, "analyst", channel);
        let records = fx.trace.drain();
        let fields: Vec<_> = records.iter().map(|r| r.field.as_str()).collect();
        // `salary` and the derived `annual_cost` are in the traced `hr` group.
        assert_eq!(fields, vec!["annual_cost", "salary"], "{channel:?}");
        for r in &records {
            assert_eq!(r.who, "analyst");
            assert_eq!(r.doctype, DOCTYPE);
            assert!(r.id.is_some());
            assert_eq!(r.via, Via::Channel(channel));
            assert!(r.authorised);
            assert_eq!(r.at, OffsetDateTime::UNIX_EPOCH);
        }
        // A record never holds a value.
        let text = format!("{records:?}");
        assert!(!text.contains("84500") && !text.contains("1014009"));
    }
}

#[test]
fn a_diff_read_of_a_trace_field_is_traced() {
    let fx = fixture();
    let change = FieldChange {
        path: "salary".into(),
        before: Some(Value::Text("1".into())),
        after: Some(Value::Text("2".into())),
    };
    fx.guard
        .redact_diff(&principal("analyst"), DOCTYPE, &doc_facts(), &[change])
        .unwrap();
    assert_eq!(seen(&fx), vec![("salary".to_owned(), true)]);
}

#[test]
fn a_refusal_is_traced_as_unauthorised() {
    let fx = fixture();
    redact(&fx, "clerk", Channel::Version);
    assert_eq!(
        seen(&fx),
        vec![
            ("annual_cost".to_owned(), false),
            ("salary".to_owned(), false)
        ]
    );
    // A masked reader got no real value, so nothing is traced.
    redact(&fx, "masked_reader", Channel::Version);
    assert!(fx.trace.is_empty());
}

#[test]
fn a_non_trace_group_emits_nothing() {
    let fx = fixture();
    let mut doc =
        spot_app_api::Document::new(name(DOCTYPE), spot_app_api::prelude::RecordId::new_v7());
    doc.set(
        name("card_number"),
        Value::Text("4111 1111 1111 1111".into()),
    );
    doc.set(name("number"), Value::Text("INV-1".into()));
    fx.guard
        .redact(
            Channel::Export,
            &principal("analyst"),
            DOCTYPE,
            &doc_facts(),
            &mut doc,
        )
        .unwrap();
    assert!(fx.trace.is_empty());
    // Child-table columns of a non-trace group neither.
    redact(&fx, "analyst", Channel::Export);
    assert!(seen(&fx).iter().all(|(f, _)| !f.starts_with("items.")));
}

#[test]
fn names_only_channels_trace_nothing() {
    let fx = fixture();
    redact(&fx, "admin", Channel::Notification);
    assert!(fx.trace.is_empty());
}

#[test]
fn a_full_queue_refuses_when_fail_closed_and_proceeds_when_not() {
    let fx = fixture_with(0);
    let mut doc = invoice(&corpus());
    let err = fx
        .guard
        .redact(
            Channel::Version,
            &principal("analyst"),
            DOCTYPE,
            &doc_facts(),
            &mut doc,
        )
        .unwrap_err();
    assert_eq!(err.code(), "fieldguard.trace_unavailable");
    // The caller that ignores the error still gets nothing.
    assert!(doc.fields().is_empty() && doc.tables().is_empty());

    let open = fx.guard.clone().with_fail_closed(false);
    let mut doc = invoice(&corpus());
    open.redact(
        Channel::Version,
        &principal("analyst"),
        DOCTYPE,
        &doc_facts(),
        &mut doc,
    )
    .unwrap();
    assert!(doc.fields().contains_key(&name("salary")));

    // A document with no traced field never needs the queue.
    let mut plain =
        spot_app_api::Document::new(name(DOCTYPE), spot_app_api::prelude::RecordId::new_v7());
    plain.set(name("number"), Value::Text("INV-1".into()));
    fx.guard
        .redact(
            Channel::Version,
            &principal("analyst"),
            DOCTYPE,
            &doc_facts(),
            &mut plain,
        )
        .unwrap();
    assert!(plain.fields().contains_key(&name("number")));
}

#[test]
fn a_lost_refusal_record_does_not_fail_the_call() {
    // A refused access is already safe; only a lost record of an authorised access fails.
    let fx = fixture_with(0);
    let mut doc = invoice(&corpus());
    fx.guard
        .redact(
            Channel::Version,
            &principal("clerk"),
            DOCTYPE,
            &doc_facts(),
            &mut doc,
        )
        .unwrap();
    assert!(!doc.fields().contains_key(&name("salary")));
}

/// A source whose facts a test can swap between two calls.
struct Swappable(RwLock<Arc<dyn ProtectionSource>>);

impl ProtectionSource for Swappable {
    fn protection(&self, doctype: &str) -> Option<Arc<DocProtection>> {
        self.0.read().unwrap().protection(doctype)
    }
}

struct Nothing;

impl ProtectionSource for Nothing {
    fn protection(&self, _doctype: &str) -> Option<Arc<DocProtection>> {
        None
    }
}

#[test]
fn a_protection_swap_changes_the_very_next_call() {
    let fx = fixture();
    let swappable = Arc::new(Swappable(RwLock::new(fx.protection.clone())));
    let guard = spot_field_guard::FieldGuard::new(
        fx.guard.engine().clone(),
        swappable.clone(),
        fx.trace.clone(),
        fx.audit.clone(),
        fx.clock.clone(),
    );
    let mut doc = invoice(&corpus());
    guard
        .redact(
            Channel::Export,
            &principal("analyst"),
            DOCTYPE,
            &doc_facts(),
            &mut doc,
        )
        .unwrap();
    assert!(doc.fields().contains_key(&name("card_number")));
    *swappable.0.write().unwrap() = Arc::new(Nothing);
    let mut doc = invoice(&corpus());
    let err = guard
        .redact(
            Channel::Export,
            &principal("analyst"),
            DOCTYPE,
            &doc_facts(),
            &mut doc,
        )
        .unwrap_err();
    assert_eq!(err.code(), "fieldguard.unknown_doctype");
    assert!(doc.fields().is_empty());
}

#[test]
fn a_permission_snapshot_swap_changes_the_very_next_call() {
    let fx = fixture();
    let p = principal("analyst");
    let before = redact(&fx, "analyst", Channel::Export);
    assert!(before.fields().contains_key(&name("card_number")));

    // A snapshot with the metadata but no grants at all: nobody holds anything.
    let empty = Snapshot::empty(&fx.meta).unwrap();
    let swapped = fx
        .guard
        .with_engine(Arc::new(PermissionEngine::new(Arc::new(empty))));
    let mut doc = invoice(&corpus());
    swapped
        .redact(Channel::Export, &p, DOCTYPE, &doc_facts(), &mut doc)
        .unwrap();
    assert!(doc.fields().is_empty());
    // The old guard is untouched: there is no shared state to invalidate.
    let again = redact(&fx, "analyst", Channel::Export);
    assert!(again.fields().contains_key(&name("card_number")));
}
