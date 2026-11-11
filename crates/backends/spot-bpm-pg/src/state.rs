//! Applying one DAG transition to a run, and the `(case, node, attempt)` idempotency key.
//!
//! `spot-dag-exec` keeps its `apply_step`/`already_applied` private, so the store repeats the
//! same, small rules here (the store cannot reuse them across the crate boundary): a claim moves
//! a `Ready` node to `Running` at the attempt the caller supplied; a completion sets the terminal
//! status, releases the newly ready nodes and skips the dead ones; a cancel stops the run. The
//! end-to-end tests prove this store follows exactly the rules the in-memory reference does.

use spot_bpm_engine::{EngineError, EngineResult};
use spot_dag::NodeId;
use spot_dag_exec::{NodeStatus, Run, RunStatus, Step};

/// Whether the step's `(case, node, attempt)` key has already been applied, so a retry is a
/// no-op rather than a double advance.
pub(crate) fn already_applied(run: &Run, step: &Step) -> bool {
    match step {
        Step::Claim { node, attempt, .. } => run
            .node(node)
            .is_some_and(|state| state.status == NodeStatus::Running && state.attempts == *attempt),
        Step::Complete { node, attempt, .. } => run.node(node).is_some_and(|state| {
            matches!(state.status, NodeStatus::Done | NodeStatus::Failed)
                && state.attempts == *attempt
        }),
        Step::Cancel { .. } => run.status == RunStatus::Cancelled,
    }
}

fn invalid_node(node: &NodeId) -> EngineError {
    EngineError::StoreFailed {
        detail: format!("the run does not track node `{node}`"),
    }
}

/// Applies one step to a run in place, exactly as the in-memory reference does.
pub(crate) fn apply_step(run: &mut Run, step: &Step) -> EngineResult<()> {
    match step {
        Step::Claim { node, attempt, at } => {
            let state = run.nodes.get_mut(node).ok_or_else(|| invalid_node(node))?;
            if state.status != NodeStatus::Ready {
                return Err(EngineError::StoreFailed {
                    detail: format!("node `{node}` is {:?}, not ready", state.status),
                });
            }
            state.status = NodeStatus::Running;
            state.attempts = *attempt;
            if state.started_at.is_none() {
                state.started_at = Some(*at);
            }
            Ok(())
        }
        Step::Complete {
            node,
            attempt,
            completion,
            at,
            taken,
            ready,
            skipped,
            status,
            ended_at,
        } => {
            if run
                .node(node)
                .is_some_and(|state| matches!(state.status, NodeStatus::Done | NodeStatus::Failed))
            {
                return Err(EngineError::StoreFailed {
                    detail: format!("node `{node}` is already terminal"),
                });
            }
            {
                let state = run.nodes.get_mut(node).ok_or_else(|| invalid_node(node))?;
                state.status = completion.status();
                state.attempts = *attempt;
                state.output = completion.output().cloned();
                state.failure = completion.reason().map(str::to_owned);
                state.ended_at = Some(*at);
                if state.started_at.is_none() {
                    state.started_at = Some(*at);
                }
            }
            run.taken.extend(taken.iter().cloned());
            for id in ready {
                if let Some(state) = run.nodes.get_mut(id)
                    && state.status == NodeStatus::Pending
                {
                    state.status = NodeStatus::Ready;
                }
            }
            for id in skipped {
                if let Some(state) = run.nodes.get_mut(id)
                    && state.status == NodeStatus::Pending
                {
                    state.status = NodeStatus::Skipped;
                }
            }
            run.status = *status;
            run.ended_at = *ended_at;
            Ok(())
        }
        Step::Cancel { at } => {
            run.status = RunStatus::Cancelled;
            run.ended_at = Some(*at);
            Ok(())
        }
    }
}
