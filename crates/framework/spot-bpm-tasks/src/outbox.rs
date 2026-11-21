//! The durable notification outbox for the task inbox (WP-BPM-05.T14).
//!
//! Until now a notification was best effort: the service queued it in memory, the host flushed
//! it after the state change, and a restart or a send dropped half way lost it. That is fine for
//! a courtesy copy but not for mail that must arrive (an approval request, an escalation). This
//! module is the durable shape:
//!
//! 1. **A row, not a promise.** An [`OutboxEntry`] is written in the same storage transaction as
//!    the task change that causes it ([`TransactionalTaskStore`]): when the state commits the
//!    entry exists, when it rolls back it does not.
//! 2. **At-least-once delivery.** An [`OutboxWorker`] claims due entries under a lease, sends
//!    them through a [`DeliveryTransport`], marks them [`OutboxStatus::Sent`], and on failure
//!    schedules the next attempt with capped exponential backoff ([`BackoffPolicy`]). After the
//!    maximum number of attempts an entry is [`OutboxStatus::Dead`] and visible to operators.
//! 3. **Duplicates are harmless.** Every entry carries an idempotency key
//!    `{task id}:{task version}:{kind}:{recipient}`, handed to the transport, so a downstream
//!    channel can suppress a repeat.
//!
//! # Ports
//!
//! [`OutboxStore`] and [`TransactionalTaskStore`] are ports owned by this crate. The PostgreSQL
//! adapter is the host's; the in-memory references ([`InMemoryOutbox`],
//! [`InMemoryOutboxTaskStore`]) define the contract the adapter must pass and are what the tests
//! use. The adapter needs a table in a migration the storage contract allows; see
//! `docs/running-spot/notification_outbox.md`.
//!
//! # Content rule
//!
//! Unchanged from [`crate::notify`]: an entry carries a [`Notification`], so ids and titles only,
//! never a form value or a comment. A failure detail kept on an entry is the transport's short
//! message, cut to [`MAX_ERROR_CHARS`] characters.
//!
//! # Purity
//!
//! No clock is read: `now` is an argument of every call. Instants computed here (a retry, a
//! lease end) are whole seconds after `now`.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_app_api::prelude::OffsetDateTime;

use crate::error::TasksResult;
use crate::event::TaskEvent;
use crate::notify::{Notification, NotificationKind, Notifier};
use crate::store::{InMemoryTaskStore, TaskStore};
use crate::task::{Task, TaskFilter, TaskId};

/// The longest failure detail an entry keeps, in characters.
pub const MAX_ERROR_CHARS: usize = 256;

/// The largest delay or lease this module computes, in seconds (100 years): it keeps every
/// computed instant inside the representable range for any sane `now`.
const MAX_SECS: u64 = 3_153_600_000;

/// Where an outbox entry is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OutboxStatus {
    /// Waiting for a (first or next) delivery attempt.
    Pending,
    /// Delivered; kept for audit.
    Sent,
    /// Gave up after the maximum attempts; for an operator to look at.
    Dead,
}

impl OutboxStatus {
    /// The stable text form, for a storage column or an operator page.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sent => "sent",
            Self::Dead => "dead",
        }
    }
}

/// The lease a worker holds on an entry while it sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    /// The worker that claimed the entry.
    pub worker: String,
    /// When the lease ends; after that another worker may claim the entry again.
    pub until: OffsetDateTime,
}

