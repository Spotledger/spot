//! The durable notification outbox: atomicity with the task state, restart redelivery, backoff,
//! dead-lettering, duplicate suppression by key, a worker that dies mid-send, and two workers
//! that never double-claim.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{block_on, now, plus, spec};
use spot_bpm_tasks::outbox::{
    BackoffPolicy, DeliveryError, DeliveryFuture, DeliveryTransport, InMemoryOutbox,
    InMemoryOutboxTaskStore, MAX_ERROR_CHARS, NotifierTransport, OutboxStatus, OutboxStore,
    OutboxWorker, RunReport, TransactionalTaskStore, entries_for, idempotency_key,
};
use spot_bpm_tasks::{
    Notification, NotificationKind, RecordingNotifier, Task, TaskAction, TaskEvent, TaskId,
    TaskStore, TasksError,
};

fn notice(task: &str, kind: NotificationKind, recipients: &[&str]) -> Notification {
    Notification {
        task: TaskId::new(task).unwrap(),
        case: "case1".to_owned(),
        node: "approve".to_owned(),
        kind,
        run_singular: "Request".to_owned(),
        title: "Approve the order?".to_owned(),
        due: None,
        recipients: recipients.iter().map(|r| (*r).to_owned()).collect(),
    }
}

fn open(id: &str) -> (Task, TaskEvent) {
    let task = Task::open(spec(id, "case1", "approve", &["alice", "bob"]));
    let event = TaskEvent::new(&task.id, TaskAction::Created, "system", now());
    (task, event)
}

fn policy() -> BackoffPolicy {
    BackoffPolicy {
        base_secs: 30,
        cap_secs: 300,
        max_attempts: 4,
        lease_secs: 600,
        batch: 50,
    }
}

/// A scripted transport: the first `fail_first` deliveries fail, every call is recorded with its
/// key, and `downstream` models a channel that suppresses a repeated key.
#[derive(Default)]
struct Scripted {
    fail_first: AtomicUsize,
    calls: Mutex<Vec<(String, String)>>,
    downstream: Mutex<BTreeMap<String, usize>>,
}

impl Scripted {
    fn failing(times: usize) -> Arc<Self> {
        let transport = Self::default();
        transport.fail_first.store(times, Ordering::SeqCst);
        Arc::new(transport)
    }

    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }

    /// Distinct messages the channel actually delivered, by key.
    fn delivered(&self) -> BTreeMap<String, usize> {
        self.downstream.lock().unwrap().clone()
    }
}

impl DeliveryTransport for Scripted {
    fn deliver<'a>(&'a self, notification: &'a Notification, key: &'a str) -> DeliveryFuture<'a> {
        self.calls
            .lock()
            .unwrap()
            .push((key.to_owned(), notification.recipients.join(",")));
        let remaining = self.fail_first.load(Ordering::SeqCst);
        if remaining > 0 {
            self.fail_first.store(remaining - 1, Ordering::SeqCst);
            return Box::pin(std::future::ready(Err(DeliveryError::new(
                "mail server unreachable",
            ))));
        }
        // The downstream channel delivers a key once and ignores a repeat.
        self.downstream.lock().unwrap().insert(key.to_owned(), 1);
        Box::pin(std::future::ready(Ok(())))
    }
}

fn worker(id: &str, store: &InMemoryOutbox, transport: Arc<dyn DeliveryTransport>) -> OutboxWorker {
    OutboxWorker::new(id, Arc::new(store.clone()), transport, policy())
}

