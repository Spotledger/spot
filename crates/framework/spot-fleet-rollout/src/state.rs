//! The rollout state machine: canary waves, automatic fleet-wide pause, explicit resume and
//! abort.
//!
//! [`Rollout::advance`] is a pure transition over observations and a caller-supplied
//! [`Moment`]. It never reads the wall clock, never resumes by itself and never starts a later
//! wave while a failure is outstanding:
//!
//! 1. a failure on any canary node pauses the whole fleet, whatever [`PauseOn`] says;
//! 2. a failure on a started non-canary wave pauses when [`PauseOn::any_failure`] is set;
//! 3. the fleet-wide error-rate and health thresholds pause outside the active wave too;
//! 4. a wave passes only when its own [`SuccessCriteria`] hold for every node **and** its soak
//!    has elapsed;
//! 5. waves never overlap unless the wave declares [`Overlap::WithPrevious`].
//!
//! A pause records which node and why. Only [`Rollout::resume`] clears it; it restarts the soak
//! of every started wave and acknowledges the failures the operator has seen, so the same
//! failure cannot silently re-pause the moment the operator resumes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::ids::{Moment, NodeId, Seed};
use crate::plan::{Fleet, Health, Overlap, ResolvedWave, RolloutPlan};

/// The kind of failure a node reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The migration itself failed.
    Migrate,
    /// The migration finished but verify did not pass.
    Verify,
    /// The node's health turned red.
    Health,
    /// The node crashed.
    Crash,
    /// The node did not answer in time.
    Timeout,
    /// A failure the caller could not classify.
    Other,
}

impl FailureKind {
    /// The stable lower-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Migrate => "migrate",
            Self::Verify => "verify",
            Self::Health => "health",
            Self::Crash => "crash",
            Self::Timeout => "timeout",
            Self::Other => "other",
        }
    }
}

/// A failure reported by one node. `detail` is an operator-facing sentence that must be safe to
/// log; it never carries a secret or a payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeFailure {
    /// The node.
    pub node: NodeId,
    /// What went wrong.
    pub kind: FailureKind,
    /// Why, in a sentence.
    pub detail: String,
}

impl NodeFailure {
    /// A failure report.
    #[must_use]
    pub fn new(node: NodeId, kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            node,
            kind,
            detail: detail.into(),
        }
    }
}

/// One node's current signal, as the telemetry health surface and the rollout's own counters
/// report it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeObservation {
    /// The node.
    pub node: NodeId,
    /// The three-colour health.
    pub health: Health,
    /// The error rate in integer permille.
    pub error_rate_permille: u32,
    /// Whether the node finished verify for the release.
    pub verified: bool,
}

impl NodeObservation {
    /// A verified, green, error-free observation.
    #[must_use]
    pub fn healthy(node: NodeId) -> Self {
        Self {
            node,
            health: Health::Green,
            error_rate_permille: 0,
            verified: true,
        }
    }

    /// A verified observation with a health and an error rate.
    #[must_use]
    pub fn new(node: NodeId, health: Health, error_rate_permille: u32, verified: bool) -> Self {
        Self {
            node,
            health,
            error_rate_permille,
            verified,
        }
    }

    /// Replace the health.
    #[must_use]
    pub fn with_health(mut self, health: Health) -> Self {
        self.health = health;
        self
    }

    /// Replace the error rate.
    #[must_use]
    pub fn with_error_rate(mut self, error_rate_permille: u32) -> Self {
        self.error_rate_permille = error_rate_permille;
        self
    }
}

/// One tick of observations.
///
/// `nodes` is the current snapshot of the nodes the caller tracks. `failures` are failures
/// **new since the previous call**: the rollout keeps an active set until an operator resumes,
/// so a caller reporting each event once is enough. Re-reporting an identical failure is
/// harmless (it is deduplicated while active).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observations {
    /// The current node signals.
    pub nodes: Vec<NodeObservation>,
    /// Failures observed since the previous call.
    pub failures: Vec<NodeFailure>,
}

impl Observations {
    /// No observations.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node signal.
    #[must_use]
    pub fn observe(mut self, observation: NodeObservation) -> Self {
        self.nodes.push(observation);
        self
    }

    /// Add a failure.
    #[must_use]
    pub fn fail(mut self, failure: NodeFailure) -> Self {
        self.failures.push(failure);
        self
    }
}

