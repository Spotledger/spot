//! End to end tests of the mail queue over `InMemoryJobStore`, `CaptureTransport` and
//! `MemoryFileStore`: enqueue, run through the worker's execute path, and look at the job
//! state, the captured mail, the sent log and the bounce records.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use spot_core::{ManualClock, SiteId};
use spot_egress::{Guard, InMemorySink, Resolver};
use spot_files::key::{FileKey, Visibility};
use spot_files::memory::MemoryFileStore;
use spot_files::store::{FileStore, PutOptions};
use spot_jobs::model::{InsertResult, JobState};
use spot_jobs::policy::QueuePolicy;
use spot_jobs::worker::{FinalAttemptHook, HandlerResult, JobContext, JobHandlerRegistry, Worker};
use spot_jobs::{InMemoryJobStore, JobStore};
use spot_mail::{
    CaptureTransport, MailError, MailTransport, PermanentFailure, Secret, SendReceipt, SmtpConfig,
    SmtpReply, TransientFailure, TransportSecurity,
};
use spot_mail_queue::*;
use time::OffsetDateTime;
use time::macros::datetime;

const SECRET: &str = "s3cret-pass-xyz-7781";
const T0: OffsetDateTime = datetime!(2026-10-21 08:00 UTC);

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

struct FixedResolver(Arc<Mutex<Vec<IpAddr>>>);

#[async_trait]
impl Resolver for FixedResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, std::io::Error> {
        Ok(self.0.lock().unwrap().clone())
    }
}

struct Directory {
    accounts: Mutex<HashMap<(SiteId, String), SmtpAccount>>,
    down: Mutex<bool>,
}

#[async_trait]
impl AccountDirectory for Directory {
    async fn account(&self, site: &SiteId, id: &str) -> Result<Option<SmtpAccount>, Unavailable> {
        if *self.down.lock().unwrap() {
            return Err(Unavailable);
        }
        Ok(self
            .accounts
            .lock()
            .unwrap()
            .get(&(site.clone(), id.to_owned()))
            .cloned())
    }
}

struct Credentials {
    calls: AtomicUsize,
    mode: Mutex<Result<(), CredentialError>>,
}

#[async_trait]
impl CredentialResolver for Credentials {
    async fn resolve(&self, _site: &SiteId, _reference: &str) -> Result<Secret, CredentialError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (*self.mode.lock().unwrap()).map(|()| Secret::new(SECRET))
    }
}

struct Factory {
    transport: Arc<dyn MailTransport>,
    builds: AtomicUsize,
    seen_secrets: Mutex<Vec<String>>,
    seen_pinned: Mutex<Vec<Option<Vec<SocketAddr>>>>,
}

impl TransportFactory for Factory {
    fn build(&self, config: SmtpConfig) -> Result<Arc<dyn MailTransport>, MailError> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        self.seen_pinned.lock().unwrap().push(config.pinned.clone());
        if let Some(credentials) = &config.credentials {
            self.seen_secrets
                .lock()
                .unwrap()
                .push(credentials.secret.expose().to_owned());
        }
        Ok(Arc::clone(&self.transport))
    }
}

/// A sent log that can be switched off.
struct SwitchedLog {
    inner: InMemorySentLog,
    down_read: Mutex<bool>,
    down_write: Mutex<bool>,
}

#[async_trait]
impl SentLog for SwitchedLog {
    async fn is_sent(&self, site: &SiteId, id: &str) -> Result<bool, Unavailable> {
        if *self.down_read.lock().unwrap() {
            return Err(Unavailable);
        }
        self.inner.is_sent(site, id).await
    }
    async fn record_sent(
        &self,
        site: &SiteId,
        id: &str,
        receipt: &SendReceipt,
    ) -> Result<bool, Unavailable> {
        if *self.down_write.lock().unwrap() {
            return Err(Unavailable);
        }
        self.inner.record_sent(site, id, receipt).await
    }
    async fn recorded(&self, site: &SiteId, id: &str) -> Result<Option<SendReceipt>, Unavailable> {
        if *self.down_read.lock().unwrap() {
            return Err(Unavailable);
        }
        self.inner.recorded(site, id).await
    }
}

/// A transport that answers with a scripted receipt.
struct Scripted(Mutex<Vec<SendReceipt>>);

impl MailTransport for Scripted {
    fn send<'a>(
        &'a self,
        _message: &'a spot_mail::OutgoingMessage,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SendReceipt, MailError>> + Send + 'a>,
    > {
        Box::pin(async move { Ok(self.0.lock().unwrap().remove(0)) })
    }
}

/// A bounce sink that always fails, counting the attempts it refused.
#[derive(Debug, Default)]
struct FailingSink(AtomicUsize);

impl FailingSink {
    fn calls(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl BounceSink for FailingSink {
    async fn record(&self, _record: BounceRecord) -> Result<(), Unavailable> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(Unavailable)
    }
}

/// A sent log that answers "not sent" once and "sent" afterwards, as when another attempt of
/// the same job records the send while this one is failing to deliver.
#[derive(Debug, Default)]
struct SentMidAttempt(Mutex<usize>);

#[async_trait]
impl SentLog for SentMidAttempt {
    async fn is_sent(&self, _site: &SiteId, _id: &str) -> Result<bool, Unavailable> {
        let mut reads = self.0.lock().unwrap();
        *reads += 1;
        Ok(*reads > 1)
    }
    async fn record_sent(
        &self,
        _site: &SiteId,
        _id: &str,
        _receipt: &SendReceipt,
    ) -> Result<bool, Unavailable> {
        Ok(false)
    }
    async fn recorded(
        &self,
        _site: &SiteId,
        _id: &str,
    ) -> Result<Option<SendReceipt>, Unavailable> {
        Ok(None)
    }
}

struct Env {
    store: Arc<InMemoryJobStore>,
    registry: Arc<JobHandlerRegistry>,
    sender: Arc<MailSender>,
    capture: Arc<CaptureTransport>,
    factory: Arc<Factory>,
    creds: Arc<Credentials>,
    directory: Arc<Directory>,
    log: Arc<SwitchedLog>,
    bounces: Arc<InMemoryBounceSink>,
    files: Arc<MemoryFileStore>,
    resolved: Arc<Mutex<Vec<IpAddr>>>,
    clock_hours: AtomicUsize,
    /// The queue policy the worker runs the job under. The sender is given none: it learns the
    /// last attempt from the job context the worker builds from this policy.
    policy: QueuePolicy,
}

fn account() -> SmtpAccount {
    SmtpAccount {
        host: "smtp.example.test".to_owned(),
        port: 587,
        security: TransportSecurity::StartTls,
        from_address: "billing@example.test".to_owned(),
        username: Some("billing@example.test".to_owned()),
        credential_ref: Some("secret/mail/billing".to_owned()),
        timeout: Duration::from_secs(10),
        pool_size: 0,
        root_certificate_pem: None,
    }
}

impl Env {
    fn new() -> Self {
        Self::with(vec!["93.184.216.34".parse().unwrap()], None)
    }

    fn with(resolved: Vec<IpAddr>, transport: Option<Arc<dyn MailTransport>>) -> Self {
        Self::with_queue(resolved, transport, None, None, QueuePolicy::default())
    }

