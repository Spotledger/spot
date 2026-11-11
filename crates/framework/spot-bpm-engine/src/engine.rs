//! The engine: the durable step loop over the execution store.
//!
//! [`Engine::step`] loads a Case and its pinned [`ProcessDefinition`], fires due timers, awaits
//! child Cases, resumes nodes left running by a crash, then claims and runs the ready nodes — in
//! that order, so a turn is deterministic. Each node attempt is stored *before* its DAG
//! transition is acknowledged, so a replay returns the stored outcome instead of calling the
//! handler again; only a `started, no outcome` attempt is re-run, and then the handler is told
//! it is a retry.
//!
//! Time is always an argument. The engine never reads a clock, never uses randomness and never
//! performs I/O of its own: the [`CaseStore`], the [`TimerPort`], the handlers and the
//! [`Faults`] injector are all ports.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_model::{Problem, ProcessVersion, RetryPolicy};
use spot_dag::{NodeId, Published};
use spot_dag_exec::{Instant, NodeStatus, RunStatus, cancel as dag_cancel, start as dag_start};

use crate::case::{Attempt, AttemptState, CaseRecord, CaseUpdate};
use crate::error::{EngineError, EngineResult};
use crate::exec::map_exec;
use crate::handler::{CaseId, HandlerRegistry};
use crate::store::{CaseStore, InMemoryCaseStore};
use crate::timer::{InMemoryTimerPort, TimerPort};

/// A compiled Process version plus the model version it was compiled from.
///
/// The DAG layer keeps node data opaque, so the retry policy, the timeout and the terminal flag
/// live only in the model's [`ProcessVersion`]; the engine keeps both together so it can read
/// them without re-parsing the process.
#[derive(Debug, Clone)]
pub struct ProcessDefinition {
    /// The model version, for retry policies, timeouts and terminal nodes.
    pub version: ProcessVersion,
    /// The compiled form the DAG runtime and the handlers read.
    pub compiled: spot_bpm_model::CompiledProcess,
}

impl ProcessDefinition {
    /// Compiles a model version, reporting every model problem at once.
    ///
    /// # Errors
    /// The model's [`Problem`]s, when the version does not compile.
    pub fn compile(version: &ProcessVersion) -> Result<Self, Vec<Problem>> {
        let compiled = spot_bpm_model::compile(version)?;
        Ok(Self {
            version: version.clone(),
            compiled,
        })
    }

    /// The process name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.version.name
    }

    /// The version number.
    #[must_use]
    pub fn number(&self) -> u32 {
        self.version.version
    }

    /// The model node with this id, when it exists.
    #[must_use]
    pub fn node(&self, node: &NodeId) -> Option<&spot_bpm_model::Node> {
        self.version
            .nodes
            .iter()
            .find(|candidate| candidate.id.as_str() == node.as_str())
    }

    /// The retry policy of a node, defaulting to no retry.
    #[must_use]
    pub fn retry_policy(&self, node: &NodeId) -> RetryPolicy {
        self.node(node).map(|node| node.retry).unwrap_or_default()
    }

    /// The node's timeout in whole seconds, when it has one.
    #[must_use]
    pub fn timeout_secs(&self, node: &NodeId) -> Option<u64> {
        self.node(node).and_then(|node| node.timeout_secs)
    }
}

/// The registry of compiled processes, keyed by `(name, version)`.
#[derive(Debug, Default)]
pub struct ProcessRegistry {
    entries: Mutex<BTreeMap<(String, u32), Arc<ProcessDefinition>>>,
}

impl ProcessRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<(String, u32), Arc<ProcessDefinition>>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Publishes a definition, replacing one with the same name and version.
    pub fn publish(&self, definition: ProcessDefinition) -> Arc<ProcessDefinition> {
        let key = (definition.name().to_owned(), definition.number());
        let shared = Arc::new(definition);
        self.lock().insert(key, shared.clone());
        shared
    }

    /// The definition with this name and version, when one is registered.
    #[must_use]
    pub fn get(&self, name: &str, version: u32) -> Option<Arc<ProcessDefinition>> {
        self.lock().get(&(name.to_owned(), version)).cloned()
    }

    /// The highest-numbered version registered for a name, when one is.
    #[must_use]
    pub fn latest(&self, name: &str) -> Option<Arc<ProcessDefinition>> {
        self.lock()
            .iter()
            .filter(|((candidate, _), _)| candidate == name)
            .max_by_key(|((_, version), _)| *version)
            .map(|(_, definition)| definition.clone())
    }
}

