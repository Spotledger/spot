//! Completing a node that is waiting, from outside the step loop, with the branch the
//! conversation decided.
//!
//! The engine completes a node whose timer fired or that was signalled with *no* branch label,
//! so a wait for a message cannot tell "the message came" from "the limit passed", and cannot
//! take its `timeout` edge or its error branch. The driver therefore completes these nodes
//! itself, through the same two commits the engine makes: first the node's stored outcome, then
//! the DAG transition. The order is the engine's own, so a crash between the two is repaired by
//! the engine's next step, which replays the stored outcome (including its branches).

use serde_json::{Map, Value};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{
    Attempt, AttemptState, CaseId, CaseStore, CaseUpdate, ERROR_CONDITION, Engine, TimerPort,
    expr::evaluate_bool,
};
use spot_dag::NodeId;
use spot_dag_exec::{NodeOutcome as DagOutcome, NodeStatus, RunStatus, complete};

use crate::error::{ConversationError, Result, codes};

/// How a waiting node ends.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// The node succeeded. `set` writes values into the Case variables (dotted path, value) in
    /// the same commit, and the branches taken are the outgoing edges whose condition holds over
    /// the variables as written, exactly as a branch node chooses them.
    Done {
        /// The node's output.
        output: Option<Value>,
        /// Variables to write.
        set: Vec<(String, Value)>,
    },
    /// The node failed and takes its error branch.
    Failed {
        /// A stable code.
        code: &'static str,
        /// A plain-language reason that names no value.
        reason: String,
    },
}

/// What [`resolve_wait`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// The node was waiting and is now complete.
    Completed,
    /// The node is past this wait: it already moved on, a later attempt replaced it, or the Case
    /// ended. Nothing is left to complete.
    Gone,
    /// The node has not stored its wait yet (the engine has not finished the attempt that
    /// registered it). Try again on the next pass.
    NotYet,
}

fn resolve_error(code: &'static str) -> ConversationError {
    ConversationError::Resolve { code }
}

/// Completes the waiting `node` of `case` (registered by `attempt`) with `resolution`.
///
/// # Errors
/// [`ConversationError::Resolve`] when the Case or its process cannot be loaded or the store
/// refuses a commit (including a stale version; the caller retries on its next pass).
pub async fn resolve_wait<S: CaseStore + 'static, T: TimerPort + 'static>(
    engine: &Engine<S, T>,
    case: &str,
    node: &str,
    attempt: u32,
    resolution: Resolution,
    now: OffsetDateTime,
) -> Result<Resolved> {
    let case_id = CaseId::new(case).map_err(|_| resolve_error(codes::CONFIG_INVALID))?;
    let node_id = NodeId::new(node).map_err(|_| resolve_error(codes::CONFIG_INVALID))?;
    let record = engine
        .load(&case_id)
        .await
        .map_err(|error| resolve_error(error.code()))?;
    if record.run.status != RunStatus::Running {
        return Ok(Resolved::Gone);
    }
    let running = record
        .run
        .node(&node_id)
        .is_some_and(|state| state.status == NodeStatus::Running);
    if !running {
        return Ok(Resolved::Gone);
    }
    let Some(stored) = record.attempt(&node_id).cloned() else {
        return Ok(Resolved::NotYet);
    };
    if stored.attempt > attempt || matches!(stored.state, AttemptState::Done | AttemptState::Failed)
    {
        return Ok(Resolved::Gone);
    }
    if stored.state != AttemptState::Waiting || stored.attempt != attempt {
        return Ok(Resolved::NotYet);
    }
    let process = engine
        .processes()
        .get(&record.run.dag_key, record.run.dag_version)
        .ok_or_else(|| resolve_error(codes::RESOLVE_FAILED))?;

    let mut variables = record.variables.clone();
    if let Resolution::Done { set, .. } = &resolution {
        for (path, value) in set {
            let parts: Vec<&str> = path.split('.').collect();
            set_path(&mut variables, &parts, value.clone());
        }
    }
    let branches = match &resolution {
        Resolution::Done { .. } => {
            let conditions: Vec<String> = process
                .compiled
                .dag
                .successors(&node_id)
                .iter()
                .filter_map(|edge| edge.condition.clone())
                .filter(|condition| condition != ERROR_CONDITION)
                .collect();
            route(&conditions, &variables, now)
        }
        Resolution::Failed { .. } => Vec::new(),
    };
    let (state, output, branches, failure, dag_outcome) = match &resolution {
        Resolution::Done { output, .. } => (
            AttemptState::Done,
            output.clone(),
            branches.clone(),
            None,
            DagOutcome::Done {
                output: output.clone(),
                branches: branches.clone(),
            },
        ),
        Resolution::Failed { code, reason } => {
            let text = format!("{code}: {reason}");
            (
                AttemptState::Failed,
                None,
                Vec::new(),
                Some(text.clone()),
                DagOutcome::Failed {
                    reason: text,
                    branches: vec![ERROR_CONDITION.to_owned()],
                },
            )
        }
    };
    let mut update = CaseUpdate::new().with_attempt(Attempt {
        node: node_id.clone(),
        attempt,
        state,
        output,
        branches,
        failure,
        started_at: stored.started_at,
        not_before: None,
        is_retry: stored.is_retry,
    });
    if variables != record.variables {
        update = update.with_variables(variables);
    }
    let stored_record = engine
        .store()
        .commit(&case_id, record.run.version, update)
        .await
        .map_err(|error| resolve_error(error.code()))?;
    let transition = complete(
        &process.compiled.dag,
        &stored_record.run,
        &node_id,
        dag_outcome,
        now,
    )
    .map_err(|_| resolve_error(codes::RESOLVE_FAILED))?;
    engine
        .store()
        .commit(
            &case_id,
            stored_record.run.version,
            CaseUpdate::new()
                .with_transition(transition)
                .clearing_timer(node_id),
        )
        .await
        .map_err(|error| resolve_error(error.code()))?;
    Ok(Resolved::Completed)
}

/// The outgoing conditions that hold over `variables` at `now`: the branches a node takes. A
/// condition that does not evaluate does not hold.
#[must_use]
pub fn route(conditions: &[String], variables: &Value, now: OffsetDateTime) -> Vec<String> {
    conditions
        .iter()
        .filter(|condition| evaluate_bool(condition, variables, now).unwrap_or(false))
        .cloned()
        .collect()
}

/// Writes `value` at the dotted path inside `current`, creating objects on the way.
pub(crate) fn set_path(current: &mut Value, parts: &[&str], value: Value) {
    let Some((head, rest)) = parts.split_first() else {
        return;
    };
    if !current.is_object() {
        *current = Value::Object(Map::new());
    }
    let Value::Object(map) = current else {
        return;
    };
    if rest.is_empty() {
        map.insert((*head).to_owned(), value);
        return;
    }
    let child = map
        .entry((*head).to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    set_path(child, rest, value);
}
