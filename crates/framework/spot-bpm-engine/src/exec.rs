//! The node execution internals: claim, run, complete, retry, wait and sub-process.
//!
//! These are the methods [`Engine::step`](crate::Engine::step) drives. Each attempt is stored
//! before its DAG transition is acknowledged, so a replay returns the stored outcome instead of
//! calling the handler again; only a `started, no outcome` attempt is re-run, with
//! [`NodeCtx::is_retry`](crate::NodeCtx::is_retry) set. Time is always an argument.

use std::time::Duration;

use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_model::RetryPolicy;
use spot_dag::NodeId;
use spot_dag_exec::{
    ExecError, Instant, NodeOutcome as DagOutcome, NodeStatus, RunStatus, claim as dag_claim,
    complete as dag_complete,
};
use spot_jobs::Backoff;

use crate::case::{Attempt, AttemptState, CaseRecord, CaseUpdate, ChildLink, TimerRecord};
use crate::engine::{EffectPoint, Engine, ProcessDefinition};
use crate::error::{EngineError, EngineResult};
use crate::handler::{
    CaseId, IdempotencyKey, NodeCtx, NodeKind, NodeOutcome, WaitSpec, node_config, node_kind,
    outgoing_edges,
};
use crate::store::CaseStore;
use crate::timer::{TimerPort, TimerRegistration};

impl<S: CaseStore + 'static, T: TimerPort + 'static> Engine<S, T> {
    /// Claims a ready node and advances it.
    pub(crate) async fn claim_and_advance(
        &self,
        record: &mut CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
        now: OffsetDateTime,
        ran: &mut Vec<NodeId>,
    ) -> EngineResult<()> {
        let transition = dag_claim(&record.run, node, now).map_err(map_exec)?;
        self.commit(record, CaseUpdate::new().with_transition(transition))
            .await?;
        self.advance(record, process, node, now, ran).await
    }

    /// Advances a node: a sub-process starts or awaits a child; anything else runs its handler.
    async fn advance(
        &self,
        record: &mut CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
        now: OffsetDateTime,
        ran: &mut Vec<NodeId>,
    ) -> EngineResult<()> {
        let kind = node_kind(&process.compiled, node);
        if kind == NodeKind::SubProcess {
            self.advance_child(record, process, node, now, ran).await
        } else {
            self.run_handler_node(record, process, node, now, ran).await
        }
    }

    /// Resumes a running node: a retry that is not yet due waits; a started attempt that passed
    /// its deadline fails; anything else advances.
    pub(crate) async fn resume_running(
        &self,
        record: &mut CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
        now: OffsetDateTime,
        ran: &mut Vec<NodeId>,
    ) -> EngineResult<()> {
        let at = Instant::from_offset(now);
        let retry_not_before = record.attempt(node).and_then(|attempt| {
            (attempt.state == AttemptState::Retrying).then_some(attempt.not_before)
        });
        if let Some(not_before) = retry_not_before
            && not_before.is_some_and(|instant| instant > at)
        {
            return Ok(());
        }
        let started = record
            .attempt(node)
            .filter(|attempt| attempt.state == AttemptState::Started)
            .map(|attempt| (attempt.attempt, attempt.is_retry));
        if let Some((attempt_number, is_retry)) = started
            && let Some(deadline) = self.deadline(record, process, node)
            && deadline <= at
        {
            let seconds = self.timeout_secs(process, node).unwrap_or(0);
            return self
                .finish_failure(
                    record,
                    process,
                    node,
                    attempt_number,
                    now,
                    format!("the node exceeded its {seconds}s timeout"),
                    is_retry,
                )
                .await;
        }
        self.advance(record, process, node, now, ran).await
    }

