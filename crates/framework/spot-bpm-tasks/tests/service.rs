//! Integration tests for the task service: actions, audit trail, inbox, races, segregation and
//! the permission filter.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use common::{block_on, now, plus, spec, world};
use spot_bpm_tasks::{
    Decision::{Approve, Reject},
    DelegationRequest, DelegationScope, Notification, NotificationKind, Notifier,
    RecordingNotifier, SodSegregation, TaskEvent, TaskService, TaskStatus, TaskStore, TasksError,
    notify::NotifyFuture,
};

#[test]
fn completion_resumes_once_and_duplicates_are_idempotent() {
    let w = world();
    let task = w
        .service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();
    let completed = block_on(
        w.service
            .complete(&task.id, "alice", Approve, Some("ok"), now()),
    )
    .unwrap();
    assert_eq!(completed.status, TaskStatus::Completed);
    assert_eq!(completed.outcome, Some(Approve));
    assert_eq!(w.engine.count(), 1, "the Case resumed exactly once");

    // The same decision again is idempotent and does not signal again.
    let again = block_on(
        w.service
            .complete(&task.id, "alice", Approve, None, plus(1)),
    )
    .unwrap();
    assert_eq!(again.status, TaskStatus::Completed);
    assert_eq!(w.engine.count(), 1, "a duplicate does not resume again");

    // A different decision on a completed task is refused.
    let different = block_on(w.service.complete(&task.id, "alice", Reject, None, plus(2)));
    assert!(matches!(different, Err(TasksError::TaskNotOpen { .. })));

    let events = w.service.events(&task.id).unwrap();
    assert_eq!(events[0].action, spot_bpm_tasks::TaskAction::Created);
    assert_eq!(events[1].action, spot_bpm_tasks::TaskAction::Completed);
    assert_eq!(events[1].actor, "alice");
    assert_eq!(events[1].branch.as_deref(), Some("approve"));
    assert_eq!(events[1].reason.as_deref(), Some("ok"));
}

#[test]
fn claim_and_release_are_audited() {
    let w = world();
    let task = w
        .service
        .open_task(spec("t1", "case1", "approve", &["alice", "bob"]))
        .unwrap();

    let claimed = w.service.claim(&task.id, "bob", now()).unwrap();
    assert_eq!(claimed.status, TaskStatus::Claimed);
    assert_eq!(claimed.claimed_by.as_deref(), Some("bob"));

    // Alice may not steal a claim Bob holds.
    let steal = w.service.claim(&task.id, "alice", now());
    assert!(matches!(steal, Err(TasksError::TaskNotOpen { .. })));

    let released = w.service.release(&task.id, "bob", now()).unwrap();
    assert_eq!(released.status, TaskStatus::Open);
    assert_eq!(released.claimed_by, None);

    // A non-claimer who is not a manager cannot release.
    let wrong = w.service.release(&task.id, "alice", now());
    assert!(matches!(wrong, Err(TasksError::NotAssignee { .. })));

    let actions: Vec<_> = w
        .service
        .events(&task.id)
        .unwrap()
        .into_iter()
        .map(|event| event.action)
        .collect();
    assert!(actions.contains(&spot_bpm_tasks::TaskAction::Claimed));
    assert!(actions.contains(&spot_bpm_tasks::TaskAction::Released));
}

#[test]
fn delegation_chain_keeps_the_audit_trail() {
    let w = world();
    let task = w
        .service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();

    let alice_to_bob = block_on(w.service.delegate(
        &task.id,
        "alice",
        DelegationRequest {
            to: "bob".to_owned(),
            scope: DelegationScope::All,
            expires_at: None,
            reason: Some("on leave".to_owned()),
        },
        now(),
    ))
    .unwrap();
    assert_eq!(alice_to_bob.delegation.as_ref().unwrap().to, "bob");

    let bob_to_carol = block_on(w.service.delegate(
        &task.id,
        "bob",
        DelegationRequest {
            to: "carol".to_owned(),
            scope: DelegationScope::Complete,
            expires_at: None,
            reason: None,
        },
        plus(1),
    ))
    .unwrap();
    assert_eq!(bob_to_carol.delegation.as_ref().unwrap().by, "bob");

    let completed = block_on(
        w.service
            .complete(&task.id, "carol", Approve, None, plus(2)),
    )
    .unwrap();
    assert_eq!(completed.status, TaskStatus::Completed);

    let events = w.service.events(&task.id).unwrap();
    let delegated: Vec<&TaskEvent> = events
        .iter()
        .filter(|event| event.action == spot_bpm_tasks::TaskAction::Delegated)
        .collect();
    assert_eq!(delegated.len(), 2, "both delegations are recorded");
    assert_eq!(delegated[0].actor, "alice");
    assert_eq!(delegated[0].detail.as_deref(), Some("bob"));
    assert_eq!(delegated[1].actor, "bob");
    assert_eq!(delegated[1].detail.as_deref(), Some("carol"));

    let completed_event = events
        .iter()
        .find(|event| event.action == spot_bpm_tasks::TaskAction::Completed)
        .unwrap();
    assert_eq!(completed_event.actor, "carol");
    assert_eq!(
        completed_event.on_behalf_of.as_deref(),
        Some("bob"),
        "who decided on whose behalf"
    );
}