/// One durable notification to one recipient.
#[derive(Clone, Debug, PartialEq)]
pub struct OutboxEntry {
    /// The idempotency key `{task id}:{task version}:{kind}:{recipient}`; it is also the entry's
    /// id, so enqueueing the same notification twice stores it once.
    pub key: String,
    /// The task the entry is about.
    pub task: TaskId,
    /// Why the notification is sent.
    pub kind: NotificationKind,
    /// The one user the entry is for.
    pub recipient: String,
    /// What is sent: ids and titles only, with `recipients` holding just this recipient.
    pub notification: Notification,
    /// When the entry was written.
    pub created_at: OffsetDateTime,
    /// How many delivery attempts were started (a claim counts, so a crash mid-send counts).
    pub attempts: u32,
    /// The earliest instant of the next attempt.
    pub next_attempt_at: OffsetDateTime,
    /// The entry's status.
    pub status: OutboxStatus,
    /// The worker lease, while an attempt is in flight (or its holder died).
    pub lease: Option<Lease>,
    /// The last failure, cut to [`MAX_ERROR_CHARS`] characters.
    pub last_error: Option<String>,
    /// When the entry was delivered.
    pub sent_at: Option<OffsetDateTime>,
}

/// The idempotency key of a notification to `recipient` about `task` at `task_version`.
#[must_use]
pub fn idempotency_key(
    task: &TaskId,
    task_version: u64,
    kind: NotificationKind,
    recipient: &str,
) -> String {
    format!("{task}:{task_version}:{}:{recipient}", kind.as_str())
}

/// The entries one notification becomes: one per distinct recipient, each due at `now`.
///
/// `task_version` is the version of the task the change produced, so two changes to the same
/// task never share a key.
#[must_use]
pub fn entries_for(
    notification: &Notification,
    task_version: u64,
    now: OffsetDateTime,
) -> Vec<OutboxEntry> {
    let recipients: BTreeSet<&String> = notification.recipients.iter().collect();
    recipients
        .into_iter()
        .map(|recipient| {
            let mut single = notification.clone();
            single.recipients = vec![recipient.clone()];
            OutboxEntry {
                key: idempotency_key(
                    &notification.task,
                    task_version,
                    notification.kind,
                    recipient,
                ),
                task: notification.task.clone(),
                kind: notification.kind,
                recipient: recipient.clone(),
                notification: single,
                created_at: now,
                attempts: 0,
                next_attempt_at: now,
                status: OutboxStatus::Pending,
                lease: None,
                last_error: None,
                sent_at: None,
            }
        })
        .collect()
}

/// Durable storage of outbox entries.
///
/// Every method that changes an entry is atomic: two workers calling [`OutboxStore::claim_due`]
/// at the same time never receive the same entry.
pub trait OutboxStore: Send + Sync {
    /// Stores new entries; an entry whose key already exists is ignored, so a replayed state
    /// change adds nothing. Returns how many entries were new.
    ///
    /// # Errors
    /// [`crate::TasksError::StoreFailed`] when the backend fails.
    fn enqueue(&self, entries: Vec<OutboxEntry>) -> TasksResult<usize>;

    /// Claims up to `limit` entries that are due at `now`: `Pending`, `next_attempt_at <= now`
    /// and not under a live lease. Each claimed entry gets a lease for `worker` until
    /// `lease_until` and its `attempts` goes up by one. Entries come oldest due first, then by
    /// key.
    ///
    /// # Errors
    /// [`crate::TasksError::StoreFailed`] when the backend fails.
    fn claim_due(
        &self,
        worker: &str,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
        limit: usize,
    ) -> TasksResult<Vec<OutboxEntry>>;

    /// Marks the entry delivered. Returns `false` and changes nothing when the entry is not
    /// pending under `worker`'s lease (it was claimed by another worker after the lease ended).
    ///
    /// # Errors
    /// [`crate::TasksError::StoreFailed`] when the backend fails.
    fn mark_sent(&self, key: &str, worker: &str, now: OffsetDateTime) -> TasksResult<bool>;

    /// Records a failed attempt. With `retry_at` the entry stays pending and is due again then;
    /// with `None` it becomes dead. The lease is released. Returns `false` and changes nothing
    /// when the entry is not pending under `worker`'s lease.
    ///
    /// # Errors
    /// [`crate::TasksError::StoreFailed`] when the backend fails.
    fn mark_failed(
        &self,
        key: &str,
        worker: &str,
        error: &str,
        retry_at: Option<OffsetDateTime>,
    ) -> TasksResult<bool>;

