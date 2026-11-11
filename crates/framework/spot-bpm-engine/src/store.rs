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

use spot_dag_exec::{ExecError, ExecStore, InMemoryExecStore, RunPlan, RunStatus};

use crate::case::{Attempt, CaseRecord, CaseUpdate, ChildLink, TimerRecord};
use crate::error::{EngineError, EngineResult};
use crate::handler::CaseId;

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
#[derive(Debug, Clone, Default)]
pub struct InMemoryCaseStore {
    exec: InMemoryExecStore,
    aux: Arc<Mutex<BTreeMap<CaseId, Aux>>>,
}

impl InMemoryCaseStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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

    /// Merges an update into the stored Case-level state.
    fn merge(id: &CaseId, aux: &mut Aux, update: &CaseUpdate) {
        let _ = id;
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
            let run = match &update.transition {
                Some(transition) => self
                    .exec
                    .apply(transition, expected_version)
                    .await
                    .map_err(map_exec)?,
                None => {
                    let current = self
                        .exec
                        .load_run(id)
                        .await
                        .map_err(map_exec)?
                        .ok_or_else(|| EngineError::CaseNotFound { id: id.to_string() })?;
                    if current.version != expected_version {
                        return Err(EngineError::VersionConflict {
                            current: current.version,
                            expected: expected_version,
                        });
                    }
                    current
                }
            };
            {
                let mut guard = self.lock();
                let aux = guard.entry(id.clone()).or_default();
                Self::merge(id, aux, &update);
            }
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
