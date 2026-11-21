//! The resolution API: one choice per conflict, all conflicts resolved, and the log.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bundle_merge::{Choice, Resolution, codes, merge3, resolve};
use spot_custom_bundle::SiteWriter;
use spot_meta::{FieldSpec, FieldType};

/// A base/ours/theirs triple that leaves exactly one `both_modified` conflict on
/// `sales_invoice.custom_region`.
fn conflicting() -> spot_bundle_merge::MergeResult {
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

    merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge")
}

#[test]
fn ours_takes_our_side_and_logs_it() {
    let result = conflicting();
    let id = result.conflicts[0].id.clone();
    let resolution = Resolution::new(&id, Choice::Ours, "glm").with_note("our change wins");

    let resolved = resolve(&result, &[resolution]).expect("resolve");
    let items = items_of(&resolved.bundle, "meta");
    assert_eq!(
        items.get("sales_invoice.custom_region").map(Vec::as_slice),
        result.conflicts[0].primary.ours.as_deref()
    );
    assert_eq!(resolved.log.len(), 1);
    let entry = &resolved.log.entries[0];
    assert_eq!(entry.conflict, id);
    assert_eq!(entry.choice, Choice::Ours);
    assert_eq!(entry.by, "glm");
    assert_eq!(entry.note.as_deref(), Some("our change wins"));
}

#[test]
fn theirs_takes_their_side() {
    let result = conflicting();
    let id = result.conflicts[0].id.clone();
    let resolved =
        resolve(&result, &[Resolution::new(&id, Choice::Theirs, "glm")]).expect("resolve");
    let items = items_of(&resolved.bundle, "meta");
    assert_eq!(
        items.get("sales_invoice.custom_region").map(Vec::as_slice),
        result.conflicts[0].primary.theirs.as_deref()
    );
}

#[test]
fn base_takes_the_ancestor() {
    let result = conflicting();
    let id = result.conflicts[0].id.clone();
    let resolved = resolve(&result, &[Resolution::new(&id, Choice::Base, "glm")]).expect("resolve");
    let items = items_of(&resolved.bundle, "meta");
    assert_eq!(
        items.get("sales_invoice.custom_region").map(Vec::as_slice),
        result.conflicts[0].primary.base.as_deref()
    );
}

#[test]
fn custom_replaces_the_primary_item() {
    let result = conflicting();
    let id = result.conflicts[0].id.clone();
    let mut spec = FieldSpec::new("custom_region", FieldType::Text);
    spec.label = Some("Chosen".to_owned());
    // The custom value is a full field item; take ours and edit it through a fresh site capture.
    let mut site = empty_site();
    site.upsert_custom_field("sales_invoice", spec, None)
        .unwrap();
    let chosen = items_of(&bundle(&site, "chosen"), "meta")
        .get("sales_invoice.custom_region")
        .cloned()
        .expect("the chosen item");
    let resolved = resolve(
        &result,
        &[Resolution::new(&id, Choice::Custom(chosen.clone()), "glm")],
    )
    .expect("resolve");
    let items = items_of(&resolved.bundle, "meta");
    assert_eq!(
        items.get("sales_invoice.custom_region").cloned(),
        Some(chosen)
    );
}

#[test]
fn an_unresolved_conflict_is_refused() {
    let result = conflicting();
    let error = resolve(&result, &[]).expect_err("a conflict is unresolved");
    assert_eq!(error.code(), codes::UNRESOLVED_CONFLICT);
    assert_eq!(error.item(), result.conflicts[0].id);
}

#[test]
fn an_unknown_conflict_id_is_refused() {
    let result = conflicting();
    let error = resolve(
        &result,
        &[Resolution::new(
            "meta:nope:both_modified",
            Choice::Ours,
            "glm",
        )],
    )
    .expect_err("no such conflict");
    assert_eq!(error.code(), codes::UNKNOWN_CONFLICT);
}

#[test]
fn a_duplicate_resolution_is_refused() {
    let result = conflicting();
    let id = result.conflicts[0].id.clone();
    let error = resolve(
        &result,
        &[
            Resolution::new(&id, Choice::Ours, "glm"),
            Resolution::new(&id, Choice::Theirs, "glm"),
        ],
    )
    .expect_err("resolved twice");
    assert_eq!(error.code(), codes::DUPLICATE_RESOLUTION);
}

#[test]
fn a_clean_merge_resolves_with_no_choices() {
    let base = empty_site();
    let mut ours = empty_site();
    add_field(&mut ours, "sales_invoice", "custom_region", None);
    let theirs = empty_site();
    let result = merge3(
        &bundle(&base, "base"),
        &bundle(&ours, "ours"),
        &bundle(&theirs, "theirs"),
    )
    .expect("merge");
    assert!(result.is_clean());
    let resolved = resolve(&result, &[]).expect("resolve");
    assert!(resolved.log.is_empty());
    assert_eq!(resolved.bundle.digest(), result.merged.digest());
}
