//! The [`CaseStore`] port: where a Case lives, and the in-memory reference implementation.
//!
//! A store holds the DAG run (through the `spot-dag-exec` execution store, so the run's node
//! states and transitions keep that layer's idempotency and optimistic versioning) *and* the
//! Case-level bookkeeping of [`CaseRecord`]. [`CaseStore::commit`] applies a whole
//! [`CaseUpdate`] in one transaction, so the DAG transition and the attempt outcome commit or
//! roll back together — the blueprint's "state and document commit or roll back together", one
//! layer up.
//!
//! [`InMemoryCaseStore`] is the reference implementation the harness and the tests use. A
//! PostgreSQL store is WP-BPM-02.T2; it implements this trait and passes the same contract.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_dag_exec::{
    ExecError, ExecStore, InMemoryExecStore, NodeStatus, RunPlan, RunStatus, Step,
};

use crate::case::{Attempt, CaseRecord, CaseUpdate, ChildLink, TimerRecord};
use crate::error::{EngineError, EngineResult};
use crate::handler::CaseId;
use crate::node_run_log::{
    Checkpoint, MAX_PAGE, NodeRunDraft, NodeRunLog, NodeRunRecord, NodeRunSeq, OpenNode,
    StoredEffect, derive_node_runs, require_instant, validate_consumer,
};

/// A boxed, `Send` future: the return type of every [`CaseStore`] method, so the trait stays
/// object-safe and a backend can be used through `Box<dyn CaseStore>`.
pub type CaseFuture<'a, T> = Pin<Box<dyn Future<Output = EngineResult<T>> + Send + 'a>>;

/// The Case-level state a store keeps beside the DAG run.
#[derive(Debug, Clone, Default, PartialEq)]
struct Aux {
    variables: serde_json::Value,
    attempts: BTreeMap<spot_dag::NodeId, Attempt>,
    timers: BTreeMap<spot_dag::NodeId, TimerRecord>,
    children: BTreeMap<spot_dag::NodeId, ChildLink>,
    initiator: Option<String>,
}

/// Durable storage of Cases.
///
/// Every method is `&self` so the trait is object-safe; a store uses interior mutability (the
/// in-memory reference) or a connection pool (PostgreSQL). [`CaseStore::commit`] is idempotent
/// for a repeated `(run, node, attempt)` transition and refuses a stale `expected_version`.
pub trait CaseStore: Send + Sync {
    /// Stores a fresh Case. Its id and business key must both be unused.
    fn create<'a>(&'a self, record: CaseRecord) -> CaseFuture<'a, CaseRecord>;

    /// The Case with this id, when it exists.
    fn load<'a>(&'a self, id: &'a CaseId) -> CaseFuture<'a, Option<CaseRecord>>;

    /// Applies a whole update against `expected_version`, returning the stored Case.
    fn commit<'a>(
        &'a self,
        id: &'a CaseId,
        expected_version: u64,
        update: CaseUpdate,
    ) -> CaseFuture<'a, CaseRecord>;

    /// Cases that may have work at `now`: those with a ready node, a due timer or a child link,
    /// at most `limit` of them, sorted by id.
    fn list_runnable<'a>(
        &'a self,
        now: spot_app_api::prelude::OffsetDateTime,
        limit: u32,
    ) -> CaseFuture<'a, Vec<CaseId>>;

    /// The Case a business key names, when one exists.
    fn find_by_key<'a>(&'a self, key: &'a str) -> CaseFuture<'a, Option<CaseRecord>>;
}

/// A factory for [`CaseStore`]s that share one durable backend.
///
/// The harness drives a factory: each `open` returns a fresh store over the same data, so a test
/// can drop one store, open another and prove that a Case survived a process restart.
pub trait CaseStoreFactory: Send + Sync {
    /// A store over the factory's data.
    fn open(&self) -> Box<dyn CaseStore>;
}

/// The in-memory reference [`CaseStore`]: the `spot-dag-exec` in-memory store for the run plus
/// one map for the Case-level state, behind a lock.
///
/// Clones share the same data, so a factory can hand out "reopened" stores over one backend.
///
/// The store is bound to one tenant for the node-run log: [`InMemoryCaseStore::with_tenant`] makes
/// an independent store, [`InMemoryCaseStore::view_for`] another tenant's view of the same data.
#[derive(Debug, Clone, Default)]
pub struct InMemoryCaseStore {
    tenant: String,
    exec: InMemoryExecStore,
    aux: Arc<Mutex<BTreeMap<CaseId, Aux>>>,
    log: Arc<Mutex<LogState>>,
}

