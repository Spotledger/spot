//! Section-aware semantic conflicts: breaks a clean item-wise merge can still leave.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bundle_merge::{ConflictKind, SemanticKind, merge3, merge3_with};
use spot_custom_bundle::SiteWriter;
use spot_meta::{Definition, FieldSpec, FieldType};

/// A script with a second `on_submit` hook on the same point.
fn two_script_site(first: &str, second: &str) -> spot_custom_bundle::FakeSite {
    empty_site()
        .with_script("validate", "accounting", "sales_invoice.validate", first)
        .with_script("on_submit", "accounting", "sales_invoice.validate", second)
}

fn semantic_kinds(result: &spot_bundle_merge::MergeResult) -> Vec<SemanticKind> {
    result
        .conflicts
        .iter()
        .filter_map(|conflict| match conflict.kind {
            ConflictKind::Semantic(kind) => Some(kind),
            _ => None,
        })
        .collect()
}

#[test]
fn an_override_on_a_field_the_other_side_removed_is_semantic() {
    let mut base = empty_site();
    add_field(&mut base, "sales_invoice", "custom_region", None);

    let mut ours = empty_site();
    add_field(&mut ours, "sales_invoice", "custom_region", None);
    set_label(&mut ours, "sales_invoice", "custom_region", "Region");

    let theirs = empty_site();

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert_eq!(
        semantic_kinds(&result),
        vec![SemanticKind::OverrideOnRemovedField]
    );
    let conflict = result
        .conflicts
        .iter()
        .find(|conflict| conflict.kind.is_semantic())
        .expect("a semantic conflict");
    assert_eq!(conflict.key(), "sales_invoice.custom_region");
    assert!(
        conflict
            .related
            .iter()
            .any(|item| item.key == "sales_invoice.custom_region.label"),
        "the surviving override is listed: {:?}",
        conflict.related
    );
    // The merged bundle keeps the override but not the field.
    let items = items_of(&result.merged, "meta");
    assert!(!items.contains_key("sales_invoice.custom_region"));
    assert!(items.contains_key("sales_invoice.custom_region.label"));
}

#[test]
fn a_field_anchored_after_a_removed_field_is_semantic() {
    let mut base = empty_site();
    add_field(&mut base, "sales_invoice", "custom_anchor", None);

    let mut ours = empty_site();
    add_field(&mut ours, "sales_invoice", "custom_anchor", None);
    add_field(
        &mut ours,
        "sales_invoice",
        "custom_child",
        Some("custom_anchor"),
    );

    let theirs = empty_site();

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert_eq!(
        semantic_kinds(&result),
        vec![SemanticKind::InsertAfterRemovedField]
    );
    let conflict = result
        .conflicts
        .iter()
        .find(|conflict| conflict.kind.is_semantic())
        .expect("a semantic conflict");
    assert_eq!(conflict.key(), "sales_invoice.custom_child");
    assert!(
        conflict
            .related
            .iter()
            .any(|item| item.key == "sales_invoice.custom_anchor")
    );
}

#[test]
fn two_sides_anchoring_to_the_same_predecessor_is_semantic() {
    let base = empty_site();

    let mut ours = empty_site();
    add_field(&mut ours, "sales_invoice", "custom_a", Some("customer"));

    let mut theirs = empty_site();
    add_field(&mut theirs, "sales_invoice", "custom_b", Some("customer"));

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert_eq!(
        semantic_kinds(&result),
        vec![SemanticKind::InsertAfterCollision]
    );
    let conflict = result
        .conflicts
        .iter()
        .find(|conflict| conflict.kind.is_semantic())
        .expect("a semantic conflict");
    assert_eq!(conflict.key(), "sales_invoice.custom_a");
    assert!(
        conflict
            .related
            .iter()
            .any(|item| item.key == "sales_invoice.custom_b")
    );
}

#[test]
fn two_sides_changing_scripts_on_the_same_hook_is_semantic() {
    let base = two_script_site(
        "export function a(): boolean { return true; }\n",
        "export function b(): boolean { return true; }\n",
    );
    let ours = two_script_site(
        "export function a(): boolean { return false; }\n",
        "export function b(): boolean { return true; }\n",
    );
    let theirs = two_script_site(
        "export function a(): boolean { return true; }\n",
        "export function b(): boolean { return false; }\n",
    );

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert_eq!(
        semantic_kinds(&result),
        vec![SemanticKind::ScriptHookCollision]
    );
    let conflict = result
        .conflicts
        .iter()
        .find(|conflict| conflict.kind.is_semantic())
        .expect("a semantic conflict");
    assert_eq!(conflict.section(), "scripts");
    let keys: Vec<&str> = conflict
        .versions()
        .map(|version| version.key.as_str())
        .collect();
    assert!(
        keys.contains(&"on_submit") && keys.contains(&"validate"),
        "{keys:?}"
    );
}

#[test]
fn a_merged_field_on_an_unknown_doctype_is_invalid_meta() {
    let base = empty_site();
    let mut ours = empty_site();
    add_field(&mut ours, "ghost", "custom_x", None);
    let theirs = empty_site();

    let definitions: Vec<Definition> = vec![definition(), customer_definition()];
    let result = merge3_with(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
        &definitions,
    )
    .expect("merge");
    assert!(
        semantic_kinds(&result).contains(&SemanticKind::InvalidMeta),
        "conflicts: {:?}",
        result.conflicts
    );
}

#[test]
fn the_plain_merge_skips_the_definition_check() {
    let base = empty_site();
    let mut ours = empty_site();
    add_field(&mut ours, "ghost", "custom_x", None);
    let theirs = empty_site();

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert!(
        !semantic_kinds(&result).contains(&SemanticKind::InvalidMeta),
        "without definitions the check is skipped: {:?}",
        result.conflicts
    );
}

#[test]
fn a_clean_merge_has_no_semantic_conflicts() {
    let mut base = empty_site();
    add_field(&mut base, "sales_invoice", "custom_region", None);

    let mut ours = empty_site();
    add_field(&mut ours, "sales_invoice", "custom_region", None);
    set_label(&mut ours, "sales_invoice", "note", "Notes");

    let mut theirs = empty_site();
    add_field(&mut theirs, "sales_invoice", "custom_region", None);
    let mut spec = FieldSpec::new("custom_extra", FieldType::Text);
    spec.label = Some("Extra".to_owned());
    theirs
        .upsert_custom_field("sales_invoice", spec, None)
        .unwrap();

    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert!(result.is_clean(), "conflicts: {:?}", result.conflicts);
}
