//! The reviewable text report.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bundle_merge::merge3;
use spot_custom_bundle::SiteWriter;
use spot_meta::{FieldSpec, FieldType};

#[test]
fn the_report_lists_every_conflict_with_its_three_versions() {
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
    let text = result.render_text();

    assert!(text.contains("both_modified"), "{text}");
    assert!(text.contains("sales_invoice.custom_region"), "{text}");
    assert!(text.contains("base "), "{text}");
    assert!(text.contains("ours "), "{text}");
    assert!(text.contains("theirs "), "{text}");
}

#[test]
fn a_clean_merge_reports_that_it_is_clean() {
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
    let text = result.render_text();
    assert!(text.contains("no conflicts"), "{text}");
}
