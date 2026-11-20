//! The longest wait and longest work of one completed attempt per step, the longest open wait,
//! and the version-1 state compatibility (WP-BPM-11.T9).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, NodeKind};
use spot_bpm_insight::{
    Accumulator, AccumulatorState, AttemptRef, Millis, NodeRunEvent, ProcessKey, StepStats,
    WaitReason, aggregate,
};
use spot_dag::NodeId;

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + seconds).unwrap()
}

fn attempt_of(process: &str, case: &str, node: &str, n: u32) -> AttemptRef {
    AttemptRef::new(
        ProcessKey::new(process, 1),
        CaseId::new(case).unwrap(),
        NodeId::new(node).unwrap(),
        n,
        NodeKind::Decision,
    )
}

fn attempt(case: &str, node: &str, n: u32) -> AttemptRef {
    attempt_of("order_flow", case, node, n)
}

/// One attempt that waits `wait` seconds and then works `work` seconds, starting at `t0`.
fn attempt_events(case: &str, node: &str, t0: i64, wait: i64, work: i64) -> Vec<NodeRunEvent> {
    let mut events = vec![NodeRunEvent::started(attempt(case, node, 1), at(t0))];
    if wait > 0 {
        events.push(NodeRunEvent::waiting(
            attempt(case, node, 1),
            at(t0),
            WaitReason::HumanTask,
        ));
        events.push(NodeRunEvent::started(attempt(case, node, 1), at(t0 + wait)));
    }
    events.push(NodeRunEvent::done(
        attempt(case, node, 1),
        at(t0 + wait + work),
    ));
    events
}

fn step(stats: &[StepStats], node: &str) -> StepStats {
    stats
        .iter()
        .find(|s| s.node.as_str() == node)
        .unwrap()
        .clone()
}

#[test]
fn the_longest_wait_and_work_are_exact_maxima_of_completed_attempts() {
    let mut events = Vec::new();
    // Waits of 5, 0, 40 and 7 seconds; works of 1, 2, 3 and 4 seconds.
    for (index, (wait, work)) in [(5, 1), (0, 2), (40, 3), (7, 4)].into_iter().enumerate() {
        events.extend(attempt_events(
            &format!("c{index}"),
            "approve",
            i64::try_from(index).unwrap() * 1000,
            wait,
            work,
        ));
    }
    let insight = aggregate(events);
    let stats = step(insight.steps(), "approve");
    assert_eq!(stats.completed, 4);
    assert_eq!(stats.max_wait, Some(Millis::from_seconds(40)));
    assert_eq!(stats.max_work, Some(Millis::from_seconds(4)));
}

#[test]
fn nothing_completed_means_none() {
    let mut acc = Accumulator::new();
    acc.push(NodeRunEvent::started(attempt("c1", "approve", 1), at(0)));
    let insight = acc.finish();
    let stats = step(insight.steps(), "approve");
    assert_eq!(stats.completed, 0);
    assert_eq!(stats.max_wait, None);
    assert_eq!(stats.max_work, None);
}

#[test]
fn a_retried_attempt_counts_as_completed() {
    let events = vec![
        NodeRunEvent::started(attempt("c1", "call", 1), at(0)),
        NodeRunEvent::waiting(
            attempt("c1", "call", 1),
            at(2),
            WaitReason::IntegrationRetry,
        ),
        NodeRunEvent::retrying(attempt("c1", "call", 1), at(12), "conn.timeout"),
        // The retry closes the first attempt when its replacement starts (a retrying event only
        // waits; see the accumulator's own test).
        NodeRunEvent::started(attempt("c1", "call", 2), at(12)),
    ];
    let insight = aggregate(events);
    let stats = step(insight.steps(), "call");
    assert_eq!(stats.retries, 1);
    assert_eq!(stats.completed, 1);
    assert_eq!(stats.max_wait, Some(Millis::from_seconds(10)));
    assert_eq!(stats.max_work, Some(Millis::from_seconds(2)));
}

