//! The transition rules: the single definition of what one [`Step`] does to a [`Run`], and of
//! when a step has already been applied.
//!
//! Every [`ExecStore`](crate::ExecStore) needs exactly these rules: the in-memory reference
//! ([`InMemoryExecStore`](crate::InMemoryExecStore)) applies them in place, and a database
//! store ([`spot-dag-pg`](https://docs.rs/spot-dag-pg)) calls the same two functions inside its
//! own transaction rather than keeping a copy. A backend that re-implemented them could drift
//! from the state machine, and the contract suite would only catch the parts it exercises.
//!
//! [`apply_step`] is pure: it changes only the [`Run`] it is given, reads no clock (the
//! transition carries its instants) and returns a typed error when the step does not fit the
//! run's current state. [`already_applied`] is the `(run, node, attempt)` idempotency key: a
//! retried claim or completion, or a repeated cancel, is a no-op.

use crate::error::{ExecError, ExecResult};
use crate::state::{NodeStatus, Run, RunStatus, Step};

/// Whether the step's `(run, node, attempt)` key has already been applied, so a retry is a
/// no-op.
///
/// A claim is applied when the node is `Running` at the step's attempt; a completion when the
/// node is `Done` or `Failed` at that attempt; a cancel when the run is already cancelled.
#[must_use]
pub fn already_applied(run: &Run, step: &Step) -> bool {
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

fn invalid_node(node: &spot_dag::NodeId) -> ExecError {
    ExecError::InvalidNode {
        node: node.to_string(),
    }
}

/// Applies one [`Step`] to `run` in place: a claim moves a `Ready` node to `Running` at the
/// caller's attempt, a completion sets the terminal status, records the taken edges, releases
/// the newly ready nodes and skips the dead ones, and a cancel stops the run.
///
/// # Errors
///
/// [`ExecError::InvalidNode`] when the step names a node the run does not track, and
/// [`ExecError::InvalidState`] when the step does not fit the node's current state (claiming a
/// node that is not `Ready`, completing a node that is already terminal).
pub fn apply_step(run: &mut Run, step: &Step) -> ExecResult<()> {
    match step {
        Step::Claim { node, attempt, at } => {
            let state = run.nodes.get_mut(node).ok_or_else(|| invalid_node(node))?;
            if state.status != NodeStatus::Ready {
                return Err(ExecError::InvalidState {
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
                return Err(ExecError::InvalidState {
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
