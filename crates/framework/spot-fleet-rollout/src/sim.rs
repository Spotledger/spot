//! The fleet simulator: 500 nodes, a seeded canary rollout, injected failures at a random node
//! and wave, and the two acceptance invariants checked mechanically.
//!
//! [`run`] builds a fleet, plans four waves, drives [`Rollout`] against a [`NodeMigrator`],
//! records every verified node in the [`EpochLedger`], and returns a [`SimReport`]. It fails
//! with [`Error::SimulationFailed`] when:
//!
//! * a failure did not pause the fleet on the same tick;
//! * a wave after the failed one started;
//! * a canary failure started anything past the canary;
//! * any node without a verified epoch was admitted by the skew rule;
//! * any node further behind than the skew limit was admitted.
//!
//! The run is deterministic for one [`SimConfig`]: the same seed, fleet and injection always
//! produce the same schedule, failure and report.

use std::collections::BTreeMap;

use crate::epoch::{Epoch, EpochLedger, SkewLimit, SkewRefusal};
use crate::error::{Error, MigrateError};
use crate::ids::{Moment, NodeId, Percent, ReleaseId, Seed, Soak, SplitMix64};
use crate::migrate::{MigrateOutcome, MigrateRequest, NodeMigrator, run_nodes};
use crate::plan::{
    ConcurrencyLimit, Fleet, FleetNode, Health, PauseOn, RolloutPlan, Selector, Wave, WaveSize,
};
use crate::state::{
    Action, FailureKind, NodeFailure, NodeObservation, Observations, PauseRecord, Rollout,
};

/// The simulator's inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimConfig {
    /// How many nodes the fleet has. Default 500.
    pub nodes: usize,
    /// How many waves the plan has, including the canary. Default 4.
    pub waves: usize,
    /// The seed for the schedule and the injected failure.
    pub seed: Seed,
    /// How much logical time one idle step advances. Default one second.
    pub tick_millis: u64,
    /// Whether to inject a failure at all.
    pub inject_failure: bool,
    /// The wave to fail, or a random one when `None`.
    pub failure_wave: Option<usize>,
    /// The index within that wave's nodes to fail, or a random one when `None`.
    pub failure_node: Option<usize>,
    /// How many offline nodes sit behind the skew limit in the ledger. Default 3.
    pub lagging_nodes: usize,
    /// The epoch the simulated release is at. Default 5.
    pub epoch: Epoch,
    /// The network's skew limit. Default 1.
    pub skew_limit: SkewLimit,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            nodes: 500,
            waves: 4,
            seed: Seed::new(1),
            tick_millis: 1_000,
            inject_failure: true,
            failure_wave: None,
            failure_node: None,
            lagging_nodes: 3,
            epoch: Epoch::new(5),
            skew_limit: SkewLimit::new(1),
        }
    }
}

/// The node and wave the simulator failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureInjection {
    /// The wave index.
    pub wave: usize,
    /// The node that failed.
    pub node: NodeId,
}

/// What one simulated rollout did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimReport {
    /// The inputs.
    pub config: SimConfig,
    /// The injected failure, when there was one.
    pub failure: Option<FailureInjection>,
    /// The waves started, in order.
    pub waves_started: Vec<usize>,
    /// The pause that ended the run, when it paused.
    pub pause: Option<PauseRecord>,
    /// Whether every wave passed.
    pub completed: bool,
    /// How many nodes the skew rule admitted.
    pub admitted: usize,
    /// How many nodes were refused because they never verified.
    pub refused_unverified: usize,
    /// How many nodes were refused because they were beyond the skew limit.
    pub refused_skew: usize,
    /// How many nodes the run considered (fleet plus offline).
    pub considered: usize,
}

