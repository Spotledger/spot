//! System actors, the derived-value lint, and the plain TOML source.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_field_guard::lint::{Derivation, check_derived};
use spot_field_guard::{
    ActorManifest, AuditRecord, Channel, FieldGuard, ProtectionSource, StaticProtection, Via,
};

#[test]
fn a_declared_group_is_allowed_and_a_trace_group_is_traced() {
    let fx = fixture();
    let nightly = ActorManifest::new("payroll_export", &["hr", "finance"]);
    fx.guard.system_access(&nightly, DOCTYPE, "salary").unwrap();
    fx.guard.system_access(&nightly, DOCTYPE, "number").unwrap();
    // The derived field needs both of its groups declared; this manifest has both.
    fx.guard
        .system_access(&nightly, DOCTYPE, "annual_cost")
        .unwrap();
    let t = fx.trace.drain();
    assert_eq!(t.len(), 2);
    assert!(
        t.iter()
            .all(|r| r.via == Via::System && r.authorised && r.who == "payroll_export")
    );
    assert!(fx.audit.is_empty());
}

#[test]
fn an_undeclared_group_is_refused_and_recorded() {
    let fx = fixture();
    let job = ActorManifest::new("nightly_sync", &["hr"]);
    for field in ["card_number", "annual_cost", "items.iban"] {
        let err = fx.guard.system_access(&job, DOCTYPE, field).unwrap_err();
        assert_eq!(err.code(), "fieldguard.undeclared_system_access", "{field}");
        assert_eq!(err.paths(), [field]);
    }
    let audit = fx.audit.drain();
    assert_eq!(audit.len(), 3);
    match &audit[0] {
        AuditRecord::UndeclaredSystemAccess {
            actor,
            doctype,
            field,
            group,
            ..
        } => {
            assert_eq!(actor, "nightly_sync");
            assert_eq!(doctype, DOCTYPE);
            assert_eq!(field, "card_number");
            assert_eq!(group, "pci");
        }
        other => panic!("{other:?}"),
    }
    // The refused access to the traced derived field is traced as unauthorised.
    let t = fx.trace.drain();
    assert_eq!(t.len(), 1);
    assert!(!t[0].authorised && t[0].field == "annual_cost");
}

#[test]
fn the_derived_value_lint_passes_the_real_metadata() {
    let fx = fixture();
    let derivations = vec![Derivation {
        doctype: DOCTYPE.into(),
        field: "annual_cost".into(),
        inputs: vec![
            (DOCTYPE.into(), "salary".into()),
            (DOCTYPE.into(), "bonus".into()),
        ],
    }];
    assert!(check_derived(&*fx.protection, &derivations).is_empty());
}

const FACTS_TOML: &str = r#"
[[doctype]]
name = "sales_invoice"
trace_groups = ["hr"]
tables = { items = "sales_invoice_item" }

[[doctype.fields]]
path = "card_number"
groups = ["pci"]
mask = { last_chars = 4 }

[[doctype.fields]]
path = "salary"
groups = ["hr"]
mask = "placeholder"

[[doctype.fields]]
path = "annual_cost"
groups = ["hr"]

[[doctype]]
name = "sales_invoice_item"

[[doctype.fields]]
path = "iban"
groups = ["bank"]
mask = { last_chars = 4 }
"#;

#[test]
fn the_lint_catches_a_derived_field_that_dropped_an_input_group() {
    // The hand-written fixture forgot that `annual_cost` is also derived from a finance
    // field: it is in `hr` only.
    let source = StaticProtection::from_toml(FACTS_TOML).unwrap();
    let derivations = vec![Derivation {
        doctype: DOCTYPE.into(),
        field: "annual_cost".into(),
        inputs: vec![
            (DOCTYPE.into(), "salary".into()),
            (DOCTYPE.into(), "card_number".into()),
            (CHILD.into(), "iban".into()),
            ("ghost".into(), "x".into()),
        ],
    }];
    let breaches = check_derived(&source, &derivations);
    let missing: Vec<_> = breaches
        .iter()
        .map(|b| (b.input.as_str(), b.missing_group.as_str()))
        .collect();
    assert_eq!(
        missing,
        vec![
            ("sales_invoice.card_number", "pci"),
            ("sales_invoice_item.iban", "bank"),
            ("ghost.x", ""),
        ]
    );
}

#[test]
fn a_plain_toml_source_drives_the_guard_without_the_metadata_adapter() {
    let fx = fixture();
    let source: Arc<dyn ProtectionSource> =
        Arc::new(StaticProtection::from_toml(FACTS_TOML).unwrap());
    let guard = FieldGuard::new(
        fx.guard.engine().clone(),
        source,
        fx.trace.clone(),
        fx.audit.clone(),
        fx.clock.clone(),
    );
    let mut doc = invoice(&corpus());
    guard
        .redact(
            Channel::Version,
            &principal("masked_reader"),
            DOCTYPE,
            &doc_facts(),
            &mut doc,
        )
        .unwrap();
    assert_eq!(
        doc.get("card_number"),
        &spot_app_api::prelude::Value::Text("***************1111".into())
    );
    assert_eq!(
        doc.get("salary"),
        &spot_app_api::prelude::Value::Text("***".into())
    );
    let bad = StaticProtection::from_toml("[[doctype]]\nname = 1");
    assert!(bad.is_err());
}
