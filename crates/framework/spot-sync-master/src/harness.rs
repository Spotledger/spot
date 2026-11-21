//! The 100-branch convergence harness (ADR-062 section 6, ADR-065 section 6).
//!
//! The acceptance for WP-EDG-04 is: **a price change reaches 100 simulated branches
//! consistently**, including branches that are offline when it is published and online when it
//! becomes effective. The harness proves it in two steps, both over the real
//! [`spot-sync-protocol`] state machines and its seeded, fault-injecting network:
//!
//! 1. **Delivery.** One run with [`DEFAULT_BRANCHES`] edges over a lossy, duplicating,
//!    reordering, partitioning network: every branch applies the whole price-list stream exactly
//!    once (ADR-062 G2 and G4 at fleet scale).
//! 2. **Activation by clock.** A price-list update is published with an `effective_from` in the
//!    future. Each branch's own arrival is the tick at which its consumer converged, from its own
//!    single-branch run with its own faults. A branch that was offline when the update was
//!    published but online before it took effect has it stored and in force at the effective
//!    instant; a branch offline **past** the effective instant did not, is reported [`late`] with
//!    its delay, and never shows the new price before it is effective (ADR-065 section 6).
//!
//! The harness reads no clock and no ambient randomness: the sim is a pure function of its seed,
//! and the effective instant is computed from the lead the caller passes in.
//!
//! [`late`]: BranchOutcome::late

use std::time::Duration;

use spot_sync_protocol::sim::{self, Direction, NetworkConfig, SimConfig};
use spot_sync_protocol::{NodeId, RecordName};

use crate::change::{ActiveView, EffectiveChange, MasterChange, StoreOutcome};
use crate::error::{Error, Result};
use crate::scope::Doctype;
use crate::subscription::{Router, Subscription};
use crate::time::Instant;

/// The branch count the acceptance criterion names.
pub const DEFAULT_BRANCHES: usize = 100;

/// The document type the harness propagates.
pub const PRICE_ENTITY: &str = "price_list";

/// The record name the harness propagates.
pub const PRICE_NAME: &str = "PL-1";

/// The version in force before the update.
pub const PRIOR_VERSION: u64 = 1;

/// The version the update publishes.
pub const UPDATE_VERSION: u64 = 2;

/// The prior price payload.
pub const PRIOR_PRICE: &str = "90.00";

/// The updated price payload.
pub const NEW_PRICE: &str = "100.00";

/// The harness's parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessConfig {
    /// How many branches take part.
    pub branches: usize,
    /// The seed base; branch `i` runs with seed `seed + i`.
    pub seed: u64,
    /// How many records the hub publishes in the price-list stream.
    pub stream_records: usize,
    /// The lead time: how far after publication the update becomes effective.
    pub lead: Duration,
    /// The simulated network's faults.
    pub network: NetworkConfig,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            branches: DEFAULT_BRANCHES,
            seed: 1,
            stream_records: 8,
            lead: Duration::from_millis(60),
            network: NetworkConfig {
                drop_permille: 100,
                duplicate_permille: 50,
                latency: 1,
                jitter: 5,
                partition_permille: 60,
                partition_ticks: 40,
                chaos_every: 10,
            },
        }
    }
}

/// What one branch did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchOutcome {
    /// The branch node.
    pub node: NodeId,
    /// When the branch's consumer converged: the update's arrival instant.
    pub arrived_at: Instant,
    /// Whether it arrived after the update's effective instant.
    pub late: bool,
    /// How late it was (zero when on time).
    pub lateness: Duration,
    /// The payload in force at the effective instant (`None` when no version was in force).
    pub view_at_effective: Option<String>,
}

/// The harness's verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvergenceReport {
    /// How many branches took part.
    pub branches: usize,
    /// How many records the fleet run published.
    pub produced: usize,
    /// How many branches had the update stored and in force at the effective instant.
    pub on_time: usize,
    /// How many branches were offline past the effective instant.
    pub late: usize,
    /// Whether every on-time branch showed the same price at the effective instant.
    pub identical_views: bool,
    /// How many branches showed the new price before it was effective (must be zero).
    pub applied_early: usize,
    /// The greatest lateness among the late branches.
    pub max_lateness: Duration,
    /// How many messages the fleet run's network dropped.
    pub dropped: u64,
    /// One entry per branch, in branch order.
    pub outcomes: Vec<BranchOutcome>,
}

impl ConvergenceReport {
    /// The branches that were offline past the effective instant.
    pub fn late_branches(&self) -> impl Iterator<Item = &BranchOutcome> {
        self.outcomes.iter().filter(|outcome| outcome.late)
    }
}