    /// Runs a handler-backed node: store the outcome, then apply the DAG transition.
    async fn run_handler_node(
        &self,
        record: &mut CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
        now: OffsetDateTime,
        ran: &mut Vec<NodeId>,
    ) -> EngineResult<()> {
        if let Some(state) = record.run.node(node).map(|state| state.status)
            && matches!(
                state,
                NodeStatus::Done | NodeStatus::Failed | NodeStatus::Skipped
            )
        {
            return Ok(());
        }
        let dag_attempt = record.run.node(node).map_or(1, |state| state.attempts);
        let stored = record.attempt(node).cloned();
        let (attempt, is_retry, already_started, started_at) = match &stored {
            Some(attempt) if attempt.state == AttemptState::Done => {
                return self
                    .apply_done(
                        record,
                        process,
                        node,
                        attempt.output.clone(),
                        attempt.branches.clone(),
                        now,
                    )
                    .await;
            }
            Some(attempt) if attempt.state == AttemptState::Failed => {
                return self
                    .apply_failed(
                        record,
                        process,
                        node,
                        attempt.failure.clone().unwrap_or_default(),
                        now,
                    )
                    .await;
            }
            Some(attempt) if attempt.state == AttemptState::Waiting => {
                return self.ensure_timer(record, node, attempt.attempt, now).await;
            }
            Some(attempt) if attempt.state == AttemptState::Retrying => (
                attempt.attempt.saturating_add(1),
                false,
                false,
                Instant::from_offset(now),
            ),
            Some(attempt) if attempt.state == AttemptState::Started => {
                (attempt.attempt, true, true, attempt.started_at)
            }
            _ => (dag_attempt.max(1), false, false, Instant::from_offset(now)),
        };

        if !already_started {
            let started = Attempt::started(node.clone(), attempt, started_at, is_retry);
            self.commit(record, CaseUpdate::new().with_attempt(started))
                .await?;
        }

        let kind = node_kind(&process.compiled, node);
        let handler = self
            .handlers
            .get(&kind)
            .ok_or_else(|| EngineError::HandlerMissing {
                node: node.to_string(),
                kind: kind.token().to_owned(),
            })?;
        let config = node_config(&process.compiled, node);
        let outgoing = outgoing_edges(&process.compiled, node);
        let deadline = self
            .deadline(record, process, node)
            .map(Instant::to_offset)
            .transpose()
            .map_err(map_exec)?;
        let mut variables = record.variables.clone();
        let key = IdempotencyKey::new(record.run.id.clone(), node.clone(), attempt);
        self.faults.check(&EffectPoint::BeforeHandler {
            case: record.run.id.clone(),
            node: node.clone(),
            attempt,
        })?;
        let outcome = {
            let mut ctx = NodeCtx {
                case: &record.run.id,
                node,
                kind,
                attempt,
                key,
                is_retry,
                now,
                deadline,
                principal: record.initiator.as_deref(),
                config: &config,
                outgoing: &outgoing,
                variables: &mut variables,
                cancelled: record.run.status == RunStatus::Cancelled,
            };
            handler.run(&mut ctx)
        };
        self.faults.check(&EffectPoint::AfterHandler {
            case: record.run.id.clone(),
            node: node.clone(),
            attempt,
        })?;
        ran.push(node.clone());

        if variables != record.variables {
            self.commit(record, CaseUpdate::new().with_variables(variables))
                .await?;
        }

        match outcome {
            NodeOutcome::Done { output, branches } => {
                let stored = Attempt {
                    node: node.clone(),
                    attempt,
                    state: AttemptState::Done,
                    output: output.clone(),
                    branches: branches.clone(),
                    failure: None,
                    started_at,
                    not_before: None,
                    is_retry,
                };
                self.commit(record, CaseUpdate::new().with_attempt(stored))
                    .await?;
                self.faults.check(&EffectPoint::AfterAttemptStored {
                    case: record.run.id.clone(),
                    node: node.clone(),
                    attempt,
                })?;
                self.apply_done(record, process, node, output, branches, now)
                    .await
            }
            NodeOutcome::Wait(spec) => {
                let timer = timer_record(node, attempt, &spec);
                let stored = Attempt {
                    node: node.clone(),
                    attempt,
                    state: AttemptState::Waiting,
                    output: None,
                    branches: Vec::new(),
                    failure: None,
                    started_at,
                    not_before: spec.due().map(Instant::from_offset),
                    is_retry,
                };
                self.commit(
                    record,
                    CaseUpdate::new().with_attempt(stored).with_timer(timer),
                )
                .await?;
                self.register_timer(record, node, attempt, &spec);
                Ok(())
            }
            NodeOutcome::Retry { after } => {
                self.retry_or_fail(
                    record,
                    process,
                    node,
                    attempt,
                    now,
                    after,
                    "the handler asked to retry".to_owned(),
                    started_at,
                    is_retry,
                )
                .await
            }
            NodeOutcome::Fail { code, reason } => {
                self.finish_failure(
                    record,
                    process,
                    node,
                    attempt,
                    now,
                    format!("{code}: {reason}"),
                    is_retry,
                )
                .await
            }
        }
    }