    /// The entry with this key.
    ///
    /// # Errors
    /// [`crate::TasksError::StoreFailed`] when the backend fails.
    fn get(&self, key: &str) -> TasksResult<Option<OutboxEntry>>;

    /// Every entry, or those of one status, sorted by key: the operator view, so dead entries
    /// are visible.
    ///
    /// # Errors
    /// [`crate::TasksError::StoreFailed`] when the backend fails.
    fn list(&self, status: Option<OutboxStatus>) -> TasksResult<Vec<OutboxEntry>>;
}

/// The in-memory reference [`OutboxStore`]; clones share the state, so a "restarted" worker over
/// a clone sees what the first one left.
#[derive(Debug, Clone, Default)]
pub struct InMemoryOutbox {
    entries: Arc<Mutex<BTreeMap<String, OutboxEntry>>>,
}

impl InMemoryOutbox {
    /// An empty outbox.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, OutboxEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn truncated(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

fn holds(entry: &OutboxEntry, worker: &str) -> bool {
    entry.status == OutboxStatus::Pending
        && entry
            .lease
            .as_ref()
            .is_some_and(|lease| lease.worker == worker)
}

impl OutboxStore for InMemoryOutbox {
    fn enqueue(&self, entries: Vec<OutboxEntry>) -> TasksResult<usize> {
        let mut stored = self.lock();
        let mut added = 0;
        for entry in entries {
            if !stored.contains_key(&entry.key) {
                stored.insert(entry.key.clone(), entry);
                added += 1;
            }
        }
        Ok(added)
    }

    fn claim_due(
        &self,
        worker: &str,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
        limit: usize,
    ) -> TasksResult<Vec<OutboxEntry>> {
        let mut stored = self.lock();
        let mut due: Vec<(OffsetDateTime, String)> = stored
            .values()
            .filter(|entry| {
                entry.status == OutboxStatus::Pending
                    && entry.next_attempt_at <= now
                    && entry.lease.as_ref().is_none_or(|lease| lease.until <= now)
            })
            .map(|entry| (entry.next_attempt_at, entry.key.clone()))
            .collect();
        due.sort();
        due.truncate(limit);
        let mut claimed = Vec::with_capacity(due.len());
        for (_, key) in due {
            if let Some(entry) = stored.get_mut(&key) {
                entry.attempts = entry.attempts.saturating_add(1);
                entry.lease = Some(Lease {
                    worker: worker.to_owned(),
                    until: lease_until,
                });
                claimed.push(entry.clone());
            }
        }
        Ok(claimed)
    }

    fn mark_sent(&self, key: &str, worker: &str, now: OffsetDateTime) -> TasksResult<bool> {
        let mut stored = self.lock();
        let Some(entry) = stored.get_mut(key) else {
            return Ok(false);
        };
        if !holds(entry, worker) {
            return Ok(false);
        }
        entry.status = OutboxStatus::Sent;
        entry.lease = None;
        entry.sent_at = Some(now);
        entry.last_error = None;
        Ok(true)
    }

    fn mark_failed(
        &self,
        key: &str,
        worker: &str,
        error: &str,
        retry_at: Option<OffsetDateTime>,
    ) -> TasksResult<bool> {
        let mut stored = self.lock();
        let Some(entry) = stored.get_mut(key) else {
            return Ok(false);
        };
        if !holds(entry, worker) {
            return Ok(false);
        }
        entry.lease = None;
        entry.last_error = Some(truncated(error));
        match retry_at {
            Some(at) => entry.next_attempt_at = at,
            None => entry.status = OutboxStatus::Dead,
        }
        Ok(true)
    }

    fn get(&self, key: &str) -> TasksResult<Option<OutboxEntry>> {
        Ok(self.lock().get(key).cloned())
    }

