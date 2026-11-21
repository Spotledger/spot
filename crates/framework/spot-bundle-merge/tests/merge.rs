//! Golden three-way merges, one per section kind.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bundle_merge::{ConflictKind, merge3};
use spot_custom_bundle::SiteWriter;
use spot_meta::{FieldSpec, FieldType, Property};
use spot_views::ViewLayer;

/// A site layer that moves `customer` after `after`.
fn move_layer(after: &str) -> ViewLayer {
    ViewLayer::parse(
        "site.toml",
        &format!(
            "doctype = \"sales_invoice\"\nkind = \"site\"\n\n[[ops]]\nop = \"move_field\"\n\
             field = \"customer\"\nafter = \"{after}\"\n"
        ),
    )
    .expect("the site layer parses")
}

#[test]
fn custom_field_added_on_one_side_is_clean() {
    let base = empty_site();
    let mut ours = empty_site();
    add_field(
        &mut ours,
        "sales_invoice",
        "custom_region",
        Some("customer"),
    );
    let theirs = empty_site();

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert!(result.is_clean(), "conflicts: {:?}", result.conflicts);
    let items = items_of(&result.merged, "meta");
    assert!(
        items.contains_key("sales_invoice.custom_region"),
        "the added field is in the merged bundle"
    );
}

#[test]
fn custom_field_changed_identically_on_both_sides_is_clean() {
    let base = empty_site();
    let mut ours = empty_site();
    add_field(&mut ours, "sales_invoice", "custom_region", None);
    let mut theirs = empty_site();
    add_field(&mut theirs, "sales_invoice", "custom_region", None);

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert!(result.is_clean());
    assert!(items_of(&result.merged, "meta").contains_key("sales_invoice.custom_region"));
}

#[test]
fn custom_field_changed_differently_on_both_sides_conflicts() {
    let mut base = empty_site();
    add_field(&mut base, "sales_invoice", "custom_region", None);

    let mut ours = empty_site();
    let mut spec = FieldSpec::new("custom_region", FieldType::Text);
    spec.label = Some("Region".to_owned());
    ours.upsert_custom_field("sales_invoice", spec, None)
        .unwrap();

    let mut theirs = empty_site();
    let mut spec = FieldSpec::new("custom_region", FieldType::Text);
    spec.label = Some("Area".to_owned());
    theirs
        .upsert_custom_field("sales_invoice", spec, None)
        .unwrap();

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert_eq!(result.conflicts.len(), 1);
    let conflict = &result.conflicts[0];
    assert_eq!(conflict.kind, ConflictKind::BothModified);
    assert_eq!(conflict.section(), "meta");
    assert_eq!(conflict.key(), "sales_invoice.custom_region");
    assert!(conflict.primary.base.is_some());
    assert!(conflict.primary.ours.is_some());
    assert!(conflict.primary.theirs.is_some());
    // The conflicted item is not in the merged bundle.
    assert!(!items_of(&result.merged, "meta").contains_key("sales_invoice.custom_region"));
}

#[test]
fn custom_field_removed_on_one_side_and_changed_on_the_other_conflicts() {
    let mut base = empty_site();
    add_field(&mut base, "sales_invoice", "custom_region", None);

    let mut ours = empty_site();
    let mut spec = FieldSpec::new("custom_region", FieldType::Text);
    spec.label = Some("Region".to_owned());
    ours.upsert_custom_field("sales_invoice", spec, None)
        .unwrap();

    let theirs = empty_site();

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].kind, ConflictKind::ModifiedVsRemoved);
}

#[test]
fn custom_field_added_differently_on_both_sides_conflicts() {
    let base = empty_site();
    let mut ours = empty_site();
    let mut spec = FieldSpec::new("custom_region", FieldType::Text);
    spec.label = Some("Region".to_owned());
    ours.upsert_custom_field("sales_invoice", spec, None)
        .unwrap();

    let mut theirs = empty_site();
    let mut spec = FieldSpec::new("custom_region", FieldType::Text);
    spec.label = Some("Area".to_owned());
    theirs
        .upsert_custom_field("sales_invoice", spec, None)
        .unwrap();

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].kind, ConflictKind::BothAddedDifferently);
    assert!(result.conflicts[0].primary.base.is_none());
}

#[test]
fn override_changed_on_one_side_is_clean() {
    let mut base = empty_site();
    set_label(&mut base, "sales_invoice", "note", "Notes");

    let mut ours = empty_site();
    set_label(&mut ours, "sales_invoice", "note", "Remarks");

    let mut theirs = empty_site();
    set_label(&mut theirs, "sales_invoice", "note", "Notes");

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert!(result.is_clean());
    let items = items_of(&result.merged, "meta");
    assert_eq!(
        items.get("sales_invoice.note.label").map(Vec::as_slice),
        Some(b"Remarks".as_slice())
    );
}

#[test]
fn view_layer_changed_differently_on_both_sides_conflicts() {
    let mut base = empty_site();
    add_layer(&mut base, move_layer("amount"));

    let mut ours = empty_site();
    add_layer(&mut ours, move_layer("status"));

    let mut theirs = empty_site();
    add_layer(&mut theirs, move_layer("note"));

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].section(), "views");
    assert_eq!(result.conflicts[0].kind, ConflictKind::BothModified);
}

