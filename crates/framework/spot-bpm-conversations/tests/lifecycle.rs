//! The long life of a conversation: a peer that never answers, a peer that cancels, and a crash
//! at every effect point of either side.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use spot_bpm_contract::OutcomeKind;
use spot_bpm_conversations::{
    Cancellation, CompensationDeferred, CompensationHook, ConversationStore, ViewThresholds, codes,
};
use spot_bpm_engine::{AttemptState, CaseRecord, CaseStore, ScriptedFaults};
use spot_dag_exec::{NodeStatus, RunStatus};
use spot_link_exchange::{MessageState, OutboxStore, RejectCode};

fn status(record: &CaseRecord, node: &str) -> NodeStatus {
    record
        .run
        .node(&spot_dag::NodeId::new(node).unwrap())
        .unwrap()
        .status
}

#[derive(Default)]
struct RecordingHook {
    calls: Mutex<Vec<Cancellation>>,
    defer_first: Mutex<bool>,
}

impl CompensationHook for RecordingHook {
    fn compensate(&self, cancellation: &Cancellation) -> Result<(), CompensationDeferred> {
        let mut defer = self.defer_first.lock().unwrap();
        if *defer {
            *defer = false;
            return Err(CompensationDeferred);
        }
        self.calls.lock().unwrap().push(cancellation.clone());
        Ok(())
    }
}

#[test]
fn a_peer_that_never_answers_ends_in_the_timeout_branch_and_a_late_answer_is_refused() {
    let w = World::new();
    let a = w.a.boot();
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    let (link, conversation) = conversation_for_case(&w.a, "req1");

    // The link is down for two hours: nothing is delivered, the wait limit (one hour) passes.
    a.advance(T0 + 2 * HOUR);
    let case = a.case("req1");
    assert_eq!(case.run.status, RunStatus::Completed);
    assert_eq!(
        status(&case, "expired"),
        NodeStatus::Done,
        "the timeout edge was taken"
    );
    assert_eq!(status(&case, "approved"), NodeStatus::Skipped);
    assert_eq!(w.a.ledger.total(), 1);

    // The contract's outcome for the wait is `expired`, which needs a person; the request says so.
    let record =
        w.a.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    let ending = record.ending.as_ref().unwrap();
    assert_eq!(ending.outcome, "expired");
    assert_eq!(ending.kind, OutcomeKind::NeedsAttention);
    assert!(ending.timed_out);
    assert_eq!(record.attention.as_ref().unwrap().code, codes::WAIT_TIMEOUT);
    assert!(record.waits.is_empty());
    let view = a
        .driver
        .view(
            &link,
            &conversation,
            at(T0 + 2 * HOUR),
            &ViewThresholds::default(),
        )
        .unwrap();
    assert!(view.conditions.iter().any(|c| c.code == "NEEDS_ATTENTION"));

    // The peer comes back and answers late: the conversation already ended, so the answer is
    // refused with a reason and held for a person, and nothing the request did is done twice.
    let b = w.b.boot();
    assert_eq!(carry(&w.a, &b, T0 + 3 * HOUR).len(), 1);
    b.advance(T0 + 3 * HOUR + SECOND);
    carry(&w.b, &a, T0 + 3 * HOUR + 2 * SECOND);
    a.advance(T0 + 3 * HOUR + 3 * SECOND);
    let held = w.a.quarantine.held();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].code, RejectCode::OutOfOrderExpired);
    assert_eq!(w.a.ledger.total(), 1);
    assert!(w.a.ledger.duplicates().is_empty());
}

#[test]
fn an_answer_that_arrives_before_the_limit_wins_over_the_timeout() {
    let w = World::new();
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    carry(&w.a, &b, T0 + SECOND);
    b.advance(T0 + 2 * SECOND);
    carry(&w.b, &a, T0 + 30 * 60 * SECOND);
    // The answer is in the inbox when the limit passes: the arrival is handled first.
    a.advance(T0 + 2 * HOUR);
    let case = a.case("req1");
    assert_eq!(status(&case, "approved"), NodeStatus::Done);
    assert_eq!(status(&case, "expired"), NodeStatus::Skipped);
}

