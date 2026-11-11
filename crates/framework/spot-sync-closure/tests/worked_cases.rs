#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The worked cases of ADR-064 section 3 and the bounds of section 4.
mod common;

use common::{Dest, Src, k, node};
use serde_json::json;
use spot_sync_closure::{
    BundleKind, ClosureError, ExtrasSet, Limits, NoRedaction, Role, StaticRedactor,
    StaticReferences, build,
};

fn catalog() -> StaticReferences {
    StaticReferences::new()
        .with_fixed_child("t", "rows", "to", "t")
        .with_fixed("transfer", "warehouse", "warehouse")
        .with_fixed("master", "other", "master")
}

fn keys(plan: &spot_sync_closure::BundlePlan, i: usize) -> Vec<String> {
    plan.bundles[i]
        .records
        .iter()
        .map(|r| r.key.id.clone())
        .collect()
}

#[test]
fn transfer_referencing_another_branchs_record_adds_it_as_an_extra() {
    let mut src = Src::default();
    let root = src.put("transfer", "T1", json!({ "warehouse": "W-A" }), 10);
    src.put("warehouse", "W-A", json!({ "name": "A" }), 3);
    let mut dest = Dest::default();
    dest.scope_entities.insert("transfer".into()); // the warehouse is out of scope for branch B
    dest.acked = Some(100);
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
    assert_eq!(plan.bundles.len(), 1);
    let roles: Vec<_> = plan.bundles[0]
        .records
        .iter()
        .map(|r| (r.key.id.as_str(), r.role))
        .collect();
    assert_eq!(roles, vec![("W-A", Role::Extra), ("T1", Role::Root)]);
    assert_eq!(plan.out_of_scope_extras, vec![k("warehouse", "W-A")]);
}

#[test]
fn hub_master_referencing_a_master_in_another_company_follows_the_chain() {
    let mut src = Src::default();
    let root = src.put("master", "M1", json!({ "other": "M2" }), 5);
    src.put("master", "M2", json!({ "other": "M3" }), 2);
    src.put("master", "M3", json!({}), 1);
    let dest = Dest::default(); // knows nothing
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
    assert_eq!(
        keys(&plan, 0),
        vec!["M3", "M2", "M1"],
        "targets come before their referrers"
    );
}

#[test]
fn a_cycle_terminates_and_each_record_is_added_once() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["A"]), 9);
    src.put("t", "A", node(&["B"]), 4);
    src.put("t", "B", node(&["A", "R"]), 4);
    let plan = build(
        &[root],
        &[],
        &Dest::default(),
        &src,
        &catalog(),
        &NoRedaction,
        Limits::default(),
    )
    .unwrap();
    let mut ids = keys(&plan, 0);
    ids.sort();
    assert_eq!(ids, vec!["A", "B", "R"]);
}

#[test]
fn target_at_or_before_the_acked_position_is_skipped_newer_is_included() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["OLD", "NEW"]), 50);
    src.put("t", "OLD", node(&[]), 20);
    src.put("t", "NEW", node(&[]), 31);
    let mut dest = Dest::default();
    dest.scope_entities.insert("t".into());
    dest.acked = Some(30);
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
    assert_eq!(keys(&plan, 0), vec!["NEW", "R"]);
    assert!(
        plan.out_of_scope_extras.is_empty(),
        "NEW is in scope, so it is not an out-of-scope extra"
    );
}

#[test]
fn unknown_acked_position_includes_the_target() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["OLD"]), 50);
    src.put("t", "OLD", node(&[]), 1);
    let mut dest = Dest::default();
    dest.scope_entities.insert("t".into());
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
    assert_eq!(keys(&plan, 0), vec!["OLD", "R"]);
}

#[test]
fn owned_and_extras_set_targets_are_skipped_and_satellites_are_not_extras() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["OWN", "EX", "SAT"]), 50);
    let sat = src.put("t", "SAT", node(&[]), 50);
    let mut dest = Dest::default();
    dest.owned.insert(k("t", "OWN"));
    let mut extras = ExtrasSet::new();
    extras.record_delivered_extra(k("t", "EX"));
    dest.extras = extras;
    let plan = build(
        &[root],
        &[sat],
        &dest,
        &src,
        &catalog(),
        &NoRedaction,
        Limits::default(),
    )
    .unwrap();
    assert_eq!(keys(&plan, 0), vec!["SAT", "R"]);
    assert_eq!(plan.bundles[0].records[0].role, Role::Satellite);
}