/// Why the rollout paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// A node reported a migration, crash, timeout or other failure.
    NodeFailure,
    /// A node in the canary wave failed. This always pauses the whole fleet.
    CanaryFailure,
    /// A node's error rate crossed a threshold.
    ErrorRate,
    /// A node's health crossed a threshold.
    Health,
    /// A node did not finish verify where the criteria required it.
    VerifyFailed,
    /// The soak elapsed but a node had not reported, so the criteria could not be shown to hold.
    MissingObservation,
    /// An operator paused the rollout.
    Manual,
    /// The state machine found a state its rules do not explain, so it failed closed.
    Invariant,
}

impl PauseReason {
    /// The stable lower-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NodeFailure => "node_failure",
            Self::CanaryFailure => "canary_failure",
            Self::ErrorRate => "error_rate",
            Self::Health => "health",
            Self::VerifyFailed => "verify_failed",
            Self::MissingObservation => "missing_observation",
            Self::Manual => "manual",
            Self::Invariant => "invariant",
        }
    }
}

/// A recorded pause: when, why, and which node and wave it names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseRecord {
    /// The reading at which the pause was recorded.
    pub at: Moment,
    /// The reason.
    pub reason: PauseReason,
    /// The node that caused it, when one does.
    pub node: Option<NodeId>,
    /// The wave that caused it, when one does.
    pub wave: Option<usize>,
    /// A sentence for the operator.
    pub detail: String,
}

/// A failure as the rollout recorded it, with the wave the node belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    /// The node.
    pub node: NodeId,
    /// The wave the node is scheduled in, when it is in the plan.
    pub wave: Option<usize>,
    /// What went wrong.
    pub kind: FailureKind,
    /// Why, in a sentence.
    pub detail: String,
}

/// What [`Rollout::advance`] tells the caller to do next.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum Action {
    /// Start this wave: dispatch its node migrations.
    StartWave {
        /// The wave index.
        index: usize,
        /// The wave name.
        name: String,
        /// How many nodes it takes.
        nodes: usize,
        /// Whether it is the canary.
        canary: bool,
    },
    /// Wait: the wave is soaking and healthy so far.
    Soak {
        /// The wave being soaked.
        index: usize,
        /// The reading at which the soak ends.
        until: Moment,
    },
    /// The whole fleet is paused. Nothing starts until an explicit resume.
    Pause {
        /// The recorded pause.
        pause: PauseRecord,
    },
    /// An explicit resume was accepted; the named wave's soak restarted.
    Resume {
        /// The wave whose soak restarted.
        index: usize,
    },
    /// Every wave passed; the release is verified across the fleet.
    Complete {
        /// How many waves were run.
        waves: usize,
    },
    /// The rollout was aborted by an operator and will not continue.
    Abort {
        /// Why.
        reason: String,
    },
}

/// The rollout's coarse state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// No wave started yet.
    NotStarted,
    /// At least one wave started and none is paused.
    Rolling,
    /// Paused by a failure, a threshold or an operator.
    Paused,
    /// Every wave passed.
    Complete,
    /// Aborted by an operator.
    Aborted,
}

#[derive(Clone, Copy, Debug, Default)]
struct WaveProgress {
    started_at: Option<Moment>,
    passed: bool,
}

enum Criteria {
    Passed,
    Pending {
        missing: NodeId,
    },
    Failed {
        node: NodeId,
        reason: PauseReason,
        detail: String,
    },
}

/// The rollout state machine over one plan, fleet and seed.
#[derive(Clone, Debug)]
pub struct Rollout {
    plan: RolloutPlan,
    schedule: Vec<ResolvedWave>,
    waves: Vec<WaveProgress>,
    active_failures: Vec<FailureRecord>,
    failure_log: Vec<FailureRecord>,
    pause: Option<PauseRecord>,
    pause_log: Vec<PauseRecord>,
    aborted: Option<String>,
    resumes: u32,
}

impl Rollout {
    /// A rollout for `plan` over `fleet`, with node order fixed by `seed`.
    ///
    /// # Errors
    ///
    /// The plan or its resolution errors from [`RolloutPlan::resolve`].
    pub fn new(plan: RolloutPlan, fleet: &Fleet, seed: Seed) -> Result<Self, Error> {
        let schedule = plan.resolve(fleet, seed)?;
        let waves = schedule.iter().map(|_| WaveProgress::default()).collect();
        Ok(Self {
            plan,
            schedule,
            waves,
            active_failures: Vec::new(),
            failure_log: Vec::new(),
            pause: None,
            pause_log: Vec::new(),
            aborted: None,
            resumes: 0,
        })
    }

