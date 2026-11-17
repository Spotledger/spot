#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Deletions travel (WP-SYN-02.T6): a tombstone is a record without image, size or references.
mod common;

use common::{Dest, Src, k, node};
use serde_json::{Value, json};
use spot_sync_closure::{
    BundleKind, Limits, NoRedaction, Redactor, Role, SourceImage, StaticReferences, build,
};

fn catalog() -> StaticReferences {
    StaticReferences::new().with_fixed_child("t", "rows", "to", "t")
}

fn dest() -> Dest {
    let mut dest = Dest::default();
    dest.scope_entities.insert("t".into());
    dest.acked = Some(100);
    dest
}

/// A redactor that panics: a tombstone must never reach it.
struct MustNotRedact;

impl Redactor for MustNotRedact {
    fn redact_down(&self, _entity: &str, _image: &mut Value) {
        panic!("a tombstone has nothing to redact");
    }
}

#[test]
fn a_tombstone_root_is_a_bundle_record_with_no_image_and_no_bytes() {
    let src = Src::default();
    let root = SourceImage::tombstone(k("t", "A"), 12);
    let plan = build(
        &[root],
        &[],
        &dest(),
        &src,
        &catalog(),
        &MustNotRedact,
        Limits::default(),
    )
    .unwrap();
    assert_eq!(plan.bundles.len(), 1);
    let bundle = &plan.bundles[0];
    assert_eq!(bundle.kind, BundleKind::Root);
    assert_eq!(bundle.bytes, 0);
    let record = &bundle.records[0];
    assert!(record.deleted);
    assert_eq!(record.role, Role::Root);
    assert_eq!(record.position, 12);
    assert_eq!(record.image, Value::Null);
}

#[test]
fn a_tombstone_satellite_keeps_its_role() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&[]), 20);
    let satellite = SourceImage::tombstone(k("t", "S"), 19);
    let plan = build(
        &[root],
        &[satellite],
        &dest(),
        &src,
        &catalog(),
        &NoRedaction,
        Limits::default(),
    )
    .unwrap();
    let roles: Vec<_> = plan.bundles[0]
        .records
        .iter()
        .map(|r| (r.key.id.as_str(), r.role, r.deleted))
        .collect();
    assert_eq!(
        roles,
        vec![("S", Role::Satellite, true), ("R", Role::Root, false)]
    );
}

#[test]
fn the_closure_of_a_root_never_adds_a_tombstone_as_an_extra() {
    let mut src = Src::default();
    // R refers to D; D was deleted. The source answers with a tombstone for D.
    let root = src.put("t", "R", node(&["D"]), 30);
    src.images
        .insert(k("t", "D"), SourceImage::tombstone(k("t", "D"), 25));
    let mut dest = dest();
    dest.acked = None; // doubt: an in-scope target would be included
    let plan = build(
        &[root],
        &[],
        &dest,
        &src,
        &catalog(),
        &NoRedaction,
        Limits::default(),
    )
    .unwrap();
    let all: Vec<_> = plan
        .bundles
        .iter()
        .flat_map(|b| &b.records)
        .map(|r| (r.key.id.as_str(), r.role))
        .collect();
    assert_eq!(all, vec![("R", Role::Root)]);
    assert!(
        plan.bundles
            .iter()
            .flat_map(|b| &b.records)
            .all(|r| !r.deleted)
    );
    assert!(plan.out_of_scope_extras.is_empty());
}

#[test]
fn a_tombstone_root_follows_no_reference_even_if_its_old_image_lists_some() {
    let mut src = Src::default();
    // The source still holds an image of the neighbour, which must not be pulled in.
    src.put("t", "N", node(&[]), 5);
    let mut root = SourceImage::tombstone(k("t", "A"), 40);
    // A stale image on a tombstone is ignored: the flag decides.
    root.image = node(&["N"]);
    let mut dest = dest();
    dest.acked = None;
    let plan = build(
        &[root],
        &[],
        &dest,
        &src,
        &catalog(),
        &NoRedaction,
        Limits::default(),
    )
    .unwrap();
    let ids: Vec<_> = plan.bundles[0]
        .records
        .iter()
        .map(|r| r.key.id.as_str())
        .collect();
    assert_eq!(ids, vec!["A"]);
    assert_eq!(plan.bundles[0].records[0].image, Value::Null);
}

#[test]
fn tombstones_count_as_records_but_not_as_bytes_against_the_bundle_bound() {
    let src = Src::default();
    let roots: Vec<SourceImage> = (0..4)
        .map(|i| SourceImage::tombstone(k("t", &format!("D{i}")), 50 + i))
        .collect();
    let limits = Limits {
        max_records: 3,
        max_bytes: 1,
        hard_limit_records: 10,
    };
    let plan = build(&roots, &[], &dest(), &src, &catalog(), &NoRedaction, limits).unwrap();
    // Four records exceed the record cap of three, but the roots never split: one root bundle.
    assert_eq!(plan.bundles.len(), 1);
    assert_eq!(plan.bundles[0].records.len(), 4);
    assert_eq!(plan.bundles[0].bytes, 0);
}

#[test]
fn a_tombstone_and_a_recreation_are_two_images_of_one_key_in_two_plans() {
    let mut src = Src::default();
    let recreated = src.put("t", "A", json!({ "rows": [] }), 60);
    let gone = SourceImage::tombstone(k("t", "A"), 55);
    let first = build(
        &[gone],
        &[],
        &dest(),
        &src,
        &catalog(),
        &NoRedaction,
        Limits::default(),
    )
    .unwrap();
    let second = build(
        &[recreated],
        &[],
        &dest(),
        &src,
        &catalog(),
        &NoRedaction,
        Limits::default(),
    )
    .unwrap();
    assert!(first.bundles[0].records[0].deleted);
    assert_eq!(first.bundles[0].records[0].position, 55);
    assert!(!second.bundles[0].records[0].deleted);
    assert_eq!(second.bundles[0].records[0].position, 60);
}