    fn list(&self, status: Option<OutboxStatus>) -> TasksResult<Vec<OutboxEntry>> {
        Ok(self
            .lock()
            .values()
            .filter(|entry| status.is_none_or(|wanted| entry.status == wanted))
            .cloned()
            .collect())
    }
}

/// A [`TaskStore`] that also writes outbox entries in the same transaction as the task change.
///
/// The notices are given as [`Notification`]s, not as entries, because the idempotency key
/// contains the version the change produces and only the store knows it. The store turns each
/// notice into entries with [`entries_for`] at the stored version.
pub trait TransactionalTaskStore: TaskStore {
    /// [`TaskStore::create`] plus the entries of `notices`. When a task already exists for the
    /// `(case, node)` pair nothing is written, entries included.
    ///
    /// # Errors
    /// [`crate::TasksError::StoreFailed`] when the backend fails; then neither the task nor an entry
    /// is stored.
    fn create_with_outbox(
        &self,
        task: Task,
        event: TaskEvent,
        notices: &[Notification],
        now: OffsetDateTime,
    ) -> TasksResult<Task>;

    /// [`TaskStore::commit`] plus the entries of `notices`. When the commit fails (version
    /// conflict, unknown task, backend failure) no entry is stored.
    ///
    /// # Errors
    /// The errors of [`TaskStore::commit`].
    fn commit_with_outbox(
        &self,
        id: &TaskId,
        expected_version: u64,
        task: Task,
        event: Option<TaskEvent>,
        notices: &[Notification],
        now: OffsetDateTime,
    ) -> TasksResult<Task>;
}

/// The in-memory reference [`TransactionalTaskStore`]: an [`InMemoryTaskStore`] and an
/// [`InMemoryOutbox`] changed under one gate, so no reader of either sees half a transaction
/// through this type's methods.
#[derive(Debug, Clone, Default)]
pub struct InMemoryOutboxTaskStore {
    tasks: InMemoryTaskStore,
    outbox: InMemoryOutbox,
    gate: Arc<Mutex<()>>,
}

impl InMemoryOutboxTaskStore {
    /// An empty store and outbox.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The outbox half, to hand to an [`OutboxWorker`].
    #[must_use]
    pub fn outbox(&self) -> InMemoryOutbox {
        self.outbox.clone()
    }

    fn gate(&self) -> MutexGuard<'_, ()> {
        self.gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl TaskStore for InMemoryOutboxTaskStore {
    fn create(&self, task: Task, event: TaskEvent) -> TasksResult<Task> {
        let _gate = self.gate();
        self.tasks.create(task, event)
    }

    fn load(&self, id: &TaskId) -> TasksResult<Option<Task>> {
        self.tasks.load(id)
    }

    fn find_by_node(&self, case: &str, node: &str) -> TasksResult<Option<Task>> {
        self.tasks.find_by_node(case, node)
    }

    fn commit(
        &self,
        id: &TaskId,
        expected_version: u64,
        task: Task,
        event: Option<TaskEvent>,
    ) -> TasksResult<Task> {
        let _gate = self.gate();
        self.tasks.commit(id, expected_version, task, event)
    }

    fn events(&self, id: &TaskId) -> TasksResult<Vec<TaskEvent>> {
        self.tasks.events(id)
    }

    fn list(&self, filter: &TaskFilter) -> TasksResult<Vec<Task>> {
        self.tasks.list(filter)
    }
}

impl TransactionalTaskStore for InMemoryOutboxTaskStore {
    fn create_with_outbox(
        &self,
        task: Task,
        event: TaskEvent,
        notices: &[Notification],
        now: OffsetDateTime,
    ) -> TasksResult<Task> {
        let _gate = self.gate();
        let existed = self.tasks.find_by_node(&task.case, &task.node)?.is_some();
        let stored = self.tasks.create(task, event)?;
        if !existed {
            let entries = notices
                .iter()
                .flat_map(|notice| entries_for(notice, stored.version, now))
                .collect();
            self.outbox.enqueue(entries)?;
        }
        Ok(stored)
    }