    /// Applies a success transition, building it from the stored outcome when the node is not
    /// already terminal.
    pub(crate) async fn apply_done(
        &self,
        record: &mut CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
        output: Option<Json>,
        branches: Vec<String>,
        now: OffsetDateTime,
    ) -> EngineResult<()> {
        if let Some(state) = record.run.node(node).map(|state| state.status)
            && state.is_terminal()
        {
            return Ok(());
        }
        let transition = dag_complete(
            &process.compiled.dag,
            &record.run,
            node,
            DagOutcome::Done { output, branches },
            now,
        )
        .map_err(map_exec)?;
        self.commit(record, CaseUpdate::new().with_transition(transition))
            .await?;
        self.faults.check(&EffectPoint::AfterTransition {
            case: record.run.id.clone(),
            node: node.clone(),
        })?;
        Ok(())
    }

    /// Applies a failure transition (the error branch), when the node is not already terminal.
    async fn apply_failed(
        &self,
        record: &mut CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
        reason: String,
        now: OffsetDateTime,
    ) -> EngineResult<()> {
        if let Some(state) = record.run.node(node).map(|state| state.status)
            && state.is_terminal()
        {
            return Ok(());
        }
        let transition = dag_complete(
            &process.compiled.dag,
            &record.run,
            node,
            DagOutcome::Failed {
                reason,
                branches: vec![crate::handler::ERROR_CONDITION.to_owned()],
            },
            now,
        )
        .map_err(map_exec)?;
        self.commit(record, CaseUpdate::new().with_transition(transition))
            .await?;
        self.faults.check(&EffectPoint::AfterTransition {
            case: record.run.id.clone(),
            node: node.clone(),
        })?;
        Ok(())
    }

    /// Retries a failed attempt per the node's policy, or fails it with the error branch.
    #[allow(clippy::too_many_arguments)]
    async fn retry_or_fail(
        &self,
        record: &mut CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
        attempt: u32,
        now: OffsetDateTime,
        after: OffsetDateTime,
        reason: String,
        started_at: Instant,
        is_retry: bool,
    ) -> EngineResult<()> {
        let policy = process.retry_policy(node);
        if attempt < policy.max_attempts {
            let at = if after > now {
                after
            } else {
                self.backoff_at(policy, attempt + 1, record, node, now)
            };
            let stored = Attempt {
                node: node.clone(),
                attempt,
                state: AttemptState::Retrying,
                output: None,
                branches: Vec::new(),
                failure: Some(reason),
                started_at,
                not_before: Some(Instant::from_offset(at)),
                is_retry,
            };
            self.commit(record, CaseUpdate::new().with_attempt(stored))
                .await?;
            Ok(())
        } else {
            let stored = Attempt {
                node: node.clone(),
                attempt,
                state: AttemptState::Failed,
                output: None,
                branches: Vec::new(),
                failure: Some(reason.clone()),
                started_at,
                not_before: None,
                is_retry,
            };
            self.commit(record, CaseUpdate::new().with_attempt(stored))
                .await?;
            self.apply_failed(record, process, node, reason, now).await
        }
    }

    /// Fails an attempt after its retries, taking the error branch.
    #[allow(clippy::too_many_arguments)]
    async fn finish_failure(
        &self,
        record: &mut CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
        attempt: u32,
        now: OffsetDateTime,
        reason: String,
        is_retry: bool,
    ) -> EngineResult<()> {
        let started_at = record
            .attempt(node)
            .map_or_else(|| Instant::from_offset(now), |attempt| attempt.started_at);
        self.retry_or_fail(
            record, process, node, attempt, now, now, reason, started_at, is_retry,
        )
        .await
    }

    /// The deterministic retry instant from the node's policy.
    fn backoff_at(
        &self,
        policy: RetryPolicy,
        next_attempt: u32,
        record: &CaseRecord,
        node: &NodeId,
        now: OffsetDateTime,
    ) -> OffsetDateTime {
        let base = policy.delay_secs.max(1);
        let backoff = Backoff {
            base: Duration::from_secs(base),
            max: Duration::from_secs(3600),
            exponent: 2,
        };
        let job_id = fnv64(&[record.run.id.as_str(), node.as_str()]);
        backoff.delay_for_attempt(next_attempt, job_id, now)
    }

    /// The node's deadline, from its start instant and configured timeout.
    fn deadline(
        &self,
        record: &CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
    ) -> Option<Instant> {
        let timeout = self.timeout_secs(process, node)?;
        let started = record.run.node(node)?.started_at?;
        let at = started.to_offset().ok()?;
        Some(Instant::from_offset(at + Duration::from_secs(timeout)))
    }

    fn timeout_secs(&self, process: &ProcessDefinition, node: &NodeId) -> Option<u64> {
        process.timeout_secs(node)
    }