#[test]
fn an_entry_exists_exactly_when_the_state_change_commits() {
    let store = InMemoryOutboxTaskStore::new();
    let outbox = store.outbox();
    let (task, event) = open("t1");

    let created = store
        .create_with_outbox(
            task.clone(),
            event.clone(),
            &[notice("t1", NotificationKind::Assigned, &["alice", "bob"])],
            now(),
        )
        .unwrap();
    assert_eq!(created.version, 1);
    let keys: Vec<String> = outbox
        .list(None)
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect();
    assert_eq!(keys, vec!["t1:1:assigned:alice", "t1:1:assigned:bob"]);

    // A replayed create (the Decision node ran again after a crash) adds no entry.
    store
        .create_with_outbox(
            task,
            event,
            &[notice("t1", NotificationKind::Assigned, &["alice", "bob"])],
            plus(5),
        )
        .unwrap();
    assert_eq!(outbox.list(None).unwrap().len(), 2);

    // A commit that loses the version race stores nothing, and its entry is not written.
    let mut changed = created.clone();
    changed.title = "changed".to_owned();
    let lost = store.commit_with_outbox(
        &created.id,
        99,
        changed.clone(),
        None,
        &[notice("t1", NotificationKind::Reminder, &["alice"])],
        plus(10),
    );
    assert!(matches!(lost, Err(TasksError::VersionConflict { .. })));
    assert_eq!(outbox.list(None).unwrap().len(), 2);
    assert_eq!(
        store.load(&created.id).unwrap().unwrap().title,
        created.title
    );

    // A commit that wins stores the entry at the new version.
    let committed = store
        .commit_with_outbox(
            &created.id,
            1,
            changed,
            Some(TaskEvent::new(
                &created.id,
                TaskAction::Reminded,
                "system",
                plus(10),
            )),
            &[notice("t1", NotificationKind::Reminder, &["alice"])],
            plus(10),
        )
        .unwrap();
    assert_eq!(committed.version, 2);
    let reminder = outbox.get("t1:2:reminder:alice").unwrap().unwrap();
    assert_eq!(reminder.status, OutboxStatus::Pending);
    assert_eq!(reminder.created_at, plus(10));
    // The event and the entry arrived together: the `Reminded` event is in the trail.
    let trail = store.events(&created.id).unwrap();
    assert_eq!(trail.last().unwrap().action, TaskAction::Reminded);
}

#[test]
fn an_entry_carries_ids_and_titles_never_content() {
    let entries = entries_for(
        &notice("t1", NotificationKind::Assigned, &["alice"]),
        1,
        now(),
    );
    let rendered = format!("{:?}", entries[0]);
    assert!(rendered.contains("Approve the order?"));
    assert!(!rendered.contains("do-not-leak"));
    // One entry per distinct recipient, each holding only its own recipient.
    let many = entries_for(
        &notice("t1", NotificationKind::Assigned, &["bob", "alice", "bob"]),
        1,
        now(),
    );
    assert_eq!(many.len(), 2);
    assert!(
        many.iter()
            .all(|e| e.notification.recipients == vec![e.recipient.clone()])
    );
    assert_eq!(
        many[0].key,
        idempotency_key(&many[0].task, 1, NotificationKind::Assigned, "alice")
    );
}

#[test]
fn a_delivered_entry_is_sent_once_and_not_picked_again() {
    let outbox = InMemoryOutbox::new();
    outbox
        .enqueue(entries_for(
            &notice("t1", NotificationKind::Assigned, &["alice"]),
            1,
            now(),
        ))
        .unwrap();
    let transport = Scripted::failing(0);
    let w = worker("w1", &outbox, transport.clone());

    let report = block_on(w.run_once(now())).unwrap();
    assert_eq!((report.claimed, report.sent), (1, 1));
    let entry = outbox.get("t1:1:assigned:alice").unwrap().unwrap();
    assert_eq!(entry.status, OutboxStatus::Sent);
    assert_eq!(entry.sent_at, Some(now()));
    assert_eq!(entry.attempts, 1);
    assert!(entry.lease.is_none());

    let again = block_on(w.run_once(plus(1_000))).unwrap();
    assert_eq!(again.claimed, 0);
    assert_eq!(transport.calls().len(), 1);
}