/// The dedupe key of one logged transition: tenant, case, node, attempt and state token.
type SeenKey = (String, String, String, u32, &'static str);

/// The node-run log of every tenant over one in-memory backend.
#[derive(Debug, Default)]
struct LogState {
    next: BTreeMap<String, i64>,
    records: Vec<(String, NodeRunRecord)>,
    seen: std::collections::BTreeSet<SeenKey>,
    checkpoints: BTreeMap<(String, String), Checkpoint>,
}

impl InMemoryCaseStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store bound to `tenant`, sharing nothing with any other store.
    #[must_use]
    pub fn with_tenant(tenant: impl Into<String>) -> Self {
        let tenant = tenant.into();
        Self {
            exec: InMemoryExecStore::with_tenant(tenant.clone()),
            tenant,
            ..Self::default()
        }
    }

    /// A store bound to `tenant` over the same data as this one. Its node-run log holds only
    /// `tenant`'s records.
    #[must_use]
    pub fn view_for(&self, tenant: impl Into<String>) -> Self {
        let tenant = tenant.into();
        Self {
            exec: self.exec.view_for(tenant.clone()),
            tenant,
            aux: Arc::clone(&self.aux),
            log: Arc::clone(&self.log),
        }
    }

    fn log_lock(&self) -> MutexGuard<'_, LogState> {
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Appends the drafts of one commit, skipping any `(case, node, attempt, state)` already
    /// logged (a replay that reached the store a second time).
    fn append_log(&self, run: &spot_dag_exec::Run, drafts: Vec<NodeRunDraft>) {
        let mut log = self.log_lock();
        for draft in drafts {
            let key = (
                self.tenant.clone(),
                run.id.to_string(),
                draft.node.to_string(),
                draft.attempt,
                draft.state.token(),
            );
            if !log.seen.insert(key) {
                continue;
            }
            let next = log.next.entry(self.tenant.clone()).or_insert(0);
            *next += 1;
            let seq = NodeRunSeq(*next);
            let record = NodeRunRecord::new(
                seq,
                run.id.clone(),
                run.dag_key.clone(),
                run.dag_version,
                run.version,
                draft,
            );
            log.records.push((self.tenant.clone(), record));
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<CaseId, Aux>> {
        self.aux
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The stored Case assembled from the run and its Case-level state.
    fn assemble(&self, run: spot_dag_exec::Run) -> CaseRecord {
        let aux = self.lock().get(&run.id).cloned().unwrap_or_default();
        CaseRecord {
            run,
            variables: aux.variables,
            attempts: aux.attempts,
            timers: aux.timers,
            children: aux.children,
            initiator: aux.initiator,
        }
    }

    /// Merges an update into the stored Case-level state. Returns `true` when the exactly-once
    /// guard kept an earlier (higher-numbered) attempt over the update's attempt.
    fn merge(aux: &mut Aux, update: &CaseUpdate) -> bool {
        let mut kept_earlier = false;
        if let Some(variables) = &update.variables {
            aux.variables = variables.clone();
        }
        if let Some(attempt) = &update.attempt {
            let replace = aux
                .attempts
                .get(&attempt.node)
                .is_none_or(|existing| attempt.attempt >= existing.attempt);
            if replace {
                aux.attempts.insert(attempt.node.clone(), attempt.clone());
            }
            kept_earlier = aux
                .attempts
                .get(&attempt.node)
                .is_some_and(|existing| existing.attempt > attempt.attempt);
        }
        if let Some(timer) = &update.timer {
            let replace = aux
                .timers
                .get(&timer.node)
                .is_none_or(|existing| timer.attempt >= existing.attempt);
            if replace {
                aux.timers.insert(timer.node.clone(), timer.clone());
            }
        }
        if let Some(node) = &update.clear_timer {
            aux.timers.remove(node);
        }
        if let Some(child) = &update.child {
            aux.children.insert(child.node.clone(), child.clone());
        }
        kept_earlier
    }
}

/// Maps a `spot-dag-exec` error onto this crate's error vocabulary.
fn map_exec(error: ExecError) -> EngineError {
    match error {
        ExecError::VersionConflict { current, expected } => {
            EngineError::VersionConflict { current, expected }
        }
        ExecError::RunNotFound { id } => EngineError::CaseNotFound { id },
        other => EngineError::StoreFailed {
            detail: other.to_string(),
        },
    }
}

impl CaseStore for InMemoryCaseStore {
    fn create<'a>(&'a self, record: CaseRecord) -> CaseFuture<'a, CaseRecord> {
        Box::pin(async move {
            let ready = record.run.ready_nodes();
            let plan = RunPlan {
                run: record.run.clone(),
                ready,
            };
            let run = self.exec.create_run(plan).await.map_err(map_exec)?;
            let aux = Aux {
                variables: record.variables,
                attempts: record.attempts,
                timers: record.timers,
                children: record.children,
                initiator: record.initiator,
            };
            self.lock().insert(run.id.clone(), aux);
            Ok(self.assemble(run))
        })
    }

    fn load<'a>(&'a self, id: &'a CaseId) -> CaseFuture<'a, Option<CaseRecord>> {
        Box::pin(async move {
            let run = self.exec.load_run(id).await.map_err(map_exec)?;
            Ok(run.map(|run| self.assemble(run)))
        })
    }

    fn commit<'a>(
        &'a self,
        id: &'a CaseId,
        expected_version: u64,
        update: CaseUpdate,
    ) -> CaseFuture<'a, CaseRecord> {
        Box::pin(async move {
            // Fail closed before anything changes: an attempt or timer needs its instant.
            require_instant(&update)?;
            let before = self.exec.load_run(id).await.map_err(map_exec)?;
            let open: Vec<(spot_dag::NodeId, u32)> = before
                .as_ref()
                .map(|run| {
                    run.nodes
                        .iter()
                        .filter(|(_, state)| state.status == NodeStatus::Running)
                        .map(|(node, state)| (node.clone(), state.attempts))
                        .collect()
                })
                .unwrap_or_default();
            let before_version = before.as_ref().map(|run| run.version);
            let run = match &update.transition {
                Some(transition) => self
                    .exec
                    .apply(transition, expected_version)
                    .await
                    .map_err(map_exec)?,
                None => {
                    let current =
                        before.ok_or_else(|| EngineError::CaseNotFound { id: id.to_string() })?;
                    if current.version != expected_version {
                        return Err(EngineError::VersionConflict {
                            current: current.version,
                            expected: expected_version,
                        });
                    }
                    current
                }
            };
            let transition_applied = update.transition.is_some()
                && before_version.is_some_and(|version| run.version > version);
            let (kept_earlier_attempt, attempts) = {
                let mut guard = self.lock();
                let aux = guard.entry(id.clone()).or_default();
                let kept = Self::merge(aux, &update);
                let attempts = aux
                    .attempts
                    .iter()
                    .map(|(node, attempt)| (node.clone(), attempt.attempt))
                    .collect::<BTreeMap<_, _>>();
                (kept, attempts)
            };
            let open = if matches!(
                update.transition.as_ref().map(|t| &t.step),
                Some(Step::Cancel { .. })
            ) {
                open.into_iter()
                    .map(|(node, dag_attempt)| OpenNode {
                        attempt: attempts.get(&node).copied().unwrap_or(dag_attempt),
                        node,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let effect = StoredEffect {
                kept_earlier_attempt,
                transition_applied,
                open,
                attempts,
            };
            let drafts = derive_node_runs(&update, &effect)?;
            self.append_log(&run, drafts);
            Ok(self.assemble(run))
        })
    }

    fn list_runnable<'a>(
        &'a self,
        now: spot_app_api::prelude::OffsetDateTime,
        limit: u32,
    ) -> CaseFuture<'a, Vec<CaseId>> {
        Box::pin(async move {
            let at = spot_dag_exec::Instant::from_offset(now);
            let take = usize::try_from(limit).unwrap_or(usize::MAX);
            // Read the Case-level flags under the lock, then drop it before any await: a
            // `MutexGuard` is not `Send`, and this future must stay `Send`.
            let flags: Vec<(CaseId, bool, bool)> = {
                let guard = self.lock();
                guard
                    .iter()
                    .map(|(id, aux)| {
                        let has_timer = aux.timers.values().any(|timer| timer.is_due(at));
                        (id.clone(), has_timer, !aux.children.is_empty())
                    })
                    .collect()
            };
            let mut ids = Vec::new();
            for (id, has_timer, has_child) in flags {
                if ids.len() >= take {
                    break;
                }
                let has_ready = self
                    .exec
                    .load_run(&id)
                    .await
                    .map_err(map_exec)?
                    .is_some_and(|run| {
                        run.status == RunStatus::Running && !run.ready_nodes().is_empty()
                    });
                if has_ready || has_timer || has_child {
                    ids.push(id);
                }
            }
            Ok(ids)
        })
    }

    fn find_by_key<'a>(&'a self, key: &'a str) -> CaseFuture<'a, Option<CaseRecord>> {
        Box::pin(async move {
            let run = self.exec.find_by_key(key).await.map_err(map_exec)?;
            Ok(run.map(|run| self.assemble(run)))
        })
    }
}

