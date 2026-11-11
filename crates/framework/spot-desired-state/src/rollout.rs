//! The bulk rollout: the wave/pause state machine of `spot-fleet-rollout`, through a trait.
//!
//! [`Rollout`] is the seam the run drives. [`FleetRollout`] adapts `spot-fleet-rollout`'s
//! state machine (WP-MIG-12) unchanged: waves select by group, tags and percentage, the canary
//! is first, a canary failure pauses the whole fleet, a wave above the failure-rate threshold
//! pauses, and only an explicit [`Rollout::resume`] ends a pause. This crate adds no second
//! wave scheduler.

use spot_fleet_rollout::Rollout as WaveMachine;

use crate::error::Error;

pub use spot_fleet_rollout::{
    Action as WaveAction, ConcurrencyLimit, Epoch, FailureKind, Fleet, FleetNode, Health, Moment,
    NodeFailure, NodeId, NodeObservation, Observations, Overlap, PauseOn, PauseReason, PauseRecord,
    Percent, ReleaseId, ResolvedWave, RolloutPlan, Seed, Selector, Soak, State as WaveState,
    SuccessCriteria, Wave, WaveSize,
};

/// The wave state machine a bulk run drives.
pub trait Rollout {
    /// The rollout plan (release, epoch, waves and pause policy).
    fn plan(&self) -> &RolloutPlan;

    /// The resolved waves, in start order.
    fn schedule(&self) -> &[ResolvedWave];

    /// The coarse state.
    fn state(&self) -> WaveState;

    /// The current pause, when paused.
    fn pause(&self) -> Option<&PauseRecord>;

    /// Advances one tick from the observations the caller collected.
    fn advance(&mut self, observations: &Observations, now: Moment) -> WaveAction;

    /// Resumes a paused rollout. The only way a pause ends.
    ///
    /// # Errors
    ///
    /// [`Error::Rollout`] when the rollout is not paused.
    fn resume(&mut self, now: Moment) -> Result<WaveAction, Error>;

    /// Aborts the rollout. Terminal.
    ///
    /// # Errors
    ///
    /// [`Error::Rollout`] when the rollout already completed.
    fn abort(&mut self, reason: &str) -> Result<WaveAction, Error>;
}

/// The `spot-fleet-rollout` state machine behind the [`Rollout`] trait.
#[derive(Clone, Debug)]
pub struct FleetRollout {
    inner: WaveMachine,
}

impl FleetRollout {
    /// A rollout for `plan` over `fleet`, with node order fixed by `seed`.
    ///
    /// # Errors
    ///
    /// [`Error::Rollout`] when the plan is structurally invalid or a wave selects no node.
    pub fn new(plan: RolloutPlan, fleet: &Fleet, seed: Seed) -> Result<Self, Error> {
        Ok(Self {
            inner: WaveMachine::new(plan, fleet, seed)?,
        })
    }
}

impl Rollout for FleetRollout {
    fn plan(&self) -> &RolloutPlan {
        self.inner.plan()
    }

    fn schedule(&self) -> &[ResolvedWave] {
        self.inner.schedule()
    }

    fn state(&self) -> WaveState {
        self.inner.state()
    }

    fn pause(&self) -> Option<&PauseRecord> {
        self.inner.pause()
    }

    fn advance(&mut self, observations: &Observations, now: Moment) -> WaveAction {
        self.inner.advance(observations, now)
    }

    fn resume(&mut self, now: Moment) -> Result<WaveAction, Error> {
        self.inner.resume(now).map_err(Error::from)
    }

    fn abort(&mut self, reason: &str) -> Result<WaveAction, Error> {
        self.inner.abort(reason).map_err(Error::from)
    }
}
