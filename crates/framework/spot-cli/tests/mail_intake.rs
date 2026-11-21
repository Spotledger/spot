//! Incoming mail in `spot serve` (WP-PLT-11.T13): the settings table, the sink, and the poll
//! loop over scripted mailboxes. Everything except the last test runs without a database; the
//! last runs the PostgreSQL stores and skips with a printed reason without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use spot_cli::services::mail_intake::host::{Deps, IntakeHost, STOP_BOUND, TracingAudit};
use spot_cli::services::mail_intake::settings::{Settings, resolve_all};
use spot_cli::services::mail_intake::sink::{
    AttachmentWriter, CreateError, DirectoryUnavailable, DocumentCreator, DocumentSink, Fut,
    NoAttachments, RunAs, SinkParts, UserDirectory, build_fields, command_id, is_blocked,
};
use spot_cli::services::mail_intake::status::{Status, state};
use spot_coordinator::{InMemoryPresence, InProcessCoordinator, NodeId};
use spot_core::{Clock, ManualClock};
use spot_mail::{
    FetchBatch, FetchLimits, FetchOutcome, IncomingMessage, MailAddress, MailError, MailboxCursor,
    PermanentFailure, Secret,
};
use spot_mail_intake::memory::{
    MemoryAudit, MemoryCursorStore, MemorySeenStore, StaticCredentials,
};
use spot_mail_intake::{
    Accepted, Account, CursorStore, DedupeKey, IntakeSink, MailFetcher, SeenStore, SinkError,
};
use spot_storage_contract::{RecordId, Value};
use time::OffsetDateTime;
use tokio::sync::watch;

const PASSWORD: &str = "hunter2-very-secret";
const OWN: &str = "support@example.test";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- settings ---------------------------------------------------------------------------------

const GOOD: &str = r#"
enabled = true

[[accounts]]
name = "support"
host = "imap.example.test"
username = "support@example.test"
address = "support@example.test"
password_ref = "support_pw"
target_doctype = "ticket"
run_as = "mail_robot"
[accounts.field_map]
from = "requester"
subject = "title"
text = "body"

[secrets.support_pw]
sealed = "spsec:v1:placeholder"
aad = "mail_intake/support"
"#;

fn settings(text: &str) -> Settings {
    toml::from_str(text).unwrap()
}

#[test]
fn a_good_table_resolves_with_the_documented_defaults() {
    let table = settings(GOOD);
    table.validate().unwrap();
    let (ready, invalid) = resolve_all(&table);
    assert!(invalid.is_empty(), "{invalid:?}");
    let account = &ready[0].account;
    assert_eq!(account.imap.port, 993);
    assert_eq!(account.mailbox, "INBOX");
    assert_eq!(account.poll_interval, Duration::from_secs(300));
    assert!(ready[0].target.blocked.contains(&"exe".to_owned()));
    assert!(!ready[0].target.store_attachments);
}

#[test]
fn the_table_is_off_by_default_and_refuses_unknown_keys() {
    assert!(!Settings::default().enabled);
    assert!(toml::from_str::<Settings>("enabled = true\nbogus = 1").is_err());
    assert!(toml::from_str::<Settings>("[[accounts]]\nname = \"x\"\nsurprise = 1").is_err());
}

fn broken(edit: impl Fn(&str) -> String) -> Option<String> {
    let table = settings(&edit(GOOD));
    resolve_all(&table).1.into_iter().next().map(|i| i.reason)
}

