//! Integration tests for the durable Case engine: the twelve-node crash sweep, retries, error
//! branches, timers, sub-processes, parallel joins and 10,000 interleaved Cases.
//!
//! Every test drives the engine through the deterministic harness: a virtual clock, an effect
//! ledger and scripted crashes. Time is always an argument; nothing here reads a real clock, and
//! every in-memory future is immediately ready, so a no-op waker is all the executor needs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_bpm_engine::{
    CaseId, CaseStore, CaseUpdate, EffectLedger, EngineError, FlakyHandler, HandlerRegistry,
    Harness, NodeKind, RecordingHandler,
};
use spot_bpm_model::{ProcessVersion, from_text};
use spot_dag::NodeId;
use spot_dag_exec::{NodeStatus, RunStatus};

/// Runs a future to completion on the current thread.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// A test whose body is an async block.
macro_rules! test {
    ($name:ident, $body:expr) => {
        #[test]
        fn $name() {
            block_on($body);
        }
    };
}

/// A minimal process: trigger, one document, a terminal join.
const LINEAR: &str = r#"
name = "linear"
app = "engine_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "work"
kind = "document"
action = "create"
doctype = "Sales Order"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "work"

[[edges]]
from = "work"
to = "done"
"#;

/// A process with a parallel split, a join and an error branch, for retries and errors.
const FAN: &str = r#"
name = "fan"
app = "engine_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "split"
kind = "logic"
logic = "parallel"

[[nodes]]
id = "left"
kind = "document"
action = "create"
doctype = "Sales Order"

[[nodes]]
id = "right"
kind = "document"
action = "create"
doctype = "Delivery Note"

[[nodes]]
id = "join"
kind = "logic"
logic = "join"

[[nodes]]
id = "call"
kind = "integration"
connector = "http"
action = "post"
retry_attempts = 3
retry_delay_secs = 1
error = "failed"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[nodes]]
id = "failed"
kind = "document"
action = "create"
doctype = "Issue"
terminal = true

[[edges]]
from = "start"
to = "split"

[[edges]]
from = "split"
to = "left"
condition = "1 == 1"

[[edges]]
from = "split"
to = "right"
condition = "2 == 2"

[[edges]]
from = "left"
to = "join"

[[edges]]
from = "right"
to = "join"

[[edges]]
from = "join"
to = "call"

[[edges]]
from = "call"
to = "done"
condition = "1 == 1"
"#;

/// A twelve-node process: parallel split, join, sub-process, timer, error branch and a decision.
const TWELVE: &str = r#"
name = "twelve"
app = "engine_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "split"
kind = "logic"
logic = "parallel"

[[nodes]]
id = "doc_a"
kind = "document"
action = "create"
doctype = "Sales Order"

[[nodes]]
id = "doc_b"
kind = "document"
action = "create"
doctype = "Delivery Note"

[[nodes]]
id = "join"
kind = "logic"
logic = "join"

[[nodes]]
id = "sub"
kind = "sub_process"
process = "child_proc"

[[nodes]]
id = "wait"
kind = "wait"
wait = "duration"
seconds = 30

[[nodes]]
id = "call"
kind = "integration"
connector = "http"
action = "post"
timeout_secs = 60
error = "on_error"

[[nodes]]
id = "on_error"
kind = "logic"
logic = "join"
terminal = true

[[nodes]]
id = "decide"
kind = "logic"
logic = "branch"

[[nodes]]
id = "done_ok"
kind = "logic"
logic = "join"
terminal = true

[[nodes]]
id = "done_alt"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "split"

[[edges]]
from = "split"
to = "doc_a"
condition = "1 == 1"

[[edges]]
from = "split"
to = "doc_b"
condition = "2 == 2"

[[edges]]
from = "doc_a"
to = "join"

[[edges]]
from = "doc_b"
to = "join"

[[edges]]
from = "join"
to = "sub"

[[edges]]
from = "sub"
to = "wait"

[[edges]]
from = "wait"
to = "call"

[[edges]]
from = "call"
to = "decide"
condition = "1 == 1"

