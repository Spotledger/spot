//! The round-trip proof (WP-ALM-01): capture a site, apply to an empty one, capture again and
//! get the same bytes; plus the tamper, reorder, diff, unknown-section, bomb and path cases.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_custom_bundle::{
    Bundle, CaptureOptions, ChangeAction, Limits, SectionRegistry, SiteReader, SiteWriter, apply,
    capture, diff, plan_apply,
};

/// The one set of capture options both captures use, so the bundles can be compared byte for
/// byte (`created_at` is an argument, never read from the clock).
fn options() -> CaptureOptions {
    CaptureOptions::new("site-a", "1.0.0", "glm", "2026-01-01T00:00:00Z")
}

fn registry() -> SectionRegistry {
    SectionRegistry::new()
}

/// Captures the rich site and applies it to an empty one, returning both bundles.
fn round_trip() -> (spot_custom_bundle::Bundle, spot_custom_bundle::Bundle) {
    let a = rich_site();
    let bundle_a = capture(&a, &options()).expect("capture A");
    let mut b = empty_site();
    let plan = plan_apply(&bundle_a, &b, &registry()).expect("plan");
    assert!(!plan.is_empty(), "the plan must have work to do");
    apply(&plan, &mut b, &registry()).expect("apply");
    let bundle_b = capture(&b, &options()).expect("capture B");
    (bundle_a, bundle_b)
}

#[test]
fn round_trip_is_byte_identical() {
    let (bundle_a, bundle_b) = round_trip();
    assert_eq!(bundle_a.digest(), bundle_b.digest());
    assert_eq!(
        bundle_a.to_bytes(),
        bundle_b.to_bytes(),
        "capture(A) -> apply to empty B -> capture(B) must be byte-identical"
    );
}

#[test]
fn second_plan_is_empty() {
    let a = rich_site();
    let bundle = capture(&a, &options()).expect("capture");
    let mut b = empty_site();
    let first = plan_apply(&bundle, &b, &registry()).expect("first plan");
    apply(&first, &mut b, &registry()).expect("first apply");
    let second = plan_apply(&bundle, &b, &registry()).expect("second plan");
    assert!(
        second.is_empty(),
        "applying twice must leave nothing to do; got {:?}",
        second.changes()
    );
    // And the site is unchanged by the second plan.
    let after = capture(&b, &options()).expect("capture after");
    assert_eq!(after.digest(), bundle.digest());
}

#[test]
fn tampered_section_is_detected() {
    let a = rich_site();
    let bundle = capture(&a, &options()).expect("capture");
    let mut entries = entries_of(&bundle);
    let index = entries
        .iter()
        .position(|(name, _)| name.starts_with("sections/meta/"))
        .expect("a meta file");
    // Flip one byte, but keep the stale digest entry: open must see the mismatch.
    if let Some(byte) = entries[index].1.first_mut() {
        *byte ^= 0x01;
    } else {
        entries[index].1.push(b'x');
    }
    let bytes = raw_zip(&entries);
    let errors = Bundle::open(&bytes, &Limits::default(), &registry(), true)
        .expect_err("a tampered bundle must be refused");
    assert!(
        errors
            .iter()
            .any(|error| error.code() == "custom_bundle.digest_mismatch"),
        "expected a digest mismatch, got {errors:?}"
    );
}

#[test]
fn reordered_zip_entries_keep_the_digest() {
    let a = rich_site();
    let bundle = capture(&a, &options()).expect("capture");
    let mut entries = entries_of(&bundle);
    entries.reverse();
    let bytes = raw_zip(&entries);
    let reopened = Bundle::open(&bytes, &Limits::default(), &registry(), true)
        .expect("a reordered archive opens");
    assert_eq!(reopened.digest(), bundle.digest());
}

