//! WP-PLT-16.T4: what refers to a field from another DocType. `referrers_in` is the lookup the
//! site host gives `spot-customise`, so removing a field another DocType fetches from is refused
//! with `customise.referenced` naming that DocType. It reads names from the metadata, so it needs
//! no database; the list is a bounded sample.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_cli::services::customise::{MAX_REFERRERS, referrers_in};
use spot_meta::{Definition, FieldSpec, FieldType};

fn field(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

fn link(name: &str, to: &str) -> FieldSpec {
    let mut spec = field(name, FieldType::Link);
    spec.options = Some(to.to_owned());
    spec
}

fn fetch(name: &str, from: &str) -> FieldSpec {
    let mut spec = field(name, FieldType::Text);
    spec.fetch_from = Some(from.to_owned());
    spec
}

fn party() -> Definition {
    let mut party = Definition::new("party");
    party.fields.push(field("party_name", FieldType::Text));
    party.fields.push(field("notes", FieldType::Text));
    party
}

fn invoice() -> Definition {
    let mut invoice = Definition::new("invoice");
    invoice.fields.push(link("customer", "party"));
    invoice
        .fields
        .push(fetch("customer_name", "customer.party_name"));
    invoice
}

#[test]
fn a_field_fetched_from_another_doctype_is_a_referrer_and_names_that_doctype() {
    let found = referrers_in(&[party(), invoice()], "party", "party_name");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, "field");
    assert_eq!(found[0].name, "invoice.customer_name");
}

#[test]
fn an_unreferenced_field_has_no_referrer() {
    assert!(referrers_in(&[party(), invoice()], "party", "notes").is_empty());
}

#[test]
fn a_fetch_through_a_link_to_another_doctype_is_not_a_referrer() {
    let mut other = Definition::new("shipment");
    other.fields.push(link("carrier", "carrier"));
    other
        .fields
        .push(fetch("carrier_name", "carrier.party_name"));
    assert!(
        referrers_in(&[party(), other], "party", "party_name").is_empty(),
        "the link points at `carrier`, not at `party`"
    );
}

#[test]
fn references_inside_the_doctype_itself_are_left_to_the_pipeline() {
    let mut party = party();
    party.fields.push(link("parent", "party"));
    party.fields.push(fetch("parent_name", "parent.party_name"));
    assert!(referrers_in(&[party], "party", "party_name").is_empty());
}

#[test]
fn a_child_table_doctype_is_a_doctype_like_any_other() {
    let mut row = Definition::new("invoice_line");
    row.is_child = true;
    row.fields.push(link("party", "party"));
    row.fields.push(fetch("party_name", "party.party_name"));
    let found = referrers_in(&[party(), row], "party", "party_name");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "invoice_line.party_name");
}

#[test]
fn the_answer_is_a_bounded_sample_in_doctype_order() {
    let mut definitions = vec![party()];
    for n in 0..(MAX_REFERRERS + 25) {
        let mut doctype = Definition::new(&format!("doc_{n:03}"));
        doctype.fields.push(link("party", "party"));
        doctype.fields.push(fetch("party_name", "party.party_name"));
        definitions.push(doctype);
    }
    let found = referrers_in(&definitions, "party", "party_name");
    assert_eq!(found.len(), MAX_REFERRERS);
    assert_eq!(found[0].name, "doc_000.party_name");
    assert_eq!(
        found[MAX_REFERRERS - 1].name,
        format!("doc_{:03}.party_name", MAX_REFERRERS - 1)
    );
}

#[test]
fn a_malformed_fetch_is_ignored_not_a_panic() {
    let mut odd = Definition::new("odd");
    odd.fields.push(link("party", "party"));
    odd.fields.push(fetch("a", "no_dot_here"));
    odd.fields.push(fetch("b", "."));
    assert!(referrers_in(&[party(), odd], "party", "party_name").is_empty());
}