#[test]
fn a_restart_redelivers_what_was_never_sent() {
    let store = InMemoryOutboxTaskStore::new();
    let (task, event) = open("t1");
    store
        .create_with_outbox(
            task,
            event,
            &[notice("t1", NotificationKind::Assigned, &["alice"])],
            now(),
        )
        .unwrap();

    // The host went down before any worker ran; a fresh worker over the same outbox sends it.
    let transport = Scripted::failing(0);
    let restarted = worker("after-restart", &store.outbox(), transport.clone());
    let report = block_on(restarted.run_once(plus(60))).unwrap();
    assert_eq!(report.sent, 1);
    assert_eq!(transport.calls()[0].0, "t1:1:assigned:alice");
}

#[test]
fn failures_back_off_with_a_doubling_capped_schedule_then_succeed() {
    let outbox = InMemoryOutbox::new();
    outbox
        .enqueue(entries_for(
            &notice("t1", NotificationKind::Escalated, &["mgr"]),
            3,
            now(),
        ))
        .unwrap();
    let transport = Scripted::failing(3);
    let w = worker("w1", &outbox, transport.clone());
    let key = "t1:3:escalated:mgr";

    // Attempt 1 fails at t0: next at +30.
    let r = block_on(w.run_once(now())).unwrap();
    assert_eq!(r.retried, 1);
    assert_eq!(outbox.get(key).unwrap().unwrap().next_attempt_at, plus(30));

    // Not due yet at +29: nothing is claimed.
    assert_eq!(block_on(w.run_once(plus(29))).unwrap().claimed, 0);

    // Attempt 2 fails at +30: next at +30+60.
    block_on(w.run_once(plus(30))).unwrap();
    assert_eq!(outbox.get(key).unwrap().unwrap().next_attempt_at, plus(90));

    // Attempt 3 fails at +90: next at +90+120.
    block_on(w.run_once(plus(90))).unwrap();
    let entry = outbox.get(key).unwrap().unwrap();
    assert_eq!(entry.next_attempt_at, plus(210));
    assert_eq!(entry.last_error.as_deref(), Some("mail server unreachable"));
    assert_eq!(entry.status, OutboxStatus::Pending);

    // Attempt 4 succeeds.
    let r = block_on(w.run_once(plus(210))).unwrap();
    assert_eq!(r.sent, 1);
    assert_eq!(outbox.get(key).unwrap().unwrap().status, OutboxStatus::Sent);
    assert_eq!(transport.calls().len(), 4);
}

#[test]
fn the_delay_never_exceeds_the_cap() {
    let outbox = InMemoryOutbox::new();
    outbox
        .enqueue(entries_for(
            &notice("t1", NotificationKind::Assigned, &["a"]),
            1,
            now(),
        ))
        .unwrap();
    let w = OutboxWorker::new(
        "w1",
        Arc::new(outbox.clone()),
        Scripted::failing(100),
        BackoffPolicy {
            max_attempts: 100,
            ..policy()
        },
    );
    let mut at = now();
    let mut previous = at;
    for _ in 0..8 {
        block_on(w.run_once(at)).unwrap();
        let next = outbox
            .get("t1:1:assigned:a")
            .unwrap()
            .unwrap()
            .next_attempt_at;
        assert!(next.unix_timestamp() - at.unix_timestamp() <= 300);
        assert!(next > previous);
        previous = next;
        at = next;
    }
}

#[test]
fn an_entry_that_keeps_failing_is_dead_and_visible() {
    let outbox = InMemoryOutbox::new();
    outbox
        .enqueue(entries_for(
            &notice("t1", NotificationKind::Assigned, &["alice"]),
            1,
            now(),
        ))
        .unwrap();
    let transport = Scripted::failing(100);
    let w = worker("w1", &outbox, transport.clone());
    let mut last = RunReport::default();
    for n in 0..4u64 {
        last = block_on(w.run_once(plus(n * 1_000))).unwrap();
    }
    assert_eq!(last.dead, 1, "the fourth failed attempt is the last");
    let dead = outbox.list(Some(OutboxStatus::Dead)).unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].attempts, 4);
    assert!(dead[0].lease.is_none());

    // A dead entry is never claimed again.
    let calls = transport.calls().len();
    block_on(w.run_once(plus(1_000_000))).unwrap();
    assert_eq!(transport.calls().len(), calls);
}