[[edges]]
from = "decide"
to = "done_ok"
condition = "1 == 1"

[[edges]]
from = "decide"
to = "done_alt"
condition = "1 == 2"
"#;

/// The child process a sub-process node runs.
const CHILD: &str = r#"
name = "child_proc"
app = "engine_test"
version = 1
status = "published"

[[nodes]]
id = "cstart"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "cwork"
kind = "document"
action = "create"
doctype = "Sales Invoice"

[[nodes]]
id = "cdone"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "cstart"
to = "cwork"

[[edges]]
from = "cwork"
to = "cdone"
"#;

/// A process whose only work node has a short timeout and an error branch.
const TIMEOUT: &str = r#"
name = "timeout_proc"
app = "engine_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "slow"
kind = "document"
action = "create"
doctype = "Sales Order"
timeout_secs = 10
error = "failed"

[[nodes]]
id = "ok"
kind = "logic"
logic = "join"
terminal = true

[[nodes]]
id = "failed"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "slow"

[[edges]]
from = "slow"
to = "ok"
condition = "1 == 1"
"#;

/// A process with an event wait a test resolves with a signal.
const EVENT_WAIT: &str = r#"
name = "event_wait"
app = "engine_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "pause"
kind = "wait"
wait = "event"
event = "paid"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "pause"

[[edges]]
from = "pause"
to = "done"
"#;

fn version(text: &str) -> ProcessVersion {
    from_text(text).expect("the process text must parse")
}

fn node(raw: &str) -> NodeId {
    NodeId::new(raw).unwrap()
}

/// A registry with the default logic handlers plus recording handlers for documents and
/// integrations.
fn handlers(ledger: &Arc<EffectLedger>) -> HandlerRegistry {
    let mut registry = HandlerRegistry::logic_default();
    registry.register(RecordingHandler::new(NodeKind::Document, ledger.clone()));
    registry.register(RecordingHandler::new(NodeKind::Integration, ledger.clone()));
    registry
}

/// A harness sharing one effect ledger with its handlers.
fn harness(ledger: &Arc<EffectLedger>) -> Harness {
    Harness::with_ledger(handlers(ledger), ledger.clone())
}

/// Steps a Case until an injected crash fires; returns whether it fired.
async fn step_until_crash(harness: &Harness, id: &CaseId, limit: usize) -> bool {
    for _ in 0..limit {
        match harness.step(id).await {
            Err(EngineError::Crashed { .. }) => return true,
            Ok(_) => {}
            Err(error) => panic!("unexpected error: {error}"),
        }
    }
    false
}

test!(a_linear_case_runs_to_completion_with_one_effect, async {
    let ledger = Arc::new(EffectLedger::new());
    let harness = harness(&ledger);
    let process = version(LINEAR);
    let id = CaseId::new("case_linear").unwrap();
    harness.start(&process, id.clone(), "linear").await.unwrap();
    let record = harness.drive(&id, 100).await.unwrap();
    assert_eq!(record.run.status, RunStatus::Completed);
    assert_eq!(ledger.distinct(), 1);
    assert_eq!(ledger.performed("case_linear:work:1"), 1);
});

test!(a_parallel_split_joins_both_branches, async {
    let ledger = Arc::new(EffectLedger::new());
    let harness = harness(&ledger);
    let process = version(FAN);
    let id = CaseId::new("case_fan").unwrap();
    harness.start(&process, id.clone(), "fan").await.unwrap();
    let record = harness.drive(&id, 200).await.unwrap();
    assert_eq!(record.run.status, RunStatus::Completed);
    assert_eq!(
        record.run.node(&node("left")).unwrap().status,
        NodeStatus::Done
    );
    assert_eq!(
        record.run.node(&node("right")).unwrap().status,
        NodeStatus::Done
    );
    assert_eq!(
        record.run.node(&node("join")).unwrap().status,
        NodeStatus::Done
    );
    assert_eq!(ledger.performed("case_fan:left:1"), 1);
    assert_eq!(ledger.performed("case_fan:right:1"), 1);
    assert!(ledger.duplicates().is_empty());
});

