//! A mail job killed on its last attempt (timeout or panic) ends with one bounce per recipient,
//! through the worker the CLI jobs service builds and the hook the mail service hands out
//! (WP-PLT-11.T9). Nothing here needs a database: the store, the sent log and the transport are
//! the in-memory ones.
//!
//! The in-memory job store leaves a job's attempt number at zero (only a reaped lease advances
//! it), so a "last attempt" here is a queue policy with `max_attempts = 0`, which the engine
//! reads as `attempt >= max_attempts`; "not the last attempt" is a policy of two.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spot_cli::config;
use spot_cli::services::jobs::{FinalAttemptHooks, build_worker};
use spot_cli::services::mail::{Mail, Settings};
use spot_core::{ManualClock, SiteId};
use spot_files::memory::MemoryFileStore;
use spot_jobs::model::{Job, JobState};
use spot_jobs::worker::{BoxFuture, FinalAttemptHook, HandlerResult, JobHandlerRegistry};
use spot_jobs::{InMemoryJobStore, JobStore, QueuePolicy};
use spot_mail::SendReceipt;
use spot_mail_queue::{
    BounceClass, InMemorySentLog, QueuedMailbox, QueuedMessage, SentLog, enqueue_mail,
};
use time::OffsetDateTime;
use tokio::sync::watch;

const MESSAGE_ID: &str = "msg-bounce-1@example.test";

fn site() -> SiteId {
    SiteId::new("acme").unwrap()
}

fn now() -> OffsetDateTime {
    time::macros::datetime!(2026-11-11 08:00 UTC)
}

fn message() -> QueuedMessage {
    let mut m = QueuedMessage::new(
        QueuedMailbox::address("noreply@localhost.invalid"),
        "Invoice",
    );
    m.to = vec![QueuedMailbox::address("buyer@customer.test")];
    m.cc = vec![QueuedMailbox::address("boss@customer.test")];
    m.text = Some("Body".to_owned());
    m
}

#[derive(Clone, Copy)]
enum Kill {
    Timeout,
    Panic,
}