#[test]
fn up_direction_adds_nothing_for_hub_owned_and_mirrored_targets() {
    // An edge sending up: the hub owns the masters, and a mirror came from the hub.
    let mut src = Src::default();
    let root = src.put("t", "INV", node(&["MASTER", "MIRROR"]), 70);
    src.put("t", "MASTER", node(&[]), 1);
    src.put("t", "MIRROR", node(&[]), 2);
    let mut dest = Dest::default();
    dest.owned.insert(k("t", "MASTER"));
    dest.owned.insert(k("t", "MIRROR"));
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
    assert_eq!(keys(&plan, 0), vec!["INV"]);
}

#[test]
fn closure_over_the_cap_becomes_three_prerequisite_bundles_in_dependency_order() {
    // chain R -> E6 -> ... -> E1, cap of 2 records per bundle.
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["E6"]), 99);
    for i in 1..=6 {
        let next: Vec<String> = if i > 1 {
            vec![format!("E{}", i - 1)]
        } else {
            vec![]
        };
        let next: Vec<&str> = next.iter().map(String::as_str).collect();
        src.put("t", &format!("E{i}"), node(&next), 1);
    }
    let limits = Limits {
        max_records: 2,
        ..Limits::default()
    };
    let plan = build(
        &[root],
        &[],
        &Dest::default(),
        &src,
        &catalog(),
        &NoRedaction,
        limits,
    )
    .unwrap();
    assert_eq!(plan.bundles.len(), 4);
    assert_eq!(keys(&plan, 0), vec!["E1", "E2"]);
    assert_eq!(keys(&plan, 1), vec!["E3", "E4"]);
    assert_eq!(keys(&plan, 2), vec!["E5", "E6"]);
    assert_eq!(keys(&plan, 3), vec!["R"]);
    assert_eq!(plan.bundles[0].kind, BundleKind::Prerequisite);
    assert_eq!(plan.bundles[3].kind, BundleKind::Root);
}

#[test]
fn a_cycle_larger_than_the_cap_stays_in_one_bundle() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["A"]), 9);
    src.put("t", "A", node(&["B"]), 1);
    src.put("t", "B", node(&["C"]), 1);
    src.put("t", "C", node(&["A"]), 1);
    let limits = Limits {
        max_records: 2,
        ..Limits::default()
    };
    let plan = build(
        &[root],
        &[],
        &Dest::default(),
        &src,
        &catalog(),
        &NoRedaction,
        limits,
    )
    .unwrap();
    assert_eq!(plan.bundles.len(), 2);
    assert_eq!(plan.bundles[0].records.len(), 3);
}

#[test]
fn the_byte_cap_splits_too() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["A", "B"]), 9);
    src.put("t", "A", node(&[]), 1);
    src.put("t", "B", node(&[]), 1);
    let one = node(&[]).to_string().len();
    let limits = Limits {
        max_bytes: one + 1,
        ..Limits::default()
    };
    let plan = build(
        &[root],
        &[],
        &Dest::default(),
        &src,
        &catalog(),
        &NoRedaction,
        limits,
    )
    .unwrap();
    assert_eq!(plan.bundles.len(), 3);
}

#[test]
fn over_the_hard_limit_is_too_large() {
    let mut src = Src::default();
    let ids: Vec<String> = (0..20).map(|i| format!("E{i}")).collect();
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let root = src.put("t", "R", node(&id_refs), 9);
    for id in &ids {
        src.put("t", id, node(&[]), 1);
    }
    let limits = Limits {
        max_records: 5,
        hard_limit_records: 10,
        ..Limits::default()
    };
    let err = build(
        &[root],
        &[],
        &Dest::default(),
        &src,
        &catalog(),
        &NoRedaction,
        limits,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ClosureError::TooLarge {
                count: 11,
                limit: 10
            }
        ),
        "{err:?}"
    );
    assert_eq!(err.code(), "sync_closure.too_large");
}

#[test]
fn a_target_nobody_has_is_an_error_not_a_silent_skip() {
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["GONE"]), 9);
    let err = build(
        &[root],
        &[],
        &Dest::default(),
        &src,
        &catalog(),
        &NoRedaction,
        Limits::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ClosureError::TargetMissing { .. }));
}