impl NodeRunLog for InMemoryCaseStore {
    fn list_after<'a>(
        &'a self,
        after: NodeRunSeq,
        limit: u32,
    ) -> CaseFuture<'a, Vec<NodeRunRecord>> {
        Box::pin(async move {
            let take = usize::try_from(limit.min(MAX_PAGE)).unwrap_or(0);
            let log = self.log_lock();
            Ok(log
                .records
                .iter()
                .filter(|(tenant, record)| tenant == &self.tenant && record.seq > after)
                .map(|(_, record)| record.clone())
                .take(take)
                .collect())
        })
    }

    fn load_checkpoint<'a>(&'a self, consumer: &'a str) -> CaseFuture<'a, Option<Checkpoint>> {
        Box::pin(async move {
            validate_consumer(consumer)?;
            let log = self.log_lock();
            Ok(log
                .checkpoints
                .get(&(self.tenant.clone(), consumer.to_owned()))
                .cloned())
        })
    }

    fn save_checkpoint<'a>(
        &'a self,
        consumer: &'a str,
        expected: NodeRunSeq,
        next: Checkpoint,
    ) -> CaseFuture<'a, bool> {
        Box::pin(async move {
            validate_consumer(consumer)?;
            if next.seq < expected {
                return Err(EngineError::StoreFailed {
                    detail: "a checkpoint cannot move backwards".to_owned(),
                });
            }
            let mut log = self.log_lock();
            let key = (self.tenant.clone(), consumer.to_owned());
            let stored = log
                .checkpoints
                .get(&key)
                .map_or(NodeRunSeq(0), |checkpoint| checkpoint.seq);
            if stored != expected {
                return Ok(false);
            }
            log.checkpoints.insert(key, next);
            Ok(true)
        })
    }

    fn prune_before<'a>(&'a self, before: NodeRunSeq) -> CaseFuture<'a, u64> {
        Box::pin(async move {
            let mut log = self.log_lock();
            let count = log.records.len();
            let tenant = self.tenant.clone();
            log.records
                .retain(|(owner, record)| owner != &tenant || record.seq >= before);
            Ok(u64::try_from(count - log.records.len()).unwrap_or(u64::MAX))
        })
    }
}