test!(a_flaky_node_retries_then_succeeds_once, async {
    let ledger = Arc::new(EffectLedger::new());
    let mut registry = HandlerRegistry::logic_default();
    registry.register(FlakyHandler::new(NodeKind::Integration, ledger.clone(), 2));
    registry.register(RecordingHandler::new(NodeKind::Document, ledger.clone()));
    let harness = Harness::with_ledger(registry, ledger.clone());
    let process = version(FAN);
    let id = CaseId::new("case_retry").unwrap();
    harness.start(&process, id.clone(), "retry").await.unwrap();
    let record = harness.drive(&id, 300).await.unwrap();
    assert_eq!(record.run.status, RunStatus::Completed);
    assert_eq!(
        record.run.node(&node("call")).unwrap().status,
        NodeStatus::Done
    );
    // Three engine attempts, one recorded effect: the two failed attempts did not side-effect.
    assert_eq!(record.attempt(&node("call")).unwrap().attempt, 3);
    assert_eq!(ledger.performed("case_retry:call:3"), 1);
    assert!(ledger.duplicates().is_empty());
});

test!(a_node_that_never_succeeds_takes_its_error_branch, async {
    let ledger = Arc::new(EffectLedger::new());
    let mut registry = HandlerRegistry::logic_default();
    registry.register(FlakyHandler::new(NodeKind::Integration, ledger.clone(), 9));
    registry.register(RecordingHandler::new(NodeKind::Document, ledger.clone()));
    let harness = Harness::with_ledger(registry, ledger.clone());
    let process = version(FAN);
    let id = CaseId::new("case_error").unwrap();
    harness.start(&process, id.clone(), "error").await.unwrap();
    let record = harness.drive(&id, 300).await.unwrap();
    assert_eq!(
        record.run.node(&node("call")).unwrap().status,
        NodeStatus::Failed
    );
    assert_eq!(
        record.run.node(&node("failed")).unwrap().status,
        NodeStatus::Done
    );
    assert_eq!(
        record.run.node(&node("done")).unwrap().status,
        NodeStatus::Skipped
    );
    // The error node is a document, so its handler ran and recorded one effect.
    assert_eq!(ledger.performed("case_error:failed:1"), 1);
});

test!(a_duration_wait_fires_and_continues, async {
    let ledger = Arc::new(EffectLedger::new());
    let harness = harness(&ledger);
    let process = version(TWELVE);
    let child = version(CHILD);
    harness.engine().publish(&child).unwrap();
    let id = CaseId::new("case_timer").unwrap();
    harness.start(&process, id.clone(), "timer").await.unwrap();
    let record = harness.drive(&id, 400).await.unwrap();
    assert_eq!(record.run.status, RunStatus::Completed);
    assert_eq!(
        record.run.node(&node("wait")).unwrap().status,
        NodeStatus::Done
    );
    assert_eq!(
        record.run.node(&node("done_ok")).unwrap().status,
        NodeStatus::Done
    );
});

test!(an_event_wait_resumes_on_a_signal, async {
    let ledger = Arc::new(EffectLedger::new());
    let harness = harness(&ledger);
    let process = version(EVENT_WAIT);
    let id = CaseId::new("case_signal").unwrap();
    harness.start(&process, id.clone(), "signal").await.unwrap();
    // Step until the wait is registered.
    for _ in 0..10 {
        let report = harness.step(&id).await.unwrap();
        if !report.made_progress() {
            break;
        }
    }
    let record = harness.engine().load(&id).await.unwrap();
    assert_eq!(
        record.run.node(&node("pause")).unwrap().status,
        NodeStatus::Running
    );
    // Signal the event; the Case resumes exactly once.
    let report = harness
        .engine()
        .signal(&id, &node("pause"), None, harness.clock().now())
        .await
        .unwrap();
    assert!(report.made_progress());
    let record = harness.drive(&id, 50).await.unwrap();
    assert_eq!(record.run.status, RunStatus::Completed);
});

