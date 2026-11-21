//! Rules compiled into the plan: parsing, the dependency graph, evaluation and refusals
//! (WP-MET-02.T2).
#![allow(missing_docs, clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::{definitions, effective, effective_from};
use spot_app_api::document::Document;
use spot_app_api::prelude::{Date, Month, Name, RecordId, Value};
use spot_meta::{Definition, EffectiveMeta};
use spot_plan::{DocPlan, FieldIx, PlanSet};

/// A fixed clock for rules that read `now()`: 2026-10-15 12:00 UTC.
fn now() -> spot_expr::DateTime {
    spot_expr::DateTime::from_micros(1_792_065_600_000_000).expect("an instant")
}

/// The compiled `invoice` plan from the fixture set.
fn invoice_plan() -> Arc<DocPlan> {
    PlanSet::build(&effective())
        .unwrap_or_else(|e| panic!("fixtures do not compile: {e:?}"))
        .get("invoice")
        .expect("invoice plan")
}

/// The fixture set with the `invoice` definition mutated.
fn effective_with_invoice(mutate: impl FnOnce(&mut Definition)) -> Arc<EffectiveMeta> {
    let mut defs = definitions();
    let invoice = defs
        .iter_mut()
        .find(|d| d.name == "invoice")
        .expect("invoice definition");
    mutate(invoice);
    effective_from(defs)
}

/// A document with a status and the given fields.
fn doc(fields: &[(&str, Value)]) -> Document {
    let mut document = Document::new(Name::new("invoice").expect("name"), RecordId::new_v7());
    for (name, value) in fields {
        document.set(Name::new(name).expect("field name"), value.clone());
    }
    document
}

#[test]
fn rule_expressions_are_parsed_into_the_plan() {
    let plan = invoice_plan();
    let posting = plan.field("posting_date").expect("posting_date");
    assert!(posting.depends_on.is_some(), "depends_on parsed");
    let total = plan.field("total").expect("total");
    assert!(total.mandatory_depends_on.is_some());
    let remarks = plan.field("remarks").expect("remarks");
    assert!(remarks.read_only_depends_on.is_some());
    let due = plan.field("due_date").expect("due_date");
    assert!(due.default_expr.is_some(), "expression default parsed");
    assert!(
        due.default.is_none(),
        "an expression default is not a Value"
    );
    let party_name = plan.field("party_name").expect("party_name");
    let fetch = party_name.fetch_from.as_ref().expect("fetch_from parsed");
    assert_eq!(fetch.link_field, "party");
    assert_eq!(fetch.target_field, "code");
}

#[test]
fn naming_is_parsed_into_the_plan() {
    let plan = invoice_plan();
    let naming = plan.naming().expect("naming parsed");
    assert!(!naming.parts.is_empty());
}

#[test]
fn unknown_field_in_a_rule_is_refused_at_its_position() {
    let meta = effective_with_invoice(|invoice| {
        invoice.fields[0].depends_on = Some("doc.nowhere == 1".to_owned());
    });
    let errors = PlanSet::build(&meta).expect_err("an unknown field must refuse");
    let error = errors
        .iter()
        .find(|e| e.code() == "plan.expr.unknown_field")
        .expect("plan.expr.unknown_field");
    assert_eq!(error.field(), Some("number"));
    if let spot_plan::PlanError::ExprUnknownField {
        unknown_field,
        position,
        ..
    } = error
    {
        assert_eq!(unknown_field, "nowhere");
        assert_eq!(*position, 4, "position of the field after `doc.`");
    } else {
        panic!("unexpected variant {error:?}");
    }
}

#[test]
fn a_malformed_expression_is_refused_with_a_position() {
    let meta = effective_with_invoice(|invoice| {
        invoice.fields[0].depends_on = Some("doc.status ==".to_owned());
    });
    let errors = PlanSet::build(&meta).expect_err("a malformed rule must refuse");
    let error = errors
        .iter()
        .find(|e| e.code() == "plan.expr.invalid")
        .expect("plan.expr.invalid");
    if let spot_plan::PlanError::ExprInvalid { rule, .. } = error {
        assert_eq!(rule, "depends_on");
    } else {
        panic!("unexpected variant {error:?}");
    }
}

