//! Applying and verifying an approved bulk plan through the wave state machine.
//!
//! A [`BulkRun`] is built only from a [`BulkPlan`], an [`ApprovedPlan`] and a passed
//! [`RehearsalReport`], and the run refuses a proof for a different plan. Each tick asks the
//! [`Rollout`] what to do next:
//!
//! * **StartWave** — apply that wave's steps through the [`StepRunner`] and remember which
//!   servers were touched;
//! * **Soak** — wait for the wave's soak to elapse;
//! * **Pause** — record the halt and stop; no later wave is ever started, because the state
//!   machine itself refuses to start one while paused;
//! * **Complete** — every wave passed and every applied step verified.
//!
//! Applied steps are verified on the next tick; a failed apply or verify becomes a node failure
//! in the next observation, so a canary failure pauses the whole fleet. [`BulkRun::resume`] is
//! the explicit action that clears the pause; it also retries steps that failed to apply, which
//! is what an operator does after fixing the node.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use spot_fleet_rollout::{FailureKind, Moment, NodeFailure, NodeId, NodeObservation, Observations};

use crate::approval::{ApprovalRules, ApprovedPlan};
use crate::error::Error;
use crate::plan::{BulkPlan, PlanStep};
use crate::rehearsal::RehearsalReport;
use crate::rollout::{Rollout, WaveAction, WaveState};
use crate::state::{ObservedState, liveness};

/// Why one step failed on one server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepFailure {
    /// The state-machine failure class.
    pub kind: FailureKind,
    /// A sentence for the operator; it must be safe to log.
    pub detail: String,
}

impl StepFailure {
    /// A failure.
    #[must_use]
    pub fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

/// Applies and verifies one step on one server.
pub trait StepRunner {
    /// Applies `step` to `server`.
    ///
    /// # Errors
    ///
    /// A [`StepFailure`] when the operation could not be applied.
    fn apply(&self, server: &NodeId, step: &PlanStep) -> Result<(), StepFailure>;

    /// Whether `step` is in effect on `server`.
    fn verify(&self, server: &NodeId, step: &PlanStep) -> bool;
}

/// What one tick did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunAction {
    /// The wave started and its steps were applied.
    WaveStarted {
        /// The wave index.
        index: usize,
        /// The wave name.
        name: String,
        /// Whether it is the canary.
        canary: bool,
        /// The servers it took.
        servers: Vec<NodeId>,
    },
    /// The wave is soaking; nothing to do until `until` (milliseconds).
    Soaking {
        /// The wave index.
        index: usize,
        /// The monotonic reading at which the soak ends.
        until: u64,
    },
    /// The fleet is halted; nothing starts until [`BulkRun::resume`].
    Halted {
        /// The recorded pause.
        pause: crate::rollout::PauseRecord,
    },
    /// An explicit resume was accepted.
    Resumed {
        /// The wave whose soak restarted.
        index: usize,
    },
    /// Every wave passed and every applied step verified.
    Complete {
        /// How many waves ran.
        waves: usize,
        /// How many servers the run applied to.
        servers: usize,
    },
    /// The run was aborted.
    Aborted {
        /// Why.
        reason: String,
    },
}

/// The checks the core applies on every run, so an adapter cannot skip them.
#[derive(Clone)]
pub struct RunGuards {
    observed: Arc<dyn ObservedState>,
    silence_deadline: i64,
    approvals: ApprovalRules,
}

impl RunGuards {
    /// Guards that hold a server back while its agent has been silent for more than
    /// `silence_deadline` microseconds, and refuse an approval the `approvals` rules reject.
    #[must_use]
    pub fn new(
        observed: Arc<dyn ObservedState>,
        silence_deadline: i64,
        approvals: ApprovalRules,
    ) -> Self {
        Self {
            observed,
            silence_deadline,
            approvals,
        }
    }
}

impl fmt::Debug for RunGuards {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunGuards")
            .field("silence_deadline", &self.silence_deadline)
            .field("approvals", &self.approvals)
            .finish_non_exhaustive()
    }
}