    /// An env on a queue whose policy differs from the default, so a test can check that the
    /// last attempt follows the policy the engine dead-letters on rather than the job's stored
    /// `max_attempts` (the in-memory store always writes 3).
    fn with_policy(policy: QueuePolicy) -> Self {
        Self::with_queue(
            vec!["93.184.216.34".parse().unwrap()],
            None,
            None,
            None,
            policy,
        )
    }

    /// An env whose sender writes bounces to `bounces` and reads the sent log from `sent_log`,
    /// so a test can make a port fail or answer differently on the second read. When a port is
    /// `None` the env's own in-memory one is used.
    fn with_ports(
        resolved: Vec<IpAddr>,
        transport: Option<Arc<dyn MailTransport>>,
        bounces: Option<Arc<dyn BounceSink>>,
        sent_log: Option<Arc<dyn SentLog>>,
    ) -> Self {
        Self::with_queue(
            resolved,
            transport,
            bounces,
            sent_log,
            QueuePolicy::default(),
        )
    }

    fn with_queue(
        resolved: Vec<IpAddr>,
        transport: Option<Arc<dyn MailTransport>>,
        bounces: Option<Arc<dyn BounceSink>>,
        sent_log: Option<Arc<dyn SentLog>>,
        policy: QueuePolicy,
    ) -> Self {
        let capture = Arc::new(CaptureTransport::new());
        let factory = Arc::new(Factory {
            transport: transport.unwrap_or_else(|| capture.clone() as Arc<dyn MailTransport>),
            builds: AtomicUsize::new(0),
            seen_secrets: Mutex::new(Vec::new()),
            seen_pinned: Mutex::new(Vec::new()),
        });
        let creds = Arc::new(Credentials {
            calls: AtomicUsize::new(0),
            mode: Mutex::new(Ok(())),
        });
        let directory = Arc::new(Directory {
            accounts: Mutex::new(HashMap::from([(
                (site("acme"), "billing".to_owned()),
                account(),
            )])),
            down: Mutex::new(false),
        });
        let log = Arc::new(SwitchedLog {
            inner: InMemorySentLog::new(),
            down_read: Mutex::new(false),
            down_write: Mutex::new(false),
        });
        let bounces_mem = Arc::new(InMemoryBounceSink::new());
        let bounce_sink: Arc<dyn BounceSink> = match bounces {
            Some(sink) => sink,
            None => bounces_mem.clone(),
        };
        let sent_log: Arc<dyn SentLog> = match sent_log {
            Some(log) => log,
            None => log.clone(),
        };
        let files = Arc::new(MemoryFileStore::new());
        let resolved = Arc::new(Mutex::new(resolved));
        let guard = Arc::new(Guard::new(
            spot_egress::EgressPolicy::default(),
            Arc::new(FixedResolver(resolved.clone())),
            Arc::new(InMemorySink::new()),
            Arc::new(ManualClock::new(T0)),
        ));
        let sender = Arc::new(
            MailSender::from_ports(MailPorts {
                accounts: directory.clone(),
                credentials: creds.clone(),
                factory: factory.clone(),
                guard,
                files: files.clone(),
                sent_log,
                bounces: bounce_sink,
            })
            .with_clock(Arc::new(ManualClock::new(T0))),
        );
        let mut registry = JobHandlerRegistry::new();
        sender.register(&mut registry);
        Self {
            store: Arc::new(InMemoryJobStore::with_clock(Arc::new(|| T0))),
            registry: Arc::new(registry),
            sender,
            capture,
            factory,
            creds,
            directory,
            log,
            bounces: bounces_mem,
            files,
            resolved,
            clock_hours: AtomicUsize::new(0),
            policy,
        }
    }

    async fn enqueue(&self, site_id: &str, message: &QueuedMessage, id: &str) -> InsertResult {
        let job = enqueue_mail(site(site_id), message, "billing", id, T0).unwrap();
        self.store.insert_if_new(job).await.unwrap()
    }

    /// Claims one due job and runs it through the worker's execute path, `hours` after T0
    /// (each call moves a clock forward so retries are due).
    async fn run_next(&self) -> bool {
        self.run_next_attempt(None).await.is_some()
    }

    /// Claims one due job and runs it through `Worker::execute_job` — the engine's own path, so
    /// `retry_or_dead` decides the state against this env's queue policy. `attempt`, when given,
    /// is the job's attempt number as the engine would hand it to the handler in `ctx.attempt`
    /// (the in-memory store leaves it at zero, since only a reaped lease advances it). Returns
    /// the state the store kept.
    async fn run_next_attempt(&self, attempt: Option<u32>) -> Option<JobState> {
        let hours = self.clock_hours.fetch_add(1, Ordering::SeqCst) as i64 + 1;
        let now = T0 + time::Duration::hours(hours * 24);
        let claimed = self
            .store
            .claim(
                &[MAIL_QUEUE.to_owned()],
                "w1",
                Duration::from_secs(60),
                now,
                1,
            )
            .await
            .unwrap();
        let claimed = claimed.into_iter().next()?;
        let mut job = claimed.job;
        if let Some(attempt) = attempt {
            job.attempt = attempt;
        }
        let id = job.id;
        let policies = HashMap::from([(MAIL_QUEUE.to_owned(), self.policy.clone())]);
        let hook: Arc<dyn FinalAttemptHook> = self.sender.clone();
        Worker::execute_job_with_hook(
            self.store.clone(),
            self.registry.clone(),
            Arc::new(policies),
            Arc::new(move || now),
            Arc::new(HashMap::new()),
            job,
            claimed.claim,
            Some(hook),
        )
        .await
        .unwrap();
        Some(self.job_state(id).await.0)
    }

    async fn job_state(&self, id: spot_jobs::JobId) -> (JobState, Option<String>) {
        let job = self.store.get(id).await.unwrap().unwrap();
        (job.state, job.last_error)
    }

    /// Runs the job for `id` as its `attempt`th attempt through the real worker path, with the
    /// queue policy this env was built on deciding what the engine does next. Every
    /// last-attempt test goes through here: the policy, the attempt the engine passes and the
    /// dead-letter rule are then the production ones rather than values set by hand.
    async fn run_attempt(&self, id: spot_jobs::JobId, attempt: u32) -> JobState {
        let state = self
            .run_next_attempt(Some(attempt))
            .await
            .expect("a job was due");
        assert_eq!(
            self.store.get(id).await.unwrap().unwrap().id,
            id,
            "the claimed job is the one the test enqueued"
        );
        state
    }

