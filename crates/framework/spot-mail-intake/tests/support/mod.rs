#![allow(dead_code, clippy::panic, clippy::unwrap_used, clippy::expect_used)]
// Every integration-test binary compiles this module; each drives only part of it.

//! Test harness: a scripted mailbox, a scripted sink and the in-memory ports wired together.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use spot_coordinator::{InMemoryPresence, InProcessCoordinator, NodeId};
use spot_core::ManualClock;
use spot_mail::{
    FetchBatch, FetchLimits, FetchOutcome, ImapSecurity, IncomingMessage, MailAddress, MailError,
    MailboxCursor, Secret,
};
use spot_mail_intake::memory::{
    MemoryAudit, MemoryCursorStore, MemorySeenStore, StaticCredentials,
};
use spot_mail_intake::{
    Accepted, Account, DedupeKey, ImapEndpoint, IntakeLimits, IntakeSink, MailFetcher, PollDeps,
    SinkError,
};
use time::OffsetDateTime;

pub const PASSWORD: &str = "hunter2-very-secret";
pub const OWN_ADDRESS: &str = "support@example.test";
pub const SITE: &str = "acme";

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A test-only unwrap that names the failure.
pub fn must<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|error| panic!("unexpected error: {error:?}"))
}

pub fn account() -> Account {
    Account {
        name: "support".to_owned(),
        imap: ImapEndpoint {
            host: "imap.example.test".to_owned(),
            port: 993,
            security: ImapSecurity::Tls,
            username: OWN_ADDRESS.to_owned(),
            timeout: Duration::from_secs(10),
            root_certificate_pem: None,
        },
        credential_ref: "mail/support".to_owned(),
        mailbox: "INBOX".to_owned(),
        address: OWN_ADDRESS.to_owned(),
        enabled: true,
        poll_interval: Duration::from_secs(60),
        limits: IntakeLimits {
            max_messages_per_poll: 50,
            max_message_bytes: 1024,
            max_messages_per_day: 1000,
        },
    }
}