/// Runs the harness and checks its invariants, returning the verdict.
///
/// Fails with [`error::HARNESS_FAILED`](crate::error::HARNESS_FAILED) when a simulated branch
/// does not converge, when a time overflows, or when an invariant is violated: every on-time
/// branch shows the new price at the effective instant, every late branch shows the prior price,
/// and no branch shows the new price before it is effective.
pub fn run(config: &HarnessConfig) -> Result<ConvergenceReport> {
    if config.branches == 0 {
        return Err(Error::invalid_argument(
            "the harness needs at least one branch",
        ));
    }
    if config.stream_records == 0 {
        return Err(Error::invalid_argument(
            "the harness publishes at least one record",
        ));
    }
    if config.lead.is_zero() {
        return Err(Error::invalid_argument(
            "the harness's lead time is positive: activation by clock needs an instant ahead of publication",
        ));
    }

    let entity = Doctype::new(PRICE_ENTITY)?;
    let name = RecordName::new(PRICE_NAME).map_err(|error| {
        Error::harness_failed(format!("the harness's record name is invalid: {error}"))
    })?;
    let prior = MasterChange::new(entity.clone(), name.clone(), PRIOR_VERSION, [], PRIOR_PRICE)?;
    let update = MasterChange::new(entity.clone(), name.clone(), UPDATE_VERSION, [], NEW_PRICE)?;

    // The routing check: every branch subscribes to the entity, so all of them are destinations.
    let subscriptions: Vec<Subscription> = (0..config.branches)
        .map(|index| {
            let node = NodeId::new(&format!("edge-{index}")).map_err(|error| {
                Error::harness_failed(format!("a branch node id is invalid: {error}"))
            })?;
            Ok(Subscription::new(node, [], [entity.clone()]))
        })
        .collect::<Result<Vec<_>>>()?;
    let router = Router::from_subscriptions(subscriptions)?;
    let destinations = router.destinations(&update);
    if destinations.len() != config.branches {
        return Err(Error::harness_failed(format!(
            "routing reaches {} branches, not {}",
            destinations.len(),
            config.branches
        )));
    }

    let publish_at = Instant::from_unix_millis(0);
    let effective_from = publish_at
        .checked_add(config.lead)
        .ok_or_else(|| Error::harness_failed("the effective instant overflowed"))?;

    // Delivery: the whole fleet converges under random outages.
    let fleet = sim::run(&sim_config(config, config.seed, config.branches)).map_err(|error| {
        Error::harness_failed(format!("the fleet run did not converge: {error}"))
    })?;
    if fleet.produced != config.stream_records
        || fleet
            .applied
            .iter()
            .any(|&applied| applied != config.stream_records)
    {
        return Err(Error::harness_failed(format!(
            "the fleet run applied {:?} of {} records",
            fleet.applied, config.stream_records
        )));
    }

    let prior_effective = EffectiveChange::new(prior, publish_at);
    let mut outcomes = Vec::with_capacity(config.branches);
    let mut on_time = 0usize;
    let mut late = 0usize;
    let mut applied_early = 0usize;
    let mut max_lateness = Duration::ZERO;

    for index in 0..config.branches {
        let branch =
            sim::run(&sim_config(config, config.seed + index as u64, 1)).map_err(|error| {
                Error::harness_failed(format!("branch {index} did not converge: {error}"))
            })?;
        let arrived_at = publish_at
            .checked_add(Duration::from_millis(branch.ticks))
            .ok_or_else(|| Error::harness_failed("a branch's arrival instant overflowed"))?;

        let mut view = ActiveView::new(entity.clone(), name.clone());
        view.store(prior_effective.clone(), publish_at)?;
        let outcome = view.store(
            EffectiveChange::new(update.clone(), effective_from),
            arrived_at,
        )?;
        let late_here = matches!(outcome, StoreOutcome::Stored { late: true, .. });
        let lateness = match outcome {
            StoreOutcome::Stored { delay, .. } => delay,
            StoreOutcome::Duplicate => Duration::ZERO,
        };

        let view_at_effective = view
            .at(effective_from)
            .map(|active| active.change().payload().to_owned());
        // Nothing may show the new price before it is effective, however early it arrived.
        if view
            .at(publish_at)
            .is_some_and(|active| active.change().payload() == NEW_PRICE)
        {
            applied_early += 1;
        }

        if late_here {
            late += 1;
            max_lateness = max_lateness.max(lateness);
            if view_at_effective.as_deref() != Some(PRIOR_PRICE) {
                return Err(Error::harness_failed(format!(
                    "late branch {index} did not show the prior price at the effective instant"
                )));
            }
        } else {
            on_time += 1;
            if view_at_effective.as_deref() != Some(NEW_PRICE) {
                return Err(Error::harness_failed(format!(
                    "on-time branch {index} did not show the new price at the effective instant"
                )));
            }
        }

        outcomes.push(BranchOutcome {
            node: NodeId::new(&format!("edge-{index}")).map_err(|error| {
                Error::harness_failed(format!("a branch node id is invalid: {error}"))
            })?,
            arrived_at,
            late: late_here,
            lateness,
            view_at_effective,
        });
    }

    if applied_early != 0 {
        return Err(Error::harness_failed(format!(
            "{applied_early} branches applied the update before it was effective"
        )));
    }

    Ok(ConvergenceReport {
        branches: config.branches,
        produced: fleet.produced,
        on_time,
        late,
        identical_views: on_time > 0,
        applied_early,
        max_lateness,
        dropped: fleet.dropped,
        outcomes,
    })
}

/// The simulator config for one run: `edges` branches pulling one hub stream.
fn sim_config(config: &HarnessConfig, seed: u64, edges: usize) -> SimConfig {
    SimConfig {
        seed,
        direction: Direction::Pull,
        site: "head-office".to_owned(),
        sources: 1,
        edges,
        records: config.stream_records,
        versions_per_name: 1,
        max_batch: 8,
        window: 16,
        produce_every: 1,
        poll_every: 30,
        retry_every: 20,
        crash_permille: 30,
        store_fail_every: 11,
        network: config.network,
        max_ticks: 100_000,
    }
}
