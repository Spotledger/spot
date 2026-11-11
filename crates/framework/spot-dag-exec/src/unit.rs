//! The [`Unit`] port: applying a node transition in the caller's storage transaction.
//!
//! The brief's acceptance is "state and document commit or roll back together". [`ExecStore`](crate::ExecStore)
//! owns its own transaction; a caller whose document change and transition must be atomic instead
//! commits both through one storage [`Transaction`] with [`Unit::commit_with`].
//!
//! # Why the transaction is a type parameter and the work returns a future
//!
//! The storage contract's [`Transaction`] is not object-safe (its methods return `impl Future`),
//! so `&mut dyn Transaction` cannot be written; `commit_with` takes the concrete transaction type
//! as a parameter `Tx: Transaction` and hands `work` a `&mut Tx`. Because the document engine's
//! writes are async, `work` returns a boxed future that borrows that transaction for its life
//! ([`WorkFuture`]); the store awaits it, applies the transition and commits. The brief's
//! `&mut dyn Tx` is realised as `&mut Tx` for both reasons.

use std::future::Future;
use std::pin::Pin;

use spot_app_api::AppResult;
use spot_storage_contract::Transaction;

use crate::state::{Run, Transition};
use crate::store::ExecFuture;

/// The boxed future a caller's `work` returns; it borrows the transaction for its lifetime.
pub type WorkFuture<'t> = Pin<Box<dyn Future<Output = AppResult<()>> + Send + 't>>;