    async fn put_file(&self, site_id: &str, rest: &str, bytes: &[u8]) -> String {
        let key = FileKey::new(Visibility::Private, rest).unwrap();
        self.files
            .put(
                &site(site_id),
                key.clone(),
                Box::pin(std::io::Cursor::new(bytes.to_vec())),
                PutOptions::new("application/pdf", 20_000_000, false),
            )
            .await
            .unwrap();
        key.as_str().to_owned()
    }
}

fn message() -> QueuedMessage {
    let mut m = QueuedMessage::new(QueuedMailbox::address("billing@example.test"), "Invoice 12");
    m.to = vec![QueuedMailbox::address("buyer@customer.test")];
    m.cc = vec![QueuedMailbox::address("boss@customer.test")];
    m.text = Some("Please find the invoice attached.".to_owned());
    m
}

fn inserted(result: InsertResult) -> spot_jobs::JobId {
    match result {
        InsertResult::Inserted(id) | InsertResult::Duplicate(id) => id,
    }
}

fn ctx_for(job: spot_jobs::Job) -> JobContext {
    JobContext {
        attempt: job.attempt,
        job,
        clock: T0,
    }
}

#[tokio::test]
async fn happy_path_sends_once_with_the_job_message_id_and_attachment() {
    let env = Env::new();
    let key = env
        .put_file("acme", "mail/invoice-12.pdf", b"%PDF-1.4 invoice")
        .await;
    let mut m = message();
    m.attachments.push(QueuedAttachment {
        filename: "invoice-12.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        file_key: key,
        content_id: None,
    });
    let id = inserted(env.enqueue("acme", &m, "msg-0001@example.test").await);
    assert!(env.run_next().await);
    let (state, error) = env.job_state(id).await;
    assert_eq!(state, JobState::Succeeded);
    assert_eq!(error, None);
    let sent = env.capture.messages();
    assert_eq!(sent.len(), 1);
    let wire = String::from_utf8_lossy(&sent[0]).into_owned();
    assert!(wire.contains("msg-0001@example.test"), "wire message id");
    assert!(wire.contains("invoice-12.pdf"), "attachment present");
    assert_eq!(env.log.inner.len(), 1);
    assert!(env.bounces.records().is_empty());
}

#[tokio::test]
async fn job_shape_is_stable_and_carries_no_bytes_or_secrets() {
    let env = Env::new();
    let key = env
        .put_file("acme", "mail/a.pdf", b"binary-content-marker")
        .await;
    let mut m = message();
    m.attachments.push(QueuedAttachment {
        filename: "a.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        file_key: key.clone(),
        content_id: None,
    });
    let job = enqueue_mail(site("acme"), &m, "billing", "msg-shape", T0).unwrap();
    assert_eq!(job.name, "platform:send_mail");
    assert_eq!(job.queue, "default");
    assert_eq!(job.idempotency_key.as_deref(), Some("msg-shape"));
    assert_eq!(job.args["version"], 1);
    assert_eq!(job.args["account"], "billing");
    let args = job.args.to_string();
    assert!(args.contains(&key), "file by key");
    assert!(!args.contains("binary-content-marker"));
    assert!(!args.contains(SECRET));
    let parsed: MailJob = serde_json::from_value(job.args).unwrap();
    assert_eq!(parsed.message, m);
}

#[tokio::test]
async fn two_enqueues_of_one_message_send_once() {
    let env = Env::new();
    let first = env.enqueue("acme", &message(), "msg-dup").await;
    let second = env.enqueue("acme", &message(), "msg-dup").await;
    assert!(matches!(first, InsertResult::Inserted(_)));
    assert!(matches!(second, InsertResult::Duplicate(_)));
    assert!(env.run_next().await);
    assert!(!env.run_next().await, "only one job exists");
    assert_eq!(env.capture.messages().len(), 1);
}

#[tokio::test]
async fn transient_failure_retries_then_succeeds() {
    let env = Env::new();
    env.capture
        .fail_next(TransientFailure::Reply(SmtpReply::new(451, "try later")).into());
    let id = inserted(env.enqueue("acme", &message(), "msg-retry").await);
    assert!(env.run_next().await);
    let (state, error) = env.job_state(id).await;
    assert!(matches!(state, JobState::Retrying { .. }), "{state:?}");
    assert_eq!(
        error.as_deref(),
        Some("mail_queue.smtp_transient reply=451")
    );
    assert!(env.capture.messages().is_empty());
    assert!(env.bounces.records().is_empty());
    assert!(env.run_next().await);
    assert_eq!(env.job_state(id).await.0, JobState::Succeeded);
    assert_eq!(env.capture.messages().len(), 1);
}

#[tokio::test]
async fn connection_failures_are_retried() {
    for failure in [
        TransientFailure::ConnectionRefused,
        TransientFailure::ConnectionReset,
        TransientFailure::Timeout,
        TransientFailure::TlsHandshake,
    ] {
        let env = Env::new();
        env.capture.fail_next(failure.into());
        let id = inserted(env.enqueue("acme", &message(), "msg-conn").await);
        env.run_next().await;
        let (state, error) = env.job_state(id).await;
        assert!(matches!(state, JobState::Retrying { .. }));
        assert_eq!(error.as_deref(), Some("mail_queue.smtp_transient"));
    }
}

#[tokio::test]
async fn permanent_failure_fails_the_job_and_bounces_every_recipient() {
    let env = Env::new();
    env.capture.fail_next(
        PermanentFailure::Reply(SmtpReply::new(550, "no such user, body: secret subject")).into(),
    );
    let id = inserted(env.enqueue("acme", &message(), "msg-5xx").await);
    env.run_next().await;
    let (state, error) = env.job_state(id).await;
    assert_eq!(state, JobState::Dead);
    assert_eq!(
        error.as_deref(),
        Some("mail_queue.smtp_permanent reply=550")
    );
    assert!(env.capture.messages().is_empty());
    let bounces = env.bounces.records();
    assert_eq!(bounces.len(), 2);
    assert!(bounces.iter().all(|b| b.class == BounceClass::Rejected
        && b.reply_code == Some(550)
        && b.code == "mail_queue.smtp_permanent"
        && b.message_id == "msg-5xx"
        && b.site == site("acme")));
    let recipients: Vec<_> = bounces.iter().map(|b| b.recipient.as_str()).collect();
    assert!(
        recipients.contains(&"buyer@customer.test") && recipients.contains(&"boss@customer.test")
    );
    let debug = format!("{bounces:?}");
    assert!(!debug.contains("Invoice 12") && !debug.contains("Please find"));
}

#[tokio::test]
async fn egress_denial_fails_without_building_a_transport() {
    let env = Env::with(vec!["10.0.0.5".parse().unwrap()], None);
    let id = inserted(env.enqueue("acme", &message(), "msg-egress").await);
    env.run_next().await;
    let (state, error) = env.job_state(id).await;
    assert_eq!(state, JobState::Dead);
    assert_eq!(error.as_deref(), Some("egress.denied_address"));
    assert_eq!(
        env.factory.builds.load(Ordering::SeqCst),
        0,
        "no connection"
    );
    assert_eq!(
        env.creds.calls.load(Ordering::SeqCst),
        0,
        "no secret fetched"
    );
    assert!(env.capture.messages().is_empty());
    assert!(
        env.bounces
            .records()
            .iter()
            .all(|b| b.class == BounceClass::EgressDenied)
    );
}

#[tokio::test]
async fn dangerous_port_is_denied_by_the_guard() {
    let env = Env::new();
    let mut acct = account();
    acct.port = 25;
    env.directory
        .accounts
        .lock()
        .unwrap()
        .insert((site("acme"), "billing".to_owned()), acct);
    let id = inserted(env.enqueue("acme", &message(), "msg-port").await);
    env.run_next().await;
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("egress.denied_port")
    );
    assert_eq!(env.factory.builds.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn missing_attachment_fails_permanently_without_sending() {
    let env = Env::new();
    let mut m = message();
    m.attachments.push(QueuedAttachment {
        filename: "gone.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        file_key: "private/mail/gone.pdf".to_owned(),
        content_id: None,
    });
    let id = inserted(env.enqueue("acme", &m, "msg-missing").await);
    env.run_next().await;
    let (state, error) = env.job_state(id).await;
    assert_eq!(state, JobState::Dead);
    assert_eq!(error.as_deref(), Some("mail_queue.attachment_missing"));
    assert!(env.capture.messages().is_empty());
    assert_eq!(env.bounces.records()[0].class, BounceClass::Attachment);
}

#[tokio::test]
async fn attachment_of_another_site_is_not_visible() {
    let env = Env::new();
    let key = env
        .put_file("other", "mail/secret.pdf", b"other tenant")
        .await;
    let mut m = message();
    m.attachments.push(QueuedAttachment {
        filename: "secret.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        file_key: key,
        content_id: None,
    });
    let id = inserted(env.enqueue("acme", &m, "msg-xsite-file").await);
    env.run_next().await;
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("mail_queue.attachment_missing")
    );
    assert!(env.capture.messages().is_empty());
}

#[tokio::test]
async fn attachments_over_the_total_limit_fail_permanently() {
    let env = Env::new();
    let big = vec![0u8; 13 * 1024 * 1024];
    let a = env.put_file("acme", "mail/big-a.bin", &big).await;
    let b = env.put_file("acme", "mail/big-b.bin", &big).await;
    let mut m = message();
    for (name, key) in [("a.bin", a), ("b.bin", b)] {
        m.attachments.push(QueuedAttachment {
            filename: name.to_owned(),
            content_type: "application/octet-stream".to_owned(),
            file_key: key,
            content_id: None,
        });
    }
    let id = inserted(env.enqueue("acme", &m, "msg-big").await);
    env.run_next().await;
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("mail_queue.attachment_too_large")
    );
    assert!(env.capture.messages().is_empty());
}