/// Runs one simulated rollout.
///
/// # Errors
///
/// [`Error::SimulationFailed`] when the acceptance invariants do not hold, and the plan,
/// ledger or identifier errors for bad inputs.
pub fn run(config: SimConfig) -> Result<SimReport, Error> {
    if config.nodes == 0 {
        return Err(Error::InvalidPlan {
            reason: "the simulated fleet has no nodes".to_owned(),
        });
    }
    if config.waves == 0 {
        return Err(Error::InvalidPlan {
            reason: "the simulated plan has no waves".to_owned(),
        });
    }

    let fleet = build_fleet(config.nodes)?;
    let plan = build_plan(&config)?;
    let schedule = plan.resolve(&fleet, config.seed)?;
    let failure = choose_failure(&config, &schedule)?;

    let hub_epoch = config.epoch;
    let mut ledger = EpochLedger::new();
    while ledger.current() < hub_epoch {
        ledger.advance()?;
    }
    if ledger.current() != hub_epoch {
        return Err(Error::SimulationFailed {
            message: format!(
                "the ledger is at epoch {} but the release is at {hub_epoch}",
                ledger.current()
            ),
        });
    }
    let offline = seed_lagging_nodes(&mut ledger, &config)?;

    let release = plan.release.clone();
    let epoch = plan.epoch;
    let limit = plan.concurrency;
    let mut rollout = Rollout::new(plan, &fleet, config.seed)?;

    let mut now = Moment::ZERO;
    let mut snapshot: BTreeMap<NodeId, NodeObservation> = BTreeMap::new();
    let mut pending_failures: Vec<NodeFailure> = Vec::new();
    let mut waves_started = Vec::new();
    let mut pause = None;
    let mut completed = false;

    let max_steps = (config.waves as u64).saturating_add(2).saturating_mul(16) + 16;
    for _ in 0..max_steps {
        let observations = Observations {
            nodes: snapshot.values().cloned().collect(),
            failures: pending_failures.clone(),
        };
        pending_failures.clear();
        match rollout.advance(&observations, now) {
            Action::StartWave { index, .. } => {
                waves_started.push(index);
                let nodes = rollout
                    .schedule()
                    .get(index)
                    .map(|wave| wave.nodes.clone())
                    .ok_or_else(|| Error::SimulationFailed {
                        message: format!("the state machine started unknown wave {index}"),
                    })?;
                let mut migrator = SimMigrator {
                    failure: failure.clone(),
                };
                let results = run_nodes(&mut migrator, &release, epoch, &nodes, limit);
                let mut failures_now = Vec::new();
                for result in results {
                    match result.outcome {
                        Ok(outcome) if outcome.verified => {
                            snapshot.insert(
                                result.node.clone(),
                                NodeObservation::healthy(result.node.clone()),
                            );
                            ledger.record_verified(result.node.clone(), outcome.epoch, now)?;
                        }
                        Ok(outcome) => {
                            snapshot.insert(
                                result.node.clone(),
                                NodeObservation::new(result.node.clone(), Health::Green, 0, false),
                            );
                            failures_now.push(NodeFailure::new(
                                result.node,
                                FailureKind::Verify,
                                outcome
                                    .detail
                                    .unwrap_or_else(|| "verify did not pass".to_owned()),
                            ));
                        }
                        Err(error) => failures_now.push(NodeFailure::new(
                            result.node,
                            FailureKind::Migrate,
                            error.message().to_owned(),
                        )),
                    }
                }
                if !failures_now.is_empty() {
                    let at_failure = Observations {
                        nodes: snapshot.values().cloned().collect(),
                        failures: failures_now,
                    };
                    match rollout.advance(&at_failure, now) {
                        Action::Pause { pause: recorded } => {
                            pause = Some(recorded);
                            break;
                        }
                        other => {
                            return Err(Error::SimulationFailed {
                                message: format!(
                                    "a failure did not pause the fleet on the same tick: {other:?}"
                                ),
                            });
                        }
                    }
                }
            }
            Action::Soak { .. } => {
                now = now.saturating_add(config.tick_millis);
            }
            Action::Pause { pause: recorded } => {
                pause = Some(recorded);
                break;
            }
            Action::Complete { .. } => {
                completed = true;
                break;
            }
            Action::Resume { .. } | Action::Abort { .. } => {
                return Err(Error::SimulationFailed {
                    message: "advance returned a terminal or operator action".to_owned(),
                });
            }
        }
    }

    check_outcome(&config, &failure, &waves_started, pause.as_ref(), completed)?;
    let (admitted, refused_unverified, refused_skew) =
        check_skew(&ledger, hub_epoch, config.skew_limit, &fleet, &offline)?;

    Ok(SimReport {
        config,
        failure,
        waves_started,
        pause,
        completed,
        admitted,
        refused_unverified,
        refused_skew,
        considered: fleet.len() + offline.len(),
    })
}