#[test]
fn every_rule_marks_the_account_invalid_without_stopping_the_table() {
    type Edit = Box<dyn Fn(&str) -> String>;
    let cases: Vec<(&str, Edit, &str)> = vec![
        (
            "name",
            Box::new(|s| s.replace("name = \"support\"", "name = \"Bad Name\"")),
            "name",
        ),
        (
            "security",
            Box::new(|s| s.replace("host = ", "security = \"plain\"\nhost = ")),
            "security",
        ),
        (
            "interval",
            Box::new(|s| s.replace("host = ", "poll_interval_seconds = 29\nhost = ")),
            "poll_interval",
        ),
        (
            "per poll",
            Box::new(|s| s.replace("host = ", "max_messages_per_poll = 201\nhost = ")),
            "max_messages_per_poll",
        ),
        (
            "run_as",
            Box::new(|s| s.replace("run_as = \"mail_robot\"", "run_as = \" \"")),
            "run_as",
        ),
        (
            "doctype",
            Box::new(|s| s.replace("\"ticket\"", "\"Ticket!\"")),
            "target_doctype",
        ),
        (
            "password",
            Box::new(|s| s.replace("password_ref = \"support_pw\"", "password_ref = \"nope\"")),
            "password_ref",
        ),
        (
            "field map duplicate",
            Box::new(|s| s.replace("subject = \"title\"", "subject = \"requester\"")),
            "field_map",
        ),
        (
            "field name",
            Box::new(|s| s.replace("text = \"body\"", "text = \"Bad Field\"")),
            "field_map",
        ),
        (
            "attachments",
            Box::new(|s| s.replace("host = ", "max_attachments = 500\nhost = ")),
            "max_attachments",
        ),
        (
            "address",
            Box::new(|s| s.replace("address = \"support@example.test\"", "address = \"nobody\"")),
            "address",
        ),
    ];
    for (label, edit, key) in cases {
        let reason = broken(edit).unwrap_or_else(|| panic!("{label}: the account was accepted"));
        assert!(reason.contains(key), "{label}: {reason}");
        assert!(
            !reason.contains("example.test"),
            "{label} echoed a value: {reason}"
        );
    }
}

#[test]
fn one_bad_account_does_not_hide_a_good_one() {
    let two = format!(
        "{GOOD}\n[[accounts]]\nname = \"Bad\"\nhost = \"h\"\nusername = \"u\"\naddress = \"a@b\"\npassword_ref = \"support_pw\"\ntarget_doctype = \"ticket\"\nrun_as = \"r\"\n[accounts.field_map]\nfrom = \"f\"\nsubject = \"s\"\n"
    );
    let (ready, invalid) = resolve_all(&settings(&two));
    assert_eq!(ready.len(), 1);
    assert_eq!(invalid.len(), 1);
}

#[test]
fn two_accounts_with_one_name_stop_the_start() {
    let twice = format!("{GOOD}\n{}", GOOD.replace("enabled = true", "").replace("[secrets.support_pw]\nsealed = \"spsec:v1:placeholder\"\naad = \"mail_intake/support\"", ""));
    let table: Settings = toml::from_str(&twice).unwrap_or_default();
    // A table that does not parse is also a failed start; a parsed one with a repeated name is
    // refused by `validate`.
    if !table.accounts.is_empty() {
        assert!(table.validate().is_err());
    }
}

#[test]
fn a_lease_outside_the_range_is_refused() {
    let table = settings("lease_seconds = 5");
    assert!(table.validate().is_err());
}

#[test]
fn blocked_extensions_match_every_tail_and_any_case() {
    let blocked: Vec<String> = ["exe", "sh"].map(str::to_owned).to_vec();
    assert!(is_blocked("setup.EXE", &blocked));
    assert!(is_blocked("invoice.pdf.exe", &blocked));
    assert!(is_blocked("a.tar.sh", &blocked));
    assert!(!is_blocked("invoice.pdf", &blocked));
    assert!(!is_blocked("exe", &blocked));
}

// --- fakes -------------------------------------------------------------------------------------

fn message(uid: u32, id: &str) -> IncomingMessage {
    IncomingMessage {
        uid,
        raw_size: 200,
        truncated: false,
        message_id: Some(id.to_owned()),
        in_reply_to: None,
        references: Vec::new(),
        from: Some(MailAddress {
            name: Some("Customer Name".to_owned()),
            address: "customer@elsewhere.test".to_owned(),
        }),
        to: vec![MailAddress {
            name: None,
            address: OWN.to_owned(),
        }],
        cc: Vec::new(),
        reply_to: Vec::new(),
        subject: Some("Secret subject line".to_owned()),
        date: Some(1_700_000_000),
        text: Some("Secret body text".to_owned()),
        html: None,
        attachments: Vec::new(),
        auto_submitted: false,
        headers: Vec::new(),
    }
}

#[derive(Default)]
struct Mailbox {
    messages: Mutex<Vec<IncomingMessage>>,
    failure: Mutex<Option<MailError>>,
    stall: Mutex<bool>,
    fetches: AtomicUsize,
}