/// One effect boundary the [`Faults`] injector may crash at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectPoint {
    /// After the attempt's `Started` outcome is stored, before the handler runs.
    BeforeHandler {
        /// The Case.
        case: CaseId,
        /// The node.
        node: NodeId,
        /// The engine attempt.
        attempt: u32,
    },
    /// After the handler returns, before its outcome is stored.
    AfterHandler {
        /// The Case.
        case: CaseId,
        /// The node.
        node: NodeId,
        /// The engine attempt.
        attempt: u32,
    },
    /// After the outcome is stored, before the DAG transition is applied.
    AfterAttemptStored {
        /// The Case.
        case: CaseId,
        /// The node.
        node: NodeId,
        /// The engine attempt.
        attempt: u32,
    },
    /// After the DAG transition is applied.
    AfterTransition {
        /// The Case.
        case: CaseId,
        /// The node.
        node: NodeId,
    },
}

impl EffectPoint {
    /// A stable label for the point, for logs and the crash message.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::BeforeHandler {
                case,
                node,
                attempt,
            } => format!("before_handler:{case}:{node}:{attempt}"),
            Self::AfterHandler {
                case,
                node,
                attempt,
            } => format!("after_handler:{case}:{node}:{attempt}"),
            Self::AfterAttemptStored {
                case,
                node,
                attempt,
            } => format!("after_attempt_stored:{case}:{node}:{attempt}"),
            Self::AfterTransition { case, node } => format!("after_transition:{case}:{node}"),
        }
    }
}

/// A fault injector: the simulation harness's crash points.
pub trait Faults: Send + Sync {
    /// Whether the engine may continue past `point`.
    ///
    /// # Errors
    /// [`EngineError::Crashed`] when the injector decides to crash here.
    fn check(&self, point: &EffectPoint) -> EngineResult<()>;
}

/// A fault injector that never crashes.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFaults;

impl Faults for NoFaults {
    fn check(&self, _point: &EffectPoint) -> EngineResult<()> {
        Ok(())
    }
}

/// What one [`Engine::step`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct StepReport {
    /// The Case stepped.
    pub case: CaseId,
    /// The nodes whose handler ran (or whose wait fired or child completed) this turn.
    pub ran: Vec<NodeId>,
    /// Whether the Case is finished after the turn.
    pub finished: bool,
    /// The Case's status after the turn.
    pub status: RunStatus,
}

impl StepReport {
    /// Whether the turn changed anything.
    #[must_use]
    pub fn made_progress(&self) -> bool {
        !self.ran.is_empty()
    }
}

/// The engine over a Case store, a timer port and a handler registry.
pub struct Engine<S: CaseStore + 'static, T: TimerPort + 'static> {
    pub(crate) store: Arc<S>,
    pub(crate) timers: Arc<T>,
    pub(crate) handlers: Arc<HandlerRegistry>,
    pub(crate) processes: Arc<ProcessRegistry>,
    pub(crate) faults: Arc<dyn Faults>,
}

impl<S: CaseStore + 'static, T: TimerPort + 'static> Engine<S, T> {
    /// Builds an engine over a store, a timer port and a handler registry.
    pub fn new(store: S, timers: T, handlers: HandlerRegistry) -> Self {
        Self {
            store: Arc::new(store),
            timers: Arc::new(timers),
            handlers: Arc::new(handlers),
            processes: Arc::new(ProcessRegistry::new()),
            faults: Arc::new(NoFaults),
        }
    }

    /// Replaces the fault injector.
    #[must_use]
    pub fn with_faults(mut self, faults: Arc<dyn Faults>) -> Self {
        self.faults = faults;
        self
    }

    /// The store the engine drives.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The handler registry.
    #[must_use]
    pub fn handlers(&self) -> &HandlerRegistry {
        &self.handlers
    }

    /// The process registry.
    #[must_use]
    pub fn processes(&self) -> &ProcessRegistry {
        &self.processes
    }