fn cancel_world() -> World {
    let mut w = World::with_halves(spot_bpm_conversations_test_half());
    w.a.half = Half::Canceller;
    w
}

fn spot_bpm_conversations_test_half() -> Half {
    Half::Watcher2
}

#[test]
fn a_cancellation_ends_both_sides_and_runs_the_compensation_once() {
    let w = cancel_world();
    let hook = Arc::new(RecordingHook::default());
    let a = w.a.boot();
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    let (b, hook_dyn) = (w.b.boot_with(Some(hook.clone())), hook.clone());
    let _ = hook_dyn;

    // The request arrives first and the approver's case starts and waits.
    let sent = carry_n(&w.a, &b, T0 + SECOND, 1);
    assert_eq!(sent.len(), 1);
    b.advance(T0 + 2 * SECOND);
    // Then the cancellation arrives while both waits are registered.
    let sent = carry_n(&w.a, &b, T0 + 3 * SECOND, 1);
    assert_eq!(sent.len(), 1);
    b.advance(T0 + 4 * SECOND);
    b.advance(T0 + 5 * SECOND);

    let (link, conversation) = conversation_for_case(&w.a, "req1");
    let record_b =
        w.b.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    let case_b = record_b.case.clone().unwrap();
    let case = b.case(&case_b);
    assert_eq!(
        status(&case, "cancelled"),
        NodeStatus::Done,
        "the cancel satisfied the first wait"
    );
    assert_eq!(
        status(&case, "second_failed"),
        NodeStatus::Done,
        "the second wait took its error branch"
    );
    assert_eq!(status(&case, "second_done"), NodeStatus::Skipped);

    let ending = record_b.ending.as_ref().unwrap();
    assert_eq!(ending.outcome, "cancelled");
    assert_eq!(ending.kind, OutcomeKind::Cancelled);
    assert!(ending.by_peer);
    assert!(record_b.compensated);
    let calls = hook.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "the compensation path starts once");
    assert_eq!(calls[0].case, case_b);
    assert_eq!(calls[0].outcome, "cancelled");

    // The sender's side ended in the same outcome by sending the cancel.
    let record_a =
        w.a.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    let ending_a = record_a.ending.unwrap();
    assert_eq!(ending_a.outcome, "cancelled");
    assert!(!ending_a.by_peer);
    assert_eq!(a.case("req1").run.status, RunStatus::Completed);

    // Nothing more can be said in a cancelled conversation.
    b.advance(T0 + 6 * SECOND);
    assert_eq!(hook.calls.lock().unwrap().len(), 1);
}

#[test]
fn both_messages_arriving_before_the_approver_looks_end_the_same_way() {
    let w = cancel_world();
    let hook = Arc::new(RecordingHook::default());
    let a = w.a.boot();
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    let b = w.b.boot_with(Some(hook.clone()));
    assert_eq!(carry(&w.a, &b, T0 + SECOND).len(), 2);
    b.advance(T0 + 2 * SECOND);
    b.advance(T0 + 3 * SECOND);
    let (link, conversation) = conversation_for_case(&w.a, "req1");
    let record_b =
        w.b.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    let case = b.case(record_b.case.as_deref().unwrap());
    assert_eq!(status(&case, "cancelled"), NodeStatus::Done);
    assert_eq!(status(&case, "second_failed"), NodeStatus::Done);
    assert_eq!(hook.calls.lock().unwrap().len(), 1);
}

#[test]
fn a_compensation_that_cannot_start_yet_is_tried_again() {
    let w = cancel_world();
    let hook = Arc::new(RecordingHook::default());
    *hook.defer_first.lock().unwrap() = true;
    let a = w.a.boot();
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    let b = w.b.boot_with(Some(hook.clone()));
    carry(&w.a, &b, T0 + SECOND);
    // The driver's first attempt is deferred; a later pass (the notice was not acknowledged
    // as handled) starts it, and only once.
    b.advance(T0 + 2 * SECOND);
    b.advance(T0 + 3 * SECOND);
    assert!(
        !*hook.defer_first.lock().unwrap(),
        "the first try was deferred"
    );
    assert_eq!(hook.calls.lock().unwrap().len(), 1);
}