#[test]
fn bad_limits_are_refused() {
    let err = build(
        &[],
        &[],
        &Dest::default(),
        &Src::default(),
        &catalog(),
        &NoRedaction,
        Limits {
            max_records: 0,
            ..Limits::default()
        },
    );
    assert!(matches!(err, Err(ClosureError::InvalidLimits { .. })));
    let err = build(
        &[],
        &[],
        &Dest::default(),
        &Src::default(),
        &catalog(),
        &NoRedaction,
        Limits {
            hard_limit_records: 1,
            ..Limits::default()
        },
    );
    assert!(matches!(err, Err(ClosureError::InvalidLimits { .. })));
}

#[test]
fn excluded_fields_are_removed_from_every_image_but_references_are_still_followed() {
    let mut src = Src::default();
    let root = src.put(
        "transfer",
        "T1",
        json!({ "warehouse": "W", "secret_note": "no" }),
        5,
    );
    src.put("warehouse", "W", json!({ "cost": 5, "name": "w" }), 1);
    let redactor = StaticRedactor::new()
        .excluding("transfer", "secret_note")
        .excluding("warehouse", "cost");
    let plan = build(
        &[root],
        &[],
        &Dest::default(),
        &src,
        &catalog(),
        &redactor,
        Limits::default(),
    )
    .unwrap();
    let recs = &plan.bundles[0].records;
    assert_eq!(recs[0].image, json!({ "name": "w" }));
    assert_eq!(recs[1].image, json!({ "warehouse": "W" }));
}

#[test]
fn dynamic_links_read_the_type_from_the_record_and_reject_undeclared_targets() {
    let refs =
        StaticReferences::new().with_dynamic("note", "ref_id", "ref_type", &["invoice", "order"]);
    let mut src = Src::default();
    let root = src.put(
        "note",
        "N",
        json!({ "ref_type": "order", "ref_id": "O1" }),
        5,
    );
    src.put("order", "O1", json!({}), 1);
    let plan = build(
        &[root],
        &[],
        &Dest::default(),
        &src,
        &refs,
        &NoRedaction,
        Limits::default(),
    )
    .unwrap();
    assert_eq!(keys(&plan, 0), vec!["O1", "N"]);

    let bad = src.put(
        "note",
        "N2",
        json!({ "ref_type": "payroll", "ref_id": "P" }),
        5,
    );
    let err = build(
        &[bad],
        &[],
        &Dest::default(),
        &src,
        &refs,
        &NoRedaction,
        Limits::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ClosureError::ReferenceUnreadable { .. }));
}

#[test]
fn text_that_looks_like_a_name_is_not_a_reference() {
    let mut src = Src::default();
    let root = src.put(
        "t",
        "R",
        json!({ "note": "see t X", "rows": [{ "memo": "X", "to": "" }] }),
        5,
    );
    src.put("t", "X", json!({}), 1);
    let plan = build(
        &[root],
        &[],
        &Dest::default(),
        &src,
        &catalog(),
        &NoRedaction,
        Limits::default(),
    )
    .unwrap();
    assert_eq!(keys(&plan, 0), vec!["R"]);
}

#[test]
fn extras_set_bookkeeping() {
    let mut s = ExtrasSet::new();
    assert!(s.record_delivered_extra(k("t", "A")));
    assert!(!s.record_delivered_extra(k("t", "A")));
    s.record_delivered_extra(k("t", "B"));
    assert_eq!(s.size(), 2);
    assert!(s.contains(&k("t", "A")) && s.routed_later_versions(&k("t", "A")));
    assert!(!s.routed_later_versions(&k("t", "Z")));
    assert!(s.growth(2).is_none());
    let g = s.growth(1).unwrap();
    assert_eq!((g.size, g.threshold), (2, 1));
}

#[test]
fn an_extra_that_references_a_root_travels_in_the_root_bundle() {
    // R -> E -> R is a cycle through the root, so E cannot be sent before R; the other extra can.
    let mut src = Src::default();
    let root = src.put("t", "R", node(&["E", "P"]), 9);
    src.put("t", "E", node(&["R"]), 1);
    src.put("t", "P", node(&[]), 1);
    let limits = Limits {
        max_records: 2,
        ..Limits::default()
    };
    let plan = build(
        &[root],
        &[],
        &Dest::default(),
        &src,
        &catalog(),
        &NoRedaction,
        limits,
    )
    .unwrap();
    assert_eq!(plan.bundles.len(), 2);
    assert_eq!(keys(&plan, 0), vec!["P"]);
    assert_eq!(keys(&plan, 1), vec!["E", "R"]);
}
