//! Integration tests for the SLA: reminders once, escalation on time, restart survival and the
//! final action.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::{block_on, now, plus, spec, world};
use spot_bpm_tasks::{
    BusinessCalendar, BusinessDaysCalendar, CountingMetrics, Escalation, EscalationTarget,
    FinalAction, InMemorySlaTimerPort, NotificationKind, Recipient, RecordingNotifier, Reminder,
    SLA_ACTOR, SlaPolicy, Task, TaskAction, TaskId, TaskService, TaskStatus, WallClockCalendar,
    human_approvers,
};

fn policy(
    reminders: Vec<Reminder>,
    escalations: Vec<Escalation>,
    final_action: FinalAction,
    final_after_seconds: Option<u64>,
) -> SlaPolicy {
    SlaPolicy {
        reminders,
        escalations,
        final_action,
        final_after_seconds,
        ..SlaPolicy::default()
    }
}

fn auto_approve_policy() -> SlaPolicy {
    SlaPolicy {
        final_action: FinalAction::AutoApprove,
        final_after_seconds: Some(300),
        auto_approve_declared: true,
        ..SlaPolicy::default()
    }
}

fn id(raw: &str) -> TaskId {
    TaskId::new(raw).unwrap()
}

#[test]
fn escalation_fires_at_the_exact_due_instant() {
    let w = world();
    let timers = Arc::new(InMemorySlaTimerPort::new());
    let service = Arc::new(
        TaskService::new(w.store.clone(), w.engine.clone())
            .with_directory(w.directory.clone())
            .with_timers(timers),
    );
    let mut task_spec = spec("t1", "case1", "approve", &["alice"]);
    task_spec.sla = Some(policy(
        Vec::new(),
        vec![Escalation {
            after_seconds: 3_600,
            to: EscalationTarget::User("mgr".to_owned()),
            notify: Vec::new(),
        }],
        FinalAction::None,
        None,
    ));
    service.open_task(task_spec).unwrap();

    assert!(
        block_on(service.tick(plus(3_599))).unwrap().is_empty(),
        "nothing fires one second early"
    );
    let fired = block_on(service.tick(plus(3_600))).unwrap();
    assert_eq!(
        fired.len(),
        1,
        "the escalation fires at the exact due instant"
    );

    let task = service.load(&id("t1")).unwrap();
    assert_eq!(task.assignees, BTreeSet::from(["mgr".to_owned()]));
    let event = service
        .events(&id("t1"))
        .unwrap()
        .into_iter()
        .find(|event| event.action == TaskAction::Escalated)
        .unwrap();
    assert_eq!(event.detail.as_deref(), Some("mgr"));
}

#[test]
fn reminders_fire_once() {
    let w = world();
    let notifier = Arc::new(RecordingNotifier::new());
    let timers = Arc::new(InMemorySlaTimerPort::new());
    let service = Arc::new(
        TaskService::new(w.store.clone(), w.engine.clone())
            .with_directory(w.directory.clone())
            .with_notifier(notifier.clone())
            .with_timers(timers),
    );
    let mut task_spec = spec("t1", "case1", "approve", &["alice"]);
    task_spec.sla = Some(policy(
        vec![Reminder {
            after_seconds: 60,
            to: vec![Recipient::Assignees],
        }],
        Vec::new(),
        FinalAction::None,
        None,
    ));
    service.open_task(task_spec).unwrap();

    assert_eq!(block_on(service.tick(plus(60))).unwrap().len(), 1);
    assert_eq!(notifier.count(), 2, "assigned plus one reminder");
    assert!(
        block_on(service.tick(plus(60))).unwrap().is_empty(),
        "the same reminder does not fire twice"
    );
    assert!(
        block_on(service.tick(plus(120))).unwrap().is_empty(),
        "and nothing else is due"
    );
    let reminded = service
        .events(&id("t1"))
        .unwrap()
        .into_iter()
        .filter(|event| event.action == TaskAction::Reminded)
        .count();
    assert_eq!(reminded, 1);
}