/// Runs the whole exchange with the requester's engine crashing after `a_crash` effect points and
/// the approver's after `b_crash`, restarting each from its durable state.
fn crash_run(a_crash: u64, b_crash: u64) {
    let w = World::new();
    {
        let a = w.a.boot_faulty(Arc::new(ScriptedFaults::after(a_crash)));
        a.start_request("req1", "REF-1", T0);
        let _ = a.settle_result(T0);
    }
    w.a.boot().settle(T0);
    let b = w.b.boot_faulty(Arc::new(ScriptedFaults::after(b_crash)));
    carry(&w.a, &b, T0 + SECOND);
    let _ = b.advance_result(T0 + 2 * SECOND);
    drop(b);
    w.b.boot().advance(T0 + 3 * SECOND);
    let a = w.a.boot();
    carry(&w.b, &a, T0 + 4 * SECOND);
    a.advance(T0 + 5 * SECOND);
    w.b.boot().advance(T0 + 6 * SECOND);

    let label = format!("a crash after {a_crash}, b crash after {b_crash}");
    assert_eq!(a.case("req1").run.status, RunStatus::Completed, "{label}");
    assert_eq!(
        w.a.ledger.total(),
        1,
        "{label}: the requester's effect once"
    );
    assert_eq!(
        w.b.ledger.total(),
        2,
        "{label}: the approver's effects once each"
    );
    assert!(w.a.ledger.duplicates().is_empty() && w.b.ledger.duplicates().is_empty());
    // One message each way, whatever was replayed.
    let sent_a = w.a.outbox.live(None).unwrap().len();
    let all_a = w.a.outbox.all().unwrap();
    let all_b = w.b.outbox.all().unwrap();
    assert_eq!(
        all_a.len(),
        1,
        "{label}: one request queued ({sent_a} live)"
    );
    assert_eq!(all_b.len(), 1, "{label}: one approval queued");
    assert!(
        all_a.iter().all(|e| e.state == MessageState::Applied),
        "{label}"
    );
}

#[test]
fn a_crash_at_every_effect_point_of_the_requester_queues_one_request() {
    for n in 0..16 {
        crash_run(n, 10_000);
    }
}

#[test]
fn a_crash_at_every_effect_point_of_the_approver_starts_and_answers_once() {
    for n in 0..24 {
        crash_run(10_000, n);
    }
}

/// The approval is already in the requester's record when its wait node first runs (the send
/// node waited for delivery), so the wait takes the message in its handler. A crash at any
/// effect point of that turn replays the same attempt, which must take the same message and not
/// register a wait for one that never comes again.
#[test]
fn a_crash_after_the_wait_took_its_message_still_takes_the_approved_branch() {
    for n in 0..24 {
        let mut w = World::new();
        w.a.wait_for = spot_bpm_conversations::WaitFor::Applied;
        {
            let (a, b) = (w.a.boot(), w.b.boot());
            a.start_request("req1", "REF-1", T0);
            a.settle(T0);
            carry(&w.a, &b, T0 + SECOND);
            b.advance(T0 + 2 * SECOND);
            carry(&w.b, &a, T0 + 3 * SECOND);
        }
        {
            let a = w.a.boot_faulty(Arc::new(ScriptedFaults::after(n)));
            let _ = a.advance_result(T0 + 4 * SECOND);
        }
        let a = w.a.boot();
        a.advance(T0 + 5 * SECOND);
        // Past the wait's limit: a replay that lost the message would now take the timeout.
        a.advance(T0 + 2 * HOUR);
        let case = a.case("req1");
        assert_eq!(
            status(&case, "approved"),
            NodeStatus::Done,
            "crash after {n}"
        );
        assert_eq!(
            status(&case, "expired"),
            NodeStatus::Skipped,
            "crash after {n}"
        );
    }
}