#[tokio::test]
async fn crash_after_send_does_not_send_again() {
    let env = Env::new();
    let id = inserted(env.enqueue("acme", &message(), "msg-crash").await);
    let claimed = env
        .store
        .claim(
            &["default".to_owned()],
            "w1",
            Duration::from_secs(60),
            T0,
            1,
        )
        .await
        .unwrap();
    let job = claimed[0].job.clone();
    // First attempt sends and records, then the worker "crashes" before completing the job.
    let first = env.sender.execute(ctx_for(job.clone())).await.unwrap();
    assert_eq!(first, HandlerResult::Ok);
    assert_eq!(env.capture.messages().len(), 1);
    // The lease expires and the job runs again.
    let again = env.sender.execute(ctx_for(job)).await.unwrap();
    assert_eq!(again, HandlerResult::Ok);
    assert_eq!(env.capture.messages().len(), 1, "no second send");
    let _ = id;
}

#[tokio::test]
async fn sent_log_is_per_site() {
    let env = Env::new();
    env.directory
        .accounts
        .lock()
        .unwrap()
        .insert((site("beta"), "billing".to_owned()), account());
    env.enqueue("acme", &message(), "msg-same-id").await;
    env.enqueue("beta", &message(), "msg-same-id").await;
    assert!(env.run_next().await);
    assert!(env.run_next().await);
    assert_eq!(
        env.capture.messages().len(),
        2,
        "same id in two sites sends twice"
    );
}

#[tokio::test]
async fn accounts_are_per_site() {
    let env = Env::new();
    let id = inserted(env.enqueue("beta", &message(), "msg-noacct").await);
    env.run_next().await;
    let (state, error) = env.job_state(id).await;
    assert_eq!(state, JobState::Dead);
    assert_eq!(error.as_deref(), Some("mail_queue.unknown_account"));
    assert!(env.capture.messages().is_empty());
}

#[tokio::test]
async fn from_address_must_be_the_accounts() {
    let env = Env::new();
    let mut m = message();
    m.from = QueuedMailbox::address("ceo@example.test");
    let id = inserted(env.enqueue("acme", &m, "msg-spoof").await);
    env.run_next().await;
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("mail_queue.sender_mismatch")
    );
    assert!(env.capture.messages().is_empty());
}

#[tokio::test]
async fn secret_never_appears_in_errors_records_or_args() {
    let env = Env::new();
    env.capture.fail_next(
        PermanentFailure::Authentication {
            reply: Some(SmtpReply::new(535, format!("auth failed for {SECRET}"))),
        }
        .into(),
    );
    let id = inserted(env.enqueue("acme", &message(), "msg-auth").await);
    env.run_next().await;
    let job = env.store.get(id).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Dead);
    assert_eq!(
        job.last_error.as_deref(),
        Some("mail_queue.smtp_authentication reply=535")
    );
    // The factory did receive the secret (it is needed to connect) ...
    assert_eq!(
        env.factory.seen_secrets.lock().unwrap().as_slice(),
        [SECRET]
    );
    // ... and it is nowhere else.
    for text in [
        format!("{job:?}"),
        format!("{:?}", env.bounces.records()),
        job.last_error.clone().unwrap_or_default(),
        job.args.to_string(),
        format!("{:?}", account()),
    ] {
        assert!(!text.contains(SECRET), "secret leaked in {text}");
    }
    assert_eq!(env.bounces.records()[0].class, BounceClass::Authentication);
}

#[tokio::test]
async fn transport_is_cached_per_account_and_rebuilt_after_an_auth_failure() {
    let env = Env::new();
    env.enqueue("acme", &message(), "msg-c1").await;
    env.enqueue("acme", &message(), "msg-c2").await;
    env.run_next().await;
    env.run_next().await;
    assert_eq!(env.factory.builds.load(Ordering::SeqCst), 1);
    assert_eq!(
        env.creds.calls.load(Ordering::SeqCst),
        1,
        "secret resolved once"
    );
    env.capture
        .fail_next(PermanentFailure::Authentication { reply: None }.into());
    env.enqueue("acme", &message(), "msg-c3").await;
    env.run_next().await;
    env.enqueue("acme", &message(), "msg-c4").await;
    env.run_next().await;
    assert_eq!(
        env.factory.builds.load(Ordering::SeqCst),
        2,
        "rebuilt after auth failure"
    );
    assert_eq!(env.creds.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn changed_account_settings_rebuild_the_transport() {
    let env = Env::new();
    env.enqueue("acme", &message(), "msg-s1").await;
    env.run_next().await;
    let mut changed = account();
    changed.host = "smtp2.example.test".to_owned();
    env.directory
        .accounts
        .lock()
        .unwrap()
        .insert((site("acme"), "billing".to_owned()), changed);
    env.enqueue("acme", &message(), "msg-s2").await;
    env.run_next().await;
    assert_eq!(env.factory.builds.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn backing_outages_retry_before_anything_is_sent() {
    let env = Env::new();
    *env.log.down_read.lock().unwrap() = true;
    let id = inserted(env.enqueue("acme", &message(), "msg-down1").await);
    env.run_next().await;
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("mail_queue.sent_log_unavailable")
    );
    *env.log.down_read.lock().unwrap() = false;

    *env.directory.down.lock().unwrap() = true;
    env.run_next().await;
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("mail_queue.directory_unavailable")
    );
    *env.directory.down.lock().unwrap() = false;

    *env.creds.mode.lock().unwrap() = Err(CredentialError::Unavailable);
    env.run_next().await;
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("mail_queue.credential_unavailable")
    );
    assert!(env.capture.messages().is_empty());
    assert!(env.bounces.records().is_empty(), "retries are not bounces");

    *env.creds.mode.lock().unwrap() = Ok(());
    env.run_next().await;
    assert_eq!(env.job_state(id).await.0, JobState::Succeeded);
    assert_eq!(env.capture.messages().len(), 1);
}

#[tokio::test]
async fn missing_credential_is_permanent() {
    let env = Env::new();
    *env.creds.mode.lock().unwrap() = Err(CredentialError::NotFound);
    let id = inserted(env.enqueue("acme", &message(), "msg-nocred").await);
    env.run_next().await;
    assert_eq!(env.job_state(id).await.0, JobState::Dead);
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("mail_queue.credential_not_found")
    );
}

