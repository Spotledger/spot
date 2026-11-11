//! The step runner over `spot-pg-migrate` (WP-STO-11).
//!
//! [`PgStepRunner`] answers the engine's questions about each step and runs it. The schema
//! work is **never re-implemented here**: every step is one call to
//! [`spot_pg_migrate::apply`](spot_pg_migrate::apply), which takes the session advisory lock,
//! re-reads the live schema, re-plans against the catalogue and does only what is left. That
//! re-plan is what makes a step resumable: a crash leaves the schema at a point the planner can
//! diff again, and the next call performs the next remaining step.
//!
//! # One step per engine step
//!
//! The runner is prepared with the target [`Catalog`] and the engine's plan, and it reads the
//! live schema to learn which planner step comes next. Each `run` executes **exactly one**
//! remaining planner step by passing `stop_after_step = Some(0)`; the planner's own
//! `migrate.interrupted` outcome then means "that step committed, stop here". The last schema
//! step runs the rest (the planner's `finalise` included) so the schema is complete before
//! verification.
//!
//! # Transactional and non-transactional DDL
//!
//! PostgreSQL has transactional DDL, so a step that `spot-pg-migrate` runs in a transaction
//! (`Run::Tx`, `Run::SetNotNull`, the planner's `finalise`) commits or aborts as one unit. The
//! statements that PostgreSQL **cannot** run in a transaction are exactly:
//!
//! - `CREATE INDEX CONCURRENTLY` (the planner's `Run::Outside` for an index on an existing
//!   table);
//! - `DROP INDEX CONCURRENTLY` (the planner's cleanup of an invalid index);
//! - the batched backfill, whose batches each commit on their own by design.
//!
//! Those steps are declared non-transactional. The engine records a compensating restore point
//! before each of them ([`StepRunner::restore_point`]) and, after a crash, calls
//! [`StepRunner::rollback`], which drops an index a failed concurrent build left invalid and
//! re-runs nothing else. A backfill needs no compensation: it only fills NULLs, and the step
//! after it makes the column required only when none are left.
//!
//! # Non-schema steps
//!
//! `PublishMetadata`, `Patch` and `ActivateBehaviour` are the metadata and patch services'
//! work, not `spot-pg-migrate`'s. The runner delegates them to a [`NonSchemaStep`] handler the
//! host wires; without one it refuses with `migration_apply_pg.step_unsupported` rather than
//! pretending the step happened.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_core::{Clock, SiteId};
use spot_migration_apply::step::{
    Checkpoint, Idempotency, RestorePoint, StepContext, StepDescriptor, StepOutcome, StepRunner,
};
use spot_migration_apply::{BoxFuture, Error as ApplyError, Result as ApplyResult};
use spot_migration_plan::{ApplyStep, ApplyStepKind};
use spot_pg_migrate::{
    ApplyOptions, LiveSchema, MigrateError, MigrationPlan, PlanOptions, Run as MigrateRun,
    plan_with,
};
use spot_postgres::PgBackend;
use spot_storage_contract::Catalog;

use crate::error::{Error, Result};
use crate::timings::{TimingPhase, TimingRecord, TimingSink};
use crate::verify::PgDriftVerifier;

/// The shape of one engine step, as the runner tells the engine about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepShape {
    /// Whether the step runs in one transaction the database aborts on failure.
    pub transactional: bool,
    /// The declared idempotency.
    pub idempotency: Option<Idempotency>,
}

impl StepShape {
    /// A schema step that `spot-pg-migrate` runs in one transaction.
    const fn transactional() -> Self {
        Self {
            transactional: true,
            idempotency: Some(Idempotency::Idempotent),
        }
    }

    /// A schema step whose statement PostgreSQL cannot run in a transaction.
    const fn concurrent() -> Self {
        Self {
            transactional: false,
            idempotency: Some(Idempotency::Idempotent),
        }
    }

    /// A non-schema step, delegated to the host's handler; its contract is idempotent.
    const fn delegated() -> Self {
        Self {
            transactional: true,
            idempotency: Some(Idempotency::Idempotent),
        }
    }
}

