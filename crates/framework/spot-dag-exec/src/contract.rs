//! The backend-independent contract suite for [`ExecStore`].
//!
//! Every store (the in-memory one here, the PostgreSQL one of WP-DAG-02.T2) runs [`run`] against
//! a factory that opens stores over the same durable data. The suite covers the version conflict,
//! idempotent completion, join resolution with dead branches, a crash between two nodes and
//! resume, a fork/join with skips, and 1,000 model-checked randomised runs. A failure is a plain
//! message naming the broken rule.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_dag::{CompiledDag, DagDefinition, Edge, JoinRule, Limits, Node, NodeId, Published};

use crate::error::ExecError;
use crate::plan::{cancel, claim, complete, start};
use crate::state::{NodeOutcome, NodeStatus, Run, RunId, RunStatus};
use crate::store::ExecStoreFactory;

/// A fresh suffix for every run and key, so the suite can share one database with itself.
static SEQ: AtomicU64 = AtomicU64::new(1);

fn unique(prefix: &str) -> String {
    format!("{prefix}{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn failed<E: std::fmt::Display>(error: E) -> String {
    format!("store error: {error}")
}

fn check(ok: bool, rule: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(rule.to_owned()) }
}

fn at() -> Result<OffsetDateTime, String> {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).map_err(|error| error.to_string())
}

fn nid(raw: &str) -> Result<NodeId, String> {
    NodeId::new(raw).map_err(|error| error.to_string())
}

fn node(id: &str, join: JoinRule) -> Result<Node, String> {
    Node::new(nid(id)?, "step", join, Json::Null).map_err(|error| error.to_string())
}

fn edge(from: &str, to: &str, condition: Option<&str>) -> Result<Edge, String> {
    Ok(Edge::new(
        nid(from)?,
        nid(to)?,
        condition.map(str::to_owned),
    ))
}

fn compile(nodes: Vec<Node>, edges: Vec<Edge>) -> Result<CompiledDag, String> {
    let def = DagDefinition::new("workflow", 1, nodes, edges);
    CompiledDag::compile(&def, &Limits::new()).map_err(|errors| format!("invalid DAG: {errors:?}"))
}

/// `a -> b`, the smallest run.
fn linear() -> Result<CompiledDag, String> {
    compile(
        vec![node("a", JoinRule::All)?, node("b", JoinRule::All)?],
        vec![edge("a", "b", None)?],
    )
}

/// A fork on a condition, joined by `join` at `d`, then `e`.
fn fork(join: JoinRule) -> Result<CompiledDag, String> {
    compile(
        vec![
            node("a", JoinRule::All)?,
            node("b", JoinRule::All)?,
            node("c", JoinRule::All)?,
            node("d", join)?,
            node("e", JoinRule::All)?,
        ],
        vec![
            edge("a", "b", Some("yes"))?,
            edge("a", "c", Some("no"))?,
            edge("b", "d", None)?,
            edge("c", "d", None)?,
            edge("d", "e", None)?,
        ],
    )
}

fn pin(dag: &CompiledDag) -> Published {
    Published {
        version: dag.version(),
        hash: dag.hash().clone(),
    }
}

async fn store_run(store: &dyn crate::store::ExecStore, dag: &CompiledDag) -> Result<Run, String> {
    let key = unique("key");
    let plan = start(
        dag,
        &pin(dag),
        RunId::new(&unique("run")).map_err(failed)?,
        key,
        at()?,
    )
    .map_err(failed)?;
    store.create_run(plan).await.map_err(failed)
}

async fn load(store: &dyn crate::store::ExecStore, id: &RunId) -> Result<Run, String> {
    store
        .load_run(id)
        .await
        .map_err(failed)?
        .ok_or_else(|| format!("run {id} vanished"))
}

/// Applies one step, returning the stored run.
async fn apply(
    store: &dyn crate::store::ExecStore,
    run: &Run,
    step: crate::state::Transition,
) -> Result<Run, String> {
    store.apply(&step, run.version).await.map_err(failed)
}

/// Runs the whole suite.
///
/// # Errors
/// The first rule that does not hold, as a plain message.
pub async fn run(factory: &dyn ExecStoreFactory) -> Result<(), String> {
    version_conflict(factory).await?;
    idempotent_completion(factory).await?;
    listing(factory).await?;
    join_resolution(factory).await?;
    fork_join_with_skips(factory).await?;
    crash_and_resume(factory).await?;
    randomised_runs(factory).await?;
    Ok(())
}

