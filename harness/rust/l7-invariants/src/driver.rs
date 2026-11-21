//! The workload driver: a seeded, multi-worker mix of transactions against a storage backend.
//!
//! # What it records
//!
//! Every operation becomes a [`HistoryEntry`]. Every attempt that commits is folded into the
//! [`Oracle`]. A transaction that fails with a retryable error is rerun whole, within the
//! [`RetryPolicy`]; one that runs out of retries is recorded as `Failed`, never as a success.
//!
//! # Indeterminate commits
//!
//! When `commit` fails with `CommitOutcomeUnknown` the driver cannot tell if the transaction
//! happened. Every write transaction enqueues a **marker** outbox event with a unique client-made
//! id as its first event, in the same transaction as its other effects. After the workers stop,
//! the driver reads the outbox back and looks for each indeterminate attempt's marker: present
//! means it committed (its effects join the oracle), absent means it did not. The final oracle is
//! therefore exact.
//!
//! # Determinism
//!
//! The *schedule* (who does what, in which order, at which isolation level) is a pure function of
//! the seed ([`Schedule::generate`]). The interleaving of workers on real threads is not, so a
//! replay repeats the plan, not necessarily every conflict.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use spot_storage_contract::{
    ChangeOp, ChildRow, DocumentData, DocumentStore, ErrorKind, FeedPosition, Fields, Name,
    NodeRef, Outbox, OutboxEvent, RecordId, RowLock, Sequences, SeriesKey, StorageBackend,
    StorageError, StorageResult, Transaction, TxOptions, Value, Wait,
};

use crate::history::{History, HistoryEntry, IncrementMode, OpKind, Outcome, Role, Touched};
use crate::model::Model;
use crate::oracle::{DocWrite, Effects, Oracle};
use crate::schedule::{PlannedOp, Profile, Rng, Schedule, Weights};

/// How a failed transaction is rerun.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts per operation, the first included.
    pub max_attempts: u32,
    /// Backoff ceiling after the first failure, microseconds; it doubles per attempt.
    pub base_backoff_us: u64,
    /// The longest backoff, microseconds.
    pub max_backoff_us: u64,
}

impl Default for RetryPolicy {
    fn default() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 8,
            base_backoff_us: 500,
            max_backoff_us: 20_000,
        }
    }
}

impl RetryPolicy {
    /// The backoff before attempt `attempt + 1`, jittered by `rng`.
    pub fn backoff(&self, attempt: u32, rng: &mut Rng) -> Duration {
        let ceiling = self
            .base_backoff_us
            .saturating_mul(1u64 << attempt.min(16))
            .min(self.max_backoff_us)
            .max(1);
        Duration::from_micros(1 + rng.below(ceiling))
    }
}

/// What to run.
#[derive(Clone, Debug)]
pub struct DriverConfig {
    /// The seed.
    pub seed: u64,
    /// Concurrent workers.
    pub workers: u32,
    /// Operations over all workers.
    pub operations: u64,
    /// The operation mix.
    pub weights: Weights,
    /// The retry policy.
    pub retry: RetryPolicy,
    /// Lock timeout declared by every transaction.
    pub lock_timeout: Duration,
    /// Statement timeout declared by every transaction.
    pub statement_timeout: Duration,
    /// The profile's name, for the history header and report.
    pub profile: String,
}

impl DriverConfig {
    /// The configuration of a named profile.
    pub fn for_profile(profile: Profile, seed: u64) -> DriverConfig {
        DriverConfig {
            seed,
            workers: profile.workers(),
            operations: profile.operations(),
            weights: Weights::default(),
            retry: RetryPolicy::default(),
            lock_timeout: Duration::from_secs(60),
            statement_timeout: Duration::from_secs(120),
            profile: profile.name().to_owned(),
        }
    }
}

/// What a run leaves behind for the checks.
pub struct RunResult {
    /// The history, ordered by start time.
    pub history: History,
    /// The exact expected state.
    pub oracle: Oracle,
    /// The seed.
    pub seed: u64,
    /// Workers used.
    pub workers: u32,
    /// Profile name.
    pub profile: String,
}

struct Pending {
    worker: u32,
    seq: u64,
    fx: Effects,
}

#[derive(Default)]
struct Pools {
    parties: Vec<RecordId>,
    orders: Vec<RecordId>,
    counters: Vec<RecordId>,
}