    /// Compiles and publishes a process version.
    ///
    /// # Errors
    /// [`EngineError::ProcessInvalid`] with every model problem, when the version does not
    /// compile.
    pub fn publish(&self, version: &ProcessVersion) -> EngineResult<Arc<ProcessDefinition>> {
        let definition = ProcessDefinition::compile(version).map_err(|problems| {
            EngineError::ProcessInvalid {
                detail: problems
                    .iter()
                    .map(|problem| problem.message.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
            }
        })?;
        Ok(self.processes.publish(definition))
    }

    /// Starts a Case from a model version, pinning its compiled form.
    ///
    /// # Errors
    /// A model problem, a duplicate id or business key, or a store failure.
    pub async fn start(
        &self,
        version: &ProcessVersion,
        id: CaseId,
        key: impl Into<String>,
        initiator: Option<String>,
        now: OffsetDateTime,
    ) -> EngineResult<CaseRecord> {
        let definition = self.publish(version)?;
        self.start_definition(&definition, id, key.into(), initiator, now)
            .await
    }

    /// Starts a Case from an already-published definition.
    ///
    /// # Errors
    /// A duplicate id or business key, or a store failure.
    pub async fn start_definition(
        &self,
        definition: &ProcessDefinition,
        id: CaseId,
        key: String,
        initiator: Option<String>,
        now: OffsetDateTime,
    ) -> EngineResult<CaseRecord> {
        let pin = Published {
            version: definition.compiled.dag.version(),
            hash: definition.compiled.dag.hash().clone(),
        };
        let plan = dag_start(&definition.compiled.dag, &pin, id, key, now).map_err(map_exec)?;
        let record = CaseRecord::new(plan.run, initiator);
        self.store.create(record).await
    }

    /// Loads a Case or reports it missing.
    ///
    /// # Errors
    /// [`EngineError::CaseNotFound`] when the store does not hold it.
    pub async fn load(&self, id: &CaseId) -> EngineResult<CaseRecord> {
        self.store
            .load(id)
            .await?
            .ok_or_else(|| EngineError::CaseNotFound { id: id.to_string() })
    }

    /// Advances a Case by one turn: fires due timers, awaits children, resumes running nodes
    /// and runs the ready nodes.
    ///
    /// # Errors
    /// A store failure, a missing process or handler, or an injected crash.
    pub async fn step(&self, id: &CaseId, now: OffsetDateTime) -> EngineResult<StepReport> {
        let mut record = self.load(id).await?;
        if record.run.status != RunStatus::Running {
            return Ok(self.report(&record, Vec::new()));
        }
        let process = self.process_for(&record)?;
        let mut ran = Vec::new();

        // 1. Fire due timers from the durable records.
        let due: Vec<NodeId> = record
            .timers
            .iter()
            .filter(|(_, timer)| timer.is_due(Instant::from_offset(now)))
            .map(|(node, _)| node.clone())
            .collect();
        for node in due {
            self.fire_timer(&mut record, &process, &node, now, &mut ran)
                .await?;
        }

        // 2. Await the children of sub-process nodes.
        let child_nodes: Vec<NodeId> = record.children.keys().cloned().collect();
        for node in child_nodes {
            self.await_child(&mut record, &process, &node, now, &mut ran)
                .await?;
        }

        // 3. Resume nodes left running (a crash) or waiting for a retry.
        let running: Vec<NodeId> = record
            .run
            .nodes
            .iter()
            .filter(|(_, state)| state.status == NodeStatus::Running)
            .map(|(node, _)| node.clone())
            .collect();
        for node in running {
            self.resume_running(&mut record, &process, &node, now, &mut ran)
                .await?;
        }

        // 4. Claim and run the ready nodes.
        let ready = record.run.ready_nodes();
        for node in ready {
            if record.run.status != RunStatus::Running {
                break;
            }
            self.claim_and_advance(&mut record, &process, &node, now, &mut ran)
                .await?;
        }

        Ok(self.report(&record, ran))
    }

    /// Signals a waiting node with an event or task payload, completing it.
    ///
    /// # Errors
    /// [`EngineError::CaseNotFound`] or a store failure.
    pub async fn signal(
        &self,
        id: &CaseId,
        node: &NodeId,
        payload: Option<Json>,
        now: OffsetDateTime,
    ) -> EngineResult<StepReport> {
        let mut record = self.load(id).await?;
        if record.run.status != RunStatus::Running {
            return Ok(self.report(&record, Vec::new()));
        }
        let process = self.process_for(&record)?;
        let mut ran = Vec::new();
        let Some(state) = record.run.node(node).map(|state| state.status) else {
            return Ok(self.report(&record, ran));
        };
        if state != NodeStatus::Running {
            return Ok(self.report(&record, ran));
        }
        let attempt = record.attempt(node).map_or_else(
            || record.run.node(node).map_or(1, |state| state.attempts),
            |a| a.attempt,
        );
        let started_at = record
            .attempt(node)
            .map_or_else(|| Instant::from_offset(now), |a| a.started_at);
        let stored = Attempt {
            node: node.clone(),
            attempt,
            state: AttemptState::Done,
            output: payload.clone(),
            branches: Vec::new(),
            failure: None,
            started_at,
            not_before: None,
            is_retry: record.attempt(node).is_some_and(|a| a.is_retry),
        };
        self.commit(&mut record, CaseUpdate::new().with_attempt(stored))
            .await?;
        self.apply_done(&mut record, &process, node, payload, Vec::new(), now)
            .await?;
        self.timers.clear(&record.run.id, node);
        ran.push(node.clone());
        Ok(self.report(&record, ran))
    }

    /// Cancels a Case and its child Cases, clearing its timers.
    ///
    /// # Errors
    /// [`EngineError::CaseNotFound`] or a store failure.
    pub fn cancel<'a>(
        &'a self,
        id: &'a CaseId,
        now: OffsetDateTime,
    ) -> Pin<Box<dyn Future<Output = EngineResult<CaseRecord>> + Send + 'a>> {
        Box::pin(async move {
            let mut record = self.load(id).await?;
            if record.run.status != RunStatus::Running {
                return Ok(record);
            }
            for link in record.children.values() {
                let child = link.child.clone();
                self.cancel(&child, now).await?;
            }
            let version = record.run.version;
            let transition = dag_cancel(&record.run, now).map_err(map_exec)?;
            let case = record.run.id.clone();
            record = self
                .store
                .commit(
                    &case,
                    version,
                    CaseUpdate::new().with_transition(transition),
                )
                .await?;
            self.timers.cancel_case(&record.run.id);
            Ok(record)
        })
    }

    /// Steps a Case until it finishes or a turn makes no progress.
    ///
    /// # Errors
    /// A store failure, a missing process or handler, or an injected crash.
    pub async fn run_until_idle(
        &self,
        id: &CaseId,
        now: OffsetDateTime,
        max_turns: usize,
    ) -> EngineResult<CaseRecord> {
        for _ in 0..max_turns {
            let report = self.step(id, now).await?;
            if report.finished {
                break;
            }
            if !report.made_progress() {
                break;
            }
        }
        self.load(id).await
    }

    /// The process a Case pins, checked against its content hash.
    fn process_for(&self, record: &CaseRecord) -> EngineResult<Arc<ProcessDefinition>> {
        let key = record.run.dag_key.clone();
        let version = record.run.dag_version;
        let definition =
            self.processes
                .get(&key, version)
                .ok_or_else(|| EngineError::ProcessNotFound {
                    key: key.clone(),
                    version,
                })?;
        if definition.compiled.dag.hash().to_string() != record.run.content_hash {
            return Err(EngineError::ProcessMismatch { key, version });
        }
        Ok(definition)
    }

    fn report(&self, record: &CaseRecord, ran: Vec<NodeId>) -> StepReport {
        StepReport {
            case: record.run.id.clone(),
            ran,
            finished: record.run.is_finished(),
            status: record.run.status,
        }
    }

    /// Commits an update against the Case's current version.
    pub(crate) async fn commit(
        &self,
        record: &mut CaseRecord,
        update: CaseUpdate,
    ) -> EngineResult<()> {
        let id = record.run.id.clone();
        let version = record.run.version;
        *record = self.store.commit(&id, version, update).await?;
        Ok(())
    }
}

/// A convenience alias for an engine over the in-memory store and timer port.
pub type InMemoryEngine = Engine<InMemoryCaseStore, InMemoryTimerPort>;

/// Builds an in-memory engine with the default handler set.
#[must_use]
pub fn in_memory_engine() -> InMemoryEngine {
    Engine::new(
        InMemoryCaseStore::new(),
        InMemoryTimerPort::new(),
        HandlerRegistry::logic_default(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn effect_points_render() {
        let point = EffectPoint::BeforeHandler {
            case: CaseId::new("case1").unwrap(),
            node: NodeId::new("n1").unwrap(),
            attempt: 2,
        };
        assert_eq!(point.label(), "before_handler:case1:n1:2");
    }
}