#[test]
fn a_terminal_event_with_no_start_is_a_zero_length_attempt() {
    let insight = aggregate([NodeRunEvent::done(attempt("c1", "tick", 1), at(5))]);
    let stats = step(insight.steps(), "tick");
    assert_eq!(stats.completed, 1);
    assert_eq!(stats.max_wait, Some(Millis::ZERO));
    assert_eq!(stats.max_work, Some(Millis::ZERO));
}

#[test]
fn an_attempt_still_waiting_does_not_change_the_maxima_but_is_the_longest_open_wait() {
    let mut events = attempt_events("c1", "approve", 0, 5, 1);
    events.push(NodeRunEvent::started(attempt("c2", "approve", 1), at(10)));
    events.push(NodeRunEvent::waiting(
        attempt("c2", "approve", 1),
        at(10),
        WaitReason::HumanTask,
    ));
    let insight = aggregate(events);
    let stats = step(insight.steps(), "approve");
    assert_eq!(stats.max_wait, Some(Millis::from_seconds(5)));
    let (process, node, waited) = insight.longest_open_wait(at(100)).unwrap();
    assert_eq!(process, ProcessKey::new("order_flow", 1));
    assert_eq!(node.as_str(), "approve");
    assert_eq!(waited, Millis::from_seconds(90));
}

#[test]
fn a_running_attempt_is_not_a_wait_and_no_open_wait_is_none() {
    let mut acc = Accumulator::new();
    acc.push(NodeRunEvent::started(attempt("c1", "approve", 1), at(0)));
    let insight = acc.finish();
    assert_eq!(insight.longest_open_wait(at(1000)), None);
    assert_eq!(aggregate(Vec::new()).longest_open_wait(at(0)), None);
}

#[test]
fn ties_in_the_longest_open_wait_break_by_process_then_node() {
    let wait = |process: &str, node: &str, case: &str| {
        vec![
            NodeRunEvent::started(attempt_of(process, case, node, 1), at(0)),
            NodeRunEvent::waiting(
                attempt_of(process, case, node, 1),
                at(0),
                WaitReason::HumanTask,
            ),
        ]
    };
    let mut events = Vec::new();
    events.extend(wait("zeta", "a", "c1"));
    events.extend(wait("alpha", "b", "c2"));
    events.extend(wait("alpha", "a", "c3"));
    // A shorter wait never wins, whatever its name.
    events.extend({
        let mut later = wait("aaa", "a", "c4");
        later[0] = NodeRunEvent::started(attempt_of("aaa", "c4", "a", 1), at(30));
        later[1] = NodeRunEvent::waiting(
            attempt_of("aaa", "c4", "a", 1),
            at(30),
            WaitReason::HumanTask,
        );
        later
    });
    let insight = aggregate(events);
    let (process, node, waited) = insight.longest_open_wait(at(60)).unwrap();
    assert_eq!(process, ProcessKey::new("alpha", 1));
    assert_eq!(node.as_str(), "a");
    assert_eq!(waited, Millis::from_seconds(60));
}

/// A version-1 state, as a literal document: one step with one completed attempt (7 s of
/// waiting, bucket 278 of the cycle-time histogram) and one attempt open since second 100.
const VERSION_ONE: &str = r#"{
  "version": 1,
  "steps": [{
    "process": {"name": "order_flow", "version": 1},
    "node": "approve",
    "kind": "decision",
    "attempts": 2, "done": 1, "failed": 0, "retries": 0, "completed": 1,
    "work": 0, "wait": 7000,
    "cycles": {"buckets": [[278, 1]], "count": 1, "sum": 7000, "min": 7000, "max": 7000, "saturated": 0},
    "queue_entered": 1, "queue_left": 1, "waiting": 0, "max_waiting": 1
  }],
  "open": [{
    "process": {"name": "order_flow", "version": 1},
    "case": "c2",
    "node": "approve",
    "attempt": 1,
    "kind": "decision",
    "owner": null,
    "started_at": {"seconds": 1700000100, "nanos": 0},
    "segment_start": {"seconds": 1700000100, "nanos": 0},
    "wait_start": null,
    "state": "started",
    "wait_reason": null,
    "work": 0, "wait_time": 0
  }],
  "ended": []
}"#;

