#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Scoped audit lists (ADR-066 section 2; WP-SYN-07.T2): what is in, what is absent, and that
//! absence leaves no trace.

mod common;

use common::{MapReferences, MapScope, audit_of, domain, rid, state, topics};
use spot_sync_catchup::{
    AuditDelivery, AuditedPlan, ClassCounts, Event, GapSummary, Mode, ModeReason, Settings,
    plan_with_audit_options,
};

const NODE: &str = "branch-1";

fn gap(event_count: u64, audit: u64) -> GapSummary {
    GapSummary {
        applied_prefix: 100,
        stream_end: 200,
        hot_retention_floor: 50,
        in_scope_event_count: event_count,
        class_counts: ClassCounts {
            state: event_count - audit,
            domain: 0,
            audit,
        },
        estimated_coalesced_records: 3,
        estimated_coalesced_bytes: 300,
        estimated_snapshot_bytes: 100_000,
        node_is_new_or_resync: false,
    }
}

/// These tests pin the pre-T6 rule, reachable as the explicit legacy option; the default and the
/// truth table are in `tests/delivery.rs`.
fn run(events: &[Event], summary: &GapSummary, scope: &MapScope) -> AuditedPlan {
    plan_with_audit_options(
        events,
        summary,
        &Settings::default(),
        &topics(),
        &MapReferences::new(),
        scope,
        NODE,
        AuditDelivery::LEGACY,
    )
    .unwrap()
}

fn positions(events: &[Event]) -> Vec<u64> {
    events.iter().map(|event| event.position).collect()
}

#[test]
fn an_audit_event_is_included_iff_its_subject_is_in_scope() {
    let events = vec![
        audit_of(110, "mine"),
        audit_of(111, "theirs"),
        audit_of(112, "mine"),
    ];
    let out = run(&events, &gap(3, 3), &MapScope::holding(&["mine"]));
    assert_eq!(positions(&out.plan.audit), vec![110, 112]);
    assert_eq!(out.counts.visible, 2);
    assert_eq!(out.counts.total, 3);
}

#[test]
fn an_audit_event_covered_by_reference_is_included_even_without_a_subject() {
    let linked = Event::new(
        120,
        "sys_sync_missing",
        spot_sync_catchup::Payload::new("{}"),
    );
    let unlinked = Event::new(
        121,
        "sys_sync_missing",
        spot_sync_catchup::Payload::new("{}"),
    );
    let out = run(
        &[linked, unlinked],
        &gap(2, 2),
        &MapScope::holding(&[]).covering(120),
    );
    assert_eq!(positions(&out.plan.audit), vec![120]);
}

#[test]
fn an_audit_event_addressed_to_another_node_is_never_delivered_even_when_in_scope() {
    // ADR-066 section 2: no broadcast. Scope is necessary, and a foreign addressee excludes.
    let events = vec![
        audit_of(110, "mine").with_addressee("branch-2"),
        audit_of(111, "theirs").with_addressee(NODE),
    ];
    let out = run(&events, &gap(2, 2), &MapScope::holding(&["mine"]));
    assert!(out.plan.audit.is_empty());
}

#[test]
fn an_in_scope_audit_event_addressed_to_this_node_is_delivered() {
    let events = vec![audit_of(110, "mine").with_addressee(NODE)];
    let out = run(&events, &gap(1, 1), &MapScope::holding(&["mine"]));
    assert_eq!(positions(&out.plan.audit), vec![110]);
}

#[test]
fn included_audit_events_are_in_strict_position_order_and_never_coalesced() {
    // Unsorted input, the same subject and the same topic three times: all three stay.
    let events = vec![audit_of(150, "a"), audit_of(120, "a"), audit_of(130, "a")];
    let out = run(&events, &gap(3, 3), &MapScope::holding(&["a"]));
    assert_eq!(positions(&out.plan.audit), vec![120, 130, 150]);
}

#[test]
fn a_record_whose_images_were_coalesced_keeps_all_its_in_scope_audit_events() {
    let events = vec![
        state(110, "a", 1),
        audit_of(111, "a"),
        state(112, "a", 2),
        audit_of(113, "a"),
        state(114, "a", 3),
    ];
    let mut summary = gap(20_000, 2);
    summary.class_counts.state = 19_998;
    let out = run(&events, &summary, &MapScope::holding(&["a"]));
    assert_eq!(out.plan.mode, Mode::Coalesced);
    let images: usize = out.plan.bulk.iter().map(|b| b.events.len()).sum();
    assert_eq!(images, 1, "three state images collapse to one");
    assert_eq!(positions(&out.plan.audit), vec![111, 113]);
}

#[test]
fn audit_events_outside_the_gap_are_not_planned() {
    let events = vec![audit_of(100, "a"), audit_of(150, "a"), audit_of(201, "a")];
    let out = run(&events, &gap(1, 1), &MapScope::holding(&["a"]));
    assert_eq!(positions(&out.plan.audit), vec![150]);
    assert_eq!(out.counts.total, 1);
}

#[test]
fn a_mandatory_audit_event_goes_to_the_lane_and_a_normal_one_to_the_audit_list() {
    let events = vec![
        state(101, "tax", 1).mandatory(),
        audit_of(105, "a").mandatory(),
        audit_of(106, "a"),
        audit_of(107, "theirs").mandatory(),
    ];
    let out = run(&events, &gap(4, 3), &MapScope::holding(&["a", "tax"]));
    assert_eq!(positions(&out.plan.mandatory_lane), vec![101, 105]);
    assert_eq!(positions(&out.plan.audit), vec![106]);
}

