//! The streaming property: a feed pushed in chunks (a backfill) gives exactly the result of the
//! whole feed, so a live feed and a replay never disagree.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, NodeKind};
use spot_bpm_insight::{Accumulator, AttemptRef, NodeRunEvent, ProcessKey, WaitReason, aggregate};
use spot_dag::NodeId;

/// One event spec: node index, state index and attempt number.
fn event_for(spec: &(u32, u32, u32, u32), index: usize) -> NodeRunEvent {
    let (node, state, attempt, _) = *spec;
    let node_name = ["approve", "call", "tick"][(node % 3) as usize];
    let attempt_ref = AttemptRef::new(
        ProcessKey::new("order_flow", 1),
        CaseId::new("case1").unwrap(),
        NodeId::new(node_name).unwrap(),
        attempt + 1,
        NodeKind::Decision,
    );
    let at = OffsetDateTime::from_unix_timestamp(1_700_000_000 + index as i64).unwrap();
    match state % 5 {
        0 => NodeRunEvent::started(attempt_ref, at),
        1 => NodeRunEvent::waiting(attempt_ref, at, WaitReason::HumanTask),
        2 => NodeRunEvent::done(attempt_ref, at),
        3 => NodeRunEvent::failed(attempt_ref, at, "step.failed"),
        _ => NodeRunEvent::retrying(attempt_ref, at, "conn.timeout"),
    }
}

proptest! {
    #[test]
    fn a_chunked_fold_equals_the_whole_fold(
        specs in prop::collection::vec((0u32..3, 0u32..5, 0u32..3, 0u32..3), 0..40),
        split in 0usize..40,
    ) {
        let events: Vec<NodeRunEvent> = specs
            .iter()
            .enumerate()
            .map(|(index, spec)| event_for(spec, index))
            .collect();
        let whole = aggregate(events.clone());
        let split = split.min(events.len());
        let mut accumulator = Accumulator::new();
        accumulator.push_all(events[..split].iter().cloned());
        accumulator.push_all(events[split..].iter().cloned());
        prop_assert_eq!(accumulator.finish(), whole);
    }

    #[test]
    fn a_live_feed_pushed_one_at_a_time_equals_the_backfill(
        specs in prop::collection::vec((0u32..3, 0u32..5, 0u32..3, 0u32..3), 0..40),
    ) {
        let events: Vec<NodeRunEvent> = specs
            .iter()
            .enumerate()
            .map(|(index, spec)| event_for(spec, index))
            .collect();
        let whole = aggregate(events.clone());
        let mut accumulator = Accumulator::new();
        for event in events {
            accumulator.push(event);
        }
        prop_assert_eq!(accumulator.finish(), whole);
    }
}