async fn version_conflict(factory: &dyn ExecStoreFactory) -> Result<(), String> {
    let store = factory.open();
    let dag = linear()?;
    let run = store_run(&*store, &dag).await?;

    let stale = cancel(&run, at()?).map_err(failed)?;
    check(
        matches!(
            store.apply(&stale, run.version + 7).await,
            Err(ExecError::VersionConflict { .. })
        ),
        "a stale version fails with VersionConflict",
    )?;
    check(
        load(&*store, &run.id).await?.version == run.version,
        "a conflicted apply writes nothing",
    )?;
    Ok(())
}

async fn idempotent_completion(factory: &dyn ExecStoreFactory) -> Result<(), String> {
    let store = factory.open();
    let dag = linear()?;
    let run = store_run(&*store, &dag).await?;
    let a = nid("a")?;
    let b = nid("b")?;

    let begin = claim(&run, &a, at()?).map_err(failed)?;
    let running = apply(&*store, &run, begin).await?;
    check(
        running.node(&a).map(|state| state.status) == Some(NodeStatus::Running),
        "a claim runs the node",
    )?;

    let done = complete(
        &dag,
        &running,
        &a,
        NodeOutcome::Done {
            output: None,
            branches: Vec::new(),
        },
        at()?,
    )
    .map_err(failed)?;
    let finished = apply(&*store, &running, done.clone()).await?;
    check(
        finished.node(&a).map(|state| state.status) == Some(NodeStatus::Done),
        "a completion marks the node done",
    )?;
    check(
        finished.node(&b).map(|state| state.status) == Some(NodeStatus::Ready),
        "a completion releases the successor",
    )?;

    // The same completion, retried against the version it was computed from.
    let retried = store.apply(&done, running.version).await.map_err(failed)?;
    check(retried == finished, "a retried completion is a no-op")?;
    check(
        retried.version == finished.version,
        "a retried completion does not advance the version",
    )?;
    check(
        retried.node(&a).map(|state| state.attempts) == Some(1),
        "a retried completion does not double the attempt",
    )?;
    check(
        retried
            .nodes
            .values()
            .filter(|state| state.status == NodeStatus::Ready)
            .count()
            == 1,
        "a retried completion does not duplicate ready nodes",
    )?;
    Ok(())
}

async fn listing(factory: &dyn ExecStoreFactory) -> Result<(), String> {
    let store = factory.open();
    let dag = linear()?;
    let key = unique("key");
    let plan = start(
        &dag,
        &pin(&dag),
        RunId::new(&unique("run")).map_err(failed)?,
        key.clone(),
        at()?,
    )
    .map_err(failed)?;
    let run = store.create_run(plan).await.map_err(failed)?;

    let found = store
        .find_by_key(&key)
        .await
        .map_err(failed)?
        .ok_or("find_by_key did not find the run")?;
    check(found.id == run.id, "find_by_key finds the run")?;
    check(
        store
            .find_by_key("absent-key")
            .await
            .map_err(failed)?
            .is_none(),
        "find_by_key is None for an absent key",
    )?;

    let ready = store.list_ready(10).await.map_err(failed)?;
    let mine = ready.iter().find(|item| item.run == run.id);
    check(
        mine.is_some_and(|item| {
            item.ready.iter().map(NodeId::as_str).collect::<Vec<_>>() == vec!["a"]
        }),
        "list_ready offers the entry node",
    )?;
    Ok(())
}

