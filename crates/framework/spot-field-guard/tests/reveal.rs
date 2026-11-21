//! The audited reveal: one document and one field per call, audited on allow and on refusal,
//! no existence leak, bulk for system actors only, nothing released if the audit fails.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_app_api::prelude::Value;
use spot_field_guard::{AuditRecord, FieldGuard, FullSink, RevealService, Via};
use spot_permissions::Principal;

const CARD: &str = "4111 1111 1111 1111";

fn service(fx: &Fixture) -> (RevealService, Arc<MapValues>) {
    let values = Arc::new(MapValues::default());
    values.put(DOCTYPE, "doc-1", "card_number", Value::Text(CARD.into()));
    values.put(DOCTYPE, "doc-1", "salary", Value::Text("84500.75".into()));
    values.put(DOCTYPE, "doc-2", "salary", Value::Text("61000.00".into()));
    (
        RevealService::new(Arc::new(fx.guard.clone()), values.clone()),
        values,
    )
}

#[test]
fn an_allowed_reveal_returns_one_value_and_audits_it() {
    let fx = fixture();
    let (svc, _) = service(&fx);
    fx.clock
        .set_wall(spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH);
    let v = svc
        .reveal(
            &principal("admin"),
            DOCTYPE,
            "doc-1",
            "card_number",
            "chargeback 4471",
        )
        .unwrap();
    assert_eq!(v, Value::Text(CARD.into()));
    let audit = fx.audit.drain();
    assert_eq!(audit.len(), 1);
    match &audit[0] {
        AuditRecord::Reveal {
            who,
            doctype,
            id,
            field,
            reason,
            allowed,
            via,
            ..
        } => {
            assert_eq!(
                (who.as_str(), doctype.as_str(), id.as_str(), field.as_str()),
                ("admin", DOCTYPE, "doc-1", "card_number")
            );
            assert_eq!(reason, "chargeback 4471");
            assert!(*allowed);
            assert_eq!(*via, Via::Api);
        }
        other => panic!("not a reveal record: {other:?}"),
    }
    // The record carries who, where and why; never the value.
    assert!(!format!("{audit:?}").contains("4111"));
}

#[test]
fn a_refused_reveal_is_audited_too() {
    let fx = fixture();
    let (svc, _) = service(&fx);
    // analyst may read the card but holds no `reveal`; clerk may not even read it.
    for who in ["analyst", "clerk", "masked_reader"] {
        let err = svc
            .reveal(
                &principal(who),
                DOCTYPE,
                "doc-1",
                "card_number",
                "curiosity",
            )
            .unwrap_err();
        assert_eq!(err.code(), "permission.reveal_denied", "{who}");
    }
    let audit = fx.audit.drain();
    assert_eq!(audit.len(), 3);
    assert!(
        audit
            .iter()
            .all(|r| matches!(r, AuditRecord::Reveal { allowed: false, .. }))
    );
}

#[test]
fn a_missing_and_a_forbidden_document_look_the_same() {
    let fx = fixture();
    let (svc, _) = service(&fx);
    let forbidden = svc
        .reveal(&principal("clerk"), DOCTYPE, "doc-1", "card_number", "r")
        .unwrap_err();
    let missing = svc
        .reveal(
            &principal("clerk"),
            DOCTYPE,
            "no-such-doc",
            "card_number",
            "r",
        )
        .unwrap_err();
    let missing_for_admin = svc
        .reveal(
            &principal("admin"),
            DOCTYPE,
            "no-such-doc",
            "card_number",
            "r",
        )
        .unwrap_err();
    let unknown_doctype = svc
        .reveal(&principal("admin"), "ghost", "doc-1", "card_number", "r")
        .unwrap_err();
    let plain_field = svc
        .reveal(&principal("admin"), DOCTYPE, "doc-1", "number", "r")
        .unwrap_err();
    assert_eq!(forbidden, missing);
    assert_eq!(forbidden, missing_for_admin);
    assert_eq!(forbidden, unknown_doctype);
    assert_eq!(forbidden, plain_field);
}