test!(a_sub_process_starts_and_awaits_its_child, async {
    let ledger = Arc::new(EffectLedger::new());
    let harness = harness(&ledger);
    let process = version(TWELVE);
    let child = version(CHILD);
    harness.engine().publish(&child).unwrap();
    let id = CaseId::new("case_sub").unwrap();
    harness.start(&process, id.clone(), "sub").await.unwrap();
    let record = harness.drive(&id, 400).await.unwrap();
    assert_eq!(record.run.status, RunStatus::Completed);
    assert_eq!(
        record.run.node(&node("sub")).unwrap().status,
        NodeStatus::Done
    );
    // The child's work node ran once, exactly.
    let child_effects: Vec<String> = ledger
        .keys()
        .into_iter()
        .filter(|key| key.ends_with(":cwork:1"))
        .collect();
    assert_eq!(child_effects.len(), 1);
    assert!(ledger.duplicates().is_empty());
});

test!(
    a_node_that_outlives_its_deadline_takes_its_error_branch,
    async {
        let ledger = Arc::new(EffectLedger::new());
        let harness = harness(&ledger);
        let process = version(TIMEOUT);
        let id = CaseId::new("case_timeout").unwrap();
        harness
            .start(&process, id.clone(), "timeout")
            .await
            .unwrap();
        // The first four boundaries are `start`'s; the fifth is `slow`'s BeforeHandler, which
        // leaves `slow` started with no outcome.
        harness.crash_after(5);
        assert!(step_until_crash(&harness, &id, 20).await);
        harness.no_crash();
        harness.clock().advance(11);
        let _ = harness.step(&id).await.unwrap();
        let record = harness.engine().load(&id).await.unwrap();
        assert_eq!(
            record.run.node(&node("slow")).unwrap().status,
            NodeStatus::Failed
        );
        // The error branch runs, then the Case ends.
        let record = harness.drive(&id, 50).await.unwrap();
        assert_eq!(
            record.run.node(&node("failed")).unwrap().status,
            NodeStatus::Done
        );
        assert_eq!(record.run.status, RunStatus::Failed);
        // The timed-out attempt never side-effected.
        assert_eq!(ledger.performed("case_timeout:slow:1"), 0);
    }
);

test!(
    a_stored_outcome_replays_without_running_the_handler_again,
    async {
        let ledger = Arc::new(EffectLedger::new());
        let harness = harness(&ledger);
        let process = version(LINEAR);
        let id = CaseId::new("case_replay").unwrap();
        harness.start(&process, id.clone(), "replay").await.unwrap();
        // `start` uses four boundaries; `work`'s BeforeHandler, AfterHandler and
        // AfterAttemptStored are the fifth, sixth and seventh. Crash after the outcome is stored.
        harness.crash_after(7);
        assert!(step_until_crash(&harness, &id, 20).await);
        harness.no_crash();
        let record = harness.drive(&id, 50).await.unwrap();
        assert_eq!(record.run.status, RunStatus::Completed);
        // The handler ran once: the stored outcome was replayed, not re-executed.
        assert_eq!(ledger.total(), 1);
        assert!(ledger.duplicates().is_empty());
    }
);

test!(cancelling_a_case_stops_it, async {
    let ledger = Arc::new(EffectLedger::new());
    let harness = harness(&ledger);
    let process = version(EVENT_WAIT);
    let id = CaseId::new("case_cancel").unwrap();
    harness.start(&process, id.clone(), "cancel").await.unwrap();
    for _ in 0..5 {
        let _ = harness.step(&id).await.unwrap();
    }
    let record = harness
        .engine()
        .cancel(&id, harness.clock().now())
        .await
        .unwrap();
    assert_eq!(record.run.status, RunStatus::Cancelled);
    let report = harness.step(&id).await.unwrap();
    assert!(!report.made_progress());
});