    fn commit_with_outbox(
        &self,
        id: &TaskId,
        expected_version: u64,
        task: Task,
        event: Option<TaskEvent>,
        notices: &[Notification],
        now: OffsetDateTime,
    ) -> TasksResult<Task> {
        let _gate = self.gate();
        let stored = self.tasks.commit(id, expected_version, task, event)?;
        let entries = notices
            .iter()
            .flat_map(|notice| entries_for(notice, stored.version, now))
            .collect();
        self.outbox.enqueue(entries)?;
        Ok(stored)
    }
}

/// A delivery failed. The detail is plain language and must not contain form values, comments or
/// secrets; the outbox stores it (cut short) and an operator reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryError {
    /// What went wrong.
    pub detail: String,
}

impl DeliveryError {
    /// A failure with this detail.
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

/// The future a [`DeliveryTransport`] returns.
pub type DeliveryFuture<'a> = Pin<Box<dyn Future<Output = Result<(), DeliveryError>> + Send + 'a>>;

/// A channel that can say whether it delivered.
///
/// [`Notifier`] cannot: its output is `()`. The outbox needs the answer to decide between "sent"
/// and "try again", so the worker sends through this port. An adapter over a real mail transport
/// maps its send result; [`NotifierTransport`] wraps a [`Notifier`] for a channel that has no
/// failure to report.
pub trait DeliveryTransport: Send + Sync {
    /// Delivers `notification` (one recipient). `idempotency_key` is stable across attempts of
    /// the same entry; a channel that can suppress duplicates should key on it. The future may
    /// be dropped at any `.await`; the entry is then retried after its lease ends.
    fn deliver<'a>(
        &'a self,
        notification: &'a Notification,
        idempotency_key: &'a str,
    ) -> DeliveryFuture<'a>;
}

/// Adapts a [`Notifier`] to a [`DeliveryTransport`] that always reports success.
#[derive(Clone)]
pub struct NotifierTransport {
    notifier: Arc<dyn Notifier>,
}

impl NotifierTransport {
    /// Wraps `notifier`.
    #[must_use]
    pub fn new(notifier: Arc<dyn Notifier>) -> Self {
        Self { notifier }
    }
}

impl DeliveryTransport for NotifierTransport {
    fn deliver<'a>(
        &'a self,
        notification: &'a Notification,
        _idempotency_key: &'a str,
    ) -> DeliveryFuture<'a> {
        Box::pin(async move {
            self.notifier.notify(notification).await;
            Ok(())
        })
    }
}

/// How the worker spaces and bounds its attempts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackoffPolicy {
    /// The delay after the first failed attempt, in seconds; it doubles with each further one.
    pub base_secs: u64,
    /// The longest delay between attempts, in seconds.
    pub cap_secs: u64,
    /// The attempts after which an entry is dead (at least 1).
    pub max_attempts: u32,
    /// How long a claim lasts, in seconds; longer than a batch takes to send.
    pub lease_secs: u64,
    /// How many entries one run claims.
    pub batch: usize,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            base_secs: 30,
            cap_secs: 3_600,
            max_attempts: 8,
            lease_secs: 300,
            batch: 50,
        }
    }
}

impl BackoffPolicy {
    /// The delay after `attempts` failed attempts (`attempts >= 1`): `base * 2^(attempts - 1)`,
    /// capped.
    #[must_use]
    pub fn delay_secs(&self, attempts: u32) -> u64 {
        let doublings = attempts.saturating_sub(1).min(63);
        let factor = 1u64 << doublings;
        self.base_secs
            .saturating_mul(factor)
            .min(self.cap_secs)
            .min(MAX_SECS)
    }
}