    /// Ensures a waiting node's timer is registered with the port, then leaves it waiting.
    async fn ensure_timer(
        &self,
        record: &mut CaseRecord,
        node: &NodeId,
        attempt: u32,
        now: OffsetDateTime,
    ) -> EngineResult<()> {
        if let Some(timer) = record.timers.get(node).cloned() {
            self.timers.register(TimerRegistration {
                case: record.run.id.clone(),
                node: node.clone(),
                attempt: timer.attempt,
                due: timer.due.and_then(|due| due.to_offset().ok()),
                event: timer.event.clone(),
                human: timer.human.clone(),
            });
        } else {
            // The durable wait record is missing (it cannot be, since it commits with the
            // attempt); complete the node so it never hangs.
            let stored = Attempt {
                node: node.clone(),
                attempt,
                state: AttemptState::Done,
                output: None,
                branches: Vec::new(),
                failure: None,
                started_at: Instant::from_offset(now),
                not_before: None,
                is_retry: false,
            };
            self.commit(record, CaseUpdate::new().with_attempt(stored))
                .await?;
        }
        Ok(())
    }

    fn register_timer(&self, record: &CaseRecord, node: &NodeId, attempt: u32, spec: &WaitSpec) {
        self.timers.register(TimerRegistration {
            case: record.run.id.clone(),
            node: node.clone(),
            attempt,
            due: spec.due(),
            event: spec
                .key()
                .filter(|_| matches!(spec, WaitSpec::ForEvent { .. }))
                .map(str::to_owned),
            human: spec
                .key()
                .filter(|_| matches!(spec, WaitSpec::HumanTask { .. }))
                .map(str::to_owned),
        });
    }

    /// Fires a due timer: completes the waiting node with its stored outcome.
    pub(crate) async fn fire_timer(
        &self,
        record: &mut CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
        now: OffsetDateTime,
        ran: &mut Vec<NodeId>,
    ) -> EngineResult<()> {
        if let Some(state) = record.run.node(node).map(|state| state.status)
            && state.is_terminal()
        {
            self.timers.clear(&record.run.id, node);
            return Ok(());
        }
        let stored = record.attempt(node).cloned();
        let attempt = stored.as_ref().map_or_else(
            || record.run.node(node).map_or(1, |state| state.attempts),
            |a| a.attempt,
        );
        let started_at = stored
            .as_ref()
            .map_or_else(|| Instant::from_offset(now), |a| a.started_at);
        let is_retry = stored.as_ref().is_some_and(|a| a.is_retry);
        let output = stored.as_ref().and_then(|a| a.output.clone());
        let done = Attempt {
            node: node.clone(),
            attempt,
            state: AttemptState::Done,
            output: output.clone(),
            branches: Vec::new(),
            failure: None,
            started_at,
            not_before: None,
            is_retry,
        };
        self.commit(record, CaseUpdate::new().with_attempt(done))
            .await?;
        let transition = dag_complete(
            &process.compiled.dag,
            &record.run,
            node,
            DagOutcome::Done {
                output,
                branches: Vec::new(),
            },
            now,
        )
        .map_err(map_exec)?;
        self.commit(
            record,
            CaseUpdate::new()
                .with_transition(transition)
                .clearing_timer(node.clone()),
        )
        .await?;
        self.timers.clear(&record.run.id, node);
        ran.push(node.clone());
        Ok(())
    }

    /// Starts a sub-process child or, when it already exists, awaits it.
    async fn advance_child(
        &self,
        record: &mut CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
        now: OffsetDateTime,
        ran: &mut Vec<NodeId>,
    ) -> EngineResult<()> {
        if record.children.contains_key(node) {
            return self.await_child(record, process, node, now, ran).await;
        }
        let config = node_config(&process.compiled, node);
        let name = config
            .get("process")
            .and_then(Json::as_str)
            .ok_or_else(|| EngineError::ConfigInvalid {
                node: node.to_string(),
                detail: "a sub-process node needs a `process` name".to_owned(),
            })?
            .to_owned();
        let version = config
            .get("version")
            .and_then(Json::as_u64)
            .map(|raw| raw as u32);
        let definition = match version {
            Some(version) => self.processes.get(&name, version),
            None => self.processes.latest(&name),
        }
        .ok_or_else(|| EngineError::ChildProcessUnknown {
            node: node.to_string(),
            process: name.clone(),
        })?;
        let child_key = format!("{}:{}", record.run.id, node);
        let child_id = match self.store.find_by_key(&child_key).await? {
            Some(existing) => existing.run.id,
            None => {
                let id = derive_child_id(&record.run.id, node)?;
                let initiator = record.initiator.clone();
                self.start_definition(&definition, id, child_key, initiator, now)
                    .await?
                    .run
                    .id
            }
        };
        let link = ChildLink {
            node: node.clone(),
            attempt: record.run.node(node).map_or(1, |state| state.attempts),
            child: child_id,
        };
        self.commit(record, CaseUpdate::new().with_child(link))
            .await?;
        self.await_child(record, process, node, now, ran).await
    }