#[test]
fn a_long_failure_detail_is_cut_before_it_is_stored() {
    struct Chatty;
    impl DeliveryTransport for Chatty {
        fn deliver<'a>(&'a self, _n: &'a Notification, _k: &'a str) -> DeliveryFuture<'a> {
            Box::pin(std::future::ready(Err(DeliveryError::new(
                "x".repeat(MAX_ERROR_CHARS * 4),
            ))))
        }
    }
    let outbox = InMemoryOutbox::new();
    outbox
        .enqueue(entries_for(
            &notice("t1", NotificationKind::Assigned, &["a"]),
            1,
            now(),
        ))
        .unwrap();
    let w = worker("w1", &outbox, Arc::new(Chatty));
    block_on(w.run_once(now())).unwrap();
    let stored = outbox.get("t1:1:assigned:a").unwrap().unwrap();
    assert_eq!(stored.last_error.unwrap().chars().count(), MAX_ERROR_CHARS);
}

#[test]
fn a_duplicate_enqueue_is_ignored_and_a_duplicate_send_is_suppressed_by_key() {
    let outbox = InMemoryOutbox::new();
    let entries = entries_for(
        &notice("t1", NotificationKind::Assigned, &["alice"]),
        1,
        now(),
    );
    assert_eq!(outbox.enqueue(entries.clone()).unwrap(), 1);
    assert_eq!(outbox.enqueue(entries).unwrap(), 0);
    assert_eq!(outbox.list(None).unwrap().len(), 1);

    // The worker dies after the transport accepted the message but before the result was
    // recorded; the lease ends and the entry is sent again with the same key.
    let transport = Scripted::failing(0);
    let first = worker("w1", &outbox, transport.clone());
    let claimed = outbox.claim_due("w1", now(), plus(600), 10).unwrap();
    assert_eq!(claimed.len(), 1);
    block_on(transport.deliver(&claimed[0].notification, &claimed[0].key)).unwrap();
    drop(first); // never records the result

    let second = worker("w2", &outbox, transport.clone());
    let report = block_on(second.run_once(plus(601))).unwrap();
    assert_eq!(report.sent, 1);

    let calls = transport.calls();
    assert_eq!(
        calls.len(),
        2,
        "at least once: the message was handed over twice"
    );
    assert_eq!(calls[0].0, calls[1].0, "with the same idempotency key");
    assert_eq!(
        transport.delivered().len(),
        1,
        "so the channel delivers one message"
    );
}

#[test]
fn a_worker_that_dies_mid_send_loses_its_lease_and_cannot_overwrite_the_result() {
    let outbox = InMemoryOutbox::new();
    outbox
        .enqueue(entries_for(
            &notice("t1", NotificationKind::Assigned, &["alice"]),
            1,
            now(),
        ))
        .unwrap();
    let key = "t1:1:assigned:alice";

    // w1 claims and then is silent (a crash or a dropped future).
    let claimed = outbox.claim_due("w1", now(), plus(600), 10).unwrap();
    assert_eq!(claimed[0].attempts, 1);

    // While the lease is live nobody else gets the entry.
    assert!(
        outbox
            .claim_due("w2", plus(599), plus(1_199), 10)
            .unwrap()
            .is_empty()
    );

    // After it ends, w2 takes it over; attempts counts the dead attempt too.
    let taken = outbox.claim_due("w2", plus(600), plus(1_200), 10).unwrap();
    assert_eq!(taken[0].attempts, 2);

    // w1 wakes up late: its result is refused and changes nothing.
    assert!(!outbox.mark_sent(key, "w1", plus(700)).unwrap());
    assert!(!outbox.mark_failed(key, "w1", "late", None).unwrap());
    assert_eq!(
        outbox.get(key).unwrap().unwrap().status,
        OutboxStatus::Pending
    );

    // w2's result is recorded.
    assert!(outbox.mark_sent(key, "w2", plus(701)).unwrap());
    assert_eq!(outbox.get(key).unwrap().unwrap().status, OutboxStatus::Sent);
}

