//! The pure state machine: [`start`], [`claim`], [`complete`] and [`cancel`].
//!
//! These are pure functions over `spot_dag`'s compiled graph and
//! [`Progress`](spot_dag::Progress): they read the run and the supplied outcome, compute the
//! new state, and hand it back as a [`Transition`]. They never touch storage, never read a
//! clock, and never mutate the run they are given; [`ExecStore::apply`](crate::ExecStore::apply)
//! stores a transition.
//!
//! # Join rules and dead branches
//!
//! Ready nodes come from [`CompiledDag::ready`], so the join rules are honoured exactly. A
//! node can also become unreachable: once a predecessor is done without taking an incoming
//! edge, or is itself skipped, that edge is *dead*. A dead edge under [`JoinRule::All`] (or an
//! [`JoinRule::Any`] node whose every incoming edge is dead) can never be satisfied, so
//! [`complete`] marks the node [`NodeStatus::Skipped`] and this cascades forward. An
//! unreachable join therefore resolves instead of hanging.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;
use spot_dag::{CompiledDag, CompiledEdge, JoinRule, NodeId, Progress, Published};

use crate::error::{ExecError, ExecResult};
use crate::instant::Instant;
use crate::state::{
    Completion, NodeOutcome, NodeState, NodeStatus, Run, RunId, RunPlan, RunStatus, Step,
    TakenEdge, Transition,
};

/// The longest a caller-supplied business key may be.
const MAX_KEY_LEN: usize = 256;

/// How a run starts: the pinned DAG, the business key and the instant are all arguments.
///
/// The run pins `dag`'s key, the published `version` and content hash, and starts in
/// [`RunStatus::Running`] with every entry node [`NodeStatus::Ready`] and every other node
/// [`NodeStatus::Pending`].
///
/// The brief spells the signature `start(def, key, now)`; a `Published` alone does not carry
/// the compiled graph (only a version and a hash), so the compiled DAG is passed alongside it
/// and the two are checked against each other.
///
/// # Errors
/// [`ExecError::KeyInvalid`] when the key is empty or too long, [`ExecError::DagMismatch`] when
/// the compiled DAG's hash is not the published one.
pub fn start(
    dag: &CompiledDag,
    published: &Published,
    id: RunId,
    key: impl Into<String>,
    now: spot_app_api::prelude::OffsetDateTime,
) -> ExecResult<RunPlan> {
    let key = key.into();
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(ExecError::KeyInvalid);
    }
    if dag.hash().as_hex() != published.hash.as_hex() {
        return Err(ExecError::DagMismatch {
            current: dag.hash().to_string(),
            expected: published.hash.to_string(),
        });
    }
    let mut nodes = BTreeMap::new();
    for node in dag.nodes() {
        let status = if dag.predecessors(&node.id).is_empty() {
            NodeStatus::Ready
        } else {
            NodeStatus::Pending
        };
        nodes.insert(
            node.id.clone(),
            NodeState {
                status,
                ..NodeState::default()
            },
        );
    }
    let run = Run {
        id,
        dag_key: dag.key().to_owned(),
        dag_version: published.version,
        content_hash: published.hash.to_string(),
        key,
        status: RunStatus::Running,
        started_at: Instant::from_offset(now),
        ended_at: None,
        version: 1,
        nodes,
        taken: BTreeSet::new(),
    };
    Ok(RunPlan {
        run,
        ready: dag.entries().to_vec(),
    })
}

/// Claims a ready node for one attempt: it becomes [`NodeStatus::Running`].
///
/// The attempt number is the node's previous attempts plus one; it is the last part of the
/// `(run, node, attempt)` idempotency key a retried completion is matched against.
///
/// # Errors
/// [`ExecError::InvalidNode`] when the run does not track the node, [`ExecError::InvalidState`]
/// when the run is not running or the node is not ready.
pub fn claim(
    run: &Run,
    node: &NodeId,
    now: spot_app_api::prelude::OffsetDateTime,
) -> ExecResult<Transition> {
    if run.status != RunStatus::Running {
        return Err(ExecError::InvalidState {
            detail: format!("the run is {:?}, not running", run.status),
        });
    }
    let state = run.node(node).ok_or_else(|| ExecError::InvalidNode {
        node: node.to_string(),
    })?;
    if state.status != NodeStatus::Ready {
        return Err(ExecError::InvalidState {
            detail: format!("node `{node}` is {:?}, not ready", state.status),
        });
    }
    Ok(Transition {
        run: run.id.clone(),
        step: Step::Claim {
            node: node.clone(),
            attempt: state.attempts.saturating_add(1),
            at: Instant::from_offset(now),
        },
    })
}