/// The arrival was handled while the engine had not yet stored the wait, so nothing woke the wait
/// and the message stayed unconsumed. The time-driven pass must give the wait that message (the
/// approved branch), neither time out nor leave the request hanging.
#[test]
fn a_message_the_arrival_pass_could_not_deliver_is_given_to_the_wait_by_the_timer_pass() {
    let w = World::new();
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    carry(&w.a, &b, T0 + SECOND);
    b.advance(T0 + 2 * SECOND);
    carry(&w.b, &a, T0 + 3 * SECOND);
    // The approval is in the record; mark its arrival as already handled, which is what is left
    // when the wake found the engine not yet waiting.
    let mut record = w.a.conversations.records().pop().unwrap();
    assert!(record.unconsumed("approve").is_some());
    let ids: Vec<_> = record.events.iter().map(|event| event.id).collect();
    spot_bpm_conversations::update(
        &*w.a.conversations,
        &record.link,
        &record.conversation,
        |r| {
            r.arrivals_done.extend(ids.iter().copied());
            Ok(())
        },
    )
    .unwrap();
    record = w.a.conversations.records().pop().unwrap();
    assert!(record.unconsumed("approve").is_some());
    a.advance(T0 + 4 * SECOND);
    assert_eq!(
        status(&a.case("req1"), "wait_approval"),
        NodeStatus::Running
    );
    // Past the deadline the wait completes with the message.
    a.advance(T0 + 2 * HOUR);
    let case = a.case("req1");
    assert_eq!(status(&case, "approved"), NodeStatus::Done);
    assert_eq!(status(&case, "expired"), NodeStatus::Skipped);
}

/// Puts the engine's stored attempt of `node` into `state`, the way the engine's own commit
/// window (attempt stored, transition written) can leave it.
fn set_wait_state(booted: &Booted<'_>, case: &str, node: &str, state: AttemptState) {
    let id = spot_bpm_engine::CaseId::new(case).unwrap();
    let node_id = spot_dag::NodeId::new(node).unwrap();
    let record = booted.case(case);
    let mut attempt = record.attempt(&node_id).unwrap().clone();
    attempt.state = state;
    let at = attempt.started_at;
    block_on(async {
        booted
            .engine
            .store()
            .commit(
                &id,
                record.run.version,
                spot_bpm_engine::CaseUpdate::new()
                    .with_attempt(attempt)
                    .with_at(at),
            )
            .await
            .unwrap();
    });
}