/// One approved, rehearsed bulk plan being applied and verified.
pub struct BulkRun {
    plan: BulkPlan,
    proof: ApprovedPlan,
    rehearsal: RehearsalReport,
    rollout: Box<dyn Rollout>,
    guards: RunGuards,
    wall: i64,
    steps_by_server: BTreeMap<NodeId, Vec<usize>>,
    applied: BTreeSet<(NodeId, usize)>,
    verified: BTreeSet<(NodeId, usize)>,
    failed: BTreeSet<(NodeId, usize)>,
    pending_failures: Vec<NodeFailure>,
    failure_log: Vec<NodeFailure>,
    waves_started: Vec<usize>,
    retry: bool,
    started_at: Moment,
    finished: Option<RunAction>,
}

impl fmt::Debug for BulkRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BulkRun")
            .field("plan", &self.plan)
            .field("proof", &self.proof)
            .field("rehearsal", &self.rehearsal)
            .field("guards", &self.guards)
            .field("state", &self.rollout.state())
            .field("applied", &self.applied.len())
            .field("verified", &self.verified.len())
            .field("failed", &self.failed.len())
            .field("waves_started", &self.waves_started)
            .field("finished", &self.finished)
            .finish()
    }
}

impl BulkRun {
    /// Starts a run. `wall` is the current unix time in microseconds, for the approval's
    /// freshness; `now` is the state machine's monotonic reading.
    ///
    /// # Errors
    ///
    /// [`Error::EmptyPlan`] for an empty plan, [`Error::PlanChanged`] when the approval is for
    /// another plan, [`Error::NotApproved`] when the approval is stale by `wall`,
    /// [`Error::RehearsalFailed`] when the rehearsal failed or checked another plan, and
    /// [`Error::Rollout`] when the wave schedule is empty or does not start with the canary.
    pub fn start(
        plan: BulkPlan,
        proof: &ApprovedPlan,
        rehearsal: &RehearsalReport,
        rollout: Box<dyn Rollout>,
        guards: RunGuards,
        now: Moment,
        wall: i64,
    ) -> Result<Self, Error> {
        if plan.is_empty() {
            return Err(Error::EmptyPlan);
        }
        let digest = plan.digest();
        if proof.digest() != &digest {
            return Err(Error::PlanChanged);
        }
        proof.ensure_fresh(&guards.approvals, wall)?;
        if !rehearsal.covers(&plan) || !rehearsal.passed() {
            return Err(Error::RehearsalFailed {
                problems: rehearsal.problems().len().max(1),
            });
        }
        let schedule = rollout.schedule();
        if schedule.is_empty() {
            return Err(Error::Rollout {
                message: "the rollout has no wave".to_owned(),
            });
        }
        if !schedule.first().is_some_and(|wave| wave.canary) {
            return Err(Error::Rollout {
                message: "the first wave is not the canary".to_owned(),
            });
        }
        let scheduled: BTreeSet<NodeId> = schedule
            .iter()
            .flat_map(|wave| wave.nodes.iter().cloned())
            .collect();
        for server in plan.servers() {
            if !scheduled.contains(&server) {
                return Err(Error::UnknownServer {
                    server: server.to_string(),
                });
            }
        }

        let mut steps_by_server: BTreeMap<NodeId, Vec<usize>> = BTreeMap::new();
        for (index, step) in plan.steps.iter().enumerate() {
            steps_by_server
                .entry(step.server.clone())
                .or_default()
                .push(index);
        }

        Ok(Self {
            plan,
            proof: proof.clone(),
            rehearsal: rehearsal.clone(),
            rollout,
            guards,
            wall,
            steps_by_server,
            applied: BTreeSet::new(),
            verified: BTreeSet::new(),
            failed: BTreeSet::new(),
            pending_failures: Vec::new(),
            failure_log: Vec::new(),
            waves_started: Vec::new(),
            retry: false,
            started_at: now,
            finished: None,
        })
    }

    /// The plan being run.
    #[must_use]
    pub fn plan(&self) -> &BulkPlan {
        &self.plan
    }

    /// The approval proof the run was started with.
    #[must_use]
    pub fn proof(&self) -> &ApprovedPlan {
        &self.proof
    }