#[tokio::test]
async fn failed_sent_log_write_after_a_send_does_not_retry() {
    let env = Env::new();
    *env.log.down_write.lock().unwrap() = true;
    let id = inserted(env.enqueue("acme", &message(), "msg-logfail").await);
    env.run_next().await;
    assert_eq!(
        env.job_state(id).await.0,
        JobState::Succeeded,
        "a retry could only send twice"
    );
    assert_eq!(env.capture.messages().len(), 1);
}

fn receipt(accepted: &[&str], rejected: &[(&str, u16)]) -> SendReceipt {
    SendReceipt {
        message_id: "wire".to_owned(),
        accepted: accepted.iter().map(|a| a.parse().unwrap()).collect(),
        rejected: rejected
            .iter()
            .map(|(a, c)| (a.parse().unwrap(), SmtpReply::new(*c, "x")))
            .collect(),
    }
}

#[tokio::test]
async fn partial_rejection_completes_and_bounces_the_rejected_recipient() {
    let scripted = Arc::new(Scripted(Mutex::new(vec![receipt(
        &["buyer@customer.test"],
        &[("boss@customer.test", 550)],
    )])));
    let env = Env::with(vec!["93.184.216.34".parse().unwrap()], Some(scripted));
    let id = inserted(env.enqueue("acme", &message(), "msg-partial").await);
    env.run_next().await;
    assert_eq!(env.job_state(id).await.0, JobState::Succeeded);
    let bounces = env.bounces.records();
    assert_eq!(bounces.len(), 1);
    assert_eq!(bounces[0].recipient, "boss@customer.test");
    assert_eq!(bounces[0].reply_code, Some(550));
    assert_eq!(bounces[0].code, "mail_queue.recipient_rejected");
    assert_eq!(env.log.inner.len(), 1);
}

#[tokio::test]
async fn all_recipients_deferred_is_retried_not_bounced() {
    let scripted = Arc::new(Scripted(Mutex::new(vec![
        receipt(
            &[],
            &[("buyer@customer.test", 451), ("boss@customer.test", 451)],
        ),
        receipt(&["buyer@customer.test", "boss@customer.test"], &[]),
    ])));
    let env = Env::with(vec!["93.184.216.34".parse().unwrap()], Some(scripted));
    let id = inserted(env.enqueue("acme", &message(), "msg-defer").await);
    env.run_next().await;
    let (state, error) = env.job_state(id).await;
    assert!(matches!(state, JobState::Retrying { .. }));
    assert_eq!(
        error.as_deref(),
        Some("mail_queue.smtp_transient reply=451")
    );
    assert!(env.bounces.records().is_empty());
    assert!(env.log.inner.is_empty());
    env.run_next().await;
    assert_eq!(env.job_state(id).await.0, JobState::Succeeded);
}

#[tokio::test]
async fn malformed_and_unknown_version_jobs_fail_without_sending() {
    let env = Env::new();
    for (idem, args) in [
        ("bad-1", serde_json::json!({"nonsense": true})),
        ("bad-2", {
            let mut job = enqueue_mail(site("acme"), &message(), "billing", "x", T0)
                .unwrap()
                .args;
            job["version"] = serde_json::json!(99);
            job
        }),
    ] {
        let new_job = spot_jobs::NewJob::new(
            MAIL_JOB_NAME.to_owned(),
            args,
            MAIL_QUEUE.to_owned(),
            site("acme"),
            T0,
        )
        .with_idempotency_key(idem.to_owned());
        let id = inserted(env.store.insert_if_new(new_job).await.unwrap());
        env.run_next().await;
        assert_eq!(env.job_state(id).await.0, JobState::Dead);
    }
    assert!(env.capture.messages().is_empty());
}

#[test]
fn enqueue_validates_ids_recipients_keys_and_limits() {
    let ok = message();
    let try_enqueue =
        |m: &QueuedMessage, account: &str, id: &str| enqueue_mail(site("acme"), m, account, id, T0);
    assert!(try_enqueue(&ok, "billing", "").is_err());
    assert!(try_enqueue(&ok, "billing", "has space").is_err());
    assert!(try_enqueue(&ok, "billing", "<angle@x>").is_err());
    assert!(try_enqueue(&ok, "billing", "line\nbreak").is_err());
    assert!(try_enqueue(&ok, "", "id").is_err());

    let mut no_rcpt = ok.clone();
    no_rcpt.to.clear();
    no_rcpt.cc.clear();
    assert!(try_enqueue(&no_rcpt, "billing", "id").is_err());

    let mut bad_addr = ok.clone();
    bad_addr.to = vec![QueuedMailbox::address("not an address")];
    assert!(try_enqueue(&bad_addr, "billing", "id").is_err());

    let mut injected = ok.clone();
    injected.subject = "hi\r\nBcc: evil@x.test".to_owned();
    assert!(try_enqueue(&injected, "billing", "id").is_err());

    let mut bad_key = ok.clone();
    bad_key.attachments.push(QueuedAttachment {
        filename: "a".to_owned(),
        content_type: "text/plain".to_owned(),
        file_key: "../../etc/passwd".to_owned(),
        content_id: None,
    });
    assert!(try_enqueue(&bad_key, "billing", "id").is_err());

    let mut many = ok.clone();
    many.bcc = (0..101)
        .map(|i| QueuedMailbox::address(format!("r{i}@x.test")))
        .collect();
    assert_eq!(
        try_enqueue(&many, "billing", "id").unwrap_err().code(),
        "mail_queue.limit_exceeded"
    );

    let mut structural = ok;
    structural.headers.push(QueuedHeader {
        name: "From".to_owned(),
        value: "x@y.test".to_owned(),
    });
    assert!(try_enqueue(&structural, "billing", "id").is_err());
}

// --- Pinned addresses (WP-PLT-11.T4). ---

fn socket(ip: &str, port: u16) -> SocketAddr {
    SocketAddr::new(ip.parse().unwrap(), port)
}

#[tokio::test]
async fn the_transport_is_built_with_the_addresses_the_guard_authorised() {
    let env = Env::new();
    let id = inserted(env.enqueue("acme", &message(), "msg-pin-1").await);
    assert!(env.run_next().await);
    assert_eq!(env.job_state(id).await.0, JobState::Succeeded);
    let seen = env.factory.seen_pinned.lock().unwrap().clone();
    assert_eq!(seen, vec![Some(vec![socket("93.184.216.34", 587)])]);
}

