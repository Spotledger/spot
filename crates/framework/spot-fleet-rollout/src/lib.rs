//! Fleet rollout for migrations (WP-MIG-12.T1, ADR-22 and ADR-062).
//!
//! The migration lifecycle decides what one site applies. This crate decides how a **fleet** of
//! sites applies it: a small canary first, then widening waves, each with a soak and a success
//! criterion; a failure anywhere on the canary pauses the whole fleet; an operator resumes
//! explicitly or aborts. Alongside the rollout it owns the **migration epoch** of one tenant
//! network and the version-skew rule an edge must pass before it may sync.
//!
//! # Pieces
//!
//! - [`plan`]: [`RolloutPlan`], [`Wave`], [`Selector`], [`WaveSize`], [`Soak`],
//!   [`SuccessCriteria`], [`PauseOn`] and the deterministic resolution of a [`Fleet`] into
//!   [`ResolvedWave`]s for a [`Seed`]. Selectors filter by tags and groups, then take a
//!   percentage; the schedule is reproducible.
//! - [`state`]: the [`Rollout`] state machine. [`Rollout::advance`] returns an [`Action`]
//!   (`StartWave`, `Soak`, `Pause`, `Complete`, `Abort`) from observations and a caller-supplied
//!   [`Moment`]. A canary failure always pauses everything and records the node and reason; a
//!   pause never ends on its own, only through [`Rollout::resume`], which restarts the soak.
//!   Waves never overlap unless the wave declares [`Overlap::WithPrevious`].
//! - [`epoch`]: [`Epoch`], [`EpochLedger`] (`node -> (epoch, verified_at)`) and the pure
//!   [`may_sync`] rule. An edge with no verified epoch, an edge ahead of the hub, or an edge
//!   beyond the network's [`SkewLimit`] is refused. This is the data ADR-066's `Hello`
//!   negotiation will consume; the wire negotiation is not built here.
//! - [`migrate`]: the [`NodeMigrator`] seam the apply engine adapts, and [`run_nodes`], which
//!   dispatches a wave in batches bounded by [`ConcurrencyLimit`].
//! - [`sim`]: a seeded 500-node simulator with injected failures at a random node and wave. It
//!   fails the run unless a failure pauses the fleet within the same tick, no later wave starts,
//!   and no unverified or over-skewed node is allowed to sync.
//!
//! # The acceptance, in one sentence each
//!
//! * **A failure at a canary halts the fleet.** [`state::PauseReason::CanaryFailure`] is
//!   produced regardless of [`PauseOn::any_failure`]; the simulator checks no later wave starts.
//! * **Edges never sync against an unverified epoch.** [`EpochLedger::may_sync`] maps "no
//!   record" to [`EdgeEpoch::Unverified`] and [`may_sync`] refuses it; the simulator checks
//!   every unverified node is refused.
//!
//! # Rules this crate keeps
//!
//! No wall clock (time is a [`Moment`] argument), no unseeded randomness, no float arithmetic,
//! no `unsafe`, no network and no storage. The plan, observations, ledger and results are plain
//! serde values for a console or the apply engine to persist.
//!
//! # Example
//!
//! ```
//! use spot_fleet_rollout::{
//!     Epoch, EpochLedger, Fleet, FleetNode, NodeId, Percent, ReleaseId, Rollout, RolloutPlan,
//!     Seed, Selector, Soak, Wave, WaveSize, Observations, Moment, Action,
//! };
//!
//! let fleet = Fleet::new(vec![
//!     FleetNode::new(NodeId::new("edge#1").unwrap(), "north"),
//!     FleetNode::new(NodeId::new("edge#2").unwrap(), "north"),
//! ]);
//! let plan = RolloutPlan::new(
//!     ReleaseId::new("2026.10.1").unwrap(),
//!     Epoch::new(1),
//!     vec![Wave::new("canary", Selector::all(), WaveSize::Nodes(1), Soak::from_secs(1)).as_canary()],
//! );
//! let mut rollout = Rollout::new(plan, &fleet, Seed::new(3)).unwrap();
//! assert!(matches!(
//!     rollout.advance(&Observations::new(), Moment::ZERO),
//!     Action::StartWave { canary: true, .. }
//! ));
//!
//! let mut ledger = EpochLedger::new();
//! ledger.advance().unwrap();
//! ledger.record_at_current(NodeId::new("edge#1").unwrap(), Moment::ZERO).unwrap();
//! assert!(ledger.may_sync(&NodeId::new("edge#1").unwrap(), Epoch::new(1), Default::default()).is_ok());
//! ```

#![warn(missing_docs)]

pub mod epoch;
pub mod error;
pub mod ids;
pub mod migrate;
pub mod plan;
pub mod sim;
pub mod state;

pub use epoch::{EdgeEpoch, Epoch, EpochLedger, EpochRecord, SkewLimit, SkewRefusal, may_sync};
pub use error::{Error, MigrateError, codes};
pub use ids::{Moment, NodeId, Percent, ReleaseId, Seed, Soak};
pub use migrate::{MigrateOutcome, MigrateRequest, NodeMigration, NodeMigrator, run_nodes};
pub use plan::{
    ConcurrencyLimit, Fleet, FleetNode, Health, Overlap, PauseOn, ResolvedWave, RolloutPlan,
    Selector, SuccessCriteria, Wave, WaveSize,
};
pub use sim::{FailureInjection, SimConfig, SimReport, run, run_seeds};
pub use state::{
    Action, FailureKind, FailureRecord, NodeFailure, NodeObservation, Observations, PauseReason,
    PauseRecord, Rollout, State,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-fleet-rollout";
