#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The audit-delivery option (WP-SYN-07.T6): the truth table, the default, and that a delivered
//! out-of-scope addressed event leaves nothing else behind.

mod common;

use common::{MapReferences, MapScope, topics};
use spot_sync_catchup::{
    Addressed, AuditDelivery, AuditedPlan, ClassCounts, Event, GapSummary, Mode, Payload, Settings,
    Unaddressed, plan_with_audit, plan_with_audit_options,
};

const NODE: &str = "branch-1";
const POS: u64 = 150;

fn gap(audit: u64) -> GapSummary {
    GapSummary {
        applied_prefix: 100,
        stream_end: 200,
        hot_retention_floor: 50,
        in_scope_event_count: audit,
        class_counts: ClassCounts {
            state: 0,
            domain: 0,
            audit,
        },
        estimated_coalesced_records: 3,
        estimated_coalesced_bytes: 300,
        estimated_snapshot_bytes: 100_000,
        node_is_new_or_resync: false,
    }
}

#[derive(Clone, Copy, Debug)]
enum Scope {
    Subject,
    Reference,
    Out,
}

#[derive(Clone, Copy, Debug)]
enum To {
    Nobody,
    ThisNode,
    OtherNode,
}

fn build(scope: Scope, to: To, mandatory: bool) -> (Event, MapScope) {
    let mut event = Event::new(POS, "sys_sync_missing", Payload::new("{}"));
    let mut oracle = MapScope::holding(&["mine"]);
    match scope {
        Scope::Subject => event = event.with_subject(common::rid("mine")),
        Scope::Reference => oracle = oracle.covering(POS),
        Scope::Out => event = event.with_subject(common::rid("theirs")),
    }
    match to {
        To::Nobody => {}
        To::ThisNode => event = event.with_addressee(NODE),
        To::OtherNode => event = event.with_addressee("branch-2"),
    }
    if mandatory {
        event = event.mandatory();
    }
    (event, oracle)
}

fn expected(d: AuditDelivery, scope: Scope, to: To) -> bool {
    let in_scope = !matches!(scope, Scope::Out);
    match to {
        To::OtherNode => false,
        To::ThisNode => d.addressed == Addressed::Always || in_scope,
        To::Nobody => d.unaddressed == Unaddressed::Deliver && in_scope,
    }
}

fn run(d: AuditDelivery, events: &[Event], oracle: &MapScope) -> AuditedPlan {
    plan_with_audit_options(
        events,
        &gap(events.len() as u64),
        &Settings::default(),
        &topics(),
        &MapReferences::new(),
        oracle,
        NODE,
        d,
    )
    .unwrap()
}