/// The engine may not have stored its waiting attempt when the timer pass looks (the recorded wait
/// is written first). The pass must then leave both the wait and the message alone, count no
/// timeout, and finish the wait with that same message once the engine is waiting.
#[test]
fn a_wait_the_engine_is_not_ready_for_is_retried_by_the_next_timer_pass() {
    let w = World::new();
    let a = w.a.boot();
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    let (link, conversation) = conversation_for_case(&w.a, "req1");
    let b = w.b.boot();
    carry(&w.a, &b, T0 + SECOND);
    b.advance(T0 + 2 * SECOND);
    // The approval reaches the requester's record but no pass has consumed it yet.
    carry(&w.b, &a, T0 + 3 * SECOND);
    let record =
        w.a.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    let event = record.unconsumed("approve").expect("the approval arrived");
    let id = event.id;
    let wait = record
        .waits
        .iter()
        .find(|wait| wait.node == "wait_approval")
        .expect("the wait is registered")
        .clone();
    assert!(wait.due_ms.is_some_and(|due| due <= T0 + 2 * HOUR));

    // The engine has the attempt in flight: the arrival pass finds it not yet waiting.
    set_wait_state(&a, "req1", "wait_approval", AttemptState::Started);
    block_on(
        a.driver
            .process_arrivals(&*w.a.inbox, &*w.a.inbox, at(T0 + 4 * SECOND)),
    )
    .unwrap();
    let record =
        w.a.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    assert!(
        record.unconsumed("approve").is_some(),
        "nothing was consumed"
    );
    assert!(record.waits.contains(&wait), "the wait is still registered");

    // Past the deadline, still not ready: no timeout, nothing consumed, the wait stays.
    let report = block_on(a.driver.tick(at(T0 + 2 * HOUR))).unwrap();
    assert_eq!(report.timed_out, 0, "a wait the engine cannot complete yet");
    let record =
        w.a.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    assert!(record.unconsumed("approve").is_some());
    assert!(record.consumed.is_empty());
    assert!(record.consumed_by.is_empty());
    assert!(record.waits.contains(&wait));
    let ending = record.ending.as_ref().expect("the reply was recorded");
    assert_eq!(ending.outcome, "approved");
    assert!(!ending.timed_out, "the timer pass did not time it out");

    // The engine is waiting now: the same message completes the wait, exactly once.
    set_wait_state(&a, "req1", "wait_approval", AttemptState::Waiting);
    let report = block_on(a.driver.tick(at(T0 + 2 * HOUR))).unwrap();
    assert_eq!(report.timed_out, 0);
    let record =
        w.a.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    assert!(record.consumed.contains(&id), "the message was consumed");
    assert_eq!(
        record.consumed_by.get(&(wait.node.clone(), wait.attempt)),
        Some(&id),
        "the attempt took exactly that message"
    );
    assert!(record.waits.is_empty(), "the wait is finished");
    let ending = record.ending.as_ref().unwrap();
    assert_eq!(ending.outcome, "approved");
    assert!(!ending.timed_out);

    a.advance(T0 + 2 * HOUR + SECOND);
    let case = a.case("req1");
    assert_eq!(status(&case, "approved"), NodeStatus::Done);
    assert_eq!(status(&case, "expired"), NodeStatus::Skipped);
    assert_eq!(w.a.ledger.total(), 1);
}

/// The pass that resolved the timeout node may crash before the record's ending is written. The
/// next pass finds the wait gone from the engine (`Gone`) but still registered in the record, and
/// must write the ending the timeout names rather than leave the conversation open without it.
#[test]
fn an_ending_the_crash_window_lost_is_written_by_the_next_timer_pass() {
    let w = World::new();
    let a = w.a.boot();
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    let (link, conversation) = conversation_for_case(&w.a, "req1");
    let record =
        w.a.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    let wait = record
        .waits
        .iter()
        .find(|wait| wait.node == "wait_approval")
        .expect("the wait is registered")
        .clone();

    // The node is completed with the timeout branch; the record was never touched.
    let resolution = spot_bpm_conversations::Resolution::Done {
        output: Some(serde_json::json!({ "timed_out": true })),
        set: vec![(
            "bpm.wait.wait_approval.result".to_owned(),
            serde_json::json!("timeout"),
        )],
    };
    let resolved = block_on(spot_bpm_conversations::resolve_wait(
        &a.engine,
        "req1",
        &wait.node,
        wait.attempt,
        resolution,
        at(T0 + 2 * HOUR),
    ))
    .unwrap();
    assert_eq!(resolved, spot_bpm_conversations::Resolved::Completed);
    let record =
        w.a.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    assert!(record.waits.contains(&wait), "the record still waits");
    assert!(record.ending.is_none(), "and has no ending yet");

    // The next timer pass finds the wait Gone with the record's wait still registered.
    let report = block_on(a.driver.tick(at(T0 + 2 * HOUR))).unwrap();
    assert_eq!(report.timed_out, 1);
    let record =
        w.a.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    assert!(record.waits.is_empty());
    let ending = record.ending.as_ref().expect("the ending was written");
    assert_eq!(ending.outcome, "expired");
    assert!(ending.timed_out);
    assert_eq!(record.attention.as_ref().unwrap().code, codes::WAIT_TIMEOUT);
}