/// Runs the metadata, patch and behaviour steps `spot-pg-migrate` does not own.
///
/// A host wires the real services; a runner without one refuses those steps. Both methods
/// must be idempotent: the engine may run them again after a restart.
pub trait NonSchemaStep: Send + Sync {
    /// Runs one non-schema step.
    ///
    /// # Errors
    /// A typed error when the step fails; the engine then restores the snapshot.
    fn run<'a>(
        &'a self,
        step: &'a ApplyStep,
        ctx: &'a StepContext,
    ) -> BoxFuture<'a, ApplyResult<StepOutcome>>;

    /// Undoes one non-schema step up to `restore_point`.
    ///
    /// # Errors
    /// A typed error when the compensating action fails.
    fn rollback<'a>(
        &'a self,
        restore_point: &'a RestorePoint,
        ctx: &'a StepContext,
    ) -> BoxFuture<'a, ApplyResult<()>>;
}

/// The default handler: every non-schema step is refused as unsupported.
#[derive(Debug, Default)]
pub struct RefuseNonSchemaSteps;

impl NonSchemaStep for RefuseNonSchemaSteps {
    fn run<'a>(
        &'a self,
        step: &'a ApplyStep,
        _ctx: &'a StepContext,
    ) -> BoxFuture<'a, ApplyResult<StepOutcome>> {
        Box::pin(async move {
            Err(ApplyError::StepFailed {
                index: 0,
                id: step.id.clone(),
                reason: Error::StepUnsupported {
                    kind: format!("{:?}", step.kind),
                }
                .to_string(),
            })
        })
    }

    fn rollback<'a>(
        &'a self,
        _restore_point: &'a RestorePoint,
        _ctx: &'a StepContext,
    ) -> BoxFuture<'a, ApplyResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// The PostgreSQL step runner.
pub struct PgStepRunner {
    backend: PgBackend,
    site: SiteId,
    catalog: Catalog,
    options: ApplyOptions,
    plan_options: PlanOptions,
    engine_steps: Vec<ApplyStep>,
    shapes: BTreeMap<usize, StepShape>,
    /// How many of the engine's steps are schema steps.
    schema_steps: usize,
    clock: Arc<dyn Clock>,
    timings: Arc<dyn TimingSink>,
    non_schema: Arc<dyn NonSchemaStep>,
    drift: PgDriftVerifier,
}

impl PgStepRunner {
    /// A runner for `site` over `backend` that moves the schema to `catalog`.
    #[must_use]
    pub fn new(
        backend: PgBackend,
        site: SiteId,
        catalog: Catalog,
        engine_steps: Vec<ApplyStep>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let schema_steps = engine_steps
            .iter()
            .filter(|step| step.kind == ApplyStepKind::Schema)
            .count();
        let mut shapes = BTreeMap::new();
        for (index, step) in engine_steps.iter().enumerate() {
            shapes.insert(index, shape_for(step.kind));
        }
        let drift = PgDriftVerifier::new(backend.clone(), catalog.clone());
        Self {
            backend,
            site,
            catalog,
            options: ApplyOptions::default(),
            plan_options: PlanOptions::default(),
            engine_steps,
            shapes,
            schema_steps,
            clock,
            timings: Arc::new(crate::timings::NoTimings),
            non_schema: Arc::new(RefuseNonSchemaSteps),
            drift,
        }
    }

    /// The same runner with different `spot-pg-migrate` options.
    #[must_use]
    pub fn with_options(mut self, options: ApplyOptions) -> Self {
        self.options = options;
        self
    }

    /// The same runner with different planner options (the defaults a new required column is
    /// filled with).
    #[must_use]
    pub fn with_plan_options(mut self, options: PlanOptions) -> Self {
        self.plan_options = options;
        self.drift = PgDriftVerifier::new(self.backend.clone(), self.catalog.clone())
            .with_plan_options(self.plan_options.clone());
        self
    }

    /// The same runner recording per-step timings.
    #[must_use]
    pub fn with_timings(mut self, timings: Arc<dyn TimingSink>) -> Self {
        self.timings = timings;
        self
    }