#[test]
fn the_truth_table_holds_for_every_combination() {
    let mut cases = 0;
    for unaddressed in [Unaddressed::Deliver, Unaddressed::Drop] {
        for addressed in [Addressed::ScopeChecked, Addressed::Always] {
            let d = AuditDelivery {
                unaddressed,
                addressed,
            };
            for scope in [Scope::Subject, Scope::Reference, Scope::Out] {
                for to in [To::Nobody, To::ThisNode, To::OtherNode] {
                    for mandatory in [false, true] {
                        let (event, oracle) = build(scope, to, mandatory);
                        let out = run(d, std::slice::from_ref(&event), &oracle);
                        let want = expected(d, scope, to);
                        let in_lane = out.plan.mandatory_lane.iter().any(|e| e.position == POS);
                        let in_audit = out.plan.audit.iter().any(|e| e.position == POS);
                        let ctx = format!("{d:?} {scope:?} {to:?} mandatory={mandatory}");
                        assert_eq!(in_lane, want && mandatory, "{ctx}");
                        assert_eq!(in_audit, want && !mandatory, "{ctx}");
                        assert_eq!(out.counts.visible, u64::from(want), "{ctx}");
                        assert_eq!(out.counts.total, 1, "{ctx}");
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 72);
}

#[test]
fn the_default_is_the_recommendation_and_plan_with_audit_uses_it() {
    let d = AuditDelivery::default();
    assert_eq!(d.unaddressed, Unaddressed::Drop);
    assert_eq!(d.addressed, Addressed::Always);
    assert_ne!(d, AuditDelivery::LEGACY);
    let events = vec![
        build(Scope::Subject, To::Nobody, false).0,
        build(Scope::Out, To::ThisNode, false).0,
    ];
    let oracle = MapScope::holding(&["mine"]);
    let via_default = plan_with_audit(
        &events,
        &gap(2),
        &Settings::default(),
        &topics(),
        &MapReferences::new(),
        &oracle,
        NODE,
    )
    .unwrap();
    assert_eq!(via_default, run(d, &events, &oracle));
}

#[test]
fn a_sys_sync_missing_about_a_record_the_node_lacks_is_delivered_by_default() {
    let missing = Event::new(POS, "sys_sync_missing", Payload::new("{}"))
        .with_subject(common::rid("not-held-yet"))
        .with_addressee(NODE);
    let oracle = MapScope::holding(&[]);
    let default = run(
        AuditDelivery::default(),
        std::slice::from_ref(&missing),
        &oracle,
    );
    assert_eq!(default.plan.audit, vec![missing.clone()]);
    let checked = run(
        AuditDelivery {
            unaddressed: Unaddressed::Drop,
            addressed: Addressed::ScopeChecked,
        },
        std::slice::from_ref(&missing),
        &oracle,
    );
    assert!(checked.plan.audit.is_empty());
}

#[test]
fn delivered_events_are_in_strict_position_order_across_both_lanes() {
    let mk = |pos: u64, mandatory: bool| {
        let e = Event::new(pos, "sys_sync_missing", Payload::new("{}"))
            .with_subject(common::rid("theirs"))
            .with_addressee(NODE);
        if mandatory { e.mandatory() } else { e }
    };
    let events = vec![mk(190, false), mk(120, true), mk(110, false), mk(150, true)];
    let out = run(AuditDelivery::default(), &events, &MapScope::holding(&[]));
    let audit: Vec<u64> = out.plan.audit.iter().map(|e| e.position).collect();
    let lane: Vec<u64> = out.plan.mandatory_lane.iter().map(|e| e.position).collect();
    assert_eq!(audit, vec![110, 190]);
    assert_eq!(lane, vec![120, 150]);
}

#[test]
fn a_dropped_event_cannot_tip_the_mode_or_appear_in_its_reasons() {
    // Many unaddressed in-scope audit events: the legacy rule counts them, the default does not.
    let events: Vec<Event> = (0..50)
        .map(|i| {
            Event::new(101 + i, "sys_sync_missing", Payload::new("{}"))
                .with_subject(common::rid("mine"))
        })
        .collect();
    let oracle = MapScope::holding(&["mine"]);
    let default = run(AuditDelivery::default(), &events, &oracle);
    assert_eq!(default.counts.visible, 0);
    assert_eq!(default.counts.total, 50);
    assert_eq!(default.plan.audit, vec![]);
    let legacy = run(AuditDelivery::LEGACY, &events, &oracle);
    assert_eq!(legacy.counts.visible, 50);
    assert_eq!(legacy.plan.audit.len(), 50);
    // With nothing delivered, the mode decision is exactly that of an empty gap.
    let empty = run(AuditDelivery::default(), &[], &oracle);
    assert_eq!(default.plan.mode, empty.plan.mode);
    assert_eq!(default.plan.reasons, empty.plan.reasons);
    assert_ne!(default.plan.mode, Mode::Snapshot);
}

#[test]
fn an_out_of_scope_addressed_event_leaves_no_trace_in_bundles_or_destination_bookkeeping() {
    let secret = Event::new(POS, "sys_sync_missing", Payload::new("{}"))
        .with_subject(common::rid("theirs"))
        .with_addressee(NODE);
    let oracle = MapScope::holding(&[]);
    let with = run(
        AuditDelivery::default(),
        std::slice::from_ref(&secret),
        &oracle,
    );
    let without = run(AuditDelivery::default(), &[], &oracle);
    // Only the audit list differs: bundles are the same, and the subject is in no bundle.
    assert_eq!(with.plan.bulk, without.plan.bulk);
    assert_eq!(with.plan.mandatory_lane, without.plan.mandatory_lane);
    assert_eq!(with.plan.audit.len(), 1);
    assert_eq!(with.counts.destination_facing().visible, 1);
}