/// Completes a running (or ready) node and advances the run.
///
/// The transition lists the nodes newly [`NodeStatus::Ready`], the dead-branch nodes newly
/// [`NodeStatus::Skipped`], the edges taken, and the run's status after the step. `dag` must be
/// the compiled graph the run pins.
///
/// # Errors
/// [`ExecError::InvalidNode`] when the DAG or the run does not have the node,
/// [`ExecError::InvalidState`] when the run is not running or the node is already terminal,
/// [`ExecError::OutputInvalid`] when the success output carries a float.
pub fn complete(
    dag: &CompiledDag,
    run: &Run,
    node: &NodeId,
    outcome: NodeOutcome,
    now: spot_app_api::prelude::OffsetDateTime,
) -> ExecResult<Transition> {
    if run.status != RunStatus::Running {
        return Err(ExecError::InvalidState {
            detail: format!("the run is {:?}, not running", run.status),
        });
    }
    if dag.node(node).is_none() {
        return Err(ExecError::InvalidNode {
            node: node.to_string(),
        });
    }
    let state = run.node(node).ok_or_else(|| ExecError::InvalidNode {
        node: node.to_string(),
    })?;
    let attempt = match state.status {
        NodeStatus::Ready => state.attempts.saturating_add(1),
        NodeStatus::Running => state.attempts,
        other => {
            return Err(ExecError::InvalidState {
                detail: format!("node `{node}` is {other:?}, not ready or running"),
            });
        }
    };

    let completion = match &outcome {
        NodeOutcome::Done { output, .. } => {
            if let Some(value) = output
                && let Some(path) = float_path(value)
            {
                return Err(ExecError::OutputInvalid { path });
            }
            Completion::Done {
                output: output.clone(),
            }
        }
        NodeOutcome::Failed { reason, .. } => Completion::Failed {
            reason: reason.clone(),
        },
    };
    let branches = outcome.branches();
    let mut taken = run.taken.clone();
    let mut new_edges = BTreeSet::new();
    for edge in dag.successors(node) {
        let take = match &edge.condition {
            None => true,
            Some(label) => branches.iter().any(|chosen| chosen == label),
        };
        if take {
            let item = TakenEdge {
                from: node.clone(),
                to: edge.node.clone(),
            };
            taken.insert(item.clone());
            new_edges.insert(item);
        }
    }

    let mut status: BTreeMap<NodeId, NodeStatus> = run
        .nodes
        .iter()
        .map(|(id, state)| (id.clone(), state.status))
        .collect();
    status.insert(node.clone(), completion.status());
    let skipped = resolve_dead_branches(dag, &mut status, &taken);

    // `CompiledDag::ready` honours the join rules; every node it returns that is still
    // `Pending` is newly ready. Terminal and running nodes are hidden from it, so treating
    // done, skipped and failed alike as "completed" here only means "do not offer again".
    let mut progress = Progress::default();
    for (id, state) in &status {
        match state {
            NodeStatus::Running => {
                progress.started.insert(id.clone());
            }
            NodeStatus::Done | NodeStatus::Skipped | NodeStatus::Failed => {
                progress.completed.insert(id.clone());
            }
            NodeStatus::Pending | NodeStatus::Ready => {}
        }
    }
    for edge in &taken {
        progress.taken.insert((edge.from.clone(), edge.to.clone()));
    }
    let ready: Vec<NodeId> = dag
        .ready(&progress)
        .into_iter()
        .filter(|id| status.get(id) == Some(&NodeStatus::Pending))
        .collect();

    let any_failed = status.values().any(|state| *state == NodeStatus::Failed);
    let live = !ready.is_empty()
        || status
            .values()
            .any(|state| matches!(state, NodeStatus::Ready | NodeStatus::Running));
    let (run_status, ended_at) = if live {
        (RunStatus::Running, None)
    } else if any_failed {
        (RunStatus::Failed, Some(Instant::from_offset(now)))
    } else {
        (RunStatus::Completed, Some(Instant::from_offset(now)))
    };

    Ok(Transition {
        run: run.id.clone(),
        step: Step::Complete {
            node: node.clone(),
            attempt,
            completion,
            at: Instant::from_offset(now),
            taken: new_edges,
            ready,
            skipped,
            status: run_status,
            ended_at,
        },
    })
}

/// Cancels a running run.
///
/// # Errors
/// [`ExecError::InvalidState`] when the run is not running.
pub fn cancel(run: &Run, now: spot_app_api::prelude::OffsetDateTime) -> ExecResult<Transition> {
    if run.status != RunStatus::Running {
        return Err(ExecError::InvalidState {
            detail: format!("the run is {:?}, not running", run.status),
        });
    }
    Ok(Transition {
        run: run.id.clone(),
        step: Step::Cancel {
            at: Instant::from_offset(now),
        },
    })
}