/// Applies a [`Transition`] in the same storage transaction as the caller's work.
///
/// `commit_with` opens one storage transaction, runs `work` (the caller's document change) with
/// its handle, applies the transition, and commits the two together. If `work` returns an error
/// the transaction rolls back and nothing is stored; a later retry finds the node exactly where
/// it was. The in-memory reference (`unit.rs` tests) applies the transition after the commit has
/// succeeded, which gives the same externally visible "all or nothing" for the failure paths it
/// is asked to prove; the PostgreSQL store of WP-DAG-02.T2 applies inside the same `PgTx` for
/// true at-commit atomicity.
///
/// The caller supplies its `work` closure; the store supplies the transaction and the apply.
pub trait Unit<Tx: Transaction> {
    /// Applies `transition` at `expected_version` inside `work`'s transaction.
    fn commit_with<'a>(
        &'a self,
        transition: &'a Transition,
        expected_version: u64,
        work: &'a mut (dyn for<'t> FnMut(&'t mut Tx) -> WorkFuture<'t> + Send),
    ) -> ExecFuture<'a, Run>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use spot_app_api::error::{StorageOp, map_storage_error};
    use spot_app_api::prelude::RecordId;
    use spot_storage_contract::{
        Catalog, DocumentStore, Isolation, Name, StorageBackend, TxOptions,
    };
    use spot_storage_fake::{FakeBackend, FakeTx};

    use super::*;
    use crate::error::{ExecError, ExecResult};
    use crate::memory::InMemoryExecStore;
    use crate::plan::{cancel, claim, complete, start};
    use crate::state::{NodeOutcome, NodeStatus, RunId, RunStatus, Step};
    use crate::store::ExecStore;
    use crate::testutil;

    /// A [`Unit`] over the in-memory exec store and the in-memory storage fake.
    struct FakeUnit {
        store: Arc<InMemoryExecStore>,
        backend: FakeBackend,
        catalog: Arc<Catalog>,
        label: Name,
    }

    impl Unit<FakeTx> for FakeUnit {
        fn commit_with<'a>(
            &'a self,
            transition: &'a Transition,
            expected_version: u64,
            work: &'a mut (dyn for<'t> FnMut(&'t mut FakeTx) -> WorkFuture<'t> + Send),
        ) -> ExecFuture<'a, Run> {
            Box::pin(async move {
                // Check the version before any work so the apply after the commit cannot fail;
                // a stale caller gets no document write at all.
                let current = self.store.load_run(&transition.run).await?.ok_or_else(|| {
                    ExecError::RunNotFound {
                        id: transition.run.to_string(),
                    }
                })?;
                if current.version != expected_version {
                    return Err(ExecError::VersionConflict {
                        current: current.version,
                        expected: expected_version,
                    });
                }
                let options = TxOptions::new(self.label.clone(), Isolation::ReadCommitted);
                let mut tx = self.backend.begin(self.catalog.clone(), options).await?;
                // On error the `?` drops `tx`, whose `Drop` rolls the transaction back.
                work(&mut tx).await?;
                tx.commit().await?;
                let run = self.store.apply(transition, expected_version).await?;
                Ok(run)
            })
        }
    }

    fn unit(
        store: Arc<InMemoryExecStore>,
        backend: FakeBackend,
        catalog: Arc<Catalog>,
    ) -> FakeUnit {
        FakeUnit {
            store,
            backend,
            catalog,
            label: Name::new("dag_exec_unit").unwrap(),
        }
    }

    fn fail_write(tx: &mut FakeTx) -> WorkFuture<'_> {
        Box::pin(async move {
            let doc = testutil::unknown_doc(RecordId::new_v7());
            tx.insert(&doc)
                .await
                .map_err(|error| map_storage_error(&error, StorageOp::Write))?;
            Ok(())
        })
    }

    fn succeed_write(tx: &mut FakeTx) -> WorkFuture<'_> {
        Box::pin(async move {
            let doc = testutil::probe_doc(RecordId::new_v7());
            tx.insert(&doc)
                .await
                .map_err(|error| map_storage_error(&error, StorageOp::Write))?;
            Ok(())
        })
    }

    fn linear_plan(id: &str, key: &str) -> (spot_dag::CompiledDag, crate::state::RunPlan) {
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
    fn failing_document_write_leaves_the_node_running() {
        let store = Arc::new(InMemoryExecStore::new());
        let (backend, catalog) = testutil::store();
        let (dag, plan) = linear_plan("run_unit", "biz-1");
        let run = testutil::block_on(store.create_run(plan)).unwrap();

        let a = spot_dag::NodeId::new("a").unwrap();
        let b = spot_dag::NodeId::new("b").unwrap();

        // Claim `a`: it is now Running at attempt 1.
        let begin = claim(&run, &a, testutil::now()).unwrap();
        let run = testutil::block_on(store.apply(&begin, run.version)).unwrap();
        assert_eq!(run.node(&a).unwrap().status, NodeStatus::Running);

        let unit = unit(store.clone(), backend, catalog);

        // The completion is computed from the run before the work, then committed with a write
        // that fails.
        let done = complete(
            &dag,
            &run,
            &a,
            NodeOutcome::Done {
                output: None,
                branches: Vec::new(),
            },
            testutil::now(),
        )
        .unwrap();
        let mut failing = fail_write;
        let result = testutil::block_on(unit.commit_with(&done, run.version, &mut failing));
        assert!(result.is_err(), "the failing write must surface");

        let reloaded = testutil::block_on(store.load_run(&run.id))
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.node(&a).unwrap().status,
            NodeStatus::Running,
            "state and document rolled back together"
        );
        assert_eq!(reloaded.version, run.version, "nothing advanced");
        assert_eq!(reloaded.node(&b).unwrap().status, NodeStatus::Pending);

        // The same transition, committed with a write that succeeds, marks the node Done and
        // releases `b` in the same commit.
        let mut succeeding = succeed_write;
        let after =
            testutil::block_on(unit.commit_with(&done, reloaded.version, &mut succeeding)).unwrap();
        assert_eq!(after.node(&a).unwrap().status, NodeStatus::Done);
        assert_eq!(after.node(&b).unwrap().status, NodeStatus::Ready);
        assert!(after.node(&a).unwrap().ended_at.is_some());
        assert_eq!(after.version, reloaded.version + 1);
    }

    #[test]
    fn a_stale_version_writes_nothing() {
        let store = Arc::new(InMemoryExecStore::new());
        let (backend, catalog) = testutil::store();
        let (dag, plan) = linear_plan("run_stale", "biz-2");
        let run = testutil::block_on(store.create_run(plan)).unwrap();
        let a = spot_dag::NodeId::new("a").unwrap();
        let done = complete(
            &dag,
            &run,
            &a,
            NodeOutcome::Done {
                output: None,
                branches: Vec::new(),
            },
            testutil::now(),
        )
        .unwrap();
        let unit = unit(store.clone(), backend, catalog);
        let mut work = succeed_write;
        let result = testutil::block_on(unit.commit_with(&done, run.version + 5, &mut work));
        assert!(matches!(result, Err(ExecError::VersionConflict { .. })));
        let reloaded = testutil::block_on(store.load_run(&run.id))
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.version, run.version);
    }

    #[test]
    fn cancel_transitions_are_idempotent() {
        let store = Arc::new(InMemoryExecStore::new());
        let dag = testutil::dag(vec![testutil::node("a")], Vec::new());
        let plan = start(
            &dag,
            &testutil::published(&dag),
            RunId::new("run_cancel").unwrap(),
            "biz-3",
            testutil::now(),
        )
        .unwrap();
        let run = testutil::block_on(store.create_run(plan)).unwrap();
        let cancelled = cancel(&run, testutil::now()).unwrap();
        let once = testutil::block_on(store.apply(&cancelled, run.version)).unwrap();
        assert_eq!(once.status, RunStatus::Cancelled);
        let twice = testutil::block_on(store.apply(&cancelled, run.version)).unwrap();
        assert_eq!(twice, once, "a repeated cancel is a no-op");
        assert!(matches!(cancelled.step, Step::Cancel { .. }));
    }

    /// `ExecResult` is part of the public surface the trait's futures deliver.
    #[allow(dead_code)]
    fn _result_is_public(_: ExecResult<Run>) {}
}