#[test]
fn restart_between_reminder_and_escalation() {
    let w = world();
    let timers = Arc::new(InMemorySlaTimerPort::new());
    let mut task_spec = spec("t1", "case1", "approve", &["alice"]);
    task_spec.sla = Some(policy(
        vec![Reminder {
            after_seconds: 60,
            to: vec![Recipient::Assignees],
        }],
        vec![Escalation {
            after_seconds: 120,
            to: EscalationTarget::User("mgr".to_owned()),
            notify: Vec::new(),
        }],
        FinalAction::None,
        None,
    ));

    {
        // The first service instance fires the reminder and registers the escalation.
        let service = Arc::new(
            TaskService::new(w.store.clone(), w.engine.clone())
                .with_directory(w.directory.clone())
                .with_timers(timers.clone()),
        );
        service.open_task(task_spec.clone()).unwrap();
        assert_eq!(block_on(service.tick(plus(60))).unwrap().len(), 1);
    }

    // A restart: a fresh service over the same store and timer port continues.
    let service = Arc::new(
        TaskService::new(w.store.clone(), w.engine.clone())
            .with_directory(w.directory.clone())
            .with_timers(timers.clone()),
    );
    let fired = block_on(service.tick(plus(120))).unwrap();
    assert_eq!(fired.len(), 1, "the escalation survived the restart");
    let task = service.load(&id("t1")).unwrap();
    assert_eq!(task.assignees, BTreeSet::from(["mgr".to_owned()]));

    let actions: Vec<_> = service
        .events(&id("t1"))
        .unwrap()
        .into_iter()
        .map(|event| event.action)
        .collect();
    assert_eq!(actions[0], TaskAction::Created);
    assert!(actions.contains(&TaskAction::Reminded));
    assert!(actions.contains(&TaskAction::Escalated));
}

#[test]
fn final_action_auto_approves_and_resumes_once() {
    let w = world();
    let metrics = Arc::new(CountingMetrics::new());
    let notifier = Arc::new(RecordingNotifier::new());
    let service = Arc::new(
        TaskService::new(w.store.clone(), w.engine.clone())
            .with_directory(w.directory.clone())
            .with_timers(Arc::new(InMemorySlaTimerPort::new()))
            .with_metrics(metrics.clone())
            .with_notifier(notifier.clone()),
    );
    let mut task_spec = spec("t1", "case1", "approve", &["alice"]);
    task_spec.sla = Some(auto_approve_policy());
    service.open_task(task_spec).unwrap();

    assert_eq!(block_on(service.tick(plus(300))).unwrap().len(), 1);
    let task = service.load(&id("t1")).unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.outcome, Some(spot_bpm_tasks::Decision::Approve));
    assert!(task.auto_completed);
    assert_eq!(w.engine.count(), 1, "the Case resumed exactly once");
    let signal = &w.engine.signals()[0];
    let payload = signal.payload.as_ref().unwrap();
    assert_eq!(payload["auto"], true);
    assert_eq!(payload["note"], "approved automatically by SLA");

    let events = service.events(&id("t1")).unwrap();
    let event = events
        .iter()
        .find(|event| event.action == TaskAction::AutoCompleted)
        .unwrap();
    assert_eq!(event.actor, SLA_ACTOR);
    assert!(event.auto);
    assert_eq!(event.sla_policy.as_deref(), Some("sla:approve"));
    assert_eq!(event.at, plus(300));
    assert!(
        human_approvers(&events).is_empty(),
        "the clock is not an approver"
    );

    assert_eq!(metrics.auto_approved_total(), 1);
    let sent = notifier.sent();
    let flagged = sent
        .iter()
        .find(|n| n.kind == NotificationKind::AutoApproved)
        .expect("the supervisor was told");
    assert_eq!(flagged.recipients, vec!["mgr".to_owned()]);
}

#[test]
fn auto_approve_on_an_undeclared_node_is_refused() {
    let w = world();
    let mut task_spec = spec("t1", "case1", "approve", &["alice"]);
    task_spec.sla = Some(SlaPolicy {
        auto_approve_declared: false,
        ..auto_approve_policy()
    });
    let error = w.service.open_task(task_spec).unwrap_err();
    assert_eq!(error.code(), "bpm_tasks.auto_approve_undeclared");
    assert!(w.service.load(&id("t1")).is_err(), "no task was created");
}

#[test]
fn auto_approve_on_a_segregated_node_is_refused_even_when_declared() {
    let w = world();
    let mut distinct = spec("t1", "case1", "approve", &["alice"]);
    distinct.sla = Some(auto_approve_policy());
    distinct.distinct_from_creator = true;
    assert_eq!(
        w.service.open_task(distinct).unwrap_err().code(),
        "bpm_tasks.auto_approve_with_segregation"
    );
    let mut duty = spec("t2", "case2", "approve", &["alice"]);
    duty.sla = Some(auto_approve_policy());
    duty.duty = Some("approve_payment".to_owned());
    assert_eq!(
        w.service.open_task(duty).unwrap_err().code(),
        "bpm_tasks.auto_approve_with_segregation"
    );
}

#[test]
fn auto_reject_needs_no_declaration_and_is_flagged_but_not_counted() {
    let w = world();
    let metrics = Arc::new(CountingMetrics::new());
    let service = TaskService::new(w.store.clone(), w.engine.clone())
        .with_directory(w.directory.clone())
        .with_metrics(metrics.clone());
    let mut task_spec = spec("t1", "case1", "approve", &["alice"]);
    task_spec.sla = Some(policy(
        Vec::new(),
        Vec::new(),
        FinalAction::AutoReject,
        Some(60),
    ));
    service.open_task(task_spec).unwrap();
    block_on(service.tick(plus(60))).unwrap();
    let events = service.events(&id("t1")).unwrap();
    let event = events
        .iter()
        .find(|event| event.action == TaskAction::AutoCompleted)
        .unwrap();
    assert!(event.auto);
    assert_eq!(event.actor, SLA_ACTOR);
    assert_eq!(metrics.auto_approved_total(), 0);
}

