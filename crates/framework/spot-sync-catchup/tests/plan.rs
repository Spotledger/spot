#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The plan: the mandatory lane first, then the bulk in the order the mode calls for
//! (ADR-066 section 2).

mod common;

use common::{MapReferences, audit, domain, rid, state, topics};
use spot_sync_catchup::{
    BundleKind, CatchUpPlan, ClassCounts, GapSummary, Mode, Settings, Share, plan, scope_for_node,
};

const NODE: &str = "branch-1";

/// A gap over the incremental limit whose coalesced plan is small enough to stay coalesced.
fn coalescing_gap() -> GapSummary {
    GapSummary {
        applied_prefix: 100,
        stream_end: 200,
        hot_retention_floor: 50,
        in_scope_event_count: 20_000,
        class_counts: ClassCounts {
            state: 20_000,
            domain: 0,
            audit: 0,
        },
        estimated_coalesced_records: 500,
        estimated_coalesced_bytes: 10_000,
        estimated_snapshot_bytes: 10_000_000,
        node_is_new_or_resync: false,
    }
}

/// A gap small enough to replay in position order.
fn incremental_gap() -> GapSummary {
    GapSummary {
        applied_prefix: 100,
        stream_end: 200,
        hot_retention_floor: 50,
        in_scope_event_count: 4,
        class_counts: ClassCounts {
            state: 4,
            domain: 0,
            audit: 0,
        },
        estimated_coalesced_records: 3,
        estimated_coalesced_bytes: 300,
        estimated_snapshot_bytes: 100_000,
        node_is_new_or_resync: false,
    }
}

/// A node that must be snapshotted.
fn snapshot_gap() -> GapSummary {
    GapSummary {
        node_is_new_or_resync: true,
        ..incremental_gap()
    }
}

/// Every record the plan carries, in the order it would be applied: the lane, then the bulk.
fn applied_order(catch_up: &CatchUpPlan) -> Vec<String> {
    let mut order: Vec<String> = catch_up
        .mandatory_lane
        .iter()
        .map(|event| format!("lane@{}", event.position))
        .collect();
    for bundle in &catch_up.bulk {
        for record in &bundle.records {
            order.push(record.id.clone());
        }
        for event in &bundle.events {
            if event.subject.is_none() {
                order.push(format!("fact@{}", event.position));
            }
        }
    }
    order
}

#[test]
fn the_mandatory_lane_comes_first_and_is_never_coalesced_away() {
    // The mandatory state event at 101 is superseded by the image at 150, and still goes
    // individually: it is a tax change, and it must be applied in the first moments of the
    // session whatever the backlog is.
    let events = vec![
        state(101, "tax", 1).mandatory(),
        state(150, "tax", 2),
        state(151, "invoice", 1),
    ];
    let catch_up = plan(
        &scope_for_node(&events, NODE, &topics()),
        &coalescing_gap(),
        &Settings::default(),
        &topics(),
        &MapReferences::new().link("invoice", "tax"),
    )
    .unwrap();
    assert_eq!(catch_up.mode, Mode::Coalesced);
    assert_eq!(catch_up.mandatory_lane.len(), 1);
    assert_eq!(catch_up.mandatory_lane[0].position, 101);
    assert_eq!(applied_order(&catch_up), vec!["lane@101", "tax", "invoice"]);
}

#[test]
fn coalesced_bulk_is_in_dependency_order_not_position_order() {
    // The invoice's image is newer than the party it references, so position order would be
    // wrong; the plan puts the party's group first.
    let events = vec![state(105, "invoice", 1), state(109, "party", 1)];
    let catch_up = plan(
        &scope_for_node(&events, NODE, &topics()),
        &coalescing_gap(),
        &Settings::default(),
        &topics(),
        &MapReferences::new().link("invoice", "party"),
    )
    .unwrap();
    assert_eq!(catch_up.bulk.len(), 2);
    assert_eq!(catch_up.bulk[0].kind, BundleKind::CoalescedImages);
    assert_eq!(catch_up.bulk[0].records, vec![rid("party")]);
    assert_eq!(catch_up.bulk[1].records, vec![rid("invoice")]);
}

#[test]
fn coalesced_facts_follow_the_images_in_position_order() {
    let events = vec![
        state(101, "a", 1),
        domain(102, "a", 1),
        audit(103, Some(NODE)),
        audit(104, Some("branch-2")),
    ];
    let catch_up = plan(
        &scope_for_node(&events, NODE, &topics()),
        &coalescing_gap(),
        &Settings::default(),
        &topics(),
        &MapReferences::new(),
    )
    .unwrap();
    assert_eq!(catch_up.bulk.len(), 2);
    assert_eq!(catch_up.bulk[1].kind, BundleKind::CoalescedFacts);
    let positions: Vec<u64> = catch_up.bulk[1].events.iter().map(|e| e.position).collect();
    assert_eq!(positions, vec![102, 103]);
}