/// A factory over one shared in-memory backend.
#[derive(Debug, Clone)]
pub struct InMemoryCaseStoreFactory {
    store: InMemoryCaseStore,
}

impl InMemoryCaseStoreFactory {
    /// A factory over an empty backend.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: InMemoryCaseStore::new(),
        }
    }

    /// A factory over an existing store.
    #[must_use]
    pub fn over(store: InMemoryCaseStore) -> Self {
        Self { store }
    }
}

impl Default for InMemoryCaseStoreFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl CaseStoreFactory for InMemoryCaseStoreFactory {
    fn open(&self) -> Box<dyn CaseStore> {
        Box::new(self.store.clone())
    }
}

/// A [`NodeRunLogFactory`](crate::node_run_log::NodeRunLogFactory) over one in-memory backend:
/// each tenant gets its own view of the same data.
#[derive(Debug, Clone, Default)]
pub struct InMemoryNodeRunLogFactory {
    root: InMemoryCaseStore,
}

impl InMemoryNodeRunLogFactory {
    /// A factory over an empty backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl crate::node_run_log::NodeRunLogFactory for InMemoryNodeRunLogFactory {
    fn open(&self, tenant: &str) -> Box<dyn crate::node_run_log::LoggedCaseStore> {
        Box::new(self.root.view_for(tenant))
    }
}