#[test]
fn an_expired_delegation_is_refused() {
    let w = world();
    let task = w
        .service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();
    block_on(w.service.delegate(
        &task.id,
        "alice",
        DelegationRequest {
            to: "bob".to_owned(),
            scope: DelegationScope::All,
            expires_at: Some(plus(10)),
            reason: None,
        },
        now(),
    ))
    .unwrap();
    let result = block_on(w.service.complete(&task.id, "bob", Approve, None, plus(11)));
    assert!(matches!(result, Err(TasksError::DelegationExpired { .. })));
}

#[test]
fn reassignment_after_absence_is_audited() {
    let w = world();
    let task = w
        .service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();

    // A non-manager cannot reassign.
    let refused = block_on(
        w.service
            .reassign(&task.id, "bob", "dave", "please take it", now()),
    );
    assert!(matches!(refused, Err(TasksError::NotSupervisor { .. })));

    let reassigned =
        block_on(
            w.service
                .reassign(&task.id, "mgr", "dave", "alice is on leave", plus(1)),
        )
        .unwrap();
    assert_eq!(
        reassigned.assignees,
        BTreeSet::from(["dave".to_owned()]),
        "the task moved to dave"
    );
    let event = w
        .service
        .events(&task.id)
        .unwrap()
        .into_iter()
        .find(|event| event.action == spot_bpm_tasks::TaskAction::Reassigned)
        .unwrap();
    assert_eq!(event.actor, "mgr");
    assert_eq!(event.detail.as_deref(), Some("dave"));
    assert_eq!(event.reason.as_deref(), Some("alice is on leave"));
}

#[test]
fn a_completion_race_has_one_winner() {
    let w = world();
    let task = w
        .service
        .open_task(spec("t1", "case1", "approve", &["alice", "bob"]))
        .unwrap();

    // Two readers both hold version 1; the store lets exactly one commit win.
    let first = w.store.load(&task.id).unwrap().unwrap();
    let second = w.store.load(&task.id).unwrap().unwrap();
    let mut changed = first.clone();
    changed.status = TaskStatus::Claimed;
    changed.claimed_by = Some("alice".to_owned());
    let event = TaskEvent::new(
        &task.id,
        spot_bpm_tasks::TaskAction::Claimed,
        "alice",
        now(),
    );
    w.store
        .commit(&task.id, first.version, changed, Some(event))
        .unwrap();
    let mut other = second.clone();
    other.status = TaskStatus::Claimed;
    other.claimed_by = Some("bob".to_owned());
    let event = TaskEvent::new(&task.id, spot_bpm_tasks::TaskAction::Claimed, "bob", now());
    let loser = w.store.commit(&task.id, second.version, other, Some(event));
    assert!(matches!(loser, Err(TasksError::VersionConflict { .. })));

    // At the service level, alice decides and bob's different decision is refused.
    let completed = block_on(w.service.complete(&task.id, "alice", Approve, None, now())).unwrap();
    assert_eq!(completed.status, TaskStatus::Completed);
    let bob = block_on(w.service.complete(&task.id, "bob", Reject, None, plus(1)));
    assert!(matches!(bob, Err(TasksError::TaskNotOpen { .. })));
    assert_eq!(w.engine.count(), 1, "only one decision resumed the Case");
}