#[tokio::test]
async fn an_answer_that_turns_private_after_the_first_send_is_refused_before_any_build() {
    let env = Env::new();
    let first = inserted(env.enqueue("acme", &message(), "msg-pin-a").await);
    assert!(env.run_next().await);
    assert_eq!(env.job_state(first).await.0, JobState::Succeeded);
    // The hostile DNS server now answers a metadata address.
    *env.resolved.lock().unwrap() = vec!["169.254.169.254".parse().unwrap()];
    let second = inserted(env.enqueue("acme", &message(), "msg-pin-b").await);
    assert!(env.run_next().await);
    let (state, error) = env.job_state(second).await;
    assert_ne!(state, JobState::Succeeded);
    assert_eq!(error.as_deref(), Some("egress.denied_address"));
    assert_eq!(env.capture.messages().len(), 1, "nothing was sent");
    assert_eq!(env.factory.builds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_changed_authorised_set_rebuilds_the_transport_and_its_pool() {
    let env = Env::new();
    for (n, ip) in [(1, "93.184.216.34"), (2, "93.184.216.34"), (3, "1.1.1.1")] {
        *env.resolved.lock().unwrap() = vec![ip.parse().unwrap()];
        let id = inserted(
            env.enqueue("acme", &message(), &format!("msg-set-{n}"))
                .await,
        );
        assert!(env.run_next().await);
        assert_eq!(env.job_state(id).await.0, JobState::Succeeded);
    }
    // Same set twice reuses the transport; the changed set builds a new one.
    assert_eq!(env.factory.builds.load(Ordering::SeqCst), 2);
    let seen = env.factory.seen_pinned.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            Some(vec![socket("93.184.216.34", 587)]),
            Some(vec![socket("1.1.1.1", 587)]),
        ]
    );
}

#[tokio::test]
async fn the_production_factory_refuses_an_unpinned_configuration() {
    let config = account().config(None);
    assert!(config.pinned.is_none());
    let error = SmtpTransportFactory.build(config).err().unwrap();
    assert!(matches!(
        error,
        MailError::Permanent(PermanentFailure::Client(_))
    ));
}

#[tokio::test]
async fn the_production_factory_accepts_a_pinned_configuration() {
    let mut config = account().config(None);
    config.pinned = Some(vec![socket("93.184.216.34", 587)]);
    assert!(SmtpTransportFactory.build(config).is_ok());
}

#[tokio::test]
async fn retries_that_run_out_bounce_every_recipient_and_complete_the_job() {
    let env = Env::new();
    env.capture.fail_next(
        TransientFailure::Reply(SmtpReply::new(451, "try later, secret-in-reply-text")).into(),
    );
    let id = inserted(env.enqueue("acme", &message(), "msg-exhausted").await);
    // Attempt 3 of the default policy's 3, through the worker: the engine would dead-letter a
    // retry here, so the handler bounces instead and the job ends `Succeeded`.
    let state = env.run_attempt(id, 3).await;
    assert_eq!(
        state,
        JobState::Succeeded,
        "the job completes, it is not dead"
    );
    assert!(env.capture.messages().is_empty(), "nothing was sent");
    let bounces = env.bounces.records();
    assert_eq!(bounces.len(), 2, "every recipient gets a record");
    for bounce in &bounces {
        assert_eq!(bounce.class, BounceClass::RetriesExhausted);
        assert_eq!(bounce.code, "mail_queue.smtp_transient");
        assert_eq!(bounce.reply_code, Some(451));
        assert_eq!(bounce.attempts, Some(3), "the attempt count that ran out");
        assert_eq!(bounce.message_id, "msg-exhausted");
        assert_eq!(bounce.site, site("acme"));
        // The worker hands the handler its own clock for the attempt, a day after T0 here.
        assert_eq!(bounce.at, T0 + time::Duration::days(1));
    }
    let recipients: Vec<_> = bounces.iter().map(|b| b.recipient.as_str()).collect();
    assert!(
        recipients.contains(&"buyer@customer.test") && recipients.contains(&"boss@customer.test")
    );
    let debug = format!("{bounces:?}");
    assert!(
        !debug.contains("try later") && !debug.contains("Invoice 12"),
        "no server text and no subject in a bounce record"
    );
    assert_eq!(env.log.inner.len(), 0, "nothing was recorded as sent");
}

#[tokio::test]
async fn an_attempt_with_attempts_left_still_asks_for_a_retry() {
    for attempt in [1, 2] {
        let env = Env::new();
        env.capture
            .fail_next(TransientFailure::Reply(SmtpReply::new(451, "try later")).into());
        let id = inserted(env.enqueue("acme", &message(), "msg-still-retry").await);
        let state = env.run_attempt(id, attempt).await;
        assert!(
            matches!(state, JobState::Retrying { .. }),
            "attempt {attempt} of 3 is not the last one: {state:?}"
        );
        assert_eq!(
            env.job_state(id).await.1.as_deref(),
            Some("mail_queue.smtp_transient reply=451")
        );
        assert!(
            env.bounces.records().is_empty(),
            "no bounce before the last attempt"
        );
    }
}

/// The blocker the review found: `Job::max_attempts` is not the engine's maximum. The in-memory
/// store writes 3 for every job, so a queue policy of 2 makes attempt 2 the last one the engine
/// allows — the handler must bounce there and the job must end `Succeeded`, not dead-letter.
#[tokio::test]
async fn a_short_queue_policy_bounces_on_its_own_last_attempt() {
    let env = Env::with_policy(QueuePolicy::short());
    assert_eq!(
        env.policy.max_attempts, 2,
        "the policy is not the default 3"
    );
    env.capture
        .fail_next(TransientFailure::Reply(SmtpReply::new(451, "try later")).into());
    let id = inserted(env.enqueue("acme", &message(), "msg-short").await);
    assert_eq!(
        env.store.get(id).await.unwrap().unwrap().max_attempts,
        3,
        "the job's stored maximum disagrees with the policy on purpose"
    );
    let state = env.run_attempt(id, 2).await;
    assert_eq!(
        state,
        JobState::Succeeded,
        "the handler bounces on the policy's last attempt instead of returning a retry the engine dead-letters"
    );
    let bounces = env.bounces.records();
    assert_eq!(bounces.len(), 2, "every recipient gets a record");
    assert!(
        bounces
            .iter()
            .all(|b| b.class == BounceClass::RetriesExhausted
                && b.attempts == Some(2)
                && b.code == "mail_queue.smtp_transient")
    );
}

/// The other half of the same blocker: with a policy of 5, attempt 3 is **not** the last one, so
/// the handler must return a retry and the engine must keep the job for its remaining attempts.
#[tokio::test]
async fn a_long_queue_policy_is_not_bounced_early() {
    let env = Env::with_policy(QueuePolicy::long());
    assert_eq!(env.policy.max_attempts, 5);
    env.capture
        .fail_next(TransientFailure::Reply(SmtpReply::new(451, "try later")).into());
    let id = inserted(env.enqueue("acme", &message(), "msg-long").await);
    let state = env.run_attempt(id, 3).await;
    assert!(
        matches!(state, JobState::Retrying { .. }),
        "attempt 3 of 5 is not the last one: {state:?}"
    );
    assert!(
        env.bounces.records().is_empty(),
        "no bounce two attempts early"
    );
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("mail_queue.smtp_transient reply=451")
    );
}

#[tokio::test]
async fn a_message_the_sent_log_has_is_never_bounced_when_retries_run_out() {
    let env = Env::new();
    let id = inserted(env.enqueue("acme", &message(), "msg-partly-sent").await);
    // A previous attempt was accepted for one recipient and refused the other: the message is
    // recorded as sent, so the exhaustion path must not bounce anybody.
    let receipt = SendReceipt {
        message_id: "msg-partly-sent".to_owned(),
        accepted: vec!["buyer@customer.test".parse::<spot_mail::Address>().unwrap()],
        rejected: vec![(
            "boss@customer.test".parse::<spot_mail::Address>().unwrap(),
            SmtpReply::new(450, "mailbox busy"),
        )],
    };
    assert!(
        env.log
            .inner
            .record_sent(&site("acme"), "msg-partly-sent", &receipt)
            .await
            .unwrap()
    );
    let result = env.run_attempt(id, 3).await;
    assert_eq!(result, JobState::Succeeded);
    assert!(env.capture.messages().is_empty(), "nothing is sent twice");
    assert!(
        env.bounces.records().is_empty(),
        "a message the sent log has is never bounced"
    );
}