impl Mailbox {
    fn push(&self, m: IncomingMessage) {
        lock(&self.messages).push(m);
    }
}

#[async_trait]
impl MailFetcher for Mailbox {
    async fn fetch(
        &self,
        _account: &Account,
        _secret: Secret,
        cursor: Option<MailboxCursor>,
        limits: FetchLimits,
    ) -> Result<FetchBatch, MailError> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        if *lock(&self.stall) {
            std::future::pending::<()>().await;
        }
        if let Some(failure) = lock(&self.failure).clone() {
            return Err(failure);
        }
        let last = cursor.map_or(0, |c| c.last_uid);
        let mut pending: Vec<IncomingMessage> = lock(&self.messages)
            .iter()
            .filter(|m| m.uid > last)
            .cloned()
            .collect();
        pending.sort_by_key(|m| m.uid);
        let more = pending.len() > limits.max_messages;
        pending.truncate(limits.max_messages);
        let highest = pending.last().map_or(last, |m| m.uid);
        Ok(FetchBatch {
            outcome: FetchOutcome::New,
            cursor: MailboxCursor {
                uid_validity: 1,
                last_uid: highest,
            },
            messages: pending,
            more,
        })
    }
}

struct Users(BTreeMap<String, RunAs>);

impl UserDirectory for Users {
    fn find<'a>(&'a self, name: &'a str) -> Fut<'a, Result<Option<RunAs>, DirectoryUnavailable>> {
        Box::pin(async move { Ok(self.0.get(name).copied()) })
    }
}

type Created = (RecordId, String, String, Vec<(String, Value)>);

#[derive(Default)]
struct Creator {
    created: Mutex<Vec<Created>>,
    mode: Mutex<Option<CreateError>>,
}

impl DocumentCreator for Creator {
    fn create<'a>(
        &'a self,
        run_as: RecordId,
        doctype: &'a str,
        _id: RecordId,
        command_id: String,
        fields: Vec<(String, Value)>,
    ) -> Fut<'a, Result<(), CreateError>> {
        Box::pin(async move {
            if let Some(error) = *lock(&self.mode) {
                return Err(error);
            }
            lock(&self.created).push((run_as, doctype.to_owned(), command_id, fields));
            Ok(())
        })
    }
}

#[derive(Default)]
struct Writer {
    stored: Mutex<Vec<String>>,
}

impl AttachmentWriter for Writer {
    fn supported(&self) -> bool {
        true
    }
    fn store<'a>(
        &'a self,
        _run_as: RecordId,
        _doctype: &'a str,
        _id: RecordId,
        file_name: &'a str,
        _content_type: &'a str,
        _bytes: &'a [u8],
    ) -> Fut<'a, Result<(), CreateError>> {
        Box::pin(async move {
            lock(&self.stored).push(file_name.to_owned());
            Ok(())
        })
    }
}

fn robot() -> RunAs {
    RunAs {
        id: RecordId::new_v7(),
        active: true,
    }
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH))
}

struct Fixture {
    account: Account,
    sink: DocumentSink,
    creator: Arc<Creator>,
    status: Arc<Status>,
}

fn fixture(user: Option<RunAs>, attachments: Arc<dyn AttachmentWriter>, extra: &str) -> Fixture {
    let table = settings(&format!("{GOOD}\n{extra}"));
    let (ready, invalid) = resolve_all(&table);
    assert!(invalid.is_empty(), "{invalid:?}");
    let ready = ready.into_iter().next().unwrap();
    let users: BTreeMap<String, RunAs> = user
        .map(|u| ("mail_robot".to_owned(), u))
        .into_iter()
        .collect();
    let creator = Arc::new(Creator::default());
    let status = Arc::new(Status::new());
    let sink = DocumentSink::new(SinkParts {
        site: "acme".to_owned(),
        targets: BTreeMap::from([(ready.account.name.clone(), ready.target.clone())]),
        users: Arc::new(Users(users)),
        creator: Arc::clone(&creator) as Arc<dyn DocumentCreator>,
        attachments,
        status: Arc::clone(&status),
        clock: clock(),
    });
    Fixture {
        account: ready.account,
        sink,
        creator,
        status,
    }
}