#[test]
fn a_reveal_needs_a_reason() {
    let fx = fixture();
    let (svc, _) = service(&fx);
    for reason in ["", "   "] {
        let err = svc
            .reveal(&principal("admin"), DOCTYPE, "doc-1", "card_number", reason)
            .unwrap_err();
        assert_eq!(err.code(), "fieldguard.reason_required");
    }
    assert!(fx.audit.is_empty());
}

#[test]
fn bulk_reveal_is_for_a_system_actor_only() {
    let fx = fixture();
    let (svc, _) = service(&fx);
    let ids = vec!["doc-1".to_owned(), "doc-2".to_owned(), "doc-3".to_owned()];
    // A person, even the tenant administrator, is refused.
    let err = svc
        .reveal_bulk(&principal("admin"), DOCTYPE, &ids, "salary", "payroll run")
        .unwrap_err();
    assert_eq!(err.code(), "fieldguard.reveal_bulk_refused");
    // The system gets the documents that exist, and the audit record carries the count.
    let found = svc
        .reveal_bulk(&Principal::system(), DOCTYPE, &ids, "salary", "payroll run")
        .unwrap();
    assert_eq!(found.len(), 2);
    let audit = fx.audit.drain();
    assert!(matches!(
        &audit[0],
        AuditRecord::RevealBulk {
            count: 3,
            allowed: false,
            ..
        }
    ));
    assert!(matches!(
        &audit[1],
        AuditRecord::RevealBulk {
            count: 3,
            allowed: true,
            ..
        }
    ));
    assert!(!format!("{audit:?}").contains("84500"));
}

#[test]
fn nothing_is_released_when_the_audit_cannot_be_written() {
    let fx = fixture();
    let (_, values) = service(&fx);
    let guard = FieldGuard::new(
        fx.guard.engine().clone(),
        fx.protection.clone(),
        fx.trace.clone(),
        Arc::new(FullSink),
        fx.clock.clone(),
    );
    let svc = RevealService::new(Arc::new(guard), values);
    let err = svc
        .reveal(&principal("admin"), DOCTYPE, "doc-1", "card_number", "r")
        .unwrap_err();
    assert_eq!(err.code(), "fieldguard.audit_unavailable");
    let err = svc
        .reveal_bulk(
            &Principal::system(),
            DOCTYPE,
            &["doc-1".to_owned()],
            "salary",
            "r",
        )
        .unwrap_err();
    assert_eq!(err.code(), "fieldguard.audit_unavailable");
}

#[test]
fn a_reveal_of_a_trace_field_is_traced_including_the_refused_ones() {
    let fx = fixture();
    let (svc, _) = service(&fx);
    svc.reveal(&principal("admin"), DOCTYPE, "doc-1", "salary", "r")
        .unwrap();
    svc.reveal(&principal("clerk"), DOCTYPE, "doc-1", "salary", "r")
        .unwrap_err();
    svc.reveal(&principal("admin"), DOCTYPE, "missing", "salary", "r")
        .unwrap_err();
    // `card_number` is in the non-trace `pci` group: audited, not traced.
    svc.reveal(&principal("admin"), DOCTYPE, "doc-1", "card_number", "r")
        .unwrap();
    let t = fx.trace.drain();
    let shape: Vec<_> = t
        .iter()
        .map(|r| (r.who.as_str(), r.field.as_str(), r.authorised, r.via))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("admin", "salary", true, Via::Api),
            ("clerk", "salary", false, Via::Api),
            ("admin", "salary", false, Via::Api),
        ]
    );
    assert!(!format!("{t:?}").contains("84500"));
}

#[test]
fn a_full_trace_queue_withholds_a_traced_reveal() {
    let fx = fixture_with(0);
    let (svc, _) = service(&fx);
    let err = svc
        .reveal(&principal("admin"), DOCTYPE, "doc-1", "salary", "r")
        .unwrap_err();
    assert_eq!(err.code(), "fieldguard.trace_unavailable");
}
