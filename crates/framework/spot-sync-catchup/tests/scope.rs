#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Scoping the gap once, before planning (ADR-066 section 2; WP-SYN-07.T4): an audit event
//! addressed to another node never reaches a plan, in any mode or lane.

mod common;

use common::{MapReferences, audit, domain, state, topics};
use proptest::prelude::*;
use spot_sync_catchup::{
    CatchUpPlan, ClassCounts, Event, EventClass, GapSummary, Settings, TopicDeclarations, plan,
    scope_for_node,
};

const NODE: &str = "branch-1";
const OTHER: &str = "branch-2";

fn summary(count: u64, new_node: bool, incremental: bool) -> GapSummary {
    GapSummary {
        applied_prefix: 0,
        stream_end: 10_000,
        hot_retention_floor: 0,
        in_scope_event_count: count,
        class_counts: ClassCounts {
            state: count,
            domain: 0,
            audit: 0,
        },
        estimated_coalesced_records: 5,
        estimated_coalesced_bytes: 500,
        estimated_snapshot_bytes: 1_000_000,
        node_is_new_or_resync: new_node,
    }
    .tap(incremental)
}

trait Tap {
    fn tap(self, incremental: bool) -> Self;
}

impl Tap for GapSummary {
    fn tap(mut self, incremental: bool) -> Self {
        // A large in-scope count pushes the mode past the incremental limit.
        if !incremental {
            self.in_scope_event_count = 20_000;
            self.class_counts.state = 20_000;
        }
        self
    }
}

fn plan_for(events: &[Event], summary: &GapSummary) -> CatchUpPlan {
    plan(
        &scope_for_node(events, NODE, &topics()),
        summary,
        &Settings::default(),
        &topics(),
        &MapReferences::new(),
    )
    .unwrap()
}

fn planned_events(catch_up: &CatchUpPlan) -> Vec<&Event> {
    catch_up
        .mandatory_lane
        .iter()
        .chain(catch_up.bulk.iter().flat_map(|bundle| bundle.events.iter()))
        .collect()
}

#[test]
fn incremental_mode_drops_an_audit_event_addressed_to_another_node() {
    let events = vec![
        state(1, "a", 1),
        audit(2, Some(OTHER)),
        audit(3, Some(NODE)),
        audit(4, None),
    ];
    let gap = scope_for_node(&events, NODE, &topics());
    assert_eq!(gap.dropped_audit(), 2);
    assert_eq!(gap.node(), NODE);
    let catch_up = plan_for(&events, &summary(4, false, true));
    let positions: Vec<u64> = planned_events(&catch_up)
        .iter()
        .map(|e| e.position)
        .collect();
    assert_eq!(positions, vec![1, 3]);
}

#[test]
fn the_mandatory_lane_keeps_its_own_audit_event_and_drops_another_nodes() {
    let events = vec![
        audit(1, Some(NODE)).mandatory(),
        audit(2, Some(OTHER)).mandatory(),
        state(3, "a", 1).mandatory(),
    ];
    let catch_up = plan_for(&events, &summary(3, false, true));
    let lane: Vec<u64> = catch_up.mandatory_lane.iter().map(|e| e.position).collect();
    assert_eq!(lane, vec![1, 3]);
}

#[test]
fn coalesced_and_snapshot_plans_carry_no_foreign_audit_event() {
    let events = vec![
        audit(1, Some(OTHER)),
        audit(2, Some(OTHER)).mandatory(),
        domain(3, "a", 1),
    ];
    for new_node in [false, true] {
        let catch_up = plan_for(&events, &summary(3, new_node, false));
        assert!(
            planned_events(&catch_up)
                .iter()
                .all(|e| e.addressed_to.as_deref() != Some(OTHER))
        );
    }
}

#[test]
fn an_event_with_an_unregistered_topic_is_refused_not_dropped() {
    let mut unknown = state(1, "a", 1);
    unknown.topic = "made_up_topic".to_owned();
    let result = plan(
        &scope_for_node(&[unknown], NODE, &topics()),
        &summary(1, false, true),
        &Settings::default(),
        &topics(),
        &MapReferences::new(),
    );
    assert_eq!(result.unwrap_err().code(), "sync_catchup.unknown_topic");
}

fn event_of(kind: u8, position: u64, who: u8) -> Event {
    let addressee = match who {
        0 => None,
        1 => Some(NODE),
        _ => Some(OTHER),
    };
    match kind {
        0 => state(position, "a", position),
        1 => domain(position, "b", position),
        _ => audit(position, addressee),
    }
}

proptest! {
    #[test]
    fn scoping_leaves_no_foreign_audit_and_keeps_every_non_audit_event(
        specs in proptest::collection::vec((0u8..3, 0u8..3, any::<bool>()), 0..40),
        node in prop_oneof![Just(NODE), Just(OTHER)],
    ) {
        let registry = topics();
        let events: Vec<Event> = specs
            .iter()
            .enumerate()
            .map(|(index, (kind, who, mandatory))| {
                let event = event_of(*kind, 1 + index as u64, *who);
                if *mandatory { event.mandatory() } else { event }
            })
            .collect();
        let gap = scope_for_node(&events, node, &registry);
        let is_audit = |e: &Event| registry.class(&e.topic) == Some(EventClass::Audit);

        let non_audit_in: Vec<&Event> = events.iter().filter(|e| !is_audit(e)).collect();
        let non_audit_out: Vec<&Event> = gap.events().iter().filter(|e| !is_audit(e)).collect();
        prop_assert_eq!(non_audit_in, non_audit_out);

        for event in gap.events().iter().filter(|e| is_audit(e)) {
            prop_assert_eq!(event.addressed_to.as_deref(), Some(node));
        }

        let summary = summary(events.len() as u64, false, true);
        let catch_up = plan(&gap, &summary, &Settings::default(), &registry, &MapReferences::new())
            .unwrap();
        for event in planned_events(&catch_up) {
            if is_audit(event) {
                prop_assert_eq!(event.addressed_to.as_deref(), Some(node));
            }
        }
    }
}