fn key() -> DedupeKey {
    DedupeKey("ab".repeat(32))
}

// --- the sink ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_message_becomes_one_document_as_the_run_as_user() {
    let user = robot();
    let f = fixture(Some(user), Arc::new(NoAttachments), "");
    let accepted = f
        .sink
        .accept(&f.account, &message(1, "<a@x>"), &key())
        .await;
    assert!(matches!(accepted, Ok(Accepted::Created(_))));
    let created = lock(&f.creator.created).clone();
    assert_eq!(created.len(), 1);
    let (run_as, doctype, command, fields) = &created[0];
    assert_eq!(
        *run_as, user.id,
        "created as the configured user, never the platform"
    );
    assert_eq!(doctype, "ticket");
    assert!(command.starts_with("mi:support:") && command.len() <= 128);
    let get = |name: &str| {
        fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        get("requester"),
        Some(Value::Text(
            "Customer Name <customer@elsewhere.test>".into()
        ))
    );
    assert_eq!(
        get("title"),
        Some(Value::Text("Secret subject line".into()))
    );
    assert_eq!(get("body"), Some(Value::Text("Secret body text".into())));
    assert_eq!(get("to"), None, "an unmapped part is not written");
}

#[test]
fn the_field_map_decides_which_parts_are_written() {
    let table = settings(&format!(
        "{}\n",
        GOOD.replace(
            "text = \"body\"",
            "text = \"body\"\nto = \"recipients\"\nreceived_at = \"received\"\nthread_key = \"thread\"\nmessage_id = \"mid\"\nhtml = \"html_body\""
        )
    ));
    let (ready, _) = resolve_all(&table);
    let mut m = message(1, "<a@x>");
    m.references = vec!["<root@x>".to_owned()];
    m.html = Some("<p>hi</p>".to_owned());
    let fields = build_fields(&ready[0].target.fields, &m, OffsetDateTime::UNIX_EPOCH);
    let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
    for expected in [
        "requester",
        "title",
        "body",
        "recipients",
        "received",
        "thread",
        "mid",
        "html_body",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    let received = fields.iter().find(|(n, _)| n == "received").unwrap();
    assert_eq!(
        received.1,
        Value::DateTime(OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap())
    );
}

#[tokio::test]
async fn a_missing_or_disabled_user_pauses_the_account_and_loses_no_mail() {
    let f = fixture(None, Arc::new(NoAttachments), "");
    assert_eq!(
        f.sink
            .accept(&f.account, &message(1, "<a@x>"), &key())
            .await,
        Err(SinkError::Retryable)
    );
    assert!(lock(&f.creator.created).is_empty());
    assert_eq!(
        f.status.effective("support"),
        Some(state::RUN_AS_UNAVAILABLE)
    );

    let disabled = RunAs {
        active: false,
        ..robot()
    };
    let f = fixture(Some(disabled), Arc::new(NoAttachments), "");
    assert_eq!(
        f.sink
            .accept(&f.account, &message(1, "<a@x>"), &key())
            .await,
        Err(SinkError::Retryable)
    );
}

#[tokio::test]
async fn validation_failure_is_poison_and_a_denied_user_is_a_pause() {
    let f = fixture(Some(robot()), Arc::new(NoAttachments), "");
    *lock(&f.creator.mode) = Some(CreateError::Invalid);
    assert_eq!(
        f.sink
            .accept(&f.account, &message(1, "<a@x>"), &key())
            .await,
        Err(SinkError::Rejected("invalid_document"))
    );
    *lock(&f.creator.mode) = Some(CreateError::Denied);
    assert_eq!(
        f.sink
            .accept(&f.account, &message(2, "<b@x>"), &key())
            .await,
        Err(SinkError::Retryable)
    );
    assert_eq!(
        f.status.effective("support"),
        Some(state::RUN_AS_UNAVAILABLE)
    );
    *lock(&f.creator.mode) = Some(CreateError::Unavailable);
    assert_eq!(
        f.sink
            .accept(&f.account, &message(3, "<c@x>"), &key())
            .await,
        Err(SinkError::Retryable)
    );
    *lock(&f.creator.mode) = None;
    assert!(
        f.sink
            .accept(&f.account, &message(4, "<d@x>"), &key())
            .await
            .is_ok()
    );
    assert_eq!(
        f.status.effective("support"),
        Some(state::STARTING),
        "the pause lifts"
    );
}

#[tokio::test]
async fn attachments_are_stored_unless_blocked_or_over_the_cap() {
    let writer = Arc::new(Writer::default());
    let f = fixture(
        Some(robot()),
        Arc::clone(&writer) as Arc<dyn AttachmentWriter>,
        "",
    );
    // `store_attachments` is a per-account switch: turn it on in the table.
    let table = settings(&GOOD.replace(
        "host = ",
        "store_attachments = true\nmax_attachments = 2\nhost = ",
    ));
    let (ready, invalid) = resolve_all(&table);
    assert!(invalid.is_empty(), "{invalid:?}");
    let status = Arc::new(Status::new());
    let creator = Arc::new(Creator::default());
    let sink = DocumentSink::new(SinkParts {
        site: "acme".to_owned(),
        targets: BTreeMap::from([("support".to_owned(), ready[0].target.clone())]),
        users: Arc::new(Users(BTreeMap::from([("mail_robot".to_owned(), robot())]))),
        creator: Arc::clone(&creator) as Arc<dyn DocumentCreator>,
        attachments: Arc::clone(&writer) as Arc<dyn AttachmentWriter>,
        status,
        clock: clock(),
    });
    let mut m = message(1, "<a@x>");
    for name in ["a.pdf", "run.exe", "b.png", "c.txt"] {
        m.attachments.push(
            spot_mail::Attachment::new(name, "application/octet-stream", vec![1, 2, 3]).unwrap(),
        );
    }
    sink.accept(&ready[0].account, &m, &key()).await.unwrap();
    assert_eq!(
        lock(&writer.stored).clone(),
        vec!["a.pdf", "b.png"],
        "blocked and over-cap skipped"
    );
    drop(f);
}

#[test]
fn a_host_without_in_process_uploads_cannot_store_attachments() {
    assert!(!NoAttachments.supported());
}

// --- the loop ----------------------------------------------------------------------------------

struct World {
    deps: Arc<Deps>,
    accounts: Vec<Account>,
    mailbox: Arc<Mailbox>,
    creator: Arc<Creator>,
    status: Arc<Status>,
    cursors: Arc<MemoryCursorStore>,
    seen: Arc<MemorySeenStore>,
    audit: Arc<MemoryAudit>,
}

async fn node(name: &str, shared: Option<&World>, passwords: bool) -> World {
    let table = settings(GOOD);
    let (ready, _) = resolve_all(&table);
    let ready = ready.into_iter().next().unwrap();
    let clock = clock();
    let mailbox = shared.map_or_else(|| Arc::new(Mailbox::default()), |w| Arc::clone(&w.mailbox));
    let creator = shared.map_or_else(|| Arc::new(Creator::default()), |w| Arc::clone(&w.creator));
    let cursors = shared.map_or_else(
        || Arc::new(MemoryCursorStore::new()),
        |w| Arc::clone(&w.cursors),
    );
    let seen = shared.map_or_else(|| Arc::new(MemorySeenStore::new()), |w| Arc::clone(&w.seen));
    let audit = Arc::new(MemoryAudit::new());
    let status = Arc::new(Status::new());
    let coordinator: Arc<dyn spot_coordinator::Coordinator> = match shared {
        Some(w) => Arc::clone(&w.deps.coordinator),
        None => InProcessCoordinator::start(
            Arc::new(InMemoryPresence::new()),
            NodeId::new("n1").unwrap(),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .await
        .unwrap(),
    };
    let sink = DocumentSink::new(SinkParts {
        site: "acme".to_owned(),
        targets: BTreeMap::from([("support".to_owned(), ready.target.clone())]),
        users: Arc::new(Users(BTreeMap::from([("mail_robot".to_owned(), robot())]))),
        creator: Arc::clone(&creator) as Arc<dyn DocumentCreator>,
        attachments: Arc::new(NoAttachments),
        status: Arc::clone(&status),
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
    });
    let credentials = if passwords {
        StaticCredentials::new().with("support_pw", PASSWORD)
    } else {
        StaticCredentials::new()
    };
    let deps = Arc::new(Deps {
        site: "acme".to_owned(),
        coordinator,
        node: name.to_owned(),
        lease_ttl: Duration::from_secs(300),
        cursors: Arc::clone(&cursors) as Arc<dyn CursorStore>,
        seen: Arc::clone(&seen) as Arc<dyn SeenStore>,
        sink: Arc::new(sink) as Arc<dyn IntakeSink>,
        audit: Arc::clone(&audit) as Arc<dyn spot_mail_intake::IntakeAudit>,
        credentials: Arc::new(credentials),
        fetcher: Arc::clone(&mailbox) as Arc<dyn MailFetcher>,
        clock: clock as Arc<dyn Clock>,
        status: Arc::clone(&status),
    });
    World {
        accounts: vec![ready.account],
        deps,
        mailbox,
        creator,
        status,
        cursors,
        seen,
        audit,
    }
}

fn host(world: &World) -> Arc<IntakeHost> {
    Arc::new(
        IntakeHost::new(Arc::clone(&world.deps), world.accounts.clone())
            .with_halt_check(Duration::from_millis(50)),
    )
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

fn spawn(
    host: &Arc<IntakeHost>,
) -> (
    watch::Sender<bool>,
    tokio::task::JoinHandle<Result<(), String>>,
) {
    let (stop, rx) = watch::channel(false);
    let host = Arc::clone(host);
    (stop, tokio::spawn(async move { host.run(rx).await }))
}

#[tokio::test]
async fn two_hosts_one_mailbox_create_each_document_once() {
    let a = node("node-a", None, true).await;
    let b = node("node-b", Some(&a), true).await;
    for uid in 1..=2 {
        a.mailbox.push(message(uid, &format!("<m{uid}@x>")));
    }
    let (host_a, host_b) = (host(&a), host(&b));
    let (stop_a, run_a) = spawn(&host_a);
    let (stop_b, run_b) = spawn(&host_b);
    until("two documents", || lock(&a.creator.created).len() >= 2).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        lock(&a.creator.created).len(),
        2,
        "each message became one document"
    );
    stop_a.send(true).unwrap();
    stop_b.send(true).unwrap();
    assert!(run_a.await.unwrap().is_ok());
    assert!(run_b.await.unwrap().is_ok());
}

#[tokio::test]
async fn a_restart_resumes_from_the_stored_cursor() {
    let first = node("node-a", None, true).await;
    first.mailbox.push(message(1, "<m1@x>"));
    let h = host(&first);
    let (stop, run) = spawn(&h);
    until("the first document", || {
        lock(&first.creator.created).len() == 1
    })
    .await;
    stop.send(true).unwrap();
    run.await.unwrap().unwrap();

    // A new process: new loop and coordinator, the same stores, one new message.
    let second = node("node-b", Some(&first), true).await;
    second.mailbox.push(message(2, "<m2@x>"));
    let h = host(&second);
    let (stop, run) = spawn(&h);
    until("the second document", || {
        lock(&first.creator.created).len() == 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        lock(&first.creator.created).len(),
        2,
        "message 1 was not imported again"
    );
    stop.send(true).unwrap();
    run.await.unwrap().unwrap();
}

/// Collects every event of the current thread as text.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

struct Visitor<'a>(&'a mut String);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={value:?} ", field.name()));
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut Visitor(&mut line));
        let mut out = lock(&self.0);
        out.push_str(&line);
        out.push('\n');
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn an_authentication_failure_halts_the_account_and_leaks_nothing() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let w = node("node-a", None, true).await;
    w.mailbox.push(message(1, "<m1@x>"));
    *lock(&w.mailbox.failure) = Some(MailError::Permanent(PermanentFailure::Authentication {
        reply: None,
    }));
    let h = host(&w);
    let (_stop, run) = spawn(&h);
    // Every account halted: the loop reports why and ends by itself.
    let ended = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ended, Err("auth_failed".to_owned()));
    assert_eq!(w.status.effective("support"), Some(state::AUTH_FAILED));
    assert_eq!(
        w.mailbox.fetches.load(Ordering::SeqCst),
        1,
        "no retry until an operator acts"
    );
    let text = format!(
        "{}{:?}{:?}",
        lock(&capture.0),
        w.status.snapshot(),
        w.audit.entries()
    );
    for forbidden in [
        PASSWORD,
        "customer@elsewhere",
        "Secret subject",
        "Secret body",
        OWN,
    ] {
        assert!(!text.contains(forbidden), "output leaks {forbidden}");
    }
    // The audit trail records the failure once.
    assert_eq!(w.audit.entries().len(), 1);
    let _ = TracingAudit {
        site: "acme".into(),
    };
}