#[test]
fn a_later_two_person_requirement_is_not_met_by_an_automatic_approval() {
    let w = world();
    // Node one is approved by the clock; node two by one person.
    let mut first = spec("t1", "case1", "approve", &["alice"]);
    first.sla = Some(auto_approve_policy());
    w.service.open_task(first).unwrap();
    block_on(w.service.tick(plus(300))).unwrap();
    let second = spec("t2", "case1", "second", &["bob"]);
    w.service.open_task(second).unwrap();
    block_on(w.service.complete(
        &id("t2"),
        "bob",
        spot_bpm_tasks::Decision::Approve,
        None,
        plus(400),
    ))
    .unwrap();

    let mut events = w.service.events(&id("t1")).unwrap();
    events.extend(w.service.events(&id("t2")).unwrap());
    let approvers = human_approvers(&events);
    assert_eq!(approvers, BTreeSet::from(["bob".to_owned()]));
    assert!(approvers.len() < 2, "two people are still required");
}

#[test]
fn calendars_compute_due_instants() {
    let wall = WallClockCalendar;
    assert_eq!(wall.instant_after(now(), 3_600), plus(3_600));

    let business = BusinessDaysCalendar;
    // now() is a Tuesday; three days later is a Friday.
    let friday = plus(3 * 86_400);
    assert_eq!(friday.date().weekday().number_days_from_monday(), 4);
    let monday = business.instant_after(friday, 86_400);
    assert_eq!(
        monday.date().weekday().number_days_from_monday(),
        0,
        "a business day after Friday is Monday"
    );
}

/// A tiny guard that the policy's steps are ordered by offset.
#[test]
fn sla_steps_are_ordered() {
    let policy = policy(
        vec![
            Reminder {
                after_seconds: 120,
                to: vec![],
            },
            Reminder {
                after_seconds: 60,
                to: vec![],
            },
        ],
        vec![Escalation {
            after_seconds: 90,
            to: EscalationTarget::Manager,
            notify: vec![],
        }],
        FinalAction::AutoReject,
        Some(180),
    );
    let offsets: Vec<u64> = policy
        .steps()
        .iter()
        .map(|step| step.after_seconds)
        .collect();
    assert_eq!(offsets, vec![60, 90, 120, 180]);
    assert_eq!(policy.step(0).unwrap().after_seconds, 60);
    assert!(policy.step(4).is_none());
}

/// A helper used by the tests above to keep the `Task` import meaningful.
#[allow(dead_code)]
fn is_open(task: &Task) -> bool {
    task.is_open()
}

/// A notifier whose first send never finishes, like a mail server that hangs.
struct HangsOnce(std::sync::atomic::AtomicBool);

impl spot_bpm_tasks::Notifier for HangsOnce {
    fn notify<'a>(
        &'a self,
        _notification: &'a spot_bpm_tasks::Notification,
    ) -> spot_bpm_tasks::notify::NotifyFuture<'a> {
        let hang = !self.0.swap(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
        })
    }
}

#[test]
fn a_hung_notifier_cannot_make_a_reminder_fire_twice() {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    let w = world();
    let service = Arc::new(
        TaskService::new(w.store.clone(), w.engine.clone())
            .with_directory(w.directory.clone())
            .with_notifier(Arc::new(HangsOnce(std::sync::atomic::AtomicBool::new(
                false,
            ))))
            .with_timers(Arc::new(InMemorySlaTimerPort::new())),
    );
    let mut task_spec = spec("t1", "case1", "approve", &["alice"]);
    task_spec.sla = Some(policy(
        vec![Reminder {
            after_seconds: 60,
            to: vec![Recipient::Assignees],
        }],
        Vec::new(),
        FinalAction::None,
        None,
    ));
    service.open_task(task_spec).unwrap();
    {
        let mut tick = pin!(service.tick(plus(60)));
        let mut cx = Context::from_waker(Waker::noop());
        // The reminder is stored and the next step registered before the hung send starts.
        assert!(matches!(tick.as_mut().poll(&mut cx), Poll::Pending));
    }
    assert!(
        block_on(service.tick(plus(60))).unwrap().is_empty(),
        "the cancelled tick does not leave the reminder due"
    );
    let reminded = service
        .events(&id("t1"))
        .unwrap()
        .into_iter()
        .filter(|event| event.action == TaskAction::Reminded)
        .count();
    assert_eq!(reminded, 1);
}