#[test]
fn incremental_bulk_keeps_position_order_and_respects_the_bundle_cap() {
    let events = vec![
        state(101, "a", 1),
        state(102, "b", 1),
        state(103, "c", 1),
        state(104, "d", 1),
        state(105, "e", 1),
    ];
    let settings = Settings {
        bundle_records: 2,
        ..Settings::default()
    };
    let catch_up = plan(
        &scope_for_node(&events, NODE, &topics()),
        &incremental_gap(),
        &settings,
        &topics(),
        &MapReferences::new().link("e", "a"),
    )
    .unwrap();
    assert_eq!(catch_up.mode, Mode::Incremental);
    // Position order even though `e` references `a`: an incremental replay follows the stream,
    // which is already in causal order (ADR-053).
    let sizes: Vec<usize> = catch_up.bulk.iter().map(|b| b.events.len()).collect();
    assert_eq!(sizes, vec![2, 2, 1]);
    assert!(
        catch_up
            .bulk
            .iter()
            .all(|b| b.kind == BundleKind::Incremental)
    );
    let positions: Vec<u64> = catch_up
        .bulk
        .iter()
        .flat_map(|b| b.events.iter().map(|e| e.position))
        .collect();
    assert_eq!(positions, vec![101, 102, 103, 104, 105]);
}

#[test]
fn a_snapshot_plan_is_one_snapshot_bundle_and_the_lane_still_comes_first() {
    let events = vec![state(101, "a", 1).mandatory(), state(102, "b", 1)];
    let summary = snapshot_gap();
    let catch_up = plan(
        &scope_for_node(&events, NODE, &topics()),
        &summary,
        &Settings::default(),
        &topics(),
        &MapReferences::new(),
    )
    .unwrap();
    assert_eq!(catch_up.mode, Mode::Snapshot);
    assert_eq!(catch_up.mandatory_lane.len(), 1);
    assert_eq!(catch_up.bulk.len(), 1);
    assert_eq!(catch_up.bulk[0].kind, BundleKind::Snapshot);
    assert!(catch_up.bulk[0].events.is_empty());
    assert_eq!(catch_up.bulk[0].bytes, summary.estimated_snapshot_bytes);
}

#[test]
fn events_already_applied_or_past_the_stream_end_are_not_planned() {
    let events = vec![
        state(99, "applied", 1),
        state(100, "prefix", 1),
        state(150, "in-gap", 1),
        state(201, "beyond-end", 1),
    ];
    let catch_up = plan(
        &scope_for_node(&events, NODE, &topics()),
        &incremental_gap(),
        &Settings::default(),
        &topics(),
        &MapReferences::new(),
    )
    .unwrap();
    let records: Vec<String> = catch_up
        .bulk
        .iter()
        .flat_map(|b| b.records.iter().map(|r| r.id.clone()))
        .collect();
    assert_eq!(records, vec!["in-gap"]);
}

#[test]
fn the_reasons_travel_with_the_plan() {
    let catch_up = plan(
        &scope_for_node(&[state(101, "a", 1)], NODE, &topics()),
        &coalescing_gap(),
        &Settings::default(),
        &topics(),
        &MapReferences::new(),
    )
    .unwrap();
    assert_eq!(catch_up.mode, Mode::Coalesced);
    assert_eq!(catch_up.reasons.len(), 1);
}

#[test]
fn an_unknown_topic_in_the_bulk_is_refused() {
    let mut unknown = state(101, "a", 1);
    unknown.topic = "made_up_topic".to_owned();
    assert_eq!(
        plan(
            &scope_for_node(&[unknown], NODE, &topics()),
            &coalescing_gap(),
            &Settings::default(),
            &topics(),
            &MapReferences::new(),
        )
        .unwrap_err()
        .code(),
        "sync_catchup.unknown_topic"
    );
}

#[test]
fn the_default_settings_are_the_adrs_starting_values() {
    let settings = Settings::default();
    assert_eq!(settings.incremental_limit, 10_000);
    assert_eq!(settings.snapshot_fraction, Share::from_percent(40).unwrap());
    assert_eq!(settings.snapshot_fraction.basis_points(), 4_000);
    assert_eq!(settings.bundle_records, 500);
}