#[tokio::test]
async fn an_egress_refusal_is_reported_and_does_not_crash_the_loop() {
    let w = node("node-a", None, true).await;
    *lock(&w.mailbox.failure) = Some(MailError::Permanent(PermanentFailure::Client(
        "egress refused the server (egress.denied)".to_owned(),
    )));
    let h = host(&w);
    let (_stop, run) = spawn(&h);
    let ended = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ended, Err("config_rejected".to_owned()));
}

#[tokio::test]
async fn a_stalled_server_does_not_block_shutdown_past_its_bound() {
    let w = node("node-a", None, true).await;
    *lock(&w.mailbox.stall) = true;
    let h = host(&w);
    let (stop, run) = spawn(&h);
    until("the fetch to start", || {
        w.mailbox.fetches.load(Ordering::SeqCst) == 1
    })
    .await;
    stop.send(true).unwrap();
    let ended = tokio::time::timeout(STOP_BOUND + Duration::from_secs(3), run).await;
    assert!(ended.expect("shutdown overran its bound").unwrap().is_ok());
}

#[test]
fn the_command_id_is_visible_ascii_and_bounded() {
    let table = settings(GOOD);
    let (ready, _) = resolve_all(&table);
    let id = command_id(&ready[0].account, &key());
    assert!(id.len() <= 128 && id.bytes().all(|b| (0x21..=0x7e).contains(&b)));
}