#[test]
fn inbox_queries_filter_by_access() {
    let w = world();
    w.service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();
    w.service
        .open_task(spec("t2", "case2", "approve", &["bob"]))
        .unwrap();
    w.service
        .open_task(spec("t3", "case3", "approve", &["carol"]))
        .unwrap();

    let alice = block_on(w.service.my_tasks("alice", now())).unwrap();
    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0].id.as_str(), "t1");

    // The manager sees alice's task in the team inbox, nobody else's.
    let team = w.service.team_tasks("mgr", now()).unwrap();
    assert_eq!(team.len(), 1);
    assert_eq!(team[0].id.as_str(), "t1");

    // A user with no task sees nothing.
    assert!(
        block_on(w.service.my_tasks("nobody", now()))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn by_process_and_overdue_queries() {
    let w = world();
    let mut first = spec("t1", "case1", "approve", &["alice"]);
    first.due = Some(plus(10));
    w.service.open_task(first).unwrap();
    let mut later = spec("t2", "case2", "approve", &["alice"]);
    later.process = "other".to_owned();
    later.due = Some(plus(1_000));
    w.service.open_task(later).unwrap();

    let by_process = block_on(w.service.by_process("alice", "approval", now())).unwrap();
    assert_eq!(by_process.len(), 1);
    assert_eq!(by_process[0].id.as_str(), "t1");

    let overdue = block_on(w.service.overdue("alice", plus(20))).unwrap();
    assert_eq!(overdue.len(), 1);
    assert_eq!(overdue[0].id.as_str(), "t1");

    assert!(
        block_on(w.service.overdue("alice", now()))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn creator_distinctness_blocks_the_creator() {
    let w = world();
    let mut task_spec = spec("t1", "case1", "approve", &["alice", "bob"]);
    task_spec.distinct_from_creator = true;
    task_spec.created_by = Some("alice".to_owned());
    let task = w.service.open_task(task_spec).unwrap();

    let refused = block_on(w.service.complete(&task.id, "alice", Approve, None, now()));
    assert!(matches!(
        refused,
        Err(TasksError::DistinctFromCreator { .. })
    ));
    let allowed = block_on(w.service.complete(&task.id, "bob", Approve, None, now()));
    assert!(allowed.is_ok());
}

#[test]
fn segregation_of_duties_blocks_a_conflicting_decision() {
    let rules = spot_sod::RuleSet::from_toml(
        r#"
[[rules]]
id = "sod.self_approve"
title = "No self approval"
description = "Creating and approving is a conflict"
severity = "high"
sides = [{ duties = ["create"] }, { duties = ["approve"] }]
"#,
    )
    .unwrap();
    let catalogue = spot_sod::RoleCatalogue::new(Vec::new());
    let existing = vec![spot_sod::Assignment::duty("bob", "create", None)];
    let gate = Arc::new(SodSegregation::new(rules, catalogue, existing, Vec::new()));

    let store = Arc::new(spot_bpm_tasks::InMemoryTaskStore::new());
    let engine = Arc::new(spot_bpm_tasks::RecordingEngine::new());
    let service = TaskService::new(store.clone(), engine).with_segregation(gate);
    let mut task_spec = spec("t1", "case1", "approve", &["bob"]);
    task_spec.duty = Some("approve".to_owned());
    let task = service.open_task(task_spec).unwrap();

    let refused = block_on(service.complete(&task.id, "bob", Approve, None, now()));
    match refused {
        Err(TasksError::SegregationBlocked { detail, .. }) => {
            assert!(detail.contains("sod.self_approve"), "{detail}");
        }
        other => panic!("expected a segregation refusal, got {other:?}"),
    }
}

#[test]
fn notifications_carry_only_ids_and_titles() {
    let w = world();
    let notifier = Arc::new(RecordingNotifier::new());
    let service = Arc::new(
        TaskService::new(w.store.clone(), w.engine.clone())
            .with_directory(w.directory.clone())
            .with_notifier(notifier.clone()),
    );
    let task = service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();
    block_on(service.complete(&task.id, "alice", Approve, None, now())).unwrap();

    let sent = notifier.sent();
    assert!(!sent.is_empty());
    let completion = sent
        .iter()
        .find(|n| n.kind == NotificationKind::Completed)
        .unwrap();
    assert_eq!(completion.task, task.id);
    assert_eq!(completion.title, "Approve the order?");
    assert_eq!(completion.recipients, vec!["alice".to_owned()]);
    // The form payload's secret value never reaches a notification.
    let rendered = format!("{completion:?}");
    assert!(!rendered.contains("do-not-leak"), "{rendered}");
}

/// A notifier that yields `yields` times before it records, to stand for a slow channel.
struct SlowNotifier {
    yields: usize,
    delivered: AtomicUsize,
    started: AtomicUsize,
}

struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl Notifier for SlowNotifier {
    fn notify<'a>(&'a self, _notification: &'a Notification) -> NotifyFuture<'a> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            for _ in 0..self.yields {
                YieldNow(false).await;
            }
            self.delivered.fetch_add(1, Ordering::SeqCst);
        })
    }
}