pub fn message(uid: u32, id: &str) -> IncomingMessage {
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
            address: OWN_ADDRESS.to_owned(),
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

struct MailboxState {
    validity: u32,
    messages: Vec<IncomingMessage>,
    failure: Option<MailError>,
}

/// A mailbox that answers like `fetch_new_pinned` does.
pub struct FakeMailbox {
    state: Mutex<MailboxState>,
    fetches: AtomicUsize,
}

impl FakeMailbox {
    pub fn new(validity: u32) -> Self {
        Self {
            state: Mutex::new(MailboxState {
                validity,
                messages: Vec::new(),
                failure: None,
            }),
            fetches: AtomicUsize::new(0),
        }
    }

    pub fn push(&self, message: IncomingMessage) {
        lock(&self.state).messages.push(message);
    }

    pub fn set_validity(&self, validity: u32) {
        lock(&self.state).validity = validity;
    }

    pub fn fail_with(&self, failure: Option<MailError>) {
        lock(&self.state).failure = failure;
    }

    pub fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl MailFetcher for FakeMailbox {
    async fn fetch(
        &self,
        _account: &Account,
        _secret: Secret,
        cursor: Option<MailboxCursor>,
        limits: FetchLimits,
    ) -> Result<FetchBatch, MailError> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let state = lock(&self.state);
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        let (outcome, last) = match cursor {
            Some(c) if c.uid_validity != state.validity => (FetchOutcome::MailboxReset, 0),
            Some(c) => (FetchOutcome::New, c.last_uid),
            None => (FetchOutcome::New, 0),
        };
        let mut pending: Vec<&IncomingMessage> =
            state.messages.iter().filter(|m| m.uid > last).collect();
        pending.sort_by_key(|m| m.uid);
        let more = pending.len() > limits.max_messages;
        pending.truncate(limits.max_messages);
        let messages: Vec<IncomingMessage> = pending
            .into_iter()
            .map(|m| {
                let mut m = m.clone();
                if m.raw_size > limits.max_message_bytes {
                    m.truncated = true;
                    m.text = None;
                    m.html = None;
                }
                m
            })
            .collect();
        let highest = messages.last().map_or(last, |m| m.uid);
        Ok(FetchBatch {
            outcome,
            cursor: MailboxCursor {
                uid_validity: state.validity,
                last_uid: last.max(highest),
            },
            messages,
            more,
        })
    }
}

#[derive(Default)]
struct SinkState {
    /// (uid, normalised message id or empty) in the order the sink was called, each call.
    calls: Vec<u32>,
    created: Vec<String>,
    keys: BTreeSet<String>,
    retry: BTreeSet<u32>,
    reject: BTreeSet<u32>,
    truncated_seen: Vec<u32>,
}

/// A sink that creates one record per message, can be told to fail on given UIDs and is
/// idempotent on the dedupe key when `idempotent` is set.
#[derive(Default)]
pub struct ScriptedSink {
    state: Mutex<SinkState>,
    pub idempotent: bool,
}

impl ScriptedSink {
    pub fn retry_on(&self, uid: u32) {
        lock(&self.state).retry.insert(uid);
    }

    pub fn clear_retry(&self) {
        lock(&self.state).retry.clear();
    }

    pub fn reject_on(&self, uid: u32) {
        lock(&self.state).reject.insert(uid);
    }

    /// UIDs the sink created a record for, in order.
    pub fn created_uids(&self) -> Vec<u32> {
        lock(&self.state)
            .created
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect()
    }

    /// Every UID the sink was called with, in order (including retries).
    pub fn calls(&self) -> Vec<u32> {
        lock(&self.state).calls.clone()
    }

    pub fn truncated_seen(&self) -> Vec<u32> {
        lock(&self.state).truncated_seen.clone()
    }
}

#[async_trait]
impl IntakeSink for ScriptedSink {
    async fn accept(
        &self,
        _account: &Account,
        message: &IncomingMessage,
        key: &DedupeKey,
    ) -> Result<Accepted, SinkError> {
        let mut state = lock(&self.state);
        state.calls.push(message.uid);
        if state.retry.contains(&message.uid) {
            return Err(SinkError::Retryable);
        }
        if state.reject.contains(&message.uid) {
            return Err(SinkError::Rejected("invalid_field"));
        }
        if message.truncated {
            state.truncated_seen.push(message.uid);
        }
        if self.idempotent && !state.keys.insert(key.as_str().to_owned()) {
            return Ok(Accepted::Duplicate);
        }
        state.created.push(message.uid.to_string());
        Ok(Accepted::Created(format!("doc-{}", message.uid)))
    }
}

pub struct Harness {
    pub coordinator: Arc<InProcessCoordinator>,
    pub clock: Arc<ManualClock>,
    pub cursors: MemoryCursorStore,
    pub seen: MemorySeenStore,
    pub sink: ScriptedSink,
    pub audit: MemoryAudit,
    pub creds: StaticCredentials,
    pub mailbox: FakeMailbox,
}

impl Harness {
    pub async fn new() -> Self {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let presence = Arc::new(InMemoryPresence::new());
        let node = must(NodeId::new("node-a"));
        let coordinator = must(InProcessCoordinator::start(presence, node, clock.clone()).await);
        Self {
            coordinator,
            clock,
            cursors: MemoryCursorStore::new(),
            seen: MemorySeenStore::new(),
            sink: ScriptedSink::default(),
            audit: MemoryAudit::new(),
            creds: StaticCredentials::new().with("mail/support", PASSWORD),
            mailbox: FakeMailbox::new(7),
        }
    }

    pub fn deps<'a>(&'a self, holder: &'a str) -> PollDeps<'a> {
        PollDeps {
            coordinator: &*self.coordinator,
            holder,
            site: SITE,
            lease_ttl: Duration::from_secs(300),
            cursors: &self.cursors,
            seen: &self.seen,
            sink: &self.sink,
            audit: &self.audit,
            credentials: &self.creds,
            fetcher: &self.mailbox,
        }
    }

    pub fn now(&self) -> OffsetDateTime {
        use spot_core::Clock as _;
        self.clock.now_utc()
    }
}

pub fn dropped(report: &spot_mail_intake::PollReport) -> BTreeMap<&'static str, u32> {
    report.dropped.clone()
}