/// `now` plus whole `secs` seconds, never more than 100 years.
fn after(now: OffsetDateTime, secs: u64) -> OffsetDateTime {
    let secs = i64::try_from(secs.min(MAX_SECS)).unwrap_or(0);
    OffsetDateTime::from_unix_timestamp(now.unix_timestamp().saturating_add(secs)).unwrap_or(now)
}

/// What one [`OutboxWorker::run_once`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunReport {
    /// Entries this run claimed.
    pub claimed: usize,
    /// Entries delivered.
    pub sent: usize,
    /// Entries whose attempt failed and will be retried.
    pub retried: usize,
    /// Entries that became dead.
    pub dead: usize,
    /// Entries whose lease another worker took over before this run recorded the result; the
    /// other worker owns them now (the notification may be delivered twice).
    pub lost_lease: usize,
}

/// Sends due outbox entries: claim under a lease, deliver, record, back off on failure.
///
/// Run [`OutboxWorker::run_once`] from a scheduler job in the host. Any number of workers may
/// run at once over one store: the claim is atomic.
#[derive(Clone)]
pub struct OutboxWorker {
    store: Arc<dyn OutboxStore>,
    transport: Arc<dyn DeliveryTransport>,
    policy: BackoffPolicy,
    id: String,
}

impl OutboxWorker {
    /// A worker named `id` (unique among concurrent workers: it names the lease).
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        store: Arc<dyn OutboxStore>,
        transport: Arc<dyn DeliveryTransport>,
        policy: BackoffPolicy,
    ) -> Self {
        Self {
            store,
            transport,
            policy,
            id: id.into(),
        }
    }

    /// The worker's id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// One pass at `now`: claims due entries, delivers each in turn and records the result.
    ///
    /// An entry already claimed `max_attempts` times whose attempts all ended without a recorded
    /// result (the worker died each time) is made dead without another send.
    ///
    /// # Errors
    /// [`crate::TasksError::StoreFailed`] when the outbox store fails; entries claimed so far keep their
    /// lease and are retried after it ends.
    pub async fn run_once(&self, now: OffsetDateTime) -> TasksResult<RunReport> {
        let max_attempts = self.policy.max_attempts.max(1);
        let claimed = self.store.claim_due(
            &self.id,
            now,
            after(now, self.policy.lease_secs),
            self.policy.batch,
        )?;
        let mut report = RunReport {
            claimed: claimed.len(),
            ..RunReport::default()
        };
        for entry in claimed {
            if entry.attempts > max_attempts {
                let recorded = self.store.mark_failed(
                    &entry.key,
                    &self.id,
                    "gave up: every attempt ended without a result",
                    None,
                )?;
                tally(&mut report, recorded, Outcome::Dead);
                continue;
            }
            match self
                .transport
                .deliver(&entry.notification, &entry.key)
                .await
            {
                Ok(()) => {
                    let recorded = self.store.mark_sent(&entry.key, &self.id, now)?;
                    tally(&mut report, recorded, Outcome::Sent);
                }
                Err(failure) if entry.attempts >= max_attempts => {
                    let recorded =
                        self.store
                            .mark_failed(&entry.key, &self.id, &failure.detail, None)?;
                    tally(&mut report, recorded, Outcome::Dead);
                }
                Err(failure) => {
                    let retry_at = after(now, self.policy.delay_secs(entry.attempts));
                    let recorded = self.store.mark_failed(
                        &entry.key,
                        &self.id,
                        &failure.detail,
                        Some(retry_at),
                    )?;
                    tally(&mut report, recorded, Outcome::Retried);
                }
            }
        }
        Ok(report)
    }
}

#[derive(Clone, Copy)]
enum Outcome {
    Sent,
    Retried,
    Dead,
}

fn tally(report: &mut RunReport, recorded: bool, outcome: Outcome) {
    if !recorded {
        report.lost_lease += 1;
        return;
    }
    match outcome {
        Outcome::Sent => report.sent += 1,
        Outcome::Retried => report.retried += 1,
        Outcome::Dead => report.dead += 1,
    }
}