    /// The plan.
    #[must_use]
    pub fn plan(&self) -> &RolloutPlan {
        &self.plan
    }

    /// The resolved schedule.
    #[must_use]
    pub fn schedule(&self) -> &[ResolvedWave] {
        &self.schedule
    }

    /// The current pause, when paused.
    #[must_use]
    pub fn pause(&self) -> Option<&PauseRecord> {
        self.pause.as_ref()
    }

    /// Every pause the rollout recorded, including ones an operator resumed.
    #[must_use]
    pub fn pause_log(&self) -> &[PauseRecord] {
        &self.pause_log
    }

    /// Every failure the rollout recorded.
    #[must_use]
    pub fn failures(&self) -> &[FailureRecord] {
        &self.failure_log
    }

    /// How many explicit resumes happened.
    #[must_use]
    pub fn resumes(&self) -> u32 {
        self.resumes
    }

    /// The coarse state.
    #[must_use]
    pub fn state(&self) -> State {
        if self.aborted.is_some() {
            return State::Aborted;
        }
        if self.pause.is_some() {
            return State::Paused;
        }
        if self.all_passed() {
            return State::Complete;
        }
        if self.waves.iter().any(|wave| wave.started_at.is_some()) {
            State::Rolling
        } else {
            State::NotStarted
        }
    }

    /// Advances the rollout by one tick.
    ///
    /// The transition order is documented in the module: failures first, then the fleet-wide
    /// thresholds, then the active waves' criteria, then completion, then the next wave.
    pub fn advance(&mut self, observations: &Observations, now: Moment) -> Action {
        if let Some(reason) = &self.aborted {
            return Action::Abort {
                reason: reason.clone(),
            };
        }
        if let Some(pause) = &self.pause {
            return Action::Pause {
                pause: pause.clone(),
            };
        }

        self.absorb(observations);

        if let Some(pause) = self.failure_pause(now) {
            return self.set_pause(pause);
        }
        if let Some(pause) = self.threshold_pause(observations, now) {
            return self.set_pause(pause);
        }

        let by_node: BTreeMap<NodeId, &NodeObservation> = observations
            .nodes
            .iter()
            .map(|observation| (observation.node.clone(), observation))
            .collect();
        let active: Vec<usize> = self
            .waves
            .iter()
            .enumerate()
            .filter(|(_, wave)| wave.started_at.is_some() && !wave.passed)
            .map(|(index, _)| index)
            .collect();

        for index in &active {
            if let Some(pause) = self.wave_pause(*index, &by_node, now) {
                return self.set_pause(pause);
            }
        }

        for index in &active {
            if self.soak_elapsed(*index, now)
                && matches!(self.evaluate_wave(*index, &by_node), Criteria::Passed)
                && let Some(wave) = self.waves.get_mut(*index)
            {
                wave.passed = true;
            }
        }

        if self.all_passed() {
            return Action::Complete {
                waves: self.schedule.len(),
            };
        }

        if let Some(index) = self.next_startable() {
            if let Some(wave) = self.waves.get_mut(index) {
                wave.started_at = Some(now);
            }
            if let Some(resolved) = self.schedule.get(index) {
                return Action::StartWave {
                    index,
                    name: resolved.name.clone(),
                    nodes: resolved.nodes.len(),
                    canary: resolved.canary,
                };
            }
        }

        if let Some(index) = active.first().copied() {
            let started = self
                .waves
                .get(index)
                .and_then(|wave| wave.started_at)
                .unwrap_or(now);
            let soak = self
                .plan
                .waves
                .get(index)
                .map_or(0, |wave| wave.soak.millis());
            return Action::Soak {
                index,
                until: started.saturating_add(soak),
            };
        }

        Action::Pause {
            pause: PauseRecord {
                at: now,
                reason: PauseReason::Invariant,
                node: None,
                wave: None,
                detail: "the rollout has nothing to start and no wave to soak".to_owned(),
            },
        }
    }

    /// Pauses the rollout explicitly, for an operator holding a rollout that is otherwise
    /// healthy.
    ///
    /// The pause is idempotent: pausing a paused rollout returns the existing record.
    pub fn pause_now(&mut self, now: Moment, reason: impl Into<String>) -> Action {
        if let Some(pause) = &self.pause {
            return Action::Pause {
                pause: pause.clone(),
            };
        }
        let pause = PauseRecord {
            at: now,
            reason: PauseReason::Manual,
            node: None,
            wave: None,
            detail: reason.into(),
        };
        self.set_pause(pause)
    }