#[test]
fn an_entry_whose_every_attempt_died_is_dead_without_another_send() {
    let outbox = InMemoryOutbox::new();
    outbox
        .enqueue(entries_for(
            &notice("t1", NotificationKind::Assigned, &["alice"]),
            1,
            now(),
        ))
        .unwrap();
    // Four workers each claim it and vanish (max_attempts is 4).
    for n in 0..4u64 {
        let at = plus(n * 1_000);
        let got = outbox
            .claim_due(&format!("dead-{n}"), at, plus(n * 1_000 + 600), 10)
            .unwrap();
        assert_eq!(got.len(), 1);
    }
    let transport = Scripted::failing(0);
    let w = worker("w9", &outbox, transport.clone());
    let report = block_on(w.run_once(plus(10_000))).unwrap();
    assert_eq!((report.claimed, report.dead, report.sent), (1, 1, 0));
    assert!(transport.calls().is_empty());
    assert_eq!(outbox.list(Some(OutboxStatus::Dead)).unwrap().len(), 1);
}

#[test]
fn two_workers_never_claim_the_same_entry() {
    let outbox = InMemoryOutbox::new();
    let mut all = Vec::new();
    for n in 0..40 {
        all.extend(entries_for(
            &notice(&format!("t{n}"), NotificationKind::Assigned, &["alice"]),
            1,
            now(),
        ));
    }
    outbox.enqueue(all).unwrap();

    let claimed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..4)
        .map(|n| {
            let outbox = outbox.clone();
            let claimed = claimed.clone();
            std::thread::spawn(move || {
                let name = format!("w{n}");
                loop {
                    let got = outbox.claim_due(&name, now(), plus(600), 3).unwrap();
                    if got.is_empty() {
                        break;
                    }
                    let mut sink = claimed.lock().unwrap();
                    sink.extend(got.into_iter().map(|e| e.key));
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let keys = claimed.lock().unwrap().clone();
    let distinct: BTreeSet<&String> = keys.iter().collect();
    assert_eq!(keys.len(), 40);
    assert_eq!(distinct.len(), 40, "no entry was handed to two workers");
}

#[test]
fn the_oldest_due_entry_is_claimed_first() {
    let outbox = InMemoryOutbox::new();
    let mut entries = entries_for(
        &notice("late", NotificationKind::Assigned, &["a"]),
        1,
        plus(50),
    );
    entries.extend(entries_for(
        &notice("early", NotificationKind::Assigned, &["a"]),
        1,
        now(),
    ));
    outbox.enqueue(entries).unwrap();
    let got = outbox.claim_due("w", plus(100), plus(700), 1).unwrap();
    assert_eq!(got[0].task.to_string(), "early");
}

#[test]
fn a_notifier_can_be_the_transport() {
    let outbox = InMemoryOutbox::new();
    outbox
        .enqueue(entries_for(
            &notice("t1", NotificationKind::Assigned, &["alice", "bob"]),
            1,
            now(),
        ))
        .unwrap();
    let recorder = Arc::new(RecordingNotifier::new());
    let w = worker(
        "w1",
        &outbox,
        Arc::new(NotifierTransport::new(recorder.clone())),
    );
    assert_eq!(block_on(w.run_once(now())).unwrap().sent, 2);
    let sent = recorder.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].recipients, vec!["alice".to_owned()]);
    assert_eq!(sent[1].recipients, vec!["bob".to_owned()]);
}