/// Runs the same configuration over several seeds.
///
/// # Errors
///
/// The first seed's error.
pub fn run_seeds(config: &SimConfig, seeds: &[u64]) -> Result<Vec<SimReport>, Error> {
    seeds
        .iter()
        .map(|seed| {
            let mut next = config.clone();
            next.seed = Seed::new(*seed);
            run(next)
        })
        .collect()
}

fn build_fleet(count: usize) -> Result<Fleet, Error> {
    let mut nodes = Vec::with_capacity(count);
    for index in 0..count {
        let id = NodeId::new(format!("node-{index:04}"))?;
        let mut node = FleetNode::new(id, format!("group-{}", index % 10)).with_tag("retail");
        if index % 7 == 0 {
            node = node.with_tag("warehouse");
        }
        nodes.push(node);
    }
    Ok(Fleet::new(nodes))
}

fn build_plan(config: &SimConfig) -> Result<RolloutPlan, Error> {
    let release = ReleaseId::new(format!("sim-{}", config.seed.get()))?;
    let mut waves = Vec::with_capacity(config.waves);
    waves.push(
        Wave::new(
            "canary",
            Selector::all(),
            WaveSize::Nodes(5),
            Soak::from_secs(1),
        )
        .as_canary(),
    );
    for index in 1..config.waves {
        let percentage = match index {
            1 => 10,
            2 => 50,
            _ => 100,
        };
        waves.push(Wave::new(
            format!("wave-{index}"),
            Selector::all(),
            WaveSize::Percent(Percent::new(percentage)?),
            Soak::from_secs(1),
        ));
    }
    let mut plan = RolloutPlan::new(release, config.epoch, waves);
    plan.pause_on = PauseOn::default();
    plan.skew_limit = config.skew_limit;
    plan.concurrency = ConcurrencyLimit::new(4)?;
    Ok(plan)
}

fn choose_failure(
    config: &SimConfig,
    schedule: &[crate::plan::ResolvedWave],
) -> Result<Option<FailureInjection>, Error> {
    if !config.inject_failure {
        return Ok(None);
    }
    let mut generator = SplitMix64::new(Seed::new(config.seed.get() ^ 0x5A17_2E11_0B1E_900D));
    let wave = match config.failure_wave {
        Some(wave) if wave < schedule.len() => wave,
        Some(wave) => {
            return Err(Error::SimulationFailed {
                message: format!("wave {wave} is outside the plan"),
            });
        }
        None => usize::try_from(generator.below(schedule.len() as u64)).unwrap_or(0),
    };
    let candidates = schedule
        .get(wave)
        .map(|resolved| &resolved.nodes)
        .ok_or_else(|| Error::SimulationFailed {
            message: format!("the resolved plan has no wave {wave}"),
        })?;
    if candidates.is_empty() {
        return Err(Error::SimulationFailed {
            message: format!("wave {wave} has no node to fail"),
        });
    }
    let index = match config.failure_node {
        Some(index) if index < candidates.len() => index,
        Some(index) => {
            return Err(Error::SimulationFailed {
                message: format!("node index {index} is outside wave {wave}"),
            });
        }
        None => usize::try_from(generator.below(candidates.len() as u64)).unwrap_or(0),
    };
    let node = candidates
        .get(index)
        .cloned()
        .ok_or_else(|| Error::SimulationFailed {
            message: format!("wave {wave} has no node at index {index}"),
        })?;
    Ok(Some(FailureInjection { wave, node }))
}

fn seed_lagging_nodes(ledger: &mut EpochLedger, config: &SimConfig) -> Result<Vec<NodeId>, Error> {
    let stale = Epoch::new(config.epoch.get().saturating_sub(2));
    let mut offline = Vec::with_capacity(config.lagging_nodes);
    for index in 0..config.lagging_nodes {
        let node = NodeId::new(format!("offline-{index:02}"))?;
        ledger.record_verified(node.clone(), stale, Moment::ZERO)?;
        offline.push(node);
    }
    Ok(offline)
}