    /// The same runner delegating non-schema steps to `handler`.
    #[must_use]
    pub fn with_non_schema(mut self, handler: Arc<dyn NonSchemaStep>) -> Self {
        self.non_schema = handler;
        self
    }

    /// The target catalogue.
    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The site the runner migrates.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The verify gate over the same backend and catalogue.
    #[must_use]
    pub fn verifier(&self) -> &PgDriftVerifier {
        &self.drift
    }

    /// The planner's plan of the work left, read fresh from the database.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the live schema cannot be read.
    pub async fn remaining(&self) -> Result<MigrationPlan> {
        let live = LiveSchema::read(&self.backend).await.map_err(migrate)?;
        Ok(plan_with(&live, &self.catalog, &self.plan_options))
    }

    /// Learns which schema steps are non-transactional by reading the live schema, so the
    /// engine's synchronous `describe` can answer. Call it before the engine starts.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the live schema cannot be read.
    pub async fn prepare(&mut self) -> Result<()> {
        let remaining = self.remaining().await?;
        let mut next = 0usize;
        for (index, step) in self.engine_steps.clone().iter().enumerate() {
            if step.kind != ApplyStepKind::Schema {
                self.shapes.insert(index, StepShape::delegated());
                continue;
            }
            let shape = match remaining.steps.get(next).map(|step| &step.run) {
                Some(MigrateRun::Outside(_) | MigrateRun::Backfill(_)) => StepShape::concurrent(),
                _ => StepShape::transactional(),
            };
            self.shapes.insert(index, shape);
            next += 1;
        }
        Ok(())
    }

    /// Runs one remaining planner step, or the rest on the last schema step.
    async fn run_schema(&self, index: usize, ctx: &StepContext) -> Result<StepOutcome> {
        let schema_seen = self
            .engine_steps
            .iter()
            .take(index + 1)
            .filter(|step| step.kind == ApplyStepKind::Schema)
            .count();
        let last_schema = self.schema_steps > 0 && schema_seen == self.schema_steps;
        let mut options = self.options.clone();
        options.stop_after_step = if last_schema { None } else { Some(0) };
        let remaining = self.remaining().await?;
        if remaining.steps.is_empty() {
            // The schema is already at the target: a restart's resume of a step that committed
            // before the crash completes it without touching anything.
            return Ok(StepOutcome {
                checkpoint: Some(Checkpoint {
                    key: format!("schema:{index}:done"),
                    detail: "the schema already needs no further step".to_owned(),
                }),
                detail: "schema already at the target".to_owned(),
            });
        }
        let started = self.now_micros();
        let result = spot_pg_migrate::apply(&self.backend, &remaining, options).await;
        let elapsed = self.now_micros().saturating_sub(started);
        let report = match result {
            Ok(report) => report,
            // The planner's hook stopped after exactly the one step this call asked for: that
            // is this step's success, not a failure.
            Err(MigrateError::Interrupted { step: 0 }) => spot_pg_migrate::ApplyReport {
                executed: vec![format!("step:{index}")],
                backfilled_rows: 0,
                plan_hash: remaining.plan_hash.clone(),
            },
            Err(error) => {
                return Err(Error::StepFailed {
                    index,
                    id: self
                        .engine_steps
                        .get(index)
                        .map_or_else(|| format!("step-{index}"), |step| step.id.clone()),
                    message: error.to_string(),
                });
            }
        };
        self.timings
            .record(TimingRecord::new(
                TimingPhase::Apply,
                elapsed,
                String::new(),
                ctx.now,
            ))
            .map_err(|error| Error::TimingFailed {
                message: error.to_string(),
            })?;
        Ok(StepOutcome {
            checkpoint: Some(Checkpoint {
                key: format!("schema:{index}:{}", report.plan_hash),
                detail: format!(
                    "{} planner step(s) ran, {} row(s) backfilled",
                    report.executed.len(),
                    report.backfilled_rows
                ),
            }),
            detail: format!("{} planner step(s) applied", report.executed.len()),
        })
    }

    fn now_micros(&self) -> i64 {
        i64::try_from(self.clock.monotonic().as_micros()).unwrap_or(i64::MAX)
    }