/// Marks every `Pending` node that can never become ready as [`NodeStatus::Skipped`], cascading
/// until nothing more changes. Returns the nodes newly skipped, sorted by id.
fn resolve_dead_branches(
    dag: &CompiledDag,
    status: &mut BTreeMap<NodeId, NodeStatus>,
    taken: &BTreeSet<TakenEdge>,
) -> Vec<NodeId> {
    let mut skipped = Vec::new();
    loop {
        let mut changed = false;
        for node in dag.nodes() {
            if status.get(&node.id) != Some(&NodeStatus::Pending) {
                continue;
            }
            let incoming = dag.predecessors(&node.id);
            if incoming.is_empty() {
                continue;
            }
            let dead = |edge: &CompiledEdge| -> bool {
                let source = status
                    .get(&edge.node)
                    .copied()
                    .unwrap_or(NodeStatus::Pending);
                let is_taken = taken.contains(&TakenEdge {
                    from: edge.node.clone(),
                    to: node.id.clone(),
                });
                match source {
                    NodeStatus::Skipped => true,
                    NodeStatus::Done | NodeStatus::Failed => !is_taken,
                    _ => false,
                }
            };
            let unreachable = match node.join {
                JoinRule::All => incoming.iter().any(dead),
                JoinRule::Any => incoming.iter().all(dead),
            };
            if unreachable {
                status.insert(node.id.clone(), NodeStatus::Skipped);
                skipped.push(node.id.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    skipped.sort();
    skipped.dedup();
    skipped
}

/// The path of the first JSON number that is not an integer, or `None`.
fn float_path(value: &Json) -> Option<String> {
    match value {
        Json::Number(number) if number.as_i64().is_none() && number.as_u64().is_none() => {
            Some(String::new())
        }
        Json::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, item)| float_path(item).map(|p| format!("[{i}]{p}"))),
        Json::Object(map) => map
            .iter()
            .find_map(|(key, item)| float_path(item).map(|p| format!("{key}{p}"))),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testutil::{self, node};

    fn linear_run() -> (CompiledDag, Run) {
        let dag = testutil::dag(vec![node("a"), node("b")], vec![testutil::edge("a", "b")]);
        let plan = start(
            &dag,
            &testutil::published(&dag),
            RunId::new("run_test").unwrap(),
            "biz",
            testutil::now(),
        )
        .unwrap();
        (dag, plan.run)
    }

    #[test]
    fn start_readies_entries_only() {
        let (_dag, run) = linear_run();
        assert_eq!(run.version, 1);
        assert_eq!(
            run.node(&NodeId::new("a").unwrap()).unwrap().status,
            NodeStatus::Ready
        );
        assert_eq!(
            run.node(&NodeId::new("b").unwrap()).unwrap().status,
            NodeStatus::Pending
        );
    }

    #[test]
    fn claim_requires_a_ready_node() {
        let (_dag, run) = linear_run();
        let a = NodeId::new("a").unwrap();
        let b = NodeId::new("b").unwrap();
        assert!(claim(&run, &b, testutil::now()).is_err(), "b is not ready");
        let transition = claim(&run, &a, testutil::now()).unwrap();
        match transition.step {
            Step::Claim { attempt, .. } => assert_eq!(attempt, 1),
            _ => panic!("expected a claim"),
        }
    }

    #[test]
    fn complete_rejects_a_terminal_node() {
        let (dag, run) = linear_run();
        let a = NodeId::new("a").unwrap();
        let outcome = NodeOutcome::Done {
            output: None,
            branches: Vec::new(),
        };
        let first = complete(&dag, &run, &a, outcome.clone(), testutil::now()).unwrap();
        // Apply the completion by hand so the node is terminal.
        let mut applied = run.clone();
        crate::rules::apply_step(&mut applied, &first.step).unwrap();
        assert!(complete(&dag, &applied, &a, outcome, testutil::now()).is_err());
    }

    #[test]
    fn output_floats_are_rejected() {
        let (dag, run) = linear_run();
        let a = NodeId::new("a").unwrap();
        let outcome = NodeOutcome::Done {
            output: Some(json!({"qty": 1.5})),
            branches: Vec::new(),
        };
        assert!(matches!(
            complete(&dag, &run, &a, outcome, testutil::now()),
            Err(ExecError::OutputInvalid { .. })
        ));
    }

    #[test]
    fn a_dag_mismatch_is_refused() {
        let dag = testutil::dag(vec![node("a")], Vec::new());
        let mut pin = testutil::published(&dag);
        pin.hash = spot_dag::ContentHash::of(&spot_dag::DagDefinition::new(
            "other",
            1,
            vec![node("z")],
            Vec::new(),
        ));
        assert!(matches!(
            start(&dag, &pin, RunId::new("r").unwrap(), "k", testutil::now()),
            Err(ExecError::DagMismatch { .. })
        ));
    }
}