fn check_outcome(
    config: &SimConfig,
    failure: &Option<FailureInjection>,
    waves_started: &[usize],
    pause: Option<&PauseRecord>,
    completed: bool,
) -> Result<(), Error> {
    match failure {
        Some(injection) => {
            let recorded = pause.ok_or_else(|| Error::SimulationFailed {
                message: format!(
                    "the injected failure at {} did not pause the fleet",
                    injection.node
                ),
            })?;
            if recorded.node.as_ref() != Some(&injection.node) {
                return Err(Error::SimulationFailed {
                    message: format!(
                        "the pause names {:?}, not the failed node {}",
                        recorded.node, injection.node
                    ),
                });
            }
            if recorded.wave != Some(injection.wave) {
                return Err(Error::SimulationFailed {
                    message: format!(
                        "the pause names wave {:?}, not the failed wave {}",
                        recorded.wave, injection.wave
                    ),
                });
            }
            if waves_started.iter().any(|wave| *wave > injection.wave) {
                return Err(Error::SimulationFailed {
                    message: format!(
                        "a wave after the failed wave {} started: {waves_started:?}",
                        injection.wave
                    ),
                });
            }
            if injection.wave == 0
                && (waves_started.len() != 1 || waves_started.first() != Some(&0))
            {
                return Err(Error::SimulationFailed {
                    message: format!("a canary failure did not halt the fleet: {waves_started:?}"),
                });
            }
        }
        None => {
            if !completed || pause.is_some() {
                return Err(Error::SimulationFailed {
                    message: "a clean run did not complete".to_owned(),
                });
            }
            if waves_started.len() != config.waves {
                return Err(Error::SimulationFailed {
                    message: format!(
                        "a clean run started {} of {} waves",
                        waves_started.len(),
                        config.waves
                    ),
                });
            }
        }
    }
    Ok(())
}

fn check_skew(
    ledger: &EpochLedger,
    hub_epoch: Epoch,
    limit: SkewLimit,
    fleet: &Fleet,
    offline: &[NodeId],
) -> Result<(usize, usize, usize), Error> {
    let ids: Vec<NodeId> = fleet
        .nodes()
        .iter()
        .map(|node| node.id.clone())
        .chain(offline.iter().cloned())
        .collect();
    let mut admitted = 0;
    let mut refused_unverified = 0;
    let mut refused_skew = 0;
    for id in &ids {
        let record = ledger.record(id);
        match ledger.may_sync(id, hub_epoch, limit) {
            Ok(()) => {
                let record = record.ok_or_else(|| Error::SimulationFailed {
                    message: format!("an unverified node {id} was admitted"),
                })?;
                if hub_epoch.get() - record.epoch.get() > u64::from(limit.get()) {
                    return Err(Error::SimulationFailed {
                        message: format!("node {id} is past the skew limit but was admitted"),
                    });
                }
                admitted += 1;
            }
            Err(SkewRefusal::Unverified) => {
                if record.is_some() {
                    return Err(Error::SimulationFailed {
                        message: format!("verified node {id} was refused as unverified"),
                    });
                }
                refused_unverified += 1;
            }
            Err(SkewRefusal::BeyondLimit { edge, .. }) => {
                if hub_epoch.get() - edge.get() <= u64::from(limit.get()) {
                    return Err(Error::SimulationFailed {
                        message: format!("node {id} is within skew but was refused"),
                    });
                }
                refused_skew += 1;
            }
            Err(SkewRefusal::EdgeAhead { .. }) => {
                return Err(Error::SimulationFailed {
                    message: format!("the ledger admitted a future epoch for {id}"),
                });
            }
        }
    }
    if admitted + refused_unverified + refused_skew != ids.len() {
        return Err(Error::SimulationFailed {
            message: "the skew counts do not cover every node".to_owned(),
        });
    }
    Ok((admitted, refused_unverified, refused_skew))
}

struct SimMigrator {
    failure: Option<FailureInjection>,
}

impl NodeMigrator for SimMigrator {
    fn migrate(&mut self, request: &MigrateRequest) -> Result<MigrateOutcome, MigrateError> {
        if self
            .failure
            .as_ref()
            .is_some_and(|failure| failure.node == request.node)
        {
            return Err(MigrateError::new("injected migration failure"));
        }
        Ok(MigrateOutcome::verified(request.epoch))
    }
}