#[tokio::test]
async fn a_send_recorded_by_another_attempt_stops_the_exhaustion_bounce() {
    let env = Env::with_ports(
        vec!["93.184.216.34".parse().unwrap()],
        None,
        None,
        Some(Arc::new(SentMidAttempt::default())),
    );
    env.capture
        .fail_next(TransientFailure::Reply(SmtpReply::new(451, "try later")).into());
    let id = inserted(env.enqueue("acme", &message(), "msg-raced").await);
    let result = env.run_attempt(id, 3).await;
    assert_eq!(
        result,
        JobState::Succeeded,
        "the second read of the sent log says the message went out"
    );
    assert!(
        env.bounces.records().is_empty(),
        "no bounce for a message another attempt recorded"
    );
}

#[tokio::test]
async fn an_unreadable_sent_log_blocks_the_exhaustion_bounce() {
    let env = Env::new();
    *env.log.down_read.lock().unwrap() = true;
    let id = inserted(env.enqueue("acme", &message(), "msg-log-down").await);
    let result = env.run_attempt(id, 3).await;
    assert_eq!(
        result,
        JobState::Dead,
        "the retry is the engine's last attempt, so it dead-letters rather than completing on a guess"
    );
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("mail_queue.sent_log_unavailable")
    );
    assert!(
        env.bounces.records().is_empty(),
        "a message that may have been sent is not bounced"
    );
}

#[tokio::test]
async fn a_failed_exhaustion_bounce_is_reported_and_not_swallowed() {
    let sink = Arc::new(FailingSink::default());
    let env = Env::with_ports(
        vec!["93.184.216.34".parse().unwrap()],
        None,
        Some(sink.clone()),
        None,
    );
    env.capture
        .fail_next(TransientFailure::Reply(SmtpReply::new(451, "try later")).into());
    let id = inserted(env.enqueue("acme", &message(), "msg-bounce-down").await);
    let result = env.run_attempt(id, 3).await;
    assert_eq!(
        result,
        JobState::Dead,
        "the retry the failed write returns is the engine's dead-letter rule on the last attempt"
    );
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("mail_queue.bounce_unavailable"),
        "a failed bounce write is reported, not swallowed"
    );
    assert_eq!(sink.calls(), 1, "the write was attempted before giving up");
    assert!(env.capture.messages().is_empty());
}

/// A transport that panics when asked to send.
struct Panicking;

impl MailTransport for Panicking {
    fn send<'a>(
        &'a self,
        _message: &'a spot_mail::OutgoingMessage,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SendReceipt, MailError>> + Send + 'a>,
    > {
        Box::pin(async move { panic!("transport blew up") })
    }
}

/// A transport that never answers, so the worker's queue timeout ends the attempt.
struct Hanging;

impl MailTransport for Hanging {
    fn send<'a>(
        &'a self,
        _message: &'a spot_mail::OutgoingMessage,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SendReceipt, MailError>> + Send + 'a>,
    > {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Err(TransientFailure::Timeout.into())
        })
    }
}

/// A bounce sink that follows a script of per-call outcomes (`true` = write, `false` = fail),
/// then writes every later call. It writes into `inner`, which the test reads.
struct ScriptedSink {
    inner: Arc<InMemoryBounceSink>,
    plan: Mutex<std::collections::VecDeque<bool>>,
}

impl ScriptedSink {
    fn new(plan: &[bool]) -> (Arc<Self>, Arc<InMemoryBounceSink>) {
        let inner = Arc::new(InMemoryBounceSink::new());
        let sink = Arc::new(Self {
            inner: inner.clone(),
            plan: Mutex::new(plan.iter().copied().collect()),
        });
        (sink, inner)
    }
}

#[async_trait]
impl BounceSink for ScriptedSink {
    async fn record(&self, record: BounceRecord) -> Result<(), Unavailable> {
        let write = self.plan.lock().unwrap().pop_front().unwrap_or(true);
        if write {
            self.inner.record(record).await
        } else {
            Err(Unavailable)
        }
    }
}

fn three_recipients() -> QueuedMessage {
    let mut m = message();
    m.bcc = vec![QueuedMailbox::address("audit@customer.test")];
    m
}

fn env_with_timeout(transport: Arc<dyn MailTransport>, timeout: Duration) -> Env {
    let mut env = Env::with(vec!["93.184.216.34".parse().unwrap()], Some(transport));
    env.policy.timeout = timeout;
    env
}

async fn stored_job(env: &Env, id: spot_jobs::JobId) -> spot_jobs::Job {
    env.store.get(id).await.unwrap().unwrap()
}

#[tokio::test]
async fn a_panic_on_the_last_attempt_bounces_every_recipient_once() {
    let env = Env::with(
        vec!["93.184.216.34".parse().unwrap()],
        Some(Arc::new(Panicking)),
    );
    let id = inserted(env.enqueue("acme", &three_recipients(), "msg-panic").await);
    let state = env.run_attempt(id, 3).await;
    assert_eq!(
        state,
        JobState::Dead,
        "the worker dead-letters the panicked job"
    );
    let bounces = env.bounces.records();
    assert_eq!(bounces.len(), 3, "one record per recipient");
    for bounce in &bounces {
        assert_eq!(bounce.class, BounceClass::RetriesExhausted);
        assert_eq!(bounce.code, "mail_queue.retries_exhausted");
        assert_eq!(bounce.job_id, id);
        assert_eq!(bounce.message_id, "msg-panic");
        assert_eq!(bounce.site, site("acme"));
        assert_eq!(bounce.attempts, Some(3));
        assert_eq!(bounce.reply_code, None);
        assert_eq!(bounce.at, T0, "stamped by the sender's clock");
    }
    // The hook running again (a repeated delivery) writes nothing new.
    env.sender
        .on_dead(&stored_job(&env, id).await, "job.handler_panicked")
        .await;
    assert_eq!(
        env.bounces.records().len(),
        3,
        "each bounce is written once"
    );
    let debug = format!("{bounces:?}");
    assert!(
        !debug.contains("Invoice 12") && !debug.contains("Please find"),
        "no subject or body in a record"
    );
}

#[tokio::test]
async fn a_timeout_on_the_last_attempt_bounces_every_recipient() {
    let env = env_with_timeout(Arc::new(Hanging), Duration::from_millis(100));
    let id = inserted(env.enqueue("acme", &message(), "msg-timeout").await);
    let state = env.run_attempt(id, 3).await;
    assert_eq!(state, JobState::Dead);
    assert_eq!(
        env.job_state(id).await.1.as_deref(),
        Some("job.timeout"),
        "the engine's own reason is untouched"
    );
    let bounces = env.bounces.records();
    assert_eq!(bounces.len(), 2);
    assert!(
        bounces
            .iter()
            .all(|b| b.class == BounceClass::RetriesExhausted)
    );
}

