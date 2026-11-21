#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A held record never leaves through the closure (ADR-066 section 5): it is not an extra, and a
//! record that needs it is withheld with it.
mod common;

use common::{Dest, Src, k, node};
use serde_json::json;
use spot_sync_closure::{BundlePlan, Limits, NoRedaction, StaticReferences, build};

fn catalog() -> StaticReferences {
    StaticReferences::new().with_fixed_child("t", "rows", "to", "t")
}

fn plan_of(roots: &[spot_sync_closure::SourceImage], dest: &Dest, src: &Src) -> BundlePlan {
    build(
        roots,
        &[],
        dest,
        src,
        &catalog(),
        &NoRedaction,
        Limits::default(),
    )
    .unwrap()
}

/// Every record in every bundle, as ids.
fn queued(plan: &BundlePlan) -> Vec<String> {
    plan.bundles
        .iter()
        .flat_map(|b| b.records.iter().map(|r| r.key.id.clone()))
        .collect()
}

#[test]
fn a_root_whose_closure_needs_a_held_record_is_withheld_with_it() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["X"]), 3);
    src.put("t", "X", node(&[]), 2);
    let mut dest = Dest {
        acked: Some(10),
        ..Dest::default()
    };
    dest.held.insert(k("t", "X"));
    let plan = plan_of(&[root], &dest, &src);
    assert!(plan.bundles.is_empty(), "{:?}", queued(&plan));
    assert!(plan.out_of_scope_extras.is_empty());
    assert_eq!(plan.withheld_by_hold, vec![(k("t", "R"), k("t", "X"))]);
}

#[test]
fn a_held_record_is_never_an_extra_even_when_it_is_above_the_acked_position() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["X"]), 9);
    src.put("t", "X", node(&[]), 8);
    let mut dest = Dest {
        acked: Some(5),
        ..Dest::default()
    };
    // Everything up to position 5 is known to the destination; X (position 8) would be an extra,
    // except that it is held: the hold must not be bypassed.
    dest.scope_entities.insert("t".to_owned());
    dest.held.insert(k("t", "X"));
    let plan = plan_of(&[root], &dest, &src);
    assert!(plan.bundles.is_empty());
    assert_eq!(plan.withheld_by_hold, vec![(k("t", "R"), k("t", "X"))]);
}

#[test]
fn a_held_record_below_the_acked_position_is_still_not_an_extra() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["X"]), 9);
    src.put("t", "X", node(&[]), 2);
    let mut dest = Dest {
        acked: Some(10),
        ..Dest::default()
    };
    dest.scope_entities.insert("t".to_owned());
    dest.held.insert(k("t", "X"));
    let plan = plan_of(&[root], &dest, &src);
    assert!(plan.bundles.is_empty());
    assert_eq!(plan.withheld_by_hold, vec![(k("t", "R"), k("t", "X"))]);
}

#[test]
fn a_satellite_that_needs_a_held_record_is_withheld_and_the_others_still_go() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&[]), 5);
    let satellite = src.put("t", "S", node(&["X"]), 6);
    let other = src.put("t", "O", node(&[]), 7);
    src.put("t", "X", node(&[]), 2);
    let mut dest = Dest::default();
    dest.held.insert(k("t", "X"));
    let plan = build(
        &[root],
        &[satellite, other],
        &dest,
        &src,
        &catalog(),
        &NoRedaction,
        Limits::default(),
    )
    .unwrap();
    assert_eq!(queued(&plan), vec!["O", "R"]);
    assert_eq!(plan.withheld_by_hold, vec![(k("t", "S"), k("t", "X"))]);
}

#[test]
fn a_held_record_withholds_every_dependent_in_the_chain() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["M"]), 4);
    src.put("t", "M", node(&["X"]), 3);
    src.put("t", "X", node(&[]), 2);
    let mut dest = Dest::default();
    dest.held.insert(k("t", "X"));
    let plan = plan_of(&[root], &dest, &src);
    assert!(plan.bundles.is_empty());
    // The middle record is not held itself, but it needs the held one: both wait for X.
    assert_eq!(
        plan.withheld_by_hold,
        vec![(k("t", "M"), k("t", "X")), (k("t", "R"), k("t", "X"))]
    );
}

#[test]
fn a_held_record_does_not_withhold_a_root_that_does_not_need_it() {
    let mut src = Src::default();
    let needs = src.put("t", "R1", node(&["X"]), 4);
    let free = src.put("t", "R2", node(&["Y"]), 5);
    src.put("t", "X", node(&[]), 2);
    src.put("t", "Y", node(&[]), 3);
    let mut dest = Dest::default();
    dest.held.insert(k("t", "X"));
    let plan = plan_of(&[needs, free], &dest, &src);
    assert_eq!(queued(&plan), vec!["Y", "R2"]);
    assert_eq!(plan.withheld_by_hold, vec![(k("t", "R1"), k("t", "X"))]);
}

#[test]
fn a_root_the_destination_cannot_decode_is_not_sent_either() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&[]), 4);
    let mut dest = Dest::default();
    dest.held.insert(k("t", "R"));
    let plan = plan_of(&[root], &dest, &src);
    assert!(plan.bundles.is_empty());
    // A held root blocks nothing else: it is simply not sent.
    assert!(plan.withheld_by_hold.is_empty());
}

#[test]
fn a_held_extra_that_nothing_needs_keeps_the_rest_of_the_closure() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["Y"]), 5);
    src.put("t", "Y", node(&["X"]), 4);
    src.put("t", "X", node(&[]), 3);
    let mut dest = Dest::default();
    dest.held.insert(k("t", "X"));
    let plan = plan_of(&[root], &dest, &src);
    // Y needs X, so nothing of this closure may go: the bundle would fail at the destination.
    assert!(plan.bundles.is_empty());
    assert_eq!(
        plan.withheld_by_hold,
        vec![(k("t", "R"), k("t", "X")), (k("t", "Y"), k("t", "X"))]
    );
}

#[test]
fn a_cycle_with_a_held_member_withholds_the_whole_cycle() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["A"]), 5);
    src.put(
        "t",
        "A",
        json!({ "rows": [{ "to": "R" }, { "to": "X" }] }),
        4,
    );
    src.put("t", "X", node(&[]), 3);
    let mut dest = Dest::default();
    dest.held.insert(k("t", "X"));
    let plan = plan_of(&[root], &dest, &src);
    assert!(plan.bundles.is_empty());
    assert_eq!(
        plan.withheld_by_hold,
        vec![(k("t", "A"), k("t", "X")), (k("t", "R"), k("t", "X"))]
    );
}

#[test]
fn the_code_is_in_the_catalogue() {
    let toml = include_str!("../../spot-errors/catalogue/sync_closure.toml");
    assert!(
        toml.contains(&format!(
            "code = \"{}\"",
            spot_sync_closure::HELD_DEPENDENCY
        )),
        "{}",
        spot_sync_closure::HELD_DEPENDENCY
    );
}