    /// The rehearsal the run was started with.
    #[must_use]
    pub fn rehearsal(&self) -> &RehearsalReport {
        &self.rehearsal
    }

    /// The monotonic reading the run started at.
    #[must_use]
    pub fn started_at(&self) -> Moment {
        self.started_at
    }

    /// The wave machine's coarse state.
    #[must_use]
    pub fn rollout_state(&self) -> WaveState {
        self.rollout.state()
    }

    /// The wave indices started so far, in start order.
    #[must_use]
    pub fn waves_started(&self) -> &[usize] {
        &self.waves_started
    }

    /// The servers at least one step was applied to, in id order.
    #[must_use]
    pub fn applied_servers(&self) -> Vec<NodeId> {
        self.applied
            .iter()
            .map(|(server, _)| server.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// The servers whose every applied step verified, in id order.
    #[must_use]
    pub fn verified_servers(&self) -> Vec<NodeId> {
        self.applied_servers()
            .into_iter()
            .filter(|server| self.server_verified(server))
            .collect()
    }

    /// Every node failure the run recorded, in the order it was found.
    #[must_use]
    pub fn failures(&self) -> &[NodeFailure] {
        &self.failure_log
    }

    /// Whether every applied step on `server` verified.
    fn server_verified(&self, server: &NodeId) -> bool {
        let Some(indexes) = self.steps_by_server.get(server) else {
            return false;
        };
        indexes.iter().all(|index| {
            let key = (server.clone(), *index);
            self.applied.contains(&key) && self.verified.contains(&key)
        })
    }

    /// Advances the run by one tick: verify what was applied, report, and obey the state
    /// machine.
    ///
    /// `wall` is the current unix time in microseconds; a server silent beyond the run's
    /// deadline at `wall` is not ready and is not applied to.
    pub fn tick(&mut self, runner: &dyn StepRunner, now: Moment, wall: i64) -> RunAction {
        self.wall = wall;
        if let Some(finished) = &self.finished {
            return finished.clone();
        }
        if self.retry {
            self.retry = false;
            self.apply_started(runner);
        }
        self.verify_applied(runner);
        let observations = self.observations();
        let action = self.rollout.advance(&observations, now);
        self.obey(action, runner)
    }

    /// Resumes a paused run. Only an explicit call ends a pause.
    ///
    /// # Errors
    ///
    /// [`Error::Rollout`] when the run is not paused.
    pub fn resume(&mut self, now: Moment) -> Result<RunAction, Error> {
        let action = self.rollout.resume(now)?;
        let WaveAction::Resume { index } = action else {
            return Err(Error::Rollout {
                message: "the state machine did not resume".to_owned(),
            });
        };
        self.finished = None;
        self.pending_failures.clear();
        self.failed.clear();
        self.retry = true;
        Ok(RunAction::Resumed { index })
    }

    /// Turns a state-machine action into a run action, applying a wave when it starts.
    fn obey(&mut self, action: WaveAction, runner: &dyn StepRunner) -> RunAction {
        match action {
            WaveAction::StartWave {
                index,
                name,
                canary,
                ..
            } => {
                self.waves_started.push(index);
                let servers = self.schedule_nodes(index);
                self.apply_servers(servers.clone(), runner);
                RunAction::WaveStarted {
                    index,
                    name,
                    canary,
                    servers,
                }
            }
            WaveAction::Soak { index, until } => RunAction::Soaking {
                index,
                until: until.millis(),
            },
            WaveAction::Pause { pause } => {
                let outcome = RunAction::Halted { pause };
                self.finished = Some(outcome.clone());
                outcome
            }
            WaveAction::Resume { index } => RunAction::Resumed { index },
            WaveAction::Complete { waves } => {
                let outcome = RunAction::Complete {
                    waves,
                    servers: self.applied_servers().len(),
                };
                self.finished = Some(outcome.clone());
                outcome
            }
            WaveAction::Abort { reason } => {
                let outcome = RunAction::Aborted { reason };
                self.finished = Some(outcome.clone());
                outcome
            }
        }
    }

    /// The nodes of one scheduled wave.
    fn schedule_nodes(&self, index: usize) -> Vec<NodeId> {
        self.rollout
            .schedule()
            .get(index)
            .map(|wave| wave.nodes.clone())
            .unwrap_or_default()
    }

    /// Applies every not-yet-applied step of every started wave (used on resume).
    fn apply_started(&mut self, runner: &dyn StepRunner) {
        let waves = self.waves_started.clone();
        for index in waves {
            let servers = self.schedule_nodes(index);
            self.apply_servers(servers, runner);
        }
    }

    /// Applies the steps of `servers`; `runner` is the caller's executor.
    fn apply_servers(&mut self, servers: Vec<NodeId>, runner: &dyn StepRunner) {
        for server in servers {
            let Some(indexes) = self.steps_by_server.get(&server).cloned() else {
                continue;
            };
            for index in indexes {
                let key = (server.clone(), index);
                if self.applied.contains(&key) {
                    continue;
                }
                let Some(step) = self.plan.steps.get(index).cloned() else {
                    continue;
                };
                // Not ready: a silent server (never seen, or past the deadline) is held back,
                // and the hold is a failure the state machine sees, so a silent canary pauses
                // the fleet instead of reading as healthy.
                if liveness(
                    self.guards.observed.as_ref(),
                    &server,
                    self.wall,
                    self.guards.silence_deadline,
                )
                .is_silent()
                {
                    self.record_failure(
                        server.clone(),
                        index,
                        FailureKind::Timeout,
                        "the server's agent is silent beyond the deadline; it is not ready",
                    );
                    continue;
                }
                match runner.apply(&server, &step) {
                    Ok(()) => {
                        self.applied.insert(key);
                    }
                    Err(failure) => {
                        self.record_failure(server.clone(), index, failure.kind, failure.detail);
                    }
                }
            }
        }
    }

    /// Verifies applied steps that are not verified or failed yet.
    fn verify_applied(&mut self, runner: &dyn StepRunner) {
        let pending: Vec<(NodeId, usize)> = self
            .applied
            .iter()
            .filter(|key| !self.verified.contains(*key) && !self.failed.contains(*key))
            .cloned()
            .collect();
        for (server, index) in pending {
            let Some(step) = self.plan.steps.get(index).cloned() else {
                continue;
            };
            if runner.verify(&server, &step) {
                self.verified.insert((server, index));
            } else {
                self.record_failure(
                    server,
                    index,
                    FailureKind::Verify,
                    "the step was applied but verify did not pass",
                );
            }
        }
    }

    /// Records one failure once per step, for the next observation.
    fn record_failure(
        &mut self,
        server: NodeId,
        index: usize,
        kind: FailureKind,
        detail: impl Into<String>,
    ) {
        if !self.failed.insert((server.clone(), index)) {
            return;
        }
        let failure = NodeFailure::new(server, kind, detail);
        self.pending_failures.push(failure.clone());
        self.failure_log.push(failure);
    }

    /// The observations the state machine sees: health and verify per touched server, plus the
    /// failures found since the previous tick.
    fn observations(&mut self) -> Observations {
        let mut observations = Observations::new();
        for server in self.applied_servers() {
            let applied = self
                .applied
                .iter()
                .filter(|(candidate, _)| candidate == &server)
                .count();
            let failed = self
                .failed
                .iter()
                .filter(|(candidate, _)| candidate == &server)
                .count();
            let verified = self.server_verified(&server);
            let health = if failed > 0 {
                crate::rollout::Health::Red
            } else {
                crate::rollout::Health::Green
            };
            let error_rate = if applied == 0 {
                0
            } else {
                u32::try_from(failed)
                    .unwrap_or(u32::MAX)
                    .saturating_mul(1_000)
                    / u32::try_from(applied).unwrap_or(1)
            };
            observations =
                observations.observe(NodeObservation::new(server, health, error_rate, verified));
        }
        for failure in std::mem::take(&mut self.pending_failures) {
            observations = observations.fail(failure);
        }
        observations
    }
}
