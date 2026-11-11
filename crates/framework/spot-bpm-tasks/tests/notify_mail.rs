//! The e-mail notifier awaits the mail transport: a slow transport is driven by the executor,
//! not by a spin loop, and a failing one never fails the action that caused the notification.
#![cfg(feature = "mail")]
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use common::{now, spec, world};
use spot_bpm_tasks::Decision::Approve;
use spot_bpm_tasks::notify::mail::MailNotifier;
use spot_bpm_tasks::{Notification, NotificationKind, Notifier, TaskId, TaskService};
use spot_mail::{
    MailError, MailTransport, Mailbox, OutgoingMessage, PermanentFailure, SendReceipt,
};

/// Yields once, waking itself, so it needs an executor that polls again.
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

/// A transport that yields `yields` times before it answers, and records what it was given.
struct FakeTransport {
    yields: usize,
    fail: bool,
    sent: Mutex<Vec<OutgoingMessage>>,
    finished: AtomicUsize,
}

impl FakeTransport {
    fn new(yields: usize, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            yields,
            fail,
            sent: Mutex::new(Vec::new()),
            finished: AtomicUsize::new(0),
        })
    }

    fn messages(&self) -> Vec<OutgoingMessage> {
        self.sent.lock().unwrap().clone()
    }
}

/// The shared handle the notifier owns, so the test keeps its own view of the transport.
struct Handle(Arc<FakeTransport>);

impl MailTransport for Handle {
    fn send<'a>(
        &'a self,
        message: &'a OutgoingMessage,
    ) -> Pin<Box<dyn Future<Output = Result<SendReceipt, MailError>> + Send + 'a>> {
        Box::pin(async move {
            let this = &self.0;
            for _ in 0..this.yields {
                YieldNow(false).await;
            }
            this.sent.lock().unwrap().push(message.clone());
            this.finished.fetch_add(1, Ordering::SeqCst);
            if this.fail {
                return Err(PermanentFailure::Client("refused".to_owned()).into());
            }
            Ok(SendReceipt {
                message_id: "id".to_owned(),
                accepted: Vec::new(),
                rejected: Vec::new(),
            })
        })
    }
}

/// A plain single-threaded executor: polls, and re-polls only when woken. It counts polls, so a
/// spin loop inside the future under test would show as an absurd count of one poll.
fn run<F: Future>(future: F) -> (F::Output, usize) {
    use std::sync::atomic::AtomicBool;
    use std::task::Wake;
    struct Flag(AtomicBool);
    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let flag = Arc::new(Flag(AtomicBool::new(true)));
    let waker = Waker::from(Arc::clone(&flag));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    let mut polls = 0;
    loop {
        assert!(
            flag.0.swap(false, Ordering::SeqCst),
            "the future is pending and was never woken"
        );
        polls += 1;
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return (value, polls);
        }
    }
}

fn notifier(transport: &Arc<FakeTransport>) -> MailNotifier<Handle> {
    MailNotifier::new(
        Handle(Arc::clone(transport)),
        Mailbox::new(None, "noreply@example.test").unwrap(),
        "example.test",
    )
}

fn notification(recipients: &[&str]) -> Notification {
    Notification {
        task: TaskId::new("t1").unwrap(),
        case: "case1".to_owned(),
        node: "approve".to_owned(),
        kind: NotificationKind::Assigned,
        run_singular: "Request".to_owned(),
        title: "Approve the order?".to_owned(),
        due: None,
        recipients: recipients.iter().map(|r| (*r).to_owned()).collect(),
    }
}

#[test]
fn a_transport_that_yields_a_thousand_times_still_delivers() {
    let transport = FakeTransport::new(1_000, false);
    let mail = notifier(&transport);
    let item = notification(&["alice"]);
    let ((), polls) = run(mail.notify(&item));
    assert_eq!(transport.messages().len(), 1);
    assert_eq!(polls, 1_001, "the executor polled once per yield, no spin");
}

#[test]
fn a_failing_transport_does_not_fail_the_action() {
    let w = world();
    let transport = FakeTransport::new(3, true);
    let service = TaskService::new(w.store.clone(), w.engine.clone())
        .with_directory(w.directory.clone())
        .with_notifier(Arc::new(notifier(&transport)));
    let task = service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();
    let (result, _) = run(service.complete(&task.id, "alice", Approve, None, now()));
    let completed = result.unwrap();
    assert_eq!(service.load(&task.id).unwrap().status, completed.status);
    assert_eq!(transport.finished.load(Ordering::SeqCst), 2, "tried both");
}

#[test]
fn an_invalid_recipient_is_skipped_and_the_valid_one_gets_the_message() {
    let transport = FakeTransport::new(0, false);
    let mail = notifier(&transport);
    let item = notification(&["alice", "not an address"]);
    run(mail.notify(&item));
    let sent = transport.messages();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to.len(), 1);
    assert_eq!(sent[0].to[0].address.to_string(), "alice@example.test");
}

#[test]
fn a_notification_with_no_valid_recipient_sends_nothing() {
    let transport = FakeTransport::new(0, false);
    let mail = notifier(&transport);
    run(mail.notify(&notification(&["not an address"])));
    assert!(transport.messages().is_empty());
}

#[test]
fn the_body_carries_ids_and_no_form_value() {
    let w = world();
    let transport = FakeTransport::new(0, false);
    let service = TaskService::new(w.store.clone(), w.engine.clone())
        .with_directory(w.directory.clone())
        .with_notifier(Arc::new(notifier(&transport)));
    let mut task_spec = spec("t1", "case1", "approve", &["alice"]);
    task_spec.form = serde_json::json!({"secret": "do-not-leak"});
    let task = service.open_task(task_spec).unwrap();
    run(service.complete(&task.id, "alice", Approve, Some("private comment"), now()))
        .0
        .unwrap();
    let sent = transport.messages();
    assert!(!sent.is_empty());
    for message in sent {
        let text = format!("{message:?}");
        assert!(!text.contains("do-not-leak"), "{text}");
        assert!(!text.contains("private comment"), "{text}");
        assert!(message.text.unwrap().contains("task t1"));
    }
}

#[test]
fn dropping_the_send_midway_is_safe() {
    let transport = FakeTransport::new(1_000, false);
    let mail = notifier(&transport);
    let item = notification(&["alice"]);
    {
        let mut send = pin!(mail.notify(&item));
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..10 {
            assert!(send.as_mut().poll(&mut cx).is_pending());
        }
    }
    assert!(transport.messages().is_empty(), "nothing was half-sent");
    // The notifier is still usable.
    run(mail.notify(&item));
    assert_eq!(transport.messages().len(), 1);
}