async fn join_resolution(factory: &dyn ExecStoreFactory) -> Result<(), String> {
    let store = factory.open();
    let dag = fork(JoinRule::All)?;
    let run = store_run(&*store, &dag).await?;
    let a = nid("a")?;

    let begin = claim(&run, &a, at()?).map_err(failed)?;
    let running = apply(&*store, &run, begin).await?;
    let done = complete(
        &dag,
        &running,
        &a,
        NodeOutcome::Done {
            output: None,
            branches: vec!["yes".to_owned()],
        },
        at()?,
    )
    .map_err(failed)?;
    let after = apply(&*store, &running, done).await?;

    check(
        after.node(&nid("c")?).map(|state| state.status) == Some(NodeStatus::Skipped),
        "the untaken branch is skipped",
    )?;
    check(
        after.node(&nid("d")?).map(|state| state.status) == Some(NodeStatus::Skipped),
        "an unreachable All-join resolves instead of hanging",
    )?;
    check(
        after.node(&nid("e")?).map(|state| state.status) == Some(NodeStatus::Skipped),
        "the dead branch cascades",
    )?;
    check(
        after.node(&nid("b")?).map(|state| state.status) == Some(NodeStatus::Ready),
        "the taken branch is ready",
    )?;
    check(
        after.status == RunStatus::Running,
        "the run is still running",
    )?;
    Ok(())
}

async fn fork_join_with_skips(factory: &dyn ExecStoreFactory) -> Result<(), String> {
    let store = factory.open();
    let dag = fork(JoinRule::Any)?;
    let mut run = store_run(&*store, &dag).await?;

    // Take `yes`: `c` is skipped, `b` runs, the Any-join `d` survives on `b`.
    let begin = claim(&run, &nid("a")?, at()?).map_err(failed)?;
    run = apply(&*store, &run, begin).await?;
    let done = complete(
        &dag,
        &run,
        &nid("a")?,
        NodeOutcome::Done {
            output: None,
            branches: vec!["yes".to_owned()],
        },
        at()?,
    )
    .map_err(failed)?;
    run = apply(&*store, &run, done).await?;
    check(
        run.node(&nid("c")?).map(|state| state.status) == Some(NodeStatus::Skipped),
        "Any-join skips only the dead branch",
    )?;
    check(
        run.node(&nid("d")?).map(|state| state.status) == Some(NodeStatus::Pending),
        "the Any-join is not dead while one edge can still be taken",
    )?;

    // Finish the surviving branch to the end.
    while let Some(node) = run.ready_nodes().first().cloned() {
        let begin = claim(&run, &node, at()?).map_err(failed)?;
        run = apply(&*store, &run, begin).await?;
        let done = complete(
            &dag,
            &run,
            &node,
            NodeOutcome::Done {
                output: None,
                branches: Vec::new(),
            },
            at()?,
        )
        .map_err(failed)?;
        run = apply(&*store, &run, done).await?;
    }
    check(run.status == RunStatus::Completed, "the run completes")?;
    check(run.is_finished(), "nothing is left ready or running")?;
    for id in ["b", "d", "e"] {
        check(
            run.node(&nid(id)?).map(|state| state.status) == Some(NodeStatus::Done),
            "the surviving path completed",
        )?;
    }
    check(
        run.node(&nid("c")?).map(|state| state.status) == Some(NodeStatus::Skipped),
        "the dead branch stayed skipped",
    )?;
    Ok(())
}

async fn crash_and_resume(factory: &dyn ExecStoreFactory) -> Result<(), String> {
    let dag = linear()?;
    let run = {
        let store = factory.open();
        let run = store_run(&*store, &dag).await?;
        let begin = claim(&run, &nid("a")?, at()?).map_err(failed)?;
        let running = apply(&*store, &run, begin).await?;
        check(
            running.node(&nid("a")?).map(|state| state.status) == Some(NodeStatus::Running),
            "the node is running before the crash",
        )?;
        running
        // `store` is dropped here: a process restart.
    };

    let store = factory.open();
    let resumed = load(&*store, &run.id).await?;
    check(
        resumed.node(&nid("a")?).map(|state| state.status) == Some(NodeStatus::Running),
        "the running node survived the restart",
    )?;
    let done = complete(
        &dag,
        &resumed,
        &nid("a")?,
        NodeOutcome::Done {
            output: None,
            branches: Vec::new(),
        },
        at()?,
    )
    .map_err(failed)?;
    let after = apply(&*store, &resumed, done).await?;
    check(
        after.node(&nid("b")?).map(|state| state.status) == Some(NodeStatus::Ready),
        "resuming advances the run exactly once",
    )?;
    check(after.status == RunStatus::Running, "the run continues")?;
    Ok(())
}

/// A tiny deterministic generator: no dependency, no entropy, no clock.
fn next(seed: &mut u64) -> u64 {
    let mut x = *seed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *seed = x;
    x
}