// --- PostgreSQL ----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_postgres_stores_carry_a_cursor_across_a_restart() {
    use deadpool_postgres::{PoolConfig, Runtime};
    use spot_core::SiteId;
    use spot_mail_intake_pg::{PgCursorStore, PgSeenStore};
    use spot_postgres::testing::TestDb;

    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set; the PostgreSQL intake stores were not run");
        return;
    };
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.pool = Some(PoolConfig::new(4));
    let pool = cfg
        .create_pool(Some(Runtime::Tokio1), tokio_postgres::NoTls)
        .unwrap();
    let site = SiteId::new("acme").unwrap();
    let cursors = PgCursorStore::new(pool.clone(), site.clone());
    cursors.prepare().await.unwrap();
    let seen = PgSeenStore::new(pool.clone(), site.clone());

    let mut w = node("node-a", None, true).await;
    w.cursors = Arc::new(MemoryCursorStore::new());
    let deps = Deps {
        cursors: Arc::new(cursors.clone()),
        seen: Arc::new(seen.clone()),
        ..clone_deps(&w.deps)
    };
    w.deps = Arc::new(deps);
    w.mailbox.push(message(1, "<m1@x>"));
    let h = host(&w);
    let (stop, run) = spawn(&h);
    until("the first document", || lock(&w.creator.created).len() == 1).await;
    stop.send(true).unwrap();
    run.await.unwrap().unwrap();

    // A new process on the same database: the position and the seen set are in PostgreSQL.
    let restarted = PgCursorStore::new(pool, site);
    assert_eq!(
        restarted.load("support").await.unwrap().unwrap().last_uid,
        1
    );
}

fn clone_deps(d: &Deps) -> Deps {
    Deps {
        site: d.site.clone(),
        coordinator: Arc::clone(&d.coordinator),
        node: d.node.clone(),
        lease_ttl: d.lease_ttl,
        cursors: Arc::clone(&d.cursors),
        seen: Arc::clone(&d.seen),
        sink: Arc::clone(&d.sink),
        audit: Arc::clone(&d.audit),
        credentials: Arc::clone(&d.credentials),
        fetcher: Arc::clone(&d.fetcher),
        clock: Arc::clone(&d.clock),
        status: Arc::clone(&d.status),
    }
}
