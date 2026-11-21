//! The in-memory reference [`ExecStore`]: the store the contract suite and the Unit demo run
//! against.
//!
//! All state sits behind one [`Mutex`]; every method is `&self` and completes without awaiting,
//! so it is safe to share across tasks. It is a reference implementation, not a database.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::ExecError;
use crate::rules::{already_applied, apply_step};
use crate::state::{Run, RunId, RunPlan, RunStatus, Transition};
use crate::store::{ExecFuture, ExecStore, ReadyRun};

/// Runs and their business keys, behind one lock.
#[derive(Debug, Default)]
struct State {
    runs: BTreeMap<(String, RunId), Run>,
    keys: BTreeMap<(String, String), RunId>,
}

/// An in-memory [`ExecStore`], the reference implementation of [`crate::contract::run`].
///
/// Clones share the same runs, so a factory can hand out "reopened" stores over one backend.
/// The store is bound to one tenant: every read, write and delete is scoped to it, and a run
/// id belonging to another tenant is not found.
#[derive(Debug, Clone, Default)]
pub struct InMemoryExecStore {
    tenant: String,
    state: Arc<Mutex<State>>,
}

impl InMemoryExecStore {
    /// An empty store bound to the default tenant.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store bound to `tenant`.
    #[must_use]
    pub fn with_tenant(tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// A store bound to `tenant` over the **same** runs as this one: another tenant's view of one
    /// backend. It is how a test proves that a run id of one tenant is not found by another.
    #[must_use]
    pub fn view_for(&self, tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            state: Arc::clone(&self.state),
        }
    }

    /// The tenant this store is bound to.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ExecStore for InMemoryExecStore {
    fn create_run<'a>(&'a self, plan: RunPlan) -> ExecFuture<'a, Run> {
        Box::pin(async move {
            if plan.run.key.is_empty() || plan.run.key.len() > 256 {
                return Err(ExecError::KeyInvalid);
            }
            let mut state = self.lock();
            if state
                .runs
                .contains_key(&(self.tenant.clone(), plan.run.id.clone()))
            {
                return Err(ExecError::RunExists {
                    id: plan.run.id.to_string(),
                });
            }
            if state
                .keys
                .contains_key(&(self.tenant.clone(), plan.run.key.clone()))
            {
                return Err(ExecError::KeyConflict {
                    key: plan.run.key.clone(),
                });
            }
            state.keys.insert(
                (self.tenant.clone(), plan.run.key.clone()),
                plan.run.id.clone(),
            );
            state
                .runs
                .insert((self.tenant.clone(), plan.run.id.clone()), plan.run.clone());
            Ok(plan.run)
        })
    }

    fn load_run<'a>(&'a self, id: &'a RunId) -> ExecFuture<'a, Option<Run>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .runs
                .get(&(self.tenant.clone(), id.clone()))
                .cloned())
        })
    }

    fn apply<'a>(
        &'a self,
        transition: &'a Transition,
        expected_version: u64,
    ) -> ExecFuture<'a, Run> {
        Box::pin(async move {
            let mut state = self.lock();
            let run = state
                .runs
                .get(&(self.tenant.clone(), transition.run.clone()))
                .cloned()
                .ok_or_else(|| ExecError::RunNotFound {
                    id: transition.run.to_string(),
                })?;
            // The idempotency key wins over the version check: a retried completion against the
            // version the caller first read must be a no-op, not a conflict.
            if already_applied(&run, &transition.step) {
                return Ok(run);
            }
            if run.version != expected_version {
                return Err(ExecError::VersionConflict {
                    current: run.version,
                    expected: expected_version,
                });
            }
            let mut updated = run;
            apply_step(&mut updated, &transition.step)?;
            updated.version += 1;
            state
                .runs
                .insert((self.tenant.clone(), updated.id.clone()), updated.clone());
            Ok(updated)
        })
    }

    fn list_ready<'a>(&'a self, limit: u32) -> ExecFuture<'a, Vec<ReadyRun>> {
        Box::pin(async move {
            let state = self.lock();
            let take = usize::try_from(limit).unwrap_or(usize::MAX);
            let ready = state
                .runs
                .iter()
                .filter(|((tenant, _), _)| tenant == &self.tenant)
                .map(|(_, run)| run)
                .filter(|run| run.status == RunStatus::Running)
                .filter_map(|run| {
                    let nodes = run.ready_nodes();
                    (!nodes.is_empty()).then(|| ReadyRun {
                        run: run.id.clone(),
                        ready: nodes,
                    })
                })
                .take(take)
                .collect();
            Ok(ready)
        })
    }

    fn find_by_key<'a>(&'a self, key: &'a str) -> ExecFuture<'a, Option<Run>> {
        Box::pin(async move {
            let state = self.lock();
            Ok(state
                .keys
                .get(&(self.tenant.clone(), key.to_owned()))
                .and_then(|id| state.runs.get(&(self.tenant.clone(), id.clone())))
                .cloned())
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::plan::{claim, start};
    use crate::testutil;

    fn plan(id: &str, key: &str) -> (spot_dag::CompiledDag, RunPlan) {
        let dag = testutil::dag(
            vec![testutil::node("a"), testutil::node("b")],
            vec![testutil::edge("a", "b")],
        );
        let plan = start(
            &dag,
            &testutil::published(&dag),
            RunId::new(id).unwrap(),
            key,
            testutil::now(),
        )
        .unwrap();
        (dag, plan)
    }

    #[test]
    fn another_tenant_cannot_read_advance_or_find_a_run_by_its_id() {
        let a = InMemoryExecStore::with_tenant("a");
        let b = a.view_for("b");
        let (_, plan) = plan("run1", "shared-key");
        let run = testutil::block_on(a.create_run(plan)).unwrap();

        assert!(testutil::block_on(b.load_run(&run.id)).unwrap().is_none());
        assert!(
            testutil::block_on(b.find_by_key("shared-key"))
                .unwrap()
                .is_none()
        );
        assert!(testutil::block_on(b.list_ready(10)).unwrap().is_empty());

        let node = spot_dag::NodeId::new("a").unwrap();
        let begin = claim(&run, &node, testutil::now()).unwrap();
        let denied = testutil::block_on(b.apply(&begin, run.version));
        assert!(matches!(denied, Err(ExecError::RunNotFound { .. })));

        // Tenant A's run is untouched by the refused apply.
        let still = testutil::block_on(a.load_run(&run.id)).unwrap().unwrap();
        assert_eq!(still.version, run.version);
        assert_eq!(testutil::block_on(a.list_ready(10)).unwrap().len(), 1);
    }

    #[test]
    fn the_same_run_id_and_key_can_live_under_two_tenants() {
        let a = InMemoryExecStore::with_tenant("a");
        let b = a.view_for("b");
        let (_, plan_a) = plan("run1", "k");
        let (_, plan_b) = plan("run1", "k");
        let run_a = testutil::block_on(a.create_run(plan_a)).unwrap();
        let run_b = testutil::block_on(b.create_run(plan_b)).unwrap();
        assert_eq!(run_a.id, run_b.id);
        // Each tenant still sees only its own, and a duplicate within a tenant is refused.
        let (_, again) = plan("run1", "other");
        assert!(matches!(
            testutil::block_on(b.create_run(again)),
            Err(ExecError::RunExists { .. })
        ));
    }
}
