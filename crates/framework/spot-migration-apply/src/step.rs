//! Steps and the seam that runs them.
//!
//! The engine never runs a schema change, a metadata publication, a patch or a behaviour
//! activation itself. It asks a [`StepRunner`] for the shape of each [`ApplyStep`] (whether it is
//! transactional, whether re-running it is safe, how long it may take) and then hands the step to
//! it. The real runner (T2) drives `spot-pg-migrate` and the metadata and patch services; tests
//! use the in-memory runner of the `testing` feature.
//!
//! # Idempotency
//!
//! A step's idempotency is **declared** by its runner. The engine never runs a step whose
//! idempotency is undeclared (`None`) or declared `AtMostOnce` a second time: when a restart
//! finds such a step started but not completed it restores the snapshot instead of resuming.

use std::time::Duration;

use spot_core::SiteId;
use spot_migration_plan::ApplyStep;

use crate::BoxFuture;
use crate::error::Result;

/// Whether re-running a step is safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Idempotency {
    /// Running the step again has the same effect as running it once.
    Idempotent,
    /// The step must not be run twice; the engine restores instead of resuming it.
    AtMostOnce,
}

/// What the runner knows about one step.
#[derive(Clone, Debug)]
pub struct StepDescriptor {
    /// The step's zero-based index in the plan.
    pub index: usize,
    /// The step itself.
    pub step: ApplyStep,
    /// Whether the step runs inside one transaction that the database aborts on failure.
    pub transactional: bool,
    /// The declared idempotency; `None` means undeclared and is treated as `AtMostOnce`.
    pub idempotency: Option<Idempotency>,
    /// The per-step timeout; `None` uses the engine's default.
    pub timeout: Option<Duration>,
}

impl StepDescriptor {
    /// Whether a restart may re-run this step after making it clean.
    ///
    /// Only a declared [`Idempotency::Idempotent`] step may be re-run; an undeclared or
    /// `AtMostOnce` step never is.
    #[must_use]
    pub fn may_rerun(&self) -> bool {
        self.idempotency == Some(Idempotency::Idempotent)
    }
}

/// A checkpoint a step recorded: the key the next run of the same step resumes from.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    /// The checkpoint key (a step-defined, stable string).
    pub key: String,
    /// A one-line, secret-free detail for the journal.
    pub detail: String,
}

/// A compensating restore point a non-transactional step recorded before it ran.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestorePoint {
    /// The restore point's id, understood by [`StepRunner::rollback`].
    pub id: String,
    /// A one-line, secret-free detail for the journal.
    pub detail: String,
}

/// What a successful step did.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StepOutcome {
    /// The checkpoint the step recorded, if any.
    pub checkpoint: Option<Checkpoint>,
    /// A one-line, secret-free detail for the journal and progress.
    pub detail: String,
}

/// What a step may look at: the site, the plan digest and the migration the hold uses.
///
/// It is owned so the engine can build one per step without lifetime plumbing.
#[derive(Clone, Debug)]
pub struct StepContext {
    /// The site being migrated.
    pub site: SiteId,
    /// The digest of the plan being applied.
    pub plan_digest: String,
    /// The migration id the side-effect hold uses.
    pub migration_id: String,
    /// Unix microseconds; the engine reads no clock.
    pub now: i64,
}

impl StepContext {
    /// A context for one step.
    #[must_use]
    pub fn new(
        site: &SiteId,
        plan_digest: impl Into<String>,
        migration_id: impl Into<String>,
        now: i64,
    ) -> Self {
        Self {
            site: site.clone(),
            plan_digest: plan_digest.into(),
            migration_id: migration_id.into(),
            now,
        }
    }
}

/// Runs the steps of a plan.
///
/// `describe` is pure and cheap; `restore_point` is called **before** a non-transactional step so
/// the engine can journal the compensating point; `run` performs the step; `rollback` undoes a
/// non-transactional step up to a restore point during recovery.
pub trait StepRunner: Send + Sync {
    /// The shape of the step at `index`.
    fn describe(&self, index: usize, step: &ApplyStep) -> StepDescriptor;

    /// A compensating restore point for a non-transactional step, computed before it runs.
    ///
    /// A transactional step returns `None`: its transaction is its own rollback.
    fn restore_point(&self, descriptor: &StepDescriptor) -> Option<RestorePoint>;

    /// Runs `descriptor`.
    ///
    /// # Errors
    /// A typed error, or [`Error::Killed`](crate::Error::Killed) from a fault-injection runner.
    fn run<'a>(
        &'a self,
        descriptor: &'a StepDescriptor,
        ctx: &'a StepContext,
    ) -> BoxFuture<'a, Result<StepOutcome>>;

    /// Undoes a non-transactional step up to `restore_point`.
    ///
    /// # Errors
    /// A typed error when the compensating action fails.
    fn rollback<'a>(
        &'a self,
        restore_point: &'a RestorePoint,
        ctx: &'a StepContext,
    ) -> BoxFuture<'a, Result<()>>;
}