impl Pools {
    fn pick(list: &[RecordId], pick: u64) -> Option<RecordId> {
        if list.is_empty() {
            None
        } else {
            list.get((pick % list.len() as u64) as usize).copied()
        }
    }
}

struct Shared {
    t0: Instant,
    group: AtomicU64,
    oracle: Mutex<Oracle>,
    history: Mutex<Vec<HistoryEntry>>,
    pools: Mutex<Pools>,
    pending: Mutex<Vec<Pending>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

enum Step {
    Commit,
    Rollback,
    Reject(&'static str),
}

enum End {
    Committed,
    RolledBack,
    Rejected(ErrorKind),
    Unknown,
    Error(StorageError),
}

#[derive(Default)]
struct AttemptRecord {
    touched: Vec<Touched>,
    values: BTreeMap<String, i64>,
}

/// The driver. Create one, then [`LoadDriver::run`] it.
pub struct LoadDriver<B: StorageBackend> {
    inner: Arc<Inner<B>>,
}

struct Inner<B: StorageBackend> {
    backend: Arc<B>,
    model: Arc<Model>,
    config: DriverConfig,
    shared: Shared,
}

impl<B: StorageBackend> LoadDriver<B> {
    /// A driver for `backend` running `config`.
    pub fn new(backend: Arc<B>, model: Arc<Model>, config: DriverConfig) -> LoadDriver<B> {
        LoadDriver {
            inner: Arc::new(Inner {
                backend,
                model,
                config,
                shared: Shared {
                    t0: Instant::now(),
                    group: AtomicU64::new(1),
                    oracle: Mutex::new(Oracle::new()),
                    history: Mutex::new(Vec::new()),
                    pools: Mutex::new(Pools::default()),
                    pending: Mutex::new(Vec::new()),
                },
            }),
        }
    }