#[test]
fn a_cycle_among_defaults_is_refused_with_the_cycle_listed() {
    let meta = effective_with_invoice(|invoice| {
        let mut a = spot_meta::FieldSpec::new("a", spot_meta::FieldType::Int);
        a.default = Some("=doc.b".to_owned());
        let mut b = spot_meta::FieldSpec::new("b", spot_meta::FieldType::Int);
        b.default = Some("=doc.a".to_owned());
        invoice.fields.push(a);
        invoice.fields.push(b);
    });
    let errors = PlanSet::build(&meta).expect_err("a cycle must refuse");
    let error = errors
        .iter()
        .find(|e| e.code() == "plan.expr.cycle")
        .expect("plan.expr.cycle");
    if let spot_plan::PlanError::ExprCycle { cycle, .. } = error {
        assert!(cycle.contains("a"), "cycle names its fields: {cycle}");
        assert!(cycle.contains("b"));
    } else {
        panic!("unexpected variant {error:?}");
    }
}

#[test]
fn fetch_from_an_unknown_target_field_is_refused() {
    let meta = effective_with_invoice(|invoice| {
        let party_name = invoice
            .fields
            .iter_mut()
            .find(|f| f.name == "party_name")
            .expect("party_name");
        party_name.fetch_from = Some("party.nowhere".to_owned());
    });
    let errors = PlanSet::build(&meta).expect_err("an unknown target must refuse");
    assert!(
        errors
            .iter()
            .any(|e| e.code() == "plan.fetch_from.unknown_target"),
        "{errors:?}"
    );
}

#[test]
fn a_naming_pattern_naming_an_unknown_field_is_refused() {
    let meta = effective_with_invoice(|invoice| {
        invoice.naming = Some("DOC-field:nowhere-.#####".to_owned());
    });
    let errors = PlanSet::build(&meta).expect_err("an unknown naming field must refuse");
    assert!(
        errors.iter().any(|e| e.code() == "plan.naming.invalid"),
        "{errors:?}"
    );
}

#[test]
fn is_mandatory_follows_the_document() {
    let plan = invoice_plan();
    let total = plan.field_ix("total").expect("total");
    let open = doc(&[("status", Value::Text("Open".into()))]);
    let paid = doc(&[("status", Value::Text("Paid".into()))]);
    assert!(!plan.is_mandatory(total, &open, now()).expect("eval"));
    assert!(plan.is_mandatory(total, &paid, now()).expect("eval"));
}

#[test]
fn is_read_only_follows_the_document() {
    let plan = invoice_plan();
    let remarks = plan.field_ix("remarks").expect("remarks");
    let open = doc(&[("status", Value::Text("Open".into()))]);
    let paid = doc(&[("status", Value::Text("Paid".into()))]);
    assert!(!plan.is_read_only(remarks, &open, now()).expect("eval"));
    assert!(plan.is_read_only(remarks, &paid, now()).expect("eval"));
}

#[test]
fn is_visible_follows_the_document() {
    let plan = invoice_plan();
    let posting = plan.field_ix("posting_date").expect("posting_date");
    let cancelled = doc(&[("status", Value::Text("Cancelled".into()))]);
    let open = doc(&[("status", Value::Text("Open".into()))]);
    assert!(!plan.is_visible(posting, &cancelled, now()).expect("eval"));
    assert!(plan.is_visible(posting, &open, now()).expect("eval"));
}

#[test]
fn the_dependency_graph_maps_reads_and_affects() {
    let plan = invoice_plan();
    let graph = plan.rules();
    let status = plan.field_ix("status").expect("status");
    let total = plan.field_ix("total").expect("total");
    let party = plan.field_ix("party").expect("party");
    let party_name = plan.field_ix("party_name").expect("party_name");

    assert!(
        graph.reads.get(&total).is_some_and(|r| r.contains(&status)),
        "total's mandatory rule reads status"
    );
    assert!(
        graph
            .affects
            .get(&status)
            .is_some_and(|a| a.contains(&total)),
        "a change to status affects total"
    );
    assert!(
        graph
            .reads
            .get(&party_name)
            .is_some_and(|r| r.contains(&party)),
        "party_name's fetch_from reads the link field"
    );
}

#[test]
fn evaluation_order_puts_read_fields_first_and_is_deterministic() {
    let plan = invoice_plan();
    let status = plan.field_ix("status").expect("status");
    let total = plan.field_ix("total").expect("total");
    let order = plan.evaluation_order(&[status]);
    let again = plan.evaluation_order(&[status]);
    assert_eq!(order, again, "deterministic");
    let status_at = order.iter().position(|ix| *ix == status);
    let total_at = order.iter().position(|ix| *ix == total);
    assert!(
        status_at.is_some() && total_at.is_some(),
        "both present: {order:?}"
    );
    assert!(status_at < total_at, "status is evaluated before total");
}