    /// Resumes a paused rollout. This is the **only** way a pause ends.
    ///
    /// The operator's acknowledgement covers the failures the pause recorded: the active
    /// failure set is cleared and every started, unpassed wave's soak restarts at `now`, so a
    /// resume never counts time spent paused as healthy soak.
    ///
    /// # Errors
    ///
    /// [`Error::NotPaused`] when the rollout is not paused.
    pub fn resume(&mut self, now: Moment) -> Result<Action, Error> {
        let pause = self.pause.take().ok_or(Error::NotPaused)?;
        self.pause_log.push(pause);
        self.active_failures.clear();
        self.resumes = self.resumes.saturating_add(1);

        let mut resumed = None;
        for (index, wave) in self.waves.iter_mut().enumerate() {
            if wave.started_at.is_some() && !wave.passed {
                wave.started_at = Some(now);
                if resumed.is_none() {
                    resumed = Some(index);
                }
            }
        }
        let index = resumed.or_else(|| self.next_startable()).unwrap_or(0);
        Ok(Action::Resume { index })
    }

    /// Aborts the rollout. Terminal.
    ///
    /// # Errors
    ///
    /// [`Error::NotRunning`] when every wave already passed. An already-aborted rollout returns
    /// its original reason.
    pub fn abort(&mut self, reason: impl Into<String>) -> Result<Action, Error> {
        if let Some(reason) = &self.aborted {
            return Ok(Action::Abort {
                reason: reason.clone(),
            });
        }
        if self.all_passed() {
            return Err(Error::NotRunning);
        }
        let reason = reason.into();
        self.aborted = Some(reason.clone());
        Ok(Action::Abort { reason })
    }

    fn set_pause(&mut self, pause: PauseRecord) -> Action {
        self.pause = Some(pause.clone());
        Action::Pause { pause }
    }

    fn absorb(&mut self, observations: &Observations) {
        for failure in &observations.failures {
            let duplicate = self.active_failures.iter().any(|record| {
                record.node == failure.node
                    && record.kind == failure.kind
                    && record.detail == failure.detail
            });
            if duplicate {
                continue;
            }
            let record = FailureRecord {
                node: failure.node.clone(),
                wave: self.wave_of(&failure.node),
                kind: failure.kind,
                detail: failure.detail.clone(),
            };
            self.active_failures.push(record.clone());
            self.failure_log.push(record);
        }
    }

    fn wave_of(&self, node: &NodeId) -> Option<usize> {
        self.schedule
            .iter()
            .find(|wave| wave.nodes.iter().any(|candidate| candidate == node))
            .map(|wave| wave.index)
    }

    fn failure_pause(&self, now: Moment) -> Option<PauseRecord> {
        for failure in &self.active_failures {
            let canary = failure
                .wave
                .and_then(|index| self.schedule.get(index))
                .is_some_and(|wave| wave.canary);
            if canary {
                return Some(PauseRecord {
                    at: now,
                    reason: PauseReason::CanaryFailure,
                    node: Some(failure.node.clone()),
                    wave: failure.wave,
                    detail: format!(
                        "canary node {} failed ({}): {}",
                        failure.node,
                        failure.kind.as_str(),
                        failure.detail
                    ),
                });
            }
            if self.plan.pause_on.any_failure {
                let started = failure.wave.is_none_or(|index| {
                    self.waves
                        .get(index)
                        .is_some_and(|wave| wave.started_at.is_some())
                });
                if started {
                    return Some(PauseRecord {
                        at: now,
                        reason: PauseReason::NodeFailure,
                        node: Some(failure.node.clone()),
                        wave: failure.wave,
                        detail: format!(
                            "node {} failed ({}): {}",
                            failure.node,
                            failure.kind.as_str(),
                            failure.detail
                        ),
                    });
                }
            }
        }
        None
    }