#[test]
fn a_non_mandatory_audit_event_is_never_in_the_lane() {
    let out = run(
        &[audit_of(110, "a")],
        &gap(1, 1),
        &MapScope::holding(&["a"]),
    );
    assert!(out.plan.mandatory_lane.is_empty());
    assert_eq!(positions(&out.plan.audit), vec![110]);
}

#[test]
fn audit_events_never_reach_the_bulk() {
    // Incremental mode replays the gap's events; the audit ones travel only in the audit list.
    let events = vec![
        state(110, "a", 1),
        audit_of(111, "a"),
        audit_of(112, "theirs"),
    ];
    let out = run(&events, &gap(3, 2), &MapScope::holding(&["a"]));
    assert_eq!(out.plan.mode, Mode::Incremental);
    let bulk: Vec<u64> = out
        .plan
        .bulk
        .iter()
        .flat_map(|b| b.events.iter().map(|e| e.position))
        .collect();
    assert_eq!(bulk, vec![110]);
    assert_eq!(positions(&out.plan.audit), vec![111]);
}

#[test]
fn a_snapshot_plan_still_carries_the_in_scope_audit_list() {
    let mut summary = gap(2, 2);
    summary.node_is_new_or_resync = true;
    let events = vec![audit_of(110, "a"), audit_of(111, "theirs")];
    let out = run(&events, &summary, &MapScope::holding(&["a"]));
    assert_eq!(out.plan.mode, Mode::Snapshot);
    assert_eq!(positions(&out.plan.audit), vec![110]);
}

#[test]
fn the_mode_cost_counts_visible_audit_events_only() {
    // The caller's summary claims 5,000 audit events; only 2 are visible. The incremental
    // limit is 10,000 by default, so the visible count keeps the gap incremental and the
    // reason reports the visible number.
    let mut summary = gap(10_001, 5_000);
    summary.class_counts.state = 5_001;
    let events = vec![audit_of(110, "a"), audit_of(111, "a")];
    let out = run(&events, &summary, &MapScope::holding(&["a"]));
    assert_eq!(out.plan.mode, Mode::Incremental);
    assert!(matches!(
        out.plan.reasons[0],
        ModeReason::SmallGap {
            in_scope_event_count: 5_003,
            ..
        }
    ));
}

#[test]
fn a_mixed_gap_has_the_golden_plan() {
    let events = vec![
        state(101, "tax", 1).mandatory(),
        audit_of(102, "tax").mandatory(),
        state(103, "inv", 1),
        domain(104, "inv", 1),
        audit_of(105, "inv"),
        audit_of(106, "secret"),
        state(107, "inv", 2),
        audit_of(108, "inv"),
    ];
    let out = run(&events, &gap(8, 4), &MapScope::holding(&["tax", "inv"]));
    assert_eq!(out.plan.mode, Mode::Incremental);
    assert_eq!(positions(&out.plan.mandatory_lane), vec![101, 102]);
    assert_eq!(positions(&out.plan.audit), vec![105, 108]);
    let bulk: Vec<u64> = out
        .plan
        .bulk
        .iter()
        .flat_map(|b| b.events.iter().map(|e| e.position))
        .collect();
    assert_eq!(bulk, vec![103, 104, 107]);
    assert_eq!(out.counts.visible, 3);
    assert_eq!(out.counts.total, 4);
}

#[test]
fn the_destination_facing_form_has_only_the_visible_count() {
    let events = vec![audit_of(110, "a"), audit_of(111, "theirs")];
    let out = run(&events, &gap(2, 2), &MapScope::holding(&["a"]));
    let text = format!("{:?}", out.counts.destination_facing());
    assert_eq!(text, "VisibleAuditCount { visible: 1 }");
    assert!(!text.contains("total"));
}

#[test]
fn plans_differing_only_in_out_of_scope_audit_events_are_equal() {
    let base = vec![
        state(103, "inv", 1),
        audit_of(105, "inv"),
        domain(106, "inv", 1),
    ];
    let mut noisy = base.clone();
    noisy.push(audit_of(104, "secret"));
    noisy.push(audit_of(107, "secret").mandatory());
    noisy.push(audit_of(150, "other"));
    let scope = MapScope::holding(&["inv"]);
    let a = run(&base, &gap(3, 1), &scope);
    let b = run(&noisy, &gap(3, 1), &scope);
    assert_eq!(a.plan, b.plan);
    assert_eq!(a.counts.destination_facing(), b.counts.destination_facing());
    assert_ne!(
        a.counts.total, b.counts.total,
        "only the hub-side total differs"
    );
}

#[test]
fn an_unregistered_topic_is_refused() {
    let event = Event::new(110, "nobody_knows", spot_sync_catchup::Payload::new("{}"))
        .with_subject(rid("a"));
    let err = plan_with_audit_options(
        &[event],
        &gap(1, 0),
        &Settings::default(),
        &topics(),
        &MapReferences::new(),
        &MapScope::holding(&["a"]),
        NODE,
        AuditDelivery::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "sync_catchup.unknown_topic");
}
