//! Two simulated tenants hold a conversation under the request/approve contract: the happy path,
//! duplicate delivery, restarts, the visible state and the delivery requirement of a send.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use spot_bpm_conversations::{ConversationStore, Direction, ViewThresholds, WaitFor};
use spot_bpm_engine::CaseRecord;
use spot_dag_exec::{NodeStatus, RunStatus};

fn status(record: &CaseRecord, node: &str) -> NodeStatus {
    record
        .run
        .node(&spot_dag::NodeId::new(node).unwrap())
        .unwrap()
        .status
}

/// One full exchange: request out, approval back.
fn run_to_completion(w: &World) {
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    assert_eq!(carry(&w.a, &b, T0 + SECOND).len(), 1);
    b.advance(T0 + 2 * SECOND);
    assert_eq!(carry(&w.b, &a, T0 + 3 * SECOND).len(), 1);
    a.advance(T0 + 4 * SECOND);
}

#[test]
fn a_request_completes_across_two_tenants() {
    let w = World::new();
    run_to_completion(&w);
    let a = w.a.boot();
    let b = w.b.boot();

    let req = a.case("req1");
    assert_eq!(req.run.status, RunStatus::Completed);
    assert_eq!(status(&req, "approved"), NodeStatus::Done);
    assert_eq!(status(&req, "expired"), NodeStatus::Skipped);
    assert_eq!(
        w.a.ledger.total(),
        1,
        "the requester recorded the approval once"
    );
    assert_eq!(
        w.b.ledger.total(),
        2,
        "the approver recorded and posted once each"
    );
    assert!(w.a.ledger.duplicates().is_empty() && w.b.ledger.duplicates().is_empty());

    // Both sides agree on how it ended, and on what was said.
    let (link_a, conv_a) = conversation_for_case(&w.a, "req1");
    let view_a = a
        .driver
        .view(
            &link_a,
            &conv_a,
            at(T0 + 5 * SECOND),
            &ViewThresholds::default(),
        )
        .unwrap();
    assert_eq!(view_a.contract, "request_approve");
    assert_eq!(view_a.contract_version, 1);
    assert_eq!(view_a.peer, "approver");
    assert_eq!(view_a.last_sent.as_deref(), Some("request"));
    assert_eq!(view_a.last_received.as_deref(), Some("approve"));
    assert_eq!(view_a.ending.as_ref().unwrap().outcome, "approved");
    assert_eq!(view_a.messages[0].delivery, "applied");
    assert_eq!(view_a.messages[1].direction, Direction::Received);
    assert!(view_a.conditions.is_empty());

    // The approver holds the same conversation, on the same contract version, started by the
    // arrival of the request and ended in the same outcome.
    let record_b = w.b.conversations.load(&link_a, &conv_a).unwrap().unwrap();
    assert_eq!(record_b.role, "approver");
    assert_eq!(record_b.contract_version, 1);
    assert_eq!(record_b.ending.as_ref().unwrap().outcome, "approved");
    let case_b = record_b.case.clone().unwrap();
    let views_b = b
        .driver
        .views_of_case(&case_b, at(T0 + 5 * SECOND), &ViewThresholds::default())
        .unwrap();
    assert_eq!(views_b.len(), 1);
    assert_eq!(views_b[0].last_received.as_deref(), Some("request"));
    assert_eq!(views_b[0].last_sent.as_deref(), Some("approve"));
    assert_eq!(b.case(&case_b).run.status, RunStatus::Completed);
}

#[test]
fn a_duplicate_delivery_starts_nothing_twice() {
    let w = World::new();
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    let batch = carry(&w.a, &b, T0 + SECOND);
    assert_eq!(batch.len(), 1);
    // The carrier delivers the same message again, twice, with no state change in between.
    for n in 2..4 {
        deliver(&b, &w.a, &batch[0], T0 + n * SECOND);
    }
    b.advance(T0 + 5 * SECOND);
    carry(&w.b, &a, T0 + 6 * SECOND);
    // And again after the answer.
    deliver(&b, &w.a, &batch[0], T0 + 7 * SECOND);
    b.advance(T0 + 8 * SECOND);
    a.advance(T0 + 9 * SECOND);
    assert_eq!(w.b.ledger.total(), 2, "one approver request, not three");
    assert_eq!(w.a.ledger.total(), 1);
    assert_eq!(a.case("req1").run.status, RunStatus::Completed);
}

#[test]
fn the_conversation_survives_a_restart_of_each_side_at_every_step() {
    let w = World::new();
    // Each call to `boot` throws the engine, handlers and timer port away and rebuilds them from
    // the durable stores, which is what a restart is.
    {
        let a = w.a.boot();
        a.start_request("req1", "REF-1", T0);
    }
    w.a.boot().settle(T0);
    carry(&w.a, &w.b.boot(), T0 + SECOND);
    w.b.boot().advance(T0 + 2 * SECOND);
    w.b.boot().advance(T0 + 2 * SECOND);
    carry(&w.b, &w.a.boot(), T0 + 3 * SECOND);
    w.a.boot().advance(T0 + 4 * SECOND);
    w.a.boot().advance(T0 + 4 * SECOND);
    assert_eq!(w.a.boot().case("req1").run.status, RunStatus::Completed);
    assert_eq!(w.a.ledger.total(), 1);
    assert_eq!(w.b.ledger.total(), 2);
}

#[test]
fn a_send_waits_for_delivery_when_asked_and_continues_when_the_peer_applies() {
    let mut w = World::new();
    w.a.wait_for = WaitFor::Applied;
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    let case = a.case("req1");
    assert_eq!(
        status(&case, "send_request"),
        NodeStatus::Running,
        "waits for delivery"
    );
    assert_eq!(status(&case, "wait_approval"), NodeStatus::Pending);

    // Delivered and applied: the next tick lets the request go on to wait for the answer.
    carry(&w.a, &b, T0 + SECOND);
    a.advance(T0 + 2 * SECOND);
    let case = a.case("req1");
    assert_eq!(status(&case, "send_request"), NodeStatus::Done);
    assert_eq!(status(&case, "wait_approval"), NodeStatus::Running);
    b.advance(T0 + 3 * SECOND);
    carry(&w.b, &a, T0 + 4 * SECOND);
    a.advance(T0 + 5 * SECOND);
    assert_eq!(a.case("req1").run.status, RunStatus::Completed);
}

#[test]
fn an_unreachable_peer_is_a_condition_and_the_request_is_not_lost() {
    let w = World::new();
    let a = w.a.boot();
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    let (link, conv) = conversation_for_case(&w.a, "req1");
    // Nothing is delivered for two hours.
    let view = a
        .driver
        .view(&link, &conv, at(T0 + 2 * HOUR), &ViewThresholds::default())
        .unwrap();
    assert_eq!(view.messages[0].delivery, "queued");
    assert_eq!(view.conditions.len(), 1);
    assert_eq!(view.conditions[0].code, "PEER_UNREACHABLE");
    assert!(view.conditions[0].sentence.contains("approver"));
    assert!(
        view.needs_attention.is_none(),
        "unreachable is not an error"
    );
    assert_eq!(a.case("req1").run.status, RunStatus::Running);
}