    /// Prepares the store, seeds it, runs the schedule on `workers` concurrent tasks, resolves
    /// indeterminate commits and returns the history and the oracle.
    pub async fn run(self) -> StorageResult<RunResult> {
        let inner = self.inner;
        inner.backend.prepare(&inner.model.catalog).await?;
        inner.setup().await?;
        let schedule = Schedule::generate(
            inner.config.seed,
            inner.config.workers,
            inner.config.operations,
            &inner.config.weights,
        );
        let mut tasks = Vec::new();
        for (worker, ops) in schedule.workers.into_iter().enumerate() {
            let inner = inner.clone();
            let worker = u32::try_from(worker).unwrap_or(u32::MAX);
            tasks.push(tokio::spawn(async move {
                inner.run_worker(worker, ops).await;
            }));
        }
        for task in tasks {
            if let Err(e) = task.await {
                return Err(StorageError::new(
                    ErrorKind::Internal,
                    format!("a driver worker panicked: {e}"),
                ));
            }
        }
        inner.resolve_pending().await?;
        let mut entries = std::mem::take(&mut *lock(&inner.shared.history));
        entries.sort_by_key(|e| (e.start_us, e.worker, e.seq));
        let mut history = History::new(
            inner.config.seed,
            inner.config.workers,
            &inner.config.profile,
        );
        history.entries = entries;
        let oracle = std::mem::take(&mut *lock(&inner.shared.oracle));
        Ok(RunResult {
            history,
            oracle,
            seed: inner.config.seed,
            workers: inner.config.workers,
            profile: inner.config.profile.clone(),
        })
    }
}

const SETUP_PARTIES: usize = 4;
const SETUP_COUNTERS: usize = 4;

impl<B: StorageBackend> Inner<B> {
    fn now_us(&self) -> u64 {
        u64::try_from(self.shared.t0.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    fn options(&self, label: &str, op: &PlannedOp) -> StorageResult<TxOptions> {
        let mut o = TxOptions::new(Name::new(label)?, op.isolation.to_contract());
        o.lock_timeout = self.config.lock_timeout;
        o.statement_timeout = self.config.statement_timeout;
        if matches!(op.kind, OpKind::Read) {
            o = o.read_only();
        }
        Ok(o)
    }

    // -- setup ------------------------------------------------------------------------------

    async fn setup(&self) -> StorageResult<()> {
        let m = &self.model;
        let mut options = TxOptions::new(
            Name::new("l7_setup")?,
            spot_storage_contract::Isolation::ReadCommitted,
        );
        options.lock_timeout = self.config.lock_timeout;
        let mut tx = self.backend.begin(m.catalog.clone(), options).await?;
        let mut fx = Effects::new(self.shared.group.fetch_add(1, Ordering::Relaxed));
        fx.start_us = self.now_us();
        let mut pools = Pools::default();
        let first = RecordId::new_v7();
        let marker = self
            .marker(&mut tx, &mut fx, &m.party, first, "setup")
            .await?;
        for i in 0..SETUP_PARTIES {
            let id = if i == 0 { first } else { RecordId::new_v7() };
            let mut fields = Fields::new();
            fields.insert(
                m.name.clone(),
                Value::Text(format!("party-setup-{i}-{marker}")),
            );
            fields.insert(m.marker.clone(), Value::Text(format!("setup-{i}-{marker}")));
            let data = DocumentData {
                entity: m.party.clone(),
                id,
                fields,
                children: BTreeMap::new(),
            };
            let version = tx.insert(&data).await?;
            fx.docs
                .push(write(&m.party, id, ChangeOp::Insert, version.0));
            pools.parties.push(id);
        }
        for i in 0..SETUP_COUNTERS {
            let id = RecordId::new_v7();
            let mut fields = Fields::new();
            fields.insert(m.label.clone(), Value::Text(format!("counter-{i}")));
            fields.insert(m.value.clone(), Value::Int(0));
            let data = DocumentData {
                entity: m.counter.clone(),
                id,
                fields,
                children: BTreeMap::new(),
            };
            let version = tx.insert(&data).await?;
            fx.docs
                .push(write(&m.counter, id, ChangeOp::Insert, version.0));
            pools.counters.push(id);
        }
        tx.commit().await?;
        fx.end_us = self.now_us();
        let mut oracle = lock(&self.shared.oracle);
        oracle.apply(&fx);
        for id in &pools.counters {
            oracle.track_counter(*id);
        }
        *lock(&self.shared.pools) = pools;
        Ok(())
    }

    // -- workers ----------------------------------------------------------------------------

    async fn run_worker(&self, worker: u32, ops: Vec<PlannedOp>) {
        let mut rng = Rng::new(
            self.config.seed
                ^ (u64::from(worker) + 1).wrapping_mul(0xA076_1D64_78BD_642F)
                ^ 0x5851_F42D_4C95_7F2D,
        );
        for (seq, op) in ops.iter().enumerate() {
            let entry = self.run_op(worker, seq as u64, op, &mut rng).await;
            lock(&self.shared.history).push(entry);
        }
    }

    async fn run_op(&self, worker: u32, seq: u64, op: &PlannedOp, rng: &mut Rng) -> HistoryEntry {
        let start_us = self.now_us();
        let mut attempts = 0u32;
        let mut record;
        let outcome = loop {
            attempts += 1;
            record = AttemptRecord::default();
            let mut fx = Effects::new(self.shared.group.fetch_add(1, Ordering::Relaxed));
            fx.start_us = self.now_us();
            let end = self.attempt(op, &mut fx, &mut record).await;
            fx.end_us = self.now_us();
            match end {
                End::Committed => {
                    self.fold(&fx);
                    break Outcome::Committed;
                }
                End::RolledBack => {
                    lock(&self.shared.oracle).abort(&fx.events);
                    break Outcome::RolledBack;
                }
                End::Rejected(kind) => {
                    lock(&self.shared.oracle).abort(&fx.events);
                    break Outcome::Rejected {
                        code: kind.code().to_owned(),
                    };
                }
                End::Unknown => {
                    if fx.events.is_empty() {
                        break Outcome::Failed {
                            code: ErrorKind::CommitOutcomeUnknown.code().to_owned(),
                            exhausted: false,
                        };
                    }
                    lock(&self.shared.pending).push(Pending { worker, seq, fx });
                    break Outcome::Indeterminate { resolved: None };
                }
                End::Error(e) => {
                    lock(&self.shared.oracle).abort(&fx.events);
                    let retryable = is_retryable(&e, &op.kind);
                    if retryable && attempts < self.config.retry.max_attempts {
                        let pause = self.config.retry.backoff(attempts, rng);
                        tokio::time::sleep(pause).await;
                        continue;
                    }
                    break Outcome::Failed {
                        code: e.kind.code().to_owned(),
                        exhausted: retryable,
                    };
                }
            }
        };
        HistoryEntry {
            worker,
            seq,
            start_us,
            end_us: self.now_us(),
            kind: op.kind.clone(),
            isolation: op.isolation,
            outcome,
            attempts,
            touched: record.touched,
            values: record.values,
        }
    }

    /// Folds a committed attempt into the oracle and the pools of known records.
    fn fold(&self, fx: &Effects) {
        lock(&self.shared.oracle).apply(fx);
        let m = &self.model;
        let mut pools = lock(&self.shared.pools);
        for w in &fx.docs {
            let list = if w.entity == m.party {
                &mut pools.parties
            } else if w.entity == m.order {
                &mut pools.orders
            } else {
                continue;
            };
            match w.op {
                ChangeOp::Insert => list.push(w.id),
                ChangeOp::Delete => list.retain(|id| *id != w.id),
                ChangeOp::Update => {}
            }
        }
    }

    /// Reads the outbox back and settles every attempt whose commit outcome was unknown.
    async fn resolve_pending(&self) -> StorageResult<()> {
        let pending = std::mem::take(&mut *lock(&self.shared.pending));
        if pending.is_empty() {
            return Ok(());
        }
        let mut present: HashSet<RecordId> = HashSet::new();
        let mut after = FeedPosition::START;
        loop {
            let page = self.backend.read_outbox(after, 5_000).await?;
            let Some(last) = page.last() else { break };
            after = last.position;
            let full = page.len() >= 5_000;
            present.extend(page.iter().map(|r| r.event.id));
            if !full {
                break;
            }
        }
        let mut verdicts: BTreeMap<(u32, u64), bool> = BTreeMap::new();
        let mut oracle = lock(&self.shared.oracle);
        for p in pending {
            let committed = p.fx.events.first().is_some_and(|m| present.contains(m));
            if committed {
                oracle.apply(&p.fx);
            } else {
                oracle.abort(&p.fx.events);
            }
            verdicts.insert((p.worker, p.seq), committed);
        }
        drop(oracle);
        for entry in lock(&self.shared.history).iter_mut() {
            if let Outcome::Indeterminate { resolved } = &mut entry.outcome
                && let Some(v) = verdicts.get(&(entry.worker, entry.seq))
            {
                *resolved = Some(*v);
            }
        }
        Ok(())
    }

    // -- one attempt ------------------------------------------------------------------------

    async fn attempt(&self, op: &PlannedOp, fx: &mut Effects, rec: &mut AttemptRecord) -> End {
        let options = match self.options(label(&op.kind), op) {
            Ok(o) => o,
            Err(e) => return End::Error(e),
        };
        let mut tx = match self
            .backend
            .begin(self.model.catalog.clone(), options)
            .await
        {
            Ok(tx) => tx,
            Err(e) => return End::Error(e),
        };
        match self.exec(&mut tx, op, fx, rec).await {
            Ok(Step::Commit) => match tx.commit().await {
                Ok(()) => End::Committed,
                Err(e) if e.kind == ErrorKind::CommitOutcomeUnknown => End::Unknown,
                Err(e) if is_clean_rejection(&e, &op.kind) => End::Rejected(e.kind),
                Err(e) => End::Error(e),
            },
            Ok(Step::Rollback) => {
                let _ = tx.rollback().await;
                End::RolledBack
            }
            Ok(Step::Reject(why)) => {
                let _ = tx.rollback().await;
                End::Rejected(match why {
                    "not_found" => ErrorKind::NotFound,
                    _ => ErrorKind::Unsupported,
                })
            }
            Err(e) => {
                let _ = tx.rollback().await;
                if is_clean_rejection(&e, &op.kind) {
                    End::Rejected(e.kind)
                } else {
                    End::Error(e)
                }
            }
        }
    }

    /// Enqueues the first event of a write transaction: its marker. Returns the marker id.
    async fn marker(
        &self,
        tx: &mut B::Tx,
        fx: &mut Effects,
        entity: &Name,
        id: RecordId,
        what: &str,
    ) -> StorageResult<RecordId> {
        self.emit(tx, fx, &self.model.topic_commit, entity, id, what)
            .await
    }

    async fn emit(
        &self,
        tx: &mut B::Tx,
        fx: &mut Effects,
        topic: &Name,
        entity: &Name,
        id: RecordId,
        what: &str,
    ) -> StorageResult<RecordId> {
        let event_id = RecordId::new_v7();
        // Recorded before the call so that an attempt that fails midway still lists its ids.
        fx.events.push(event_id);
        tx.enqueue(&OutboxEvent {
            id: event_id,
            topic: topic.clone(),
            subject: NodeRef {
                entity: entity.clone(),
                id,
            },
            subject_version: None,
            schema_version: 1,
            payload: format!("{{\"op\":\"{what}\",\"group\":{}}}", fx.group),
        })
        .await?;
        Ok(event_id)
    }

    async fn exec(
        &self,
        tx: &mut B::Tx,
        op: &PlannedOp,
        fx: &mut Effects,
        rec: &mut AttemptRecord,
    ) -> StorageResult<Step> {
        let m = &self.model;
        match &op.kind {
            OpKind::CreateParty => {
                let id = RecordId::new_v7();
                let marker = self.marker(tx, fx, &m.party, id, "create_party").await?;
                let mut fields = Fields::new();
                fields.insert(m.name.clone(), Value::Text(format!("party-{marker}")));
                fields.insert(m.marker.clone(), Value::Text(marker.to_string()));
                let data = DocumentData {
                    entity: m.party.clone(),
                    id,
                    fields,
                    children: BTreeMap::new(),
                };
                let version = tx.insert(&data).await?;
                note(fx, rec, &m.party, id, ChangeOp::Insert, version.0);
                Ok(Step::Commit)
            }
            OpKind::CreateOrder { lines } => {
                let id = RecordId::new_v7();
                let marker = self.marker(tx, fx, &m.order, id, "create_order").await?;
                self.insert_order(tx, fx, rec, id, &marker.to_string(), *lines, None, None)
                    .await?;
                Ok(Step::Commit)
            }
            OpKind::CreateLinkedOrder { lines, watch } => {
                let (party, watched) = {
                    let pools = lock(&self.shared.pools);
                    (
                        Pools::pick(&pools.parties, op.pick),
                        Pools::pick(&pools.parties, op.pick >> 20),
                    )
                };
                let Some(party) = party else {
                    return Ok(Step::Reject("not_found"));
                };
                let id = RecordId::new_v7();
                let marker = self
                    .marker(tx, fx, &m.order, id, "create_linked_order")
                    .await?;
                let watch = if *watch { watched } else { None };
                self.insert_order(
                    tx,
                    fx,
                    rec,
                    id,
                    &marker.to_string(),
                    *lines,
                    Some(party),
                    watch,
                )
                .await?;
                Ok(Step::Commit)
            }
            OpKind::UpdateOrder => {
                let Some(id) = Pools::pick(&lock(&self.shared.pools).orders, op.pick) else {
                    return Ok(Step::Reject("not_found"));
                };
                self.marker(tx, fx, &m.order, id, "update_order").await?;
                let Some(doc) = tx.load(&m.order, id, RowLock::None).await? else {
                    return Ok(Step::Reject("not_found"));
                };
                let mut data = doc.data.clone();
                let total = match data.fields.get(&m.total) {
                    Some(Value::Int(t)) => *t,
                    _ => 0,
                };
                data.fields.insert(m.total.clone(), Value::Int(total + 1));
                data.fields.insert(
                    m.tag.clone(),
                    Value::Text(format!("v{}", doc.version.0 + 1)),
                );
                let rows = data.children.entry(m.lines.clone()).or_default();
                rows.push(self.line_row("upd", 1));
                let version = tx.update(&data, doc.version).await?;
                note(fx, rec, &m.order, id, ChangeOp::Update, version.0);
                rec.values.insert("total".into(), total + 1);
                Ok(Step::Commit)
            }
            OpKind::DeleteOrder => {
                let Some(id) = Pools::pick(&lock(&self.shared.pools).orders, op.pick) else {
                    return Ok(Step::Reject("not_found"));
                };
                self.delete_known(tx, fx, rec, &m.order, id, "delete_order")
                    .await
            }
            OpKind::DeleteParty => {
                let Some(id) = Pools::pick(&lock(&self.shared.pools).parties, op.pick) else {
                    return Ok(Step::Reject("not_found"));
                };
                self.delete_known(tx, fx, rec, &m.party, id, "delete_party")
                    .await
            }
            OpKind::IncrementCounter { mode } => {
                let Some(id) = Pools::pick(&lock(&self.shared.pools).counters, op.pick) else {
                    return Ok(Step::Reject("not_found"));
                };
                self.marker(tx, fx, &m.counter, id, "increment").await?;
                let lock_mode = match mode {
                    IncrementMode::VersionCheck => RowLock::None,
                    IncrementMode::RowLock => RowLock::Update(Wait::Block),
                };
                let Some(doc) = tx.load(&m.counter, id, lock_mode).await? else {
                    return Ok(Step::Reject("not_found"));
                };
                let value = match doc.data.fields.get(&m.value) {
                    Some(Value::Int(v)) => *v,
                    _ => 0,
                };
                let mut data = doc.data.clone();
                data.fields.insert(m.value.clone(), Value::Int(value + 1));
                let version = tx.update(&data, doc.version).await?;
                note(fx, rec, &m.counter, id, ChangeOp::Update, version.0);
                fx.increments.push(id);
                rec.values.insert("value".into(), value + 1);
                Ok(Step::Commit)
            }
            OpKind::AllocateSequence { series, rollback } => {
                let key_text = format!("l7-series-{series}");
                let key = SeriesKey::new(&key_text)?;
                let ticket = RecordId::new_v7();
                let marker = self.marker(tx, fx, &m.ticket, ticket, "allocate").await?;
                let number = tx.allocate(&key, 1).await?;
                let mut fields = Fields::new();
                fields.insert(m.series.clone(), Value::Text(key_text.clone()));
                fields.insert(
                    m.number.clone(),
                    Value::Int(i64::try_from(number).unwrap_or(i64::MAX)),
                );
                fields.insert(m.marker.clone(), Value::Text(marker.to_string()));
                let data = DocumentData {
                    entity: m.ticket.clone(),
                    id: ticket,
                    fields,
                    children: BTreeMap::new(),
                };
                let version = tx.insert(&data).await?;
                note(fx, rec, &m.ticket, ticket, ChangeOp::Insert, version.0);
                // The allocation sits inside a larger transaction: an order is created with it.
                let order = RecordId::new_v7();
                self.insert_order(
                    tx,
                    fx,
                    rec,
                    order,
                    &format!("{marker}-order"),
                    1,
                    None,
                    None,
                )
                .await?;
                rec.values
                    .insert("number".into(), i64::try_from(number).unwrap_or(i64::MAX));
                if *rollback {
                    return Ok(Step::Rollback);
                }
                fx.numbers.push((key_text, number));
                Ok(Step::Commit)
            }
            OpKind::Emit { events } => {
                let (entity, id) = {
                    let pools = lock(&self.shared.pools);
                    match Pools::pick(&pools.orders, op.pick) {
                        Some(id) => (m.order.clone(), id),
                        None => (m.party.clone(), RecordId::new_v7()),
                    }
                };
                self.marker(tx, fx, &entity, id, "emit").await?;
                for _ in 0..*events {
                    self.emit(tx, fx, &m.topic_note, &entity, id, "note")
                        .await?;
                }
                rec.values.insert("events".into(), i64::from(*events));
                Ok(Step::Commit)
            }
            OpKind::Read => {
                let Some(id) = Pools::pick(&lock(&self.shared.pools).orders, op.pick) else {
                    return Ok(Step::Reject("not_found"));
                };
                let Some(doc) = tx.load(&m.order, id, RowLock::None).await? else {
                    return Ok(Step::Reject("not_found"));
                };
                let total = match doc.data.fields.get(&m.total) {
                    Some(Value::Int(t)) => *t,
                    _ => -1,
                };
                let lines = doc
                    .data
                    .children
                    .get(&m.lines)
                    .map_or(&[][..], Vec::as_slice);
                let sum: i64 = lines
                    .iter()
                    .map(|r| match r.fields.get(&m.qty) {
                        Some(Value::Int(q)) => *q,
                        _ => 0,
                    })
                    .sum();
                rec.touched.push(Touched {
                    entity: m.order.to_string(),
                    id: id.to_string(),
                    role: Role::Read,
                    version: Some(doc.version.0),
                });
                rec.values.insert("total".into(), total);
                rec.values.insert("lines_sum".into(), sum);
                if total != sum {
                    fx.read_anomalies.push((
                        id.to_string(),
                        format!("torn_read total={total} lines_sum={sum}"),
                    ));
                }
                Ok(Step::Commit)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_order(
        &self,
        tx: &mut B::Tx,
        fx: &mut Effects,
        rec: &mut AttemptRecord,
        id: RecordId,
        marker: &str,
        lines: u32,
        party: Option<RecordId>,
        watch: Option<RecordId>,
    ) -> StorageResult<()> {
        let m = &self.model;
        let rows: Vec<ChildRow> = (0..lines)
            .map(|i| self.line_row("item", i64::from(i) + 1))
            .collect();
        let total: i64 = (0..lines).map(|i| i64::from(i) + 1).sum();
        let mut fields = Fields::new();
        fields.insert(m.marker.clone(), Value::Text(marker.to_owned()));
        fields.insert(m.party_link.clone(), party.map_or(Value::Null, Value::Id));
        fields.insert(m.watch_link.clone(), watch.map_or(Value::Null, Value::Id));
        fields.insert(m.total.clone(), Value::Int(total));
        fields.insert(m.tag.clone(), Value::Null);
        let mut children = BTreeMap::new();
        children.insert(m.lines.clone(), rows);
        let data = DocumentData {
            entity: m.order.clone(),
            id,
            fields,
            children,
        };
        let version = tx.insert(&data).await?;
        note(fx, rec, &m.order, id, ChangeOp::Insert, version.0);
        Ok(())
    }

    fn line_row(&self, item: &str, qty: i64) -> ChildRow {
        let mut fields = Fields::new();
        fields.insert(
            self.model.item.clone(),
            Value::Text(format!("{item}-{qty}")),
        );
        fields.insert(self.model.qty.clone(), Value::Int(qty));
        ChildRow {
            id: RecordId::new_v7(),
            fields,
        }
    }

    async fn delete_known(
        &self,
        tx: &mut B::Tx,
        fx: &mut Effects,
        rec: &mut AttemptRecord,
        entity: &Name,
        id: RecordId,
        what: &str,
    ) -> StorageResult<Step> {
        self.marker(tx, fx, entity, id, what).await?;
        let Some(doc) = tx.load(entity, id, RowLock::None).await? else {
            return Ok(Step::Reject("not_found"));
        };
        tx.delete(entity, id, doc.version).await?;
        note(fx, rec, entity, id, ChangeOp::Delete, doc.version.0);
        Ok(Step::Commit)
    }
}

fn write(entity: &Name, id: RecordId, op: ChangeOp, version: u64) -> DocWrite {
    DocWrite {
        entity: entity.clone(),
        id,
        op,
        version,
    }
}

fn note(
    fx: &mut Effects,
    rec: &mut AttemptRecord,
    entity: &Name,
    id: RecordId,
    op: ChangeOp,
    version: u64,
) {
    fx.docs.push(write(entity, id, op, version));
    rec.touched.push(Touched {
        entity: entity.to_string(),
        id: id.to_string(),
        role: match op {
            ChangeOp::Insert => Role::Created,
            ChangeOp::Update => Role::Updated,
            ChangeOp::Delete => Role::Deleted,
        },
        version: Some(version),
    });
}

fn label(kind: &OpKind) -> &'static str {
    match kind {
        OpKind::CreateParty => "l7_create_party",
        OpKind::CreateOrder { .. } => "l7_create_order",
        OpKind::CreateLinkedOrder { .. } => "l7_create_linked_order",
        OpKind::UpdateOrder => "l7_update_order",
        OpKind::DeleteOrder => "l7_delete_order",
        OpKind::DeleteParty => "l7_delete_party",
        OpKind::IncrementCounter { .. } => "l7_increment_counter",
        OpKind::AllocateSequence { .. } => "l7_allocate_sequence",
        OpKind::Emit { .. } => "l7_emit",
        OpKind::Read => "l7_read",
    }
}

/// Errors after which the whole operation is run again.
fn is_retryable(e: &StorageError, kind: &OpKind) -> bool {
    e.kind.is_retryable()
        || e.kind == ErrorKind::LockTimeout
        // A version-checked increment that lost the race simply reads again.
        || (e.kind == ErrorKind::StaleVersion && matches!(kind, OpKind::IncrementCounter { .. }))
}

/// Errors that mean "the store refused, nothing happened", which is a normal outcome of racing
/// workers and not a failure.
fn is_clean_rejection(e: &StorageError, kind: &OpKind) -> bool {
    if is_retryable(e, kind) {
        return false;
    }
    matches!(
        e.kind,
        ErrorKind::NotFound
            | ErrorKind::StaleVersion
            | ErrorKind::ReferenceViolation
            | ErrorKind::AlreadyExists
            | ErrorKind::LockNotAvailable
    )
}