#[tokio::test]
async fn a_panic_before_the_last_attempt_does_not_bounce() {
    let env = Env::with(
        vec!["93.184.216.34".parse().unwrap()],
        Some(Arc::new(Panicking)),
    );
    let id = inserted(env.enqueue("acme", &message(), "msg-panic-early").await);
    let state = env.run_attempt(id, 1).await;
    assert_eq!(
        state,
        JobState::Dead,
        "a panic is final for the job whichever attempt it is"
    );
    assert!(
        env.bounces.records().is_empty(),
        "the worker calls the hook on the last attempt only; nobody is bounced early"
    );
}

#[tokio::test]
async fn the_hook_leaves_recipients_the_receipt_already_accounts_for() {
    let env = Env::new();
    let id = inserted(
        env.enqueue("acme", &three_recipients(), "msg-partial-dead")
            .await,
    );
    let receipt = SendReceipt {
        message_id: "msg-partial-dead".to_owned(),
        accepted: vec!["buyer@customer.test".parse::<spot_mail::Address>().unwrap()],
        rejected: vec![(
            "boss@customer.test".parse::<spot_mail::Address>().unwrap(),
            SmtpReply::new(550, "no such user"),
        )],
    };
    env.log
        .inner
        .record_sent(&site("acme"), "msg-partial-dead", &receipt)
        .await
        .unwrap();
    env.sender
        .on_dead(&stored_job(&env, id).await, "job.timeout")
        .await;
    let bounces = env.bounces.records();
    assert_eq!(bounces.len(), 1, "only the recipient nobody accounted for");
    assert_eq!(bounces[0].recipient, "audit@customer.test");
}

#[tokio::test]
async fn the_hook_writes_nothing_when_the_sent_log_cannot_be_read() {
    let env = Env::new();
    let id = inserted(env.enqueue("acme", &message(), "msg-dead-log-down").await);
    *env.log.down_read.lock().unwrap() = true;
    env.sender
        .on_dead(&stored_job(&env, id).await, "job.timeout")
        .await;
    assert!(
        env.bounces.records().is_empty(),
        "a message that may have been sent is not bounced"
    );
}

#[tokio::test]
async fn the_hook_ignores_other_jobs_and_unreadable_arguments() {
    let env = Env::new();
    let id = inserted(env.enqueue("acme", &message(), "msg-not-mine").await);
    let mut other = stored_job(&env, id).await;
    other.name = "billing:send_statement".to_owned();
    env.sender.on_dead(&other, "job.timeout").await;
    let mut broken = stored_job(&env, id).await;
    broken.args = serde_json::json!({"nonsense": true});
    env.sender.on_dead(&broken, "job.timeout").await;
    assert!(env.bounces.records().is_empty());
}

#[tokio::test]
async fn a_partial_exhaustion_write_then_a_retry_ends_with_one_record_per_recipient() {
    let (sink, records) = ScriptedSink::new(&[true, false]);
    let env = Env::with_ports(
        vec!["93.184.216.34".parse().unwrap()],
        None,
        Some(sink),
        None,
    );
    for _ in 0..2 {
        env.capture
            .fail_next(TransientFailure::Reply(SmtpReply::new(451, "try later")).into());
    }
    let id = inserted(
        env.enqueue("acme", &three_recipients(), "msg-partial")
            .await,
    );
    let mut job = stored_job(&env, id).await;
    job.attempt = 3;
    let first = env.sender.execute(ctx_for(job.clone())).await.unwrap();
    assert!(
        matches!(first, HandlerResult::Retry(ref why) if why == "mail_queue.bounce_unavailable")
    );
    assert_eq!(
        records.records().len(),
        1,
        "the write stopped after the first recipient"
    );
    let second = env.sender.execute(ctx_for(job)).await.unwrap();
    assert!(matches!(second, HandlerResult::Ok));
    assert_eq!(
        records.records().len(),
        3,
        "exactly one record per recipient"
    );
}

#[tokio::test]
async fn a_partial_permanent_failure_write_then_a_retry_ends_with_one_record_per_recipient() {
    let (sink, records) = ScriptedSink::new(&[true, false]);
    let env = Env::with_ports(
        vec!["93.184.216.34".parse().unwrap()],
        None,
        Some(sink),
        None,
    );
    for _ in 0..2 {
        env.capture
            .fail_next(PermanentFailure::Reply(SmtpReply::new(550, "refused")).into());
    }
    let id = inserted(
        env.enqueue("acme", &three_recipients(), "msg-partial-perm")
            .await,
    );
    let mut job = stored_job(&env, id).await;
    job.attempt = 1;
    let first = env.sender.execute(ctx_for(job.clone())).await.unwrap();
    assert!(matches!(first, HandlerResult::Retry(_)));
    assert_eq!(records.records().len(), 1);
    let second = env.sender.execute(ctx_for(job)).await.unwrap();
    assert!(matches!(second, HandlerResult::Fail(_)));
    assert_eq!(records.records().len(), 3);
    assert!(
        records
            .records()
            .iter()
            .all(|b| b.class == BounceClass::Rejected)
    );
}

fn record_for(
    site_name: &str,
    job: u64,
    recipient: &str,
    class: BounceClass,
    at: OffsetDateTime,
) -> BounceRecord {
    BounceRecord {
        site: site(site_name),
        job_id: spot_jobs::JobId(job),
        message_id: "m".to_owned(),
        recipient: recipient.to_owned(),
        class,
        code: "mail_queue.smtp_permanent",
        reply_code: None,
        attempts: None,
        at,
    }
}

#[tokio::test]
async fn the_bounce_key_is_job_recipient_and_class_within_a_site() {
    let sink = InMemoryBounceSink::new();
    let first = record_for("acme", 7, "a@x.test", BounceClass::Rejected, T0);
    sink.record(first.clone()).await.unwrap();
    // The same key again, with a later time and another code: a no-op that returns Ok.
    let mut again = first.clone();
    again.at = T0 + time::Duration::hours(1);
    again.code = "mail_queue.retries_exhausted";
    sink.record(again).await.unwrap();
    assert_eq!(
        sink.records(),
        vec![first],
        "the first record is kept unchanged"
    );
    sink.record(record_for(
        "acme",
        7,
        "a@x.test",
        BounceClass::RetriesExhausted,
        T0,
    ))
    .await
    .unwrap();
    sink.record(record_for("acme", 8, "a@x.test", BounceClass::Rejected, T0))
        .await
        .unwrap();
    sink.record(record_for("acme", 7, "b@x.test", BounceClass::Rejected, T0))
        .await
        .unwrap();
    sink.record(record_for(
        "globex",
        7,
        "a@x.test",
        BounceClass::Rejected,
        T0,
    ))
    .await
    .unwrap();
    assert_eq!(
        sink.records().len(),
        5,
        "another class, job, recipient or site is another bounce"
    );
}

#[tokio::test]
async fn one_failed_hook_write_does_not_stop_the_other_recipients() {
    let (sink, records) = ScriptedSink::new(&[false]);
    let env = Env::with_ports(
        vec!["93.184.216.34".parse().unwrap()],
        None,
        Some(sink),
        None,
    );
    let id = inserted(
        env.enqueue("acme", &three_recipients(), "msg-hook-flaky")
            .await,
    );
    env.sender
        .on_dead(&stored_job(&env, id).await, "job.timeout")
        .await;
    assert_eq!(
        records.records().len(),
        2,
        "the two writes that could succeed did"
    );
    // A second delivery of the hook fills in the one that failed, and repeats nothing.
    env.sender
        .on_dead(&stored_job(&env, id).await, "job.timeout")
        .await;
    assert_eq!(records.records().len(), 3);
}