#[test]
fn changing_one_field_rule_rebuilds_only_that_doctype() {
    let base = PlanSet::build(&effective()).expect("base");
    let base_invoice = base
        .get("invoice")
        .expect("invoice")
        .plan_hash()
        .as_hex()
        .to_owned();
    let base_party = base
        .get("party")
        .expect("party")
        .plan_hash()
        .as_hex()
        .to_owned();

    let changed_meta = effective_with_invoice(|invoice| {
        let tag = invoice
            .fields
            .iter_mut()
            .find(|f| f.name == "tag")
            .expect("tag");
        tag.depends_on = Some("doc.status == 'Paid'".to_owned());
    });
    let changed = PlanSet::build(&changed_meta).expect("changed");
    let changed_invoice = changed
        .get("invoice")
        .expect("invoice")
        .plan_hash()
        .as_hex()
        .to_owned();
    let changed_party = changed
        .get("party")
        .expect("party")
        .plan_hash()
        .as_hex()
        .to_owned();

    assert_ne!(
        base_invoice, changed_invoice,
        "the changed rule changes the hash"
    );
    assert_eq!(
        base_party, changed_party,
        "an unchanged DocType keeps its hash"
    );
}

#[test]
fn the_same_metadata_produces_identical_hashes() {
    let set1 = PlanSet::build(&effective()).expect("build 1");
    let set2 = PlanSet::build(&effective()).expect("build 2");
    for doctype in ["invoice", "party", "pick"] {
        let one = set1.get(doctype).expect(doctype);
        let two = set2.get(doctype).expect(doctype);
        assert_eq!(one.plan_hash(), two.plan_hash(), "{doctype} hash stable");
    }
}

#[test]
fn a_runtime_custom_field_can_carry_a_rule() {
    let mut reg = common::registry();
    let mut custom = spot_meta::FieldSpec::new("custom_flag", spot_meta::FieldType::Bool);
    custom.depends_on = Some("doc.status == 'Paid'".to_owned());
    reg.add_custom_field("invoice", custom, None)
        .unwrap_or_else(|e| panic!("the custom field is refused: {e:?}"));
    let plan = PlanSet::build(&reg.effective())
        .unwrap_or_else(|e| panic!("plans do not compile: {e:?}"))
        .get("invoice")
        .expect("invoice plan");
    let custom_ix = plan.field_ix("custom_flag").expect("custom_flag");
    let status_ix = plan.field_ix("status").expect("status");
    assert!(
        plan.rules()
            .reads
            .get(&custom_ix)
            .is_some_and(|r| r.contains(&status_ix)),
        "the custom field's rule is parsed into the graph"
    );
}

#[test]
fn a_document_evaluates_with_stored_dates_and_decimals() {
    // Exercises the value conversion: a date and a decimal in the context evaluate cleanly.
    let plan = invoice_plan();
    let posting = plan.field_ix("posting_date").expect("posting_date");
    let document = doc(&[
        ("status", Value::Text("Open".into())),
        (
            "posting_date",
            Value::Date(Date::from_calendar_date(2026, Month::January, 31).expect("date")),
        ),
        ("total", Value::Decimal("12.50".parse().expect("decimal"))),
    ]);
    assert!(plan.is_visible(posting, &document, now()).expect("eval"));
    let _ = FieldIx::new(0);
}

#[test]
fn a_rule_that_reads_the_clock_uses_the_callers_now() {
    let meta = effective_with_invoice(|invoice| {
        invoice.fields[0].depends_on = Some("year(now()) == 2026".to_owned());
    });
    let plan = PlanSet::build(&meta)
        .expect("the rule compiles")
        .get("invoice")
        .expect("invoice plan");
    let number = plan.field_ix("number").expect("number");
    let open = doc(&[("status", Value::Text("Open".into()))]);
    let year_2027 = spot_expr::DateTime::from_micros(1_810_000_000_000_000).expect("an instant");
    assert!(plan.is_visible(number, &open, now()).expect("eval"));
    assert!(!plan.is_visible(number, &open, year_2027).expect("eval"));
}