async fn randomised_runs(factory: &dyn ExecStoreFactory) -> Result<(), String> {
    let dag = fork(JoinRule::Any)?;
    let store = factory.open();
    let mut seed = 0x5eed_1234_abcd_ef01_u64;
    for _ in 0..1_000 {
        let run = store_run(&*store, &dag).await?;
        let branch = if next(&mut seed).is_multiple_of(2) {
            "yes"
        } else {
            "no"
        };
        let mut run = run;
        while let Some(node) = run.ready_nodes().first().cloned() {
            let begin = claim(&run, &node, at()?).map_err(failed)?;
            run = apply(&*store, &run, begin).await?;
            let branches = if node == nid("a")? {
                vec![branch.to_owned()]
            } else {
                Vec::new()
            };
            let done = complete(
                &dag,
                &run,
                &node,
                NodeOutcome::Done {
                    output: None,
                    branches,
                },
                at()?,
            )
            .map_err(failed)?;
            run = apply(&*store, &run, done).await?;
        }
        check(
            run.status == RunStatus::Completed,
            "every model run completes",
        )?;
        let done_b = run
            .node(&nid("b")?)
            .is_some_and(|s| s.status == NodeStatus::Done);
        let done_c = run
            .node(&nid("c")?)
            .is_some_and(|s| s.status == NodeStatus::Done);
        check(done_b ^ done_c, "exactly one fork branch ran")?;
        let skipped = run
            .nodes
            .values()
            .filter(|state| state.status == NodeStatus::Skipped)
            .count();
        check(skipped == 1, "exactly one fork branch was skipped")?;
        check(
            run.node(&nid("e")?).map(|state| state.status) == Some(NodeStatus::Done),
            "the join and its successor always run",
        )?;
        check(
            run.nodes
                .values()
                .filter(|state| state.status == NodeStatus::Done)
                .all(|state| state.attempts == 1),
            "no node ran twice",
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use proptest::prelude::*;

    use super::*;
    use crate::memory::InMemoryExecStore;
    use crate::store::{ExecStore, ExecStoreFactory};
    use crate::testutil;

    /// A factory whose `open` hands back a store sharing one in-memory backend, the shape a
    /// real (PostgreSQL) factory has.
    struct InMemoryFactory(Arc<InMemoryExecStore>);

    impl ExecStoreFactory for InMemoryFactory {
        fn open(&self) -> Box<dyn ExecStore> {
            Box::new(self.0.as_ref().clone())
        }
    }

    #[test]
    fn in_memory_store_passes_the_contract() {
        let factory = InMemoryFactory(Arc::new(InMemoryExecStore::new()));
        testutil::block_on(run(&factory)).unwrap();
    }

    #[test]
    fn the_suite_is_repeatable_on_one_backend() {
        let factory = InMemoryFactory(Arc::new(InMemoryExecStore::new()));
        testutil::block_on(run(&factory)).unwrap();
        testutil::block_on(run(&factory)).unwrap();
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Whatever the decision takes, the run finishes and exactly one branch ran.
        #[test]
        fn any_decision_completes(done in any::<bool>()) {
            let factory = InMemoryFactory(Arc::new(InMemoryExecStore::new()));
            testutil::block_on(async {
                let store = factory.open();
                let dag = fork(JoinRule::Any).unwrap();
                let run = store_run(&*store, &dag).await.unwrap();
                let mut run = run;
                while let Some(node) = run.ready_nodes().first().cloned() {
                    let begin = claim(&run, &node, at().unwrap()).unwrap();
                    run = apply(&*store, &run, begin).await.unwrap();
                    let branches = if node == nid("a").unwrap() {
                        let label = if done { "yes" } else { "no" };
                        vec![label.to_owned()]
                    } else {
                        Vec::new()
                    };
                    let step = complete(
                        &dag,
                        &run,
                        &node,
                        NodeOutcome::Done { output: None, branches },
                        at().unwrap(),
                    )
                    .unwrap();
                    run = apply(&*store, &run, step).await.unwrap();
                }
                prop_assert_eq!(run.status, RunStatus::Completed);
                let b = run.node(&nid("b").unwrap()).unwrap().status;
                let c = run.node(&nid("c").unwrap()).unwrap().status;
                prop_assert!(b != c, "one branch done, the other skipped");
                Ok(())
            })
            .unwrap();
        }
    }
}