    /// Awaits a sub-process child: completes the parent node when the child finished.
    pub(crate) async fn await_child(
        &self,
        record: &mut CaseRecord,
        process: &ProcessDefinition,
        node: &NodeId,
        now: OffsetDateTime,
        ran: &mut Vec<NodeId>,
    ) -> EngineResult<()> {
        if let Some(state) = record.run.node(node).map(|state| state.status)
            && state.is_terminal()
        {
            return Ok(());
        }
        let Some(link) = record.children.get(node).cloned() else {
            return Ok(());
        };
        let Some(child) = self.store.load(&link.child).await? else {
            return Ok(());
        };
        match child.run.status {
            RunStatus::Running => Ok(()),
            RunStatus::Completed => {
                let output = child_output(&child);
                let attempt = link.attempt;
                let started_at = record
                    .attempt(node)
                    .map_or_else(|| Instant::from_offset(now), |a| a.started_at);
                let stored = Attempt {
                    node: node.clone(),
                    attempt,
                    state: AttemptState::Done,
                    output: output.clone(),
                    branches: Vec::new(),
                    failure: None,
                    started_at,
                    not_before: None,
                    is_retry: false,
                };
                self.commit(record, CaseUpdate::new().with_attempt(stored))
                    .await?;
                ran.push(node.clone());
                self.apply_done(record, process, node, output, Vec::new(), now)
                    .await
            }
            _ => {
                let attempt = link.attempt;
                let reason = format!(
                    "the child case `{}` ended as {:?}",
                    link.child, child.run.status
                );
                self.finish_failure(record, process, node, attempt, now, reason, false)
                    .await
            }
        }
    }
}

/// A timer record from a wait spec.
fn timer_record(node: &NodeId, attempt: u32, spec: &WaitSpec) -> TimerRecord {
    TimerRecord {
        node: node.clone(),
        attempt,
        due: spec.due().map(Instant::from_offset),
        event: spec
            .key()
            .filter(|_| matches!(spec, WaitSpec::ForEvent { .. }))
            .map(str::to_owned),
        human: spec
            .key()
            .filter(|_| matches!(spec, WaitSpec::HumanTask { .. }))
            .map(str::to_owned),
    }
}

/// The output a finished child Case hands its parent: every done node's output, keyed by node.
fn child_output(child: &CaseRecord) -> Option<Json> {
    let mut outputs = serde_json::Map::new();
    for (node, state) in &child.run.nodes {
        if state.status == NodeStatus::Done
            && let Some(output) = &state.output
        {
            outputs.insert(node.to_string(), output.clone());
        }
    }
    if outputs.is_empty() {
        None
    } else {
        Some(Json::Object(outputs))
    }
}

/// The deterministic id of a sub-process node's child Case.
fn derive_child_id(case: &CaseId, node: &NodeId) -> EngineResult<CaseId> {
    let hash = fnv64(&[case.as_str(), node.as_str()]);
    CaseId::new(&format!("sub_{hash:016x}")).map_err(|error| EngineError::StoreFailed {
        detail: error.to_string(),
    })
}

/// A deterministic 64-bit FNV-1a hash, so retry jitter and child ids never need randomness.
fn fnv64(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Maps a `spot-dag-exec` error onto this crate's vocabulary.
pub(crate) fn map_exec(error: ExecError) -> EngineError {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn fnv_is_deterministic_and_distinct() {
        assert_eq!(fnv64(&["a", "b"]), fnv64(&["a", "b"]));
        assert_ne!(fnv64(&["a", "b"]), fnv64(&["a", "c"]));
    }

    #[test]
    fn child_ids_are_valid_and_distinct() {
        let case = CaseId::new("case1").unwrap();
        let a = derive_child_id(&case, &NodeId::new("node_a").unwrap()).unwrap();
        let b = derive_child_id(&case, &NodeId::new("node_b").unwrap()).unwrap();
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("sub_"));
    }
}
