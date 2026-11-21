//! The memory bound (WP-BPM-11.T7): the exported state is `O(steps x buckets)` plus
//! `O(open attempts)`, however many attempts were folded.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, NodeKind};
use spot_bpm_insight::{Accumulator, AttemptRef, BUCKETS, NodeRunEvent, ProcessKey};
use spot_dag::NodeId;

const STEPS: usize = 10;
/// The fixed ceiling for ten steps: 300 KB of JSON.
const CEILING: usize = 300_000;

/// The instant `ms` milliseconds after a fixed epoch.
fn at(ms: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(
        1_700_000_000_000_000_000_i128 + i128::from(ms) * 1_000_000,
    )
    .unwrap()
}

fn state_bytes(acc: &Accumulator) -> usize {
    serde_json::to_string(&acc.export_state()).unwrap().len()
}

#[test]
fn a_million_completed_attempts_over_ten_steps_stay_under_a_fixed_size() {
    let nodes: Vec<NodeId> = (0..STEPS)
        .map(|i| NodeId::new(&format!("step{i}")).unwrap())
        .collect();
    let process = ProcessKey::new("order_flow", 1);
    let mut acc = Accumulator::new();
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut at_100k = 0;
    for n in 0..1_000_000_u64 {
        // A cheap deterministic generator: durations from 0 ms to about 3 hours.
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let duration_ms = i64::try_from((seed >> 33) % 10_800_000).unwrap();
        let node = nodes[usize::try_from(n % STEPS as u64).unwrap()].clone();
        let case = CaseId::new(&format!("case{n}")).unwrap();
        let attempt = AttemptRef::new(process.clone(), case, node, 1, NodeKind::Decision);
        let start = i64::try_from(n).unwrap();
        acc.push(NodeRunEvent::started(attempt.clone(), at(start)));
        acc.push(NodeRunEvent::done(attempt, at(start + duration_ms)));
        if n == 99_999 {
            at_100k = state_bytes(&acc);
        }
    }
    let state = acc.export_state();
    assert!(state.open.is_empty(), "ended attempts hold nothing");
    assert!(state.ended.len() <= 256, "the replay window is constant");
    assert_eq!(state.steps.len(), STEPS);
    for step in &state.steps {
        assert_eq!(step.completed, 100_000);
        assert!(step.cycles.buckets.len() <= usize::from(BUCKETS));
    }
    let bytes = state_bytes(&acc);
    assert!(bytes < CEILING, "state is {bytes} bytes");
    assert!(
        bytes <= at_100k * 2,
        "state grew with history: {at_100k} bytes at 100k attempts, {bytes} at 1M"
    );
}