struct Rig {
    store: Arc<InMemoryJobStore>,
    mail: Arc<Mail>,
    sent_log: Arc<InMemorySentLog>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Rig {
    /// A worker over `policy` whose `platform:send_mail` handler is killed the way `kill`
    /// says. `register_hook` is the line production relies on: the mail hook registered with
    /// the hook set the worker is built with.
    async fn start(kill: Kill, max_attempts: u32, register_hook: bool) -> Self {
        let sent_log = Arc::new(InMemorySentLog::new());
        let clock = Arc::new(ManualClock::new(now()));
        let mail = Mail::new(Settings {
            site: site(),
            mail: config::Mail::default(),
            files: Arc::new(MemoryFileStore::new()),
            sent_log: Some(Arc::clone(&sent_log) as Arc<dyn SentLog>),
            clock,
        });
        let hooks = FinalAttemptHooks::new();
        if register_hook {
            hooks.register(mail.final_attempt_hook());
        }
        let mut registry = JobHandlerRegistry::new();
        registry.register(spot_mail_queue::MAIL_JOB_NAME, move |_ctx| {
            Box::pin(async move {
                match kill {
                    Kill::Timeout => {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        Ok(HandlerResult::Ok)
                    }
                    Kill::Panic => panic!("the handler panics"),
                }
            })
        });
        let policy = QueuePolicy {
            timeout: Duration::from_millis(150),
            max_attempts,
            ..QueuePolicy::default()
        };
        let store = Arc::new(InMemoryJobStore::new());
        let worker = build_worker(
            Arc::clone(&store) as Arc<dyn JobStore>,
            Arc::new(registry),
            HashMap::from([("default".to_owned(), policy)]),
            Arc::new(spot_core::SystemClock::new()),
            hooks,
        );
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let _ = worker
                .run("w1", vec!["default".to_owned()], rx, Duration::from_secs(1))
                .await;
        });
        Self {
            store,
            mail,
            sent_log,
            stop,
            task,
        }
    }

    async fn enqueue(&self) -> spot_jobs::JobId {
        let job = enqueue_mail(site(), &message(), "default", MESSAGE_ID, now()).unwrap();
        match self.store.insert_if_new(job).await.unwrap() {
            spot_jobs::model::InsertResult::Inserted(id)
            | spot_jobs::model::InsertResult::Duplicate(id) => id,
        }
    }

    /// Waits until the job is no longer pending or running, and returns its state.
    async fn settled(&self, id: spot_jobs::JobId) -> JobState {
        for _ in 0..200 {
            let job = self.store.get(id).await.unwrap().unwrap();
            if !matches!(job.state, JobState::Pending | JobState::Running) {
                return job.state;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the job never settled");
    }

    async fn finish(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

fn recipients(mail: &Mail) -> Vec<String> {
    let mut found: Vec<String> = mail.bounces().iter().map(|b| b.recipient.clone()).collect();
    found.sort();
    found
}

async fn one_bounce_per_recipient(kill: Kill) {
    let rig = Rig::start(kill, 0, true).await;
    let id = rig.enqueue().await;
    assert_eq!(rig.settled(id).await, JobState::Dead);
    let bounces = rig.mail.bounces();
    assert_eq!(
        recipients(&rig.mail),
        ["boss@customer.test", "buyer@customer.test"],
        "exactly one bounce for each recipient"
    );
    assert!(
        bounces
            .iter()
            .all(|b| b.class == BounceClass::RetriesExhausted && b.job_id == id)
    );
    rig.finish().await;
}

#[tokio::test]
async fn a_mail_job_that_times_out_on_its_last_attempt_bounces_every_recipient_once() {
    one_bounce_per_recipient(Kill::Timeout).await;
}

#[tokio::test]
async fn a_mail_job_that_panics_on_its_last_attempt_bounces_every_recipient_once() {
    one_bounce_per_recipient(Kill::Panic).await;
}

#[tokio::test]
async fn without_the_hook_registered_no_bounce_is_written() {
    // The mutation that shows the other tests depend on the registration.
    let rig = Rig::start(Kill::Timeout, 0, false).await;
    let id = rig.enqueue().await;
    assert_eq!(rig.settled(id).await, JobState::Dead);
    assert!(rig.mail.bounces().is_empty());
    rig.finish().await;
}

#[tokio::test]
async fn a_recipient_the_server_already_answered_gets_no_bounce() {
    let rig = Rig::start(Kill::Timeout, 0, true).await;
    rig.sent_log
        .record_sent(
            &site(),
            MESSAGE_ID,
            &SendReceipt {
                message_id: MESSAGE_ID.to_owned(),
                accepted: vec!["buyer@customer.test".parse().unwrap()],
                rejected: Vec::new(),
            },
        )
        .await
        .unwrap();
    let id = rig.enqueue().await;
    assert_eq!(rig.settled(id).await, JobState::Dead);
    assert_eq!(recipients(&rig.mail), ["boss@customer.test"]);
    rig.finish().await;
}

#[tokio::test]
async fn a_panic_before_the_last_attempt_writes_no_bounce() {
    let rig = Rig::start(Kill::Panic, 2, true).await;
    let id = rig.enqueue().await;
    rig.settled(id).await;
    assert!(rig.mail.bounces().is_empty());
    rig.finish().await;
}

struct Counting(Arc<AtomicUsize>);

impl FinalAttemptHook for Counting {
    fn on_dead<'a>(&'a self, _job: &'a Job, _reason: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
        })
    }
}

struct Panicking;

impl FinalAttemptHook for Panicking {
    fn on_dead<'a>(&'a self, _job: &'a Job, _reason: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move { panic!("the first hook panics") })
    }
}

struct PanicsBuilding;

impl FinalAttemptHook for PanicsBuilding {
    fn on_dead<'a>(&'a self, _job: &'a Job, _reason: &'a str) -> BoxFuture<'a, ()> {
        panic!("this hook panics before it returns a future")
    }
}

#[tokio::test]
async fn a_panicking_hook_does_not_stop_the_hooks_after_it() {
    let hooks = FinalAttemptHooks::new();
    let (first, second) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    hooks.register(Arc::new(Counting(Arc::clone(&first))));
    hooks.register(Arc::new(Panicking));
    hooks.register(Arc::new(PanicsBuilding));
    hooks.register(Arc::new(Counting(Arc::clone(&second))));
    assert_eq!(hooks.len(), 4);
    let store = InMemoryJobStore::new();
    let new = enqueue_mail(site(), &message(), "default", MESSAGE_ID, now()).unwrap();
    let id = match store.insert_if_new(new).await.unwrap() {
        spot_jobs::model::InsertResult::Inserted(id)
        | spot_jobs::model::InsertResult::Duplicate(id) => id,
    };
    let job = store.get(id).await.unwrap().unwrap();
    hooks.on_dead(&job, "job.timeout").await;
    assert_eq!(
        first.load(Ordering::SeqCst),
        1,
        "registration order: first runs"
    );
    assert_eq!(
        second.load(Ordering::SeqCst),
        1,
        "the hook after two panics still runs"
    );
}
