//! Owned by WP-MIG-11.T2 (audit).
//!
//! The migration audit as lifecycle operations: `list`, `show` and `explain` over the context's
//! [`RunStore`](spot_migration_audit::RunStore), the chain integrity check [`verify`], and
//! [`record_verb`], the wrapper every verb of the lifecycle runs under so that **each verb writes
//! its run record at the start and the end, including on refusal and on error, with the reason
//! code**.
//!
//! # Rules
//!
//! * Every call needs the `migration.run` permission (`migration.not_permitted` otherwise: a
//!   user without the right sees nothing). The stores are scoped to the site's chain, so the runs of
//!   another site are never visible from this context.
//! * The record is evidence: counts, digests and reason codes only, never row data or a secret
//!   (the recorder's events carry digests; this module adds only the verb's name and the code).
//! * Fail closed: [`record_verb`] begins the run **before** the work and refuses to do the work
//!   when the start cannot be recorded (`migration.store_failed`).

use std::future::Future;
use std::sync::Arc;

use spot_migration_apply::BoxFuture;
use spot_migration_audit::{
    ArtifactKind, ArtifactRef, MigrationRun, Outcome, PlanRef, PlanSummary, RunEvidence, RunFilter,
    RunId, RunRecorder, RunSpec,
};
use spot_migration_audit::{FinishEvent, explain as explain_run};
use spot_migration_plan::ReleasePlan;

use crate::catalogue::HandlerRegistry;
use crate::context::LifecycleContext;
use crate::error::LifecycleError;

/// What a chain check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainStatus {
    /// Entries whose hash and link were checked.
    pub checked: u64,
    /// The sequence number of the head.
    pub head: u64,
    /// Each break as `(sequence, stable failure code)`, in order found.
    pub breaks: Vec<(u64, String)>,
}

impl ChainStatus {
    /// Whether the chain holds.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.breaks.is_empty()
    }

    /// The first broken sequence number.
    #[must_use]
    pub fn first_break(&self) -> Option<u64> {
        self.breaks.iter().map(|(seq, _)| *seq).min()
    }
}

/// The host's chain verifier (re-walks the migration chain of the site).
pub trait ChainVerifier: Send + Sync {
    /// Verifies the whole chain.
    ///
    /// # Errors
    /// `migration.store_failed` when the chain cannot be read.
    fn verify(&self) -> BoxFuture<'_, Result<ChainStatus, LifecycleError>>;
}

/// The runs that pass `filter`, oldest first.
///
/// # Errors
/// `migration.not_permitted`; `migration.port_unavailable` without a run store;
/// `migration.store_failed`.
pub async fn list(
    ctx: &LifecycleContext,
    filter: RunFilter,
) -> Result<Vec<MigrationRun>, LifecycleError> {
    ctx.require_permission()?;
    let mut runs = ctx
        .runs()?
        .list(filter)
        .await
        .map_err(|_| LifecycleError::Store)?;
    runs.sort_by_key(|run| run.started);
    Ok(runs)
}

/// One run with the chain entries that make it up.
///
/// # Errors
/// As [`list`]; `migration.unknown_plan` when there is no such run.
pub async fn show(ctx: &LifecycleContext, run_id: &str) -> Result<RunEvidence, LifecycleError> {
    ctx.require_permission()?;
    let id = RunId::new(run_id).map_err(|_| LifecycleError::UnknownPlan)?;
    ctx.runs()?
        .get(&id)
        .await
        .map_err(|_| LifecycleError::Store)?
        .ok_or(LifecycleError::UnknownPlan)
}

/// One run in plain language.
///
/// # Errors
/// As [`show`].
pub async fn explain(ctx: &LifecycleContext, run_id: &str) -> Result<String, LifecycleError> {
    Ok(explain_run(&show(ctx, run_id).await?.run).render())
}

/// Re-walks the chain and reports the first broken link.
///
/// # Errors
/// `migration.not_permitted`; `migration.store_failed`.
pub async fn verify(
    ctx: &LifecycleContext,
    verifier: &dyn ChainVerifier,
) -> Result<ChainStatus, LifecycleError> {
    ctx.require_permission()?;
    verifier.verify().await
}

/// The plan as a run record keeps it: the stored document's digest and the plan's summary.
#[must_use]
pub fn plan_ref(plan: &ReleasePlan, stored_json: &str, location: &str) -> PlanRef {
    PlanRef {
        artifact: ArtifactRef::of(ArtifactKind::Plan, location, stored_json.as_bytes()),
        summary: PlanSummary::of(plan),
    }
}

/// The outcome a verb that did not complete is recorded with: `refused` for a policy or
/// permission refusal, `failed` for a failure.
#[must_use]
pub fn outcome_of(error: &LifecycleError) -> Outcome {
    if error.exit_code() == 2 {
        Outcome::Refused
    } else {
        Outcome::Failed
    }
}

/// Runs `work` under a run record: the run begins before the work, and finishes after it with
/// `on_success` or, when the work fails or is refused, with `refused` or `failed`. The finish event
/// of `spot-migration-audit` carries the outcome only, so the reason code travels in the error the
/// caller gets back (an open question of the task: a `reason` field on `FinishEvent`).
///
/// The actor is the context's (`system` or `user:<name>`), which is also what
/// [`LifecycleContext::permit`] checks the run handle against.
///
/// # Errors
/// The work's own error, unchanged; `migration.store_failed` when the run cannot be begun (the work
/// is not started) or finished (the work's success is then not reported as one: evidence is part of
/// the operation).
#[allow(clippy::too_many_arguments)]
pub async fn record_verb<T, F>(
    ctx: &LifecycleContext,
    recorder: &Arc<dyn RunRecorder>,
    run_id: &str,
    plan_digest: &str,
    plan: PlanRef,
    on_success: Outcome,
    work: F,
) -> Result<T, LifecycleError>
where
    F: Future<Output = Result<T, LifecycleError>>,
{
    let id = RunId::new(run_id).map_err(|_| LifecycleError::Store)?;
    let handle = recorder
        .begin(RunSpec {
            id,
            plan_digest: plan_digest.to_owned(),
            plan,
            actor: ctx.actor().user_id(),
            at: ctx.now(),
        })
        .await
        .map_err(|_| LifecycleError::Store)?;
    let result = work.await;
    let outcome = match &result {
        Ok(_) => on_success,
        Err(error) => outcome_of(error),
    };
    recorder
        .finish(
            &handle,
            FinishEvent {
                outcome,
                at: ctx.now(),
            },
        )
        .await
        .map_err(|_| LifecycleError::Store)?;
    result
}

/// The audit operations have no management catalogue entries of their own in this skeleton; the
/// CLI and the console call the functions above. Nothing is registered.
pub fn register(_ctx: &Arc<LifecycleContext>, _registry: &mut HandlerRegistry) {}