    fn threshold_pause(&self, observations: &Observations, now: Moment) -> Option<PauseRecord> {
        for observation in &observations.nodes {
            let wave = self.wave_of(&observation.node);
            let relevant = wave.is_some_and(|index| {
                self.waves
                    .get(index)
                    .is_some_and(|progress| progress.started_at.is_some())
                    || self
                        .schedule
                        .get(index)
                        .is_some_and(|resolved| resolved.canary)
            });
            if !relevant {
                continue;
            }
            if observation.error_rate_permille > self.plan.pause_on.max_error_rate_permille {
                return Some(PauseRecord {
                    at: now,
                    reason: PauseReason::ErrorRate,
                    node: Some(observation.node.clone()),
                    wave,
                    detail: format!(
                        "node {} reports {} permille errors, above the pause threshold of {}",
                        observation.node,
                        observation.error_rate_permille,
                        self.plan.pause_on.max_error_rate_permille
                    ),
                });
            }
            if observation.health < self.plan.pause_on.min_health {
                return Some(PauseRecord {
                    at: now,
                    reason: PauseReason::Health,
                    node: Some(observation.node.clone()),
                    wave,
                    detail: format!(
                        "node {} is {:?}, below the pause threshold of {:?}",
                        observation.node, observation.health, self.plan.pause_on.min_health
                    ),
                });
            }
        }
        None
    }

    fn wave_pause(
        &self,
        index: usize,
        by_node: &BTreeMap<NodeId, &NodeObservation>,
        now: Moment,
    ) -> Option<PauseRecord> {
        match self.evaluate_wave(index, by_node) {
            Criteria::Passed => None,
            Criteria::Failed {
                node,
                reason,
                detail,
            } => Some(PauseRecord {
                at: now,
                reason,
                node: Some(node),
                wave: Some(index),
                detail,
            }),
            Criteria::Pending { missing } => {
                if self.soak_elapsed(index, now) {
                    Some(PauseRecord {
                        at: now,
                        reason: PauseReason::MissingObservation,
                        node: Some(missing.clone()),
                        wave: Some(index),
                        detail: format!(
                            "the soak of wave {index} elapsed before node {missing} reported"
                        ),
                    })
                } else {
                    None
                }
            }
        }
    }

    fn evaluate_wave(
        &self,
        index: usize,
        by_node: &BTreeMap<NodeId, &NodeObservation>,
    ) -> Criteria {
        let (Some(wave), Some(resolved)) = (self.plan.waves.get(index), self.schedule.get(index))
        else {
            return Criteria::Passed;
        };
        let mut missing = None;
        for node in &resolved.nodes {
            let Some(observation) = by_node.get(node) else {
                if missing.is_none() {
                    missing = Some(node.clone());
                }
                continue;
            };
            if wave.success_criteria.require_verified && !observation.verified {
                return Criteria::Failed {
                    node: node.clone(),
                    reason: PauseReason::VerifyFailed,
                    detail: format!("node {node} did not verify"),
                };
            }
            if observation.error_rate_permille > wave.success_criteria.max_error_rate_permille {
                return Criteria::Failed {
                    node: node.clone(),
                    reason: PauseReason::ErrorRate,
                    detail: format!(
                        "node {node} reports {} permille errors, above the wave threshold of {}",
                        observation.error_rate_permille,
                        wave.success_criteria.max_error_rate_permille
                    ),
                };
            }
            if observation.health < wave.success_criteria.min_health {
                return Criteria::Failed {
                    node: node.clone(),
                    reason: PauseReason::Health,
                    detail: format!(
                        "node {node} is {:?}, below the wave threshold of {:?}",
                        observation.health, wave.success_criteria.min_health
                    ),
                };
            }
        }
        match missing {
            Some(node) => Criteria::Pending { missing: node },
            None => Criteria::Passed,
        }
    }

    fn soak_elapsed(&self, index: usize, now: Moment) -> bool {
        let Some(started) = self.waves.get(index).and_then(|wave| wave.started_at) else {
            return false;
        };
        let soak = self
            .plan
            .waves
            .get(index)
            .map_or(0, |wave| wave.soak.millis());
        now.elapsed_since(started) >= soak
    }

    fn next_startable(&self) -> Option<usize> {
        for index in 0..self.schedule.len() {
            let progress = self.waves.get(index)?;
            if progress.started_at.is_some() {
                continue;
            }
            if index == 0 {
                return Some(0);
            }
            let previous = self.waves.get(index - 1);
            let previous_passed = previous.is_some_and(|wave| wave.passed);
            let previous_started = previous.is_some_and(|wave| wave.started_at.is_some());
            let overlaps = self
                .plan
                .waves
                .get(index)
                .is_some_and(|wave| wave.overlap == Overlap::WithPrevious);
            if previous_passed || (overlaps && previous_started) {
                return Some(index);
            }
            return None;
        }
        None
    }

    fn all_passed(&self) -> bool {
        !self.waves.is_empty() && self.waves.iter().all(|wave| wave.passed)
    }
}
