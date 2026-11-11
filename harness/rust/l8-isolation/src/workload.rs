//! Generating and running isolation workloads.
//!
//! Two workloads, chosen because their values make the order of writes recoverable (the
//! property Elle relies on):
//!
//! - **list-append**: every write appends a **unique** value to a key's JSON list; a read returns
//!   the whole list, so the list reveals the exact order the appends were applied.
//! - **rw-register**: every write sets a key's integer register to a **unique** value; a read
//!   returns the value.
//!
//! A transaction is a small bundle of micro-operations over one to six keys of a small key
//! space; some operations read, some write, and some read a key and then write it back (the
//! read-modify-write shape a lost update needs). Keys are chosen with a configurable hot-key
//! skew so conflicts actually happen, and a configurable share of transactions is read-only so
//! read skew is reachable. Everything a transaction does is decided by the seed *before* it
//! runs; what the backend returns is not, so a failing seed replays the plan but not necessarily
//! the same interleaving (the same honest limit as the L7 driver).
//!
//! Transactions run at the level under test, retry on the errors the contract says to retry
//! (serialization failure, deadlock, lock timeout, a stale version on an optimistic write), and
//! record `committed`, `rolled back` or `indeterminate` exactly as the L7 driver does. A commit
//! whose outcome is unknown is resolved afterwards by reading the written values back.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use spot_l7_invariants::driver::RetryPolicy;
use spot_l7_invariants::history::{Level, Outcome};
use spot_l7_invariants::schedule::{Profile, Rng};
use spot_storage_contract::{
    DocumentStore, ErrorKind, Fields, Isolation, Name, RecordId, RowLock, StorageBackend,
    StorageError, StorageResult, Transaction, TxOptions, Value,
};

use crate::history::{Observed, TxnEntry, TxnHistory, TxnOp, Workload};
use crate::model::Model;

/// How big a run is, and how its workload is shaped.
#[derive(Clone, Debug)]
pub struct WorkloadConfig {
    /// The workload.
    pub workload: Workload,
    /// The seed: the whole plan is a pure function of it.
    pub seed: u64,
    /// Concurrent workers.
    pub workers: u32,
    /// Transactions over all workers.
    pub transactions: u64,
    /// The key space.
    pub keys: u32,
    /// How many keys are "hot".
    pub hot_keys: u32,
    /// The percentage of operations that target a hot key.
    pub hot_percent: u32,
    /// Micro-operations per transaction, minimum.
    pub txn_len_min: u32,
    /// Micro-operations per transaction, maximum.
    pub txn_len_max: u32,
    /// The percentage of transactions that only read.
    pub read_only_percent: u32,
    /// The percentage of a read-write transaction's operations that write.
    pub write_percent: u32,
    /// Of the writes, the percentage that first read the key (read-modify-write).
    pub double_percent: u32,
    /// The profile's name.
    pub profile: String,
    /// The retry policy (reused from the L7 driver).
    pub retry: RetryPolicy,
    /// Lock timeout declared by every transaction.
    pub lock_timeout: Duration,
    /// Statement timeout declared by every transaction.
    pub statement_timeout: Duration,
}

impl WorkloadConfig {
    /// The configuration of a named profile.
    pub fn for_profile(profile: Profile, workload: Workload, seed: u64) -> WorkloadConfig {
        let (workers, transactions, keys) = match profile {
            Profile::Smoke => (8, 300, 12),
            Profile::Standard => (32, 5_000, 128),
            Profile::Nightly => (64, 100_000, 1_024),
        };
        WorkloadConfig {
            workload,
            seed,
            workers,
            transactions,
            keys,
            hot_keys: (keys / 4).max(1),
            hot_percent: 60,
            txn_len_min: 2,
            txn_len_max: 6,
            read_only_percent: 20,
            write_percent: 55,
            double_percent: 70,
            profile: profile.name().to_owned(),
            retry: RetryPolicy::default(),
            lock_timeout: Duration::from_secs(60),
            statement_timeout: Duration::from_secs(120),
        }
    }

    /// The profile named by `SPOT_L8_PROFILE` (or the L7 variable), defaulting to `smoke`.
    pub fn profile_from_env() -> Profile {
        std::env::var("SPOT_L8_PROFILE")
            .or_else(|_| std::env::var("SPOT_L7_PROFILE"))
            .ok()
            .and_then(|s| Profile::parse(&s))
            .unwrap_or(Profile::Smoke)
    }
}

/// A planned transaction, as the test-facing tuple form returned by [`plan`].
///
/// The tuple is `(worker, sequence, operations, read_only)`.
pub type PlannedTxn = (u32, u64, Vec<(u32, Option<String>)>, bool);