    /// Drops every invalid index of the current schema, the leftover of a failed concurrent
    /// build. A valid index is never touched.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the cleanup cannot be run.
    pub async fn drop_invalid_indexes(&self) -> Result<u64> {
        let client =
            crate::schema::backend_client(&self.backend, "dropping invalid indexes").await?;
        let rows = client
            .query(
                "SELECT c.relname::text FROM pg_index i \
                 JOIN pg_class c ON c.oid = i.indexrelid \
                 WHERE NOT i.indisvalid \
                   AND c.relnamespace = to_regnamespace(current_schema())::oid",
                &[],
            )
            .await
            .map_err(|error| Error::database("listing invalid indexes", &error))?;
        let mut dropped = 0u64;
        for row in rows {
            let name: String = row.get(0);
            let quoted = format!("\"{}\"", name.replace('"', "\"\""));
            client
                .batch_execute(&format!("DROP INDEX CONCURRENTLY IF EXISTS {quoted}"))
                .await
                .map_err(|error| Error::database("dropping an invalid index", &error))?;
            dropped += 1;
        }
        Ok(dropped)
    }
}

/// The shape of a step from its kind alone, before the live schema is read.
fn shape_for(kind: ApplyStepKind) -> StepShape {
    match kind {
        ApplyStepKind::Schema => StepShape::transactional(),
        _ => StepShape::delegated(),
    }
}

impl StepRunner for PgStepRunner {
    fn describe(&self, index: usize, step: &ApplyStep) -> StepDescriptor {
        let shape = self
            .shapes
            .get(&index)
            .copied()
            .unwrap_or_else(|| shape_for(step.kind));
        StepDescriptor {
            index,
            step: step.clone(),
            transactional: shape.transactional,
            idempotency: shape.idempotency,
            timeout: None,
        }
    }

    fn restore_point(&self, descriptor: &StepDescriptor) -> Option<RestorePoint> {
        if descriptor.transactional {
            return None;
        }
        Some(RestorePoint {
            id: format!("pg-migrate:invalid-indexes:{}", descriptor.index),
            detail: "drop any index a failed concurrent build left invalid".to_owned(),
        })
    }

    fn run<'a>(
        &'a self,
        descriptor: &'a StepDescriptor,
        ctx: &'a StepContext,
    ) -> BoxFuture<'a, ApplyResult<StepOutcome>> {
        Box::pin(async move {
            if descriptor.index >= self.engine_steps.len() {
                return Err(ApplyError::StepFailed {
                    index: descriptor.index,
                    id: descriptor.step.id.clone(),
                    reason: Error::StepOutOfPlan {
                        index: descriptor.index,
                        steps: self.engine_steps.len(),
                    }
                    .to_string(),
                });
            }
            match descriptor.step.kind {
                ApplyStepKind::Schema => {
                    self.run_schema(descriptor.index, ctx)
                        .await
                        .map_err(|error| ApplyError::StepFailed {
                            index: descriptor.index,
                            id: descriptor.step.id.clone(),
                            reason: error.to_string(),
                        })
                }
                _ => self.non_schema.run(&descriptor.step, ctx).await,
            }
        })
    }

    fn rollback<'a>(
        &'a self,
        restore_point: &'a RestorePoint,
        ctx: &'a StepContext,
    ) -> BoxFuture<'a, ApplyResult<()>> {
        Box::pin(async move {
            if restore_point.id.starts_with("pg-migrate:invalid-indexes:") {
                self.drop_invalid_indexes()
                    .await
                    .map(|_dropped| ())
                    .map_err(|error| ApplyError::RestoreFailed {
                        message: error.to_string(),
                    })
            } else {
                self.non_schema.rollback(restore_point, ctx).await
            }
        })
    }
}

impl std::fmt::Debug for PgStepRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgStepRunner")
            .field("site", &self.site)
            .field("steps", &self.engine_steps.len())
            .field("schema_steps", &self.schema_steps)
            .finish_non_exhaustive()
    }
}

fn migrate(error: MigrateError) -> Error {
    Error::ControlUnavailable {
        operation: "reading the live schema".to_owned(),
        message: error.to_string(),
    }
}