#[test]
fn a_version_one_state_loads_and_the_new_fields_cover_only_what_is_folded_after_the_resume() {
    let state: AccumulatorState = serde_json::from_str(VERSION_ONE).unwrap();
    assert_eq!(state.version, 1);
    let mut resumed = Accumulator::from_state(state).unwrap();

    // The rest of the feed: the open attempt c2 waits 3 s and works 2 s, then c3 works 5 s.
    let rest = vec![
        NodeRunEvent::waiting(attempt("c2", "approve", 1), at(102), WaitReason::HumanTask),
        NodeRunEvent::started(attempt("c2", "approve", 1), at(105)),
        NodeRunEvent::done(attempt("c2", "approve", 1), at(105)),
        NodeRunEvent::started(attempt("c3", "approve", 1), at(160)),
        NodeRunEvent::done(attempt("c3", "approve", 1), at(165)),
    ];
    resumed.push_all(rest.clone());

    // The straight fold of the whole feed, including the attempt the version-1 state summarised.
    let mut whole = vec![
        NodeRunEvent::started(attempt("c1", "approve", 1), at(0)),
        NodeRunEvent::waiting(attempt("c1", "approve", 1), at(0), WaitReason::HumanTask),
        NodeRunEvent::started(attempt("c1", "approve", 1), at(7)),
        NodeRunEvent::done(attempt("c1", "approve", 1), at(7)),
        NodeRunEvent::started(attempt("c2", "approve", 1), at(100)),
    ];
    whole.extend(rest);
    let straight = aggregate(whole);

    let mut resumed_stats = step(resumed.finish().steps(), "approve");
    let mut straight_stats = step(straight.steps(), "approve");

    // Only after-the-resume attempts feed the new fields of the resumed fold.
    assert_eq!(resumed_stats.max_wait, Some(Millis::from_seconds(3)));
    assert_eq!(resumed_stats.max_work, Some(Millis::from_seconds(5)));
    assert_eq!(straight_stats.max_wait, Some(Millis::from_seconds(7)));
    assert_eq!(straight_stats.max_work, Some(Millis::from_seconds(5)));

    // Everything else is identical.
    resumed_stats.max_wait = None;
    resumed_stats.max_work = None;
    straight_stats.max_wait = None;
    straight_stats.max_work = None;
    assert_eq!(resumed_stats, straight_stats);
}

#[test]
fn an_exported_state_is_version_two_and_resumes_with_the_maxima_intact() {
    let mut acc = Accumulator::new();
    acc.push_all(attempt_events("c1", "approve", 0, 5, 1));
    let state = acc.export_state();
    assert_eq!(state.version, 2);
    let json = serde_json::to_string(&state).unwrap();
    let mut resumed = Accumulator::from_state(serde_json::from_str(&json).unwrap()).unwrap();
    resumed.push_all(attempt_events("c2", "approve", 100, 2, 9));
    let stats = step(resumed.finish().steps(), "approve");
    assert_eq!(stats.max_wait, Some(Millis::from_seconds(5)));
    assert_eq!(stats.max_work, Some(Millis::from_seconds(9)));
}

#[test]
fn an_unknown_state_version_is_still_refused() {
    let mut state: AccumulatorState = serde_json::from_str(VERSION_ONE).unwrap();
    state.version = 3;
    assert_eq!(
        Accumulator::from_state(state).unwrap_err().code(),
        "bpm_insight.state_version"
    );
}