#[test]
fn script_changed_on_one_side_is_clean() {
    let mut base = empty_site();
    base = base.with_script(
        "validate",
        "accounting",
        "sales_invoice.validate",
        "export function validate(): boolean { return true; }\n",
    );

    let mut ours = empty_site();
    ours = ours.with_script(
        "validate",
        "accounting",
        "sales_invoice.validate",
        "export function validate(): boolean { return false; }\n",
    );

    let mut theirs = empty_site();
    theirs = theirs.with_script(
        "validate",
        "accounting",
        "sales_invoice.validate",
        "export function validate(): boolean { return true; }\n",
    );

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert!(result.is_clean());
    assert!(items_of(&result.merged, "scripts").contains_key("validate"));
}

#[test]
fn rule_changed_differently_on_both_sides_conflicts() {
    let mut base = empty_site();
    base = base.with_rule("big", "doc.amount > 0");

    let mut ours = empty_site();
    ours = ours.with_rule("big", "doc.amount > 1");

    let mut theirs = empty_site();
    theirs = theirs.with_rule("big", "doc.amount > 2");

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].section(), "rules");
    assert_eq!(result.conflicts[0].kind, ConflictKind::BothModified);
}

#[test]
fn fixtures_changed_on_one_side_is_clean() {
    let mut base = empty_site();
    base = base.with_fixtures(
        "sales_invoice",
        vec![serde_json::json!({"id": "INV-1", "amount": 1})],
    );

    let mut ours = empty_site();
    ours = ours.with_fixtures(
        "sales_invoice",
        vec![serde_json::json!({"id": "INV-1", "amount": 2})],
    );

    let mut theirs = empty_site();
    theirs = theirs.with_fixtures(
        "sales_invoice",
        vec![serde_json::json!({"id": "INV-1", "amount": 1})],
    );

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert!(result.is_clean());
    assert!(items_of(&result.merged, "fixtures").contains_key("sales_invoice"));
}

#[test]
fn disjoint_additions_merge_to_the_union() {
    let base = empty_site();
    let mut ours = empty_site();
    add_field(
        &mut ours,
        "sales_invoice",
        "custom_region",
        Some("customer"),
    );
    let mut theirs = empty_site();
    add_field(&mut theirs, "sales_invoice", "custom_code", None);

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert!(result.is_clean());

    // The golden: a site carrying both fields captures to the same merged bundle.
    let mut union = empty_site();
    add_field(
        &mut union,
        "sales_invoice",
        "custom_region",
        Some("customer"),
    );
    add_field(&mut union, "sales_invoice", "custom_code", None);
    let expected = bundle(&union, "ours");
    assert_eq!(result.merged.digest(), expected.digest());
    assert_eq!(result.merged.to_bytes(), expected.to_bytes());
}

#[test]
fn an_unrelated_property_override_still_merges() {
    let mut base = empty_site();
    set_label(&mut base, "sales_invoice", "note", "Notes");

    let mut ours = empty_site();
    set_label(&mut ours, "sales_invoice", "note", "Notes");
    ours.set_override("sales_invoice", Some("note"), Property::Required(true))
        .unwrap();

    let mut theirs = empty_site();
    set_label(&mut theirs, "sales_invoice", "note", "Notes");

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert!(result.is_clean());
    let items = items_of(&result.merged, "meta");
    assert_eq!(
        items.get("sales_invoice.note.required").map(Vec::as_slice),
        Some(b"true".as_slice())
    );
}

#[test]
fn unknown_section_files_merge_file_wise() {
    use std::collections::BTreeMap;

    use spot_custom_bundle::{Bundle, Limits, SectionData};

    let captured = bundle(&empty_site(), "base");
    let with_workflow = |body: &[u8]| {
        let mut sections = captured.sections().clone();
        sections.insert(
            "workflows".to_owned(),
            SectionData {
                schema: 1,
                files: BTreeMap::from([("approval.toml".to_owned(), body.to_vec())]),
            },
        );
        Bundle::pack(captured.header().clone(), sections, &Limits::default()).expect("pack")
    };

    let base = with_workflow(b"step = 1\n");
    let ours = with_workflow(b"step = 2\n");
    let theirs = with_workflow(b"step = 1\n");

    let result = merge3(&base, &ours, &theirs).expect("merge");
    assert!(result.is_clean());
    assert_eq!(
        result
            .merged
            .section("workflows")
            .expect("the unknown section is carried")
            .files
            .get("approval.toml")
            .map(Vec::as_slice),
        Some(b"step = 2\n".as_slice())
    );

    let conflicting = merge3(
        &base,
        &with_workflow(b"step = 3\n"),
        &with_workflow(b"step = 4\n"),
    )
    .expect("merge");
    assert_eq!(conflicting.conflicts.len(), 1);
    assert_eq!(conflicting.conflicts[0].section(), "workflows");
    assert_eq!(conflicting.conflicts[0].key(), "approval.toml");
}