fn slow_world(yields: usize) -> (common::World, Arc<SlowNotifier>) {
    let w = world();
    let notifier = Arc::new(SlowNotifier {
        yields,
        delivered: AtomicUsize::new(0),
        started: AtomicUsize::new(0),
    });
    let service = TaskService::new(w.store.clone(), w.engine.clone())
        .with_directory(w.directory.clone())
        .with_notifier(notifier.clone());
    (
        common::World {
            service: Arc::new(service),
            ..w
        },
        notifier,
    )
}

#[test]
fn a_slow_notifier_still_lets_complete_return_the_stored_task() {
    let (w, notifier) = slow_world(1_000);
    let task = w
        .service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();
    let completed = block_on(w.service.complete(&task.id, "alice", Approve, None, now())).unwrap();
    assert_eq!(completed.status, TaskStatus::Completed);
    assert_eq!(w.service.load(&task.id).unwrap().status, completed.status);
    assert_eq!(w.engine.count(), 1, "the Case was resumed");
    assert_eq!(
        notifier.delivered.load(Ordering::SeqCst),
        2,
        "the queued assignment and the completion both went out"
    );
}

#[test]
fn dropping_an_action_mid_send_keeps_the_stored_state() {
    let (w, notifier) = slow_world(1_000);
    let task = w
        .service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();
    {
        let mut action = pin!(w.service.complete(&task.id, "alice", Approve, None, now()));
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..5 {
            assert!(action.as_mut().poll(&mut cx).is_pending());
        }
        // The action future is dropped here, in the middle of the notifier's send.
    }
    assert_eq!(notifier.started.load(Ordering::SeqCst), 1);
    assert_eq!(notifier.delivered.load(Ordering::SeqCst), 0);
    let stored = w.service.load(&task.id).unwrap();
    assert_eq!(stored.status, TaskStatus::Completed);
    // Nothing is stuck: the service still works afterwards.
    block_on(w.service.flush_notifications());
    let again = block_on(w.service.complete(&task.id, "alice", Approve, None, now())).unwrap();
    assert_eq!(again.id, task.id);
}

#[test]
fn queued_assignments_go_out_on_flush_in_order() {
    let w = world();
    let notifier = Arc::new(RecordingNotifier::new());
    let service = TaskService::new(w.store.clone(), w.engine.clone())
        .with_directory(w.directory.clone())
        .with_notifier(notifier.clone());
    service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();
    service
        .open_task(spec("t2", "case2", "approve", &["bob"]))
        .unwrap();
    assert_eq!(
        notifier.count(),
        0,
        "opening a task never waits on a notifier"
    );
    block_on(service.flush_notifications());
    let sent = notifier.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].recipients, vec!["alice".to_owned()]);
    assert_eq!(sent[1].recipients, vec!["bob".to_owned()]);
    block_on(service.flush_notifications());
    assert_eq!(notifier.count(), 2, "a flush sends each notification once");
}

#[test]
fn the_pending_queue_is_bounded_when_the_host_never_flushes() {
    let w = world();
    let notifier = Arc::new(RecordingNotifier::new());
    let service = TaskService::new(w.store.clone(), w.engine.clone())
        .with_directory(w.directory.clone())
        .with_notifier(notifier.clone());
    for index in 0..1_100 {
        service
            .open_task(spec(
                &format!("t{index}"),
                &format!("c{index}"),
                "approve",
                &["alice"],
            ))
            .unwrap();
    }
    block_on(service.flush_notifications());
    let sent = notifier.sent();
    assert_eq!(sent.len(), 1_024, "the queue keeps a bounded number");
    assert_eq!(
        sent.last().unwrap().task.as_str(),
        "t1099",
        "the newest are kept, the oldest dropped"
    );
}