/// The whole plan of a run: one entry per transaction.
pub fn plan(config: &WorkloadConfig) -> Vec<PlannedTxn> {
    let mut out = Vec::with_capacity(config.transactions as usize);
    let mut next_value: i64 = 1;
    for i in 0..config.transactions {
        let worker = (i % u64::from(config.workers.max(1))) as u32;
        let seq = i / u64::from(config.workers.max(1));
        let mut rng = Rng::new(config.seed ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let read_only = rng.chance(u64::from(config.read_only_percent), 100);
        let len = config.txn_len_min
            + (rng.below(u64::from(
                config.txn_len_max.saturating_sub(config.txn_len_min) + 1,
            )) as u32);
        let mut events: Vec<(u32, bool, String)> = Vec::new();
        let mut reads: Vec<u32> = Vec::new();
        for _ in 0..len {
            let key = pick_key(&mut rng, config);
            let write = !read_only && rng.chance(u64::from(config.write_percent), 100);
            if !write {
                reads.push(key);
                continue;
            }
            let double = rng.chance(u64::from(config.double_percent), 100);
            let value = match config.workload {
                Workload::ListAppend => format!("v{next_value}"),
                Workload::RwRegister => next_value.to_string(),
            };
            next_value += 1;
            events.push((key, double, value));
        }
        // Writes are emitted in ascending key order so two transactions never take row locks in
        // opposite orders and deadlock. Reads take no lock; a read-modify-write pair keeps its
        // read immediately before its write.
        events.sort_by_key(|(key, _, _)| *key);
        let mut ops = Vec::with_capacity(events.len() * 2 + reads.len());
        for (key, double, value) in events {
            if double {
                ops.push((key, None));
            }
            ops.push((key, Some(value)));
        }
        ops.extend(reads.into_iter().map(|key| (key, None)));
        out.push((worker, seq, ops, read_only));
    }
    out
}

fn pick_key(rng: &mut Rng, config: &WorkloadConfig) -> u32 {
    if config.hot_keys > 0 && rng.chance(u64::from(config.hot_percent), 100) {
        rng.below(u64::from(config.hot_keys)) as u32
    } else {
        rng.below(u64::from(config.keys.max(1))) as u32
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

enum Attempt {
    Committed(Vec<TxnOp>),
    Indeterminate(Vec<TxnOp>),
    Retry(StorageError),
    Failed(StorageError),
}

struct Shared<B: StorageBackend> {
    backend: Arc<B>,
    model: Arc<Model>,
    workload: Workload,
    key_ids: Vec<(String, RecordId)>,
    level: Level,
    retry: RetryPolicy,
    lock_timeout: Duration,
    statement_timeout: Duration,
    t0: Instant,
    entries: Mutex<Vec<TxnEntry>>,
    unresolved: Mutex<Vec<(u32, u64, Vec<TxnOp>)>>,
}

impl<B: StorageBackend> Shared<B> {
    fn now_us(&self) -> u64 {
        u64::try_from(self.t0.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    fn options(&self, read_only: bool) -> StorageResult<TxOptions> {
        let mut o = TxOptions::new(Name::new("l8_workload")?, self.level.to_contract());
        o.lock_timeout = self.lock_timeout;
        o.statement_timeout = self.statement_timeout;
        if read_only {
            o = o.read_only();
        }
        Ok(o)
    }

    fn observe(&self, fields: &Fields) -> Observed {
        match self.workload {
            Workload::RwRegister => {
                let v = match fields.get(&self.model.reg) {
                    Some(Value::Int(i)) => Some(i.to_string()),
                    Some(Value::Text(t)) => Some(t.clone()),
                    _ => None,
                };
                Observed::Register(v)
            }
            Workload::ListAppend => {
                let list = match fields.get(&self.model.body) {
                    Some(Value::Json(j)) => {
                        serde_json::from_str::<Vec<String>>(j).unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                Observed::List(list)
            }
        }
    }

    fn apply_write(
        &self,
        fields: &mut Fields,
        key: &str,
        value: &str,
    ) -> Result<TxnOp, StorageError> {
        match self.workload {
            Workload::RwRegister => {
                let n: i64 = value.parse().map_err(|_| {
                    StorageError::new(ErrorKind::InvalidValue, "a register value is an integer")
                })?;
                fields.insert(self.model.reg.clone(), Value::Int(n));
                Ok(TxnOp::Write {
                    key: key.to_owned(),
                    value: value.to_owned(),
                })
            }
            Workload::ListAppend => {
                let mut list: Vec<String> = fields
                    .get(&self.model.body)
                    .and_then(|v| match v {
                        Value::Json(j) => serde_json::from_str(j).ok(),
                        _ => None,
                    })
                    .unwrap_or_default();
                list.push(value.to_owned());
                let json = serde_json::to_string(&list).map_err(|e| {
                    StorageError::new(ErrorKind::InvalidValue, format!("body is not a list: {e}"))
                })?;
                fields.insert(self.model.body.clone(), Value::Json(json));
                Ok(TxnOp::Append {
                    key: key.to_owned(),
                    value: value.to_owned(),
                })
            }
        }
    }
}

/// A planned transaction in the driver's internal form:
/// `(transaction id, sequence, operations, read_only)`.
type WorkerPlan = Vec<(u64, u64, Vec<(u32, Option<String>)>, bool)>;

/// Runs the workload on a backend at `level` and returns the history.
pub async fn run_on<B: StorageBackend>(
    backend: Arc<B>,
    model: Arc<Model>,
    config: &WorkloadConfig,
    level: Level,
) -> StorageResult<TxnHistory> {
    backend.prepare(&model.catalog).await?;
    let key_ids = setup(&backend, &model, config.keys).await?;
    let shared = Arc::new(Shared {
        backend,
        model,
        workload: config.workload,
        key_ids,
        level,
        retry: config.retry,
        lock_timeout: config.lock_timeout,
        statement_timeout: config.statement_timeout,
        t0: Instant::now(),
        entries: Mutex::new(Vec::new()),
        unresolved: Mutex::new(Vec::new()),
    });

    let planned = plan(config);
    let mut workers: BTreeMap<u32, WorkerPlan> = BTreeMap::new();
    for (i, (w, seq, ops, ro)) in planned.into_iter().enumerate() {
        workers
            .entry(w)
            .or_default()
            .push((i as u64 + 1, seq, ops, ro));
    }
    let mut tasks = Vec::new();
    for (worker, txns) in workers {
        let shared = shared.clone();
        tasks.push(tokio::spawn(async move {
            let mut rng = Rng::new(
                u64::from(worker).wrapping_mul(0xA076_1D64_78BD_642F) ^ 0x5851_F42D_4C95_7F2D,
            );
            for (id, seq, ops, read_only) in txns {
                let entry = run_txn(&shared, id, worker, seq, ops, read_only, &mut rng).await;
                lock(&shared.entries).push(entry);
            }
        }));
    }
    for task in tasks {
        if let Err(e) = task.await {
            return Err(StorageError::new(
                ErrorKind::Internal,
                format!("a workload worker panicked: {e}"),
            ));
        }
    }
    resolve_unresolved(&shared).await?;

    let mut entries = std::mem::take(&mut *lock(&shared.entries));
    entries.sort_by_key(|e| (e.start_us, e.worker, e.seq));
    let mut history = TxnHistory::new(
        config.seed,
        config.workers,
        &config.profile,
        config.workload,
        config.keys,
    );
    history.entries = entries;
    Ok(history)
}

async fn setup<B: StorageBackend>(
    backend: &Arc<B>,
    model: &Arc<Model>,
    keys: u32,
) -> StorageResult<Vec<(String, RecordId)>> {
    let mut options = TxOptions::new(Name::new("l8_setup")?, Isolation::ReadCommitted);
    options.lock_timeout = Duration::from_secs(60);
    let mut tx = backend.begin(model.catalog.clone(), options).await?;
    let mut ids = Vec::with_capacity(keys as usize);
    for i in 0..keys {
        let name = format!("k{i}");
        let id = RecordId::new_v7();
        let data = model.empty_item(id, &name);
        tx.insert(&data).await?;
        ids.push((name, id));
    }
    tx.commit().await?;
    Ok(ids)
}

async fn run_txn<B: StorageBackend>(
    shared: &Arc<Shared<B>>,
    id: u64,
    worker: u32,
    seq: u64,
    ops: Vec<(u32, Option<String>)>,
    read_only: bool,
    rng: &mut Rng,
) -> TxnEntry {
    let start_us = shared.now_us();
    let mut attempts = 0u32;
    let (outcome, recorded) = loop {
        attempts += 1;
        match attempt(shared, &ops, read_only).await {
            Attempt::Committed(recorded) => break (Outcome::Committed, recorded),
            Attempt::Indeterminate(recorded) => {
                lock(&shared.unresolved).push((worker, seq, recorded.clone()));
                break (Outcome::Indeterminate { resolved: None }, recorded);
            }
            Attempt::Retry(e) => {
                if attempts < shared.retry.max_attempts {
                    let pause = shared.retry.backoff(attempts, rng);
                    tokio::time::sleep(pause).await;
                    continue;
                }
                break (
                    Outcome::Failed {
                        code: e.kind.code().to_owned(),
                        exhausted: true,
                    },
                    Vec::new(),
                );
            }
            Attempt::Failed(e) => {
                break (
                    Outcome::Failed {
                        code: e.kind.code().to_owned(),
                        exhausted: false,
                    },
                    Vec::new(),
                );
            }
        }
    };
    TxnEntry {
        txn: id,
        worker,
        seq,
        start_us,
        end_us: shared.now_us(),
        isolation: shared.level,
        outcome,
        attempts,
        ops: recorded,
    }
}

async fn attempt<B: StorageBackend>(
    shared: &Arc<Shared<B>>,
    ops: &[(u32, Option<String>)],
    read_only: bool,
) -> Attempt {
    let m = &shared.model;
    let options = match shared.options(read_only) {
        Ok(o) => o,
        Err(e) => return Attempt::Failed(e),
    };
    let mut tx = match shared.backend.begin(m.catalog.clone(), options).await {
        Ok(tx) => tx,
        Err(e) => return classify(e),
    };
    let mut recorded: Vec<TxnOp> = Vec::with_capacity(ops.len());
    for (key_index, write) in ops {
        let (key, id) = &shared.key_ids[*key_index as usize];
        match write {
            None => match tx.load(&m.item, *id, RowLock::None).await {
                Ok(Some(doc)) => recorded.push(TxnOp::Read {
                    key: key.clone(),
                    observed: shared.observe(&doc.data.fields),
                }),
                Ok(None) => {
                    let _ = tx.rollback().await;
                    return Attempt::Failed(StorageError::new(
                        ErrorKind::NotFound,
                        format!("key {key} vanished"),
                    ));
                }
                Err(e) => {
                    let _ = tx.rollback().await;
                    return classify(e);
                }
            },
            Some(value) => {
                let loaded = match tx.load(&m.item, *id, RowLock::None).await {
                    Ok(Some(doc)) => doc,
                    Ok(None) => {
                        let _ = tx.rollback().await;
                        return Attempt::Failed(StorageError::new(
                            ErrorKind::NotFound,
                            format!("key {key} vanished"),
                        ));
                    }
                    Err(e) => {
                        let _ = tx.rollback().await;
                        return classify(e);
                    }
                };
                let mut data = loaded.data.clone();
                let written = shared.apply_write(&mut data.fields, key, value);
                match tx.update(&data, loaded.version).await {
                    Ok(_) => match written {
                        Ok(op) => recorded.push(op),
                        Err(e) => {
                            let _ = tx.rollback().await;
                            return Attempt::Failed(e);
                        }
                    },
                    Err(e) => {
                        let _ = tx.rollback().await;
                        return classify(e);
                    }
                }
            }
        }
    }
    match tx.commit().await {
        Ok(()) => Attempt::Committed(recorded),
        Err(e) if e.kind == ErrorKind::CommitOutcomeUnknown => Attempt::Indeterminate(recorded),
        Err(e) => classify(e),
    }
}

fn classify(e: StorageError) -> Attempt {
    if is_retryable(&e) {
        Attempt::Retry(e)
    } else {
        Attempt::Failed(e)
    }
}

fn is_retryable(e: &StorageError) -> bool {
    e.kind.is_retryable()
        || matches!(
            e.kind,
            ErrorKind::LockTimeout | ErrorKind::StaleVersion | ErrorKind::LockNotAvailable
        )
}

async fn resolve_unresolved<B: StorageBackend>(shared: &Arc<Shared<B>>) -> StorageResult<()> {
    let pending = std::mem::take(&mut *lock(&shared.unresolved));
    if pending.is_empty() {
        return Ok(());
    }
    let m = &shared.model;
    let mut verdicts: BTreeMap<(u32, u64), bool> = BTreeMap::new();
    for (worker, seq, ops) in pending {
        let mut committed = false;
        let mut saw_write = false;
        for op in &ops {
            let Some(value) = op.written() else { continue };
            saw_write = true;
            let Some((_, id)) = shared.key_ids.iter().find(|(k, _)| k == op.key()) else {
                continue;
            };
            let options = TxOptions::new(Name::new("l8_resolve")?, Isolation::ReadCommitted);
            let mut tx = shared.backend.begin(m.catalog.clone(), options).await?;
            if let Ok(Some(doc)) = tx.load(&m.item, *id, RowLock::None).await {
                let visible = match shared.observe(&doc.data.fields) {
                    Observed::Register(v) => v.as_deref() == Some(value),
                    Observed::List(v) => v.iter().any(|x| x == value),
                };
                committed |= visible;
            }
            let _ = tx.rollback().await;
        }
        verdicts.insert((worker, seq), saw_write && committed);
    }
    for entry in lock(&shared.entries).iter_mut() {
        if let Outcome::Indeterminate { resolved } = &mut entry.outcome
            && let Some(v) = verdicts.get(&(entry.worker, entry.seq))
        {
            *resolved = Some(*v);
        }
    }
    Ok(())
}