#[test]
fn removing_an_override_is_exactly_one_diff() {
    let a = rich_site();
    let bundle_a = capture(&a, &options()).expect("capture A");

    let mut a2 = rich_site();
    a2.clear_override("sales_invoice", Some("note"), "required")
        .expect("clear the override");
    let bundle_a2 = capture(&a2, &options()).expect("capture A2");

    let changes = diff(&bundle_a, &bundle_a2, &registry());
    assert_eq!(changes.len(), 1, "exactly the removed item: {changes:?}");
    assert_eq!(changes[0].section, "meta");
    assert_eq!(changes[0].action, ChangeAction::Remove);
    assert_eq!(changes[0].key, "sales_invoice.note.required");

    // And the other direction adds it back.
    let back = diff(&bundle_a2, &bundle_a, &registry());
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].action, ChangeAction::Add);
}

#[test]
fn unknown_section_is_refused_or_carried() {
    use std::collections::BTreeMap;

    let a = rich_site();
    let base = capture(&a, &options()).expect("capture");
    let mut sections = base.sections().clone();
    sections.insert(
        "workflows".to_owned(),
        spot_custom_bundle::SectionData {
            schema: 1,
            files: BTreeMap::from([("approval.toml".to_owned(), b"opaque bytes".to_vec())]),
        },
    );
    let with_unknown = Bundle::pack(base.header().clone(), sections, &Limits::default())
        .expect("pack with an unknown section");

    let refused = Bundle::open(
        &with_unknown.to_bytes(),
        &Limits::default(),
        &registry(),
        false,
    )
    .expect_err("without allow_unknown the kind is refused");
    assert!(
        refused
            .iter()
            .any(|error| error.code() == "custom_bundle.unknown_section")
    );

    let carried = Bundle::open(
        &with_unknown.to_bytes(),
        &Limits::default(),
        &registry(),
        true,
    )
    .expect("with allow_unknown the kind is carried");
    assert_eq!(
        carried.unknown_kinds().collect::<Vec<_>>(),
        vec!["workflows"]
    );
    // Re-packing keeps the opaque section's bytes exactly.
    let repacked = carried.repack().expect("repack");
    let again = Bundle::open(&repacked, &Limits::default(), &registry(), true).expect("reopen");
    assert_eq!(
        again
            .section("workflows")
            .expect("the section is carried")
            .files
            .get("approval.toml")
            .expect("the file is carried"),
        b"opaque bytes"
    );
}

#[test]
fn zip_bomb_is_refused() {
    let big = vec![0_u8; 1 << 20];
    let entries = vec![
        ("bundle.toml".to_owned(), b"format = 1".to_vec()),
        ("sections/meta/big.toml".to_owned(), big),
    ];
    let bytes = raw_zip(&entries);
    let limits = Limits {
        max_compressed_bytes: 1 << 20,
        max_total_bytes: 1 << 16,
        max_entries: 100,
        max_file_bytes: 4096,
    };
    let errors =
        Bundle::open(&bytes, &limits, &registry(), true).expect_err("a bomb must be refused");
    assert!(
        errors
            .iter()
            .any(|error| error.code() == "custom_bundle.too_large"),
        "expected too_large, got {errors:?}"
    );
}

#[test]
fn path_escape_is_refused() {
    let entries = vec![
        ("bundle.toml".to_owned(), b"format = 1".to_vec()),
        ("../escape.toml".to_owned(), b"x".to_vec()),
    ];
    let bytes = raw_zip(&entries);
    let errors = Bundle::open(&bytes, &Limits::default(), &registry(), true)
        .expect_err("an escaping name must be refused");
    assert!(
        errors
            .iter()
            .any(|error| error.code() == "custom_bundle.illegal_path"),
        "expected illegal_path, got {errors:?}"
    );
}

#[test]
fn rollback_restores_the_site() {
    let mut site = empty_site();
    let before = site.rules().len();
    site.begin().expect("begin");
    site.upsert_rule("temp", "doc.qty > 0").expect("upsert");
    assert_eq!(site.rules().len(), before + 1);
    site.rollback().expect("rollback");
    assert_eq!(site.rules().len(), before, "rollback restores the state");
}