test!(
    the_twelve_node_process_survives_a_crash_at_every_effect_boundary,
    async {
        // Measure how many effect boundaries the process has, with no crash.
        let probe_ledger = Arc::new(EffectLedger::new());
        let probe = harness(&probe_ledger);
        let process = version(TWELVE);
        let child = version(CHILD);
        probe.engine().publish(&child).unwrap();
        let probe_id = CaseId::new("case_probe").unwrap();
        probe
            .start(&process, probe_id.clone(), "probe")
            .await
            .unwrap();
        probe.drive(&probe_id, 400).await.unwrap();
        let boundaries = probe.faults().seen();
        assert!(boundaries > 20, "the process has many effect boundaries");
        assert!(probe_ledger.duplicates().is_empty());

        for k in 1..=boundaries {
            let ledger = Arc::new(EffectLedger::new());
            let harness = harness(&ledger);
            harness.engine().publish(&child).unwrap();
            let id = CaseId::new(&format!("case_crash_{k}")).unwrap();
            harness
                .start(&process, id.clone(), &format!("crash-{k}"))
                .await
                .unwrap();
            harness.crash_after(k);
            let record = harness.drive(&id, 800).await.unwrap();
            assert_eq!(record.run.status, RunStatus::Completed, "k = {k}");
            assert!(
                ledger.duplicates().is_empty(),
                "k = {k}: duplicate side effects"
            );
            assert_eq!(ledger.performed(&format!("{id}:doc_a:1")), 1, "k = {k}");
            assert_eq!(ledger.performed(&format!("{id}:doc_b:1")), 1, "k = {k}");
            assert_eq!(ledger.performed(&format!("{id}:call:1")), 1, "k = {k}");
            // Four effects: doc_a, doc_b, the child's work node and the integration call.
            assert_eq!(ledger.distinct(), 4, "k = {k}");
        }
    }
);

test!(
    ten_thousand_interleaved_cases_run_without_starvation,
    async {
        const CASES: usize = 10_000;
        let ledger = Arc::new(EffectLedger::new());
        let harness = harness(&ledger);
        let process = version(LINEAR);
        harness.engine().publish(&process).unwrap();

        let mut ids = Vec::with_capacity(CASES);
        for index in 0..CASES {
            let id = CaseId::new(&format!("case_{index}")).unwrap();
            harness
                .start(&process, id.clone(), &format!("bulk-{index}"))
                .await
                .unwrap();
            ids.push(id);
        }

        // Round-robin: every unfinished Case gets one turn per round, so no Case is starved.
        let mut finished = vec![false; CASES];
        let mut rounds = 0;
        while finished.iter().any(|done| !done) {
            rounds += 1;
            assert!(rounds < 50, "the Cases did not drain");
            for (index, id) in ids.iter().enumerate() {
                if finished[index] {
                    continue;
                }
                let report = harness.step(id).await.unwrap();
                if report.finished {
                    finished[index] = true;
                }
            }
        }

        assert_eq!(rounds, 3, "a three-node Case finishes in three rounds");
        for (index, id) in ids.iter().enumerate() {
            let record = harness.engine().load(id).await.unwrap();
            assert_eq!(record.run.status, RunStatus::Completed, "case {index}");
        }
        // Exactly one side effect per Case, and never a duplicate.
        assert_eq!(ledger.distinct(), CASES);
        assert_eq!(ledger.total(), CASES as u64);
        assert!(ledger.duplicates().is_empty());
    }
);

test!(the_store_lists_runnable_cases_and_finds_by_key, async {
    let ledger = Arc::new(EffectLedger::new());
    let harness = harness(&ledger);
    let process = version(EVENT_WAIT);
    let id = CaseId::new("case_list").unwrap();
    harness
        .start(&process, id.clone(), "list-key")
        .await
        .unwrap();

    let found = harness
        .engine()
        .store()
        .find_by_key("list-key")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.run.id, id);

    let runnable = harness
        .engine()
        .store()
        .list_runnable(harness.clock().now(), 10)
        .await
        .unwrap();
    assert!(runnable.contains(&id));

    // A stale commit is refused.
    let record = harness.engine().load(&id).await.unwrap();
    let stale = CaseUpdate::new();
    let result = harness
        .engine()
        .store()
        .commit(&id, record.run.version + 5, stale)
        .await;
    assert!(matches!(result, Err(EngineError::VersionConflict { .. })));
});
