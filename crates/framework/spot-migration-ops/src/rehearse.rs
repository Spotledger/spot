//! Owned by WP-MIG-06.T5 (rehearse).
//!
//! `rehearse` runs an **approved** plan on a clone of the site and records what happened. It is
//! what makes an apply safe to approve, and it never touches the live database.
//!
//! # The flow
//!
//! 1. The actor needs the `migration.run` permission. An unknown plan is `migration.unknown_plan`;
//!    a plan that is not approved (`approved` or `rehearsed`) is `migration.not_approved`; a stored
//!    plan whose text no longer matches its hash is `migration.plan_changed`.
//! 2. **Disk guard.** The free space must be at least 2.5 times the snapshot size, otherwise
//!    `migration.insufficient_space` and nothing is started. An unreadable free space is a refusal
//!    too (fail closed).
//! 3. The approval service issues the proof the engine requires (`authorise_apply`: an audited
//!    ticket bound to this plan's digest).
//! 4. The host's [`RehearsalRunner`] snapshots the site, restores it into a scratch database named
//!    from the plan hash and the clock, applies the plan with the real apply engine **against the
//!    scratch**, measures the financial fingerprint, runs the invariants (and the replay when the
//!    policy says so) and destroys the scratch on every exit path (the engine's guard type). A
//!    **wall-clock budget** ([`RehearsalHost::budget`]) aborts a rehearsal that overruns; dropping
//!    the run drops the clone guard, so the scratch is cleaned.
//! 5. The report is stored in the [`RehearsalLedger`] by plan hash and clock, **always**: a pass, a
//!    failure, an error and a timeout. The plan moves from `approved` to `rehearsed` if and only if
//!    the rehearsal completed with no invariant failure and no unexplained fingerprint difference.
//!
//! The fingerprint of a site with no registered sources is empty and recorded as a pass with the
//! sentence "no fingerprint sources registered" in the report text: the ERP's queries are
//! registered by Foundation later.
//!
//! # Threads
//!
//! The stores are synchronous and reach PostgreSQL through the blocking bridge, which refuses a
//! tokio thread. [`rehearse`] therefore runs every store call through [`off_runtime`] and awaits
//! the engine on the caller's runtime.
//!
//! # Scratch leftovers
//!
//! A process killed mid-rehearsal can leave a scratch database behind. [`clean`] asks the host to
//! drop the leftovers of this site's rehearsals; the host refuses to drop anything that is not a
//! marked rehearsal scratch (never the live database).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use spot_core::SiteId;
use spot_mgmt_ops::{OpId, Validated};
use spot_migration_apply::BoxFuture;
use spot_migration_approval::{ApplyTicket, AuthoriseError, Checks};
use spot_migration_plan::ReleasePlan;
use spot_rehearsal::{Rehearsal, RehearsalOutcome};

use crate::catalogue::{HandlerOutput, HandlerRegistry, OperationHandler};
use crate::context::{
    AuditSpec, LifecycleContext, PlanHash, PlanState, RehearsalLedger, RehearsalRecord, off_runtime,
};
use crate::error::LifecycleError;
use crate::plan::{check_site, load, map_approval, param, parse_stored, subject};

/// The free space must be at least this many tenths of the snapshot size (2.5 times).
pub const SPACE_FACTOR_TENTHS: u64 = 25;

/// A rehearsal that completed and may mark the plan rehearsed.
pub const VERDICT_PASS: &str = "pass";
/// A rehearsal that completed with an invariant failure or an unexplained difference.
pub const VERDICT_FAIL: &str = "fail";
/// A rehearsal the engine could not complete.
pub const VERDICT_ERROR: &str = "error";
/// A rehearsal aborted by its wall-clock budget.
pub const VERDICT_TIMEOUT: &str = "timeout";

/// What one run of the engine produced, flattened (no row data: counts, digests, names).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunResult {
    /// `true` when the rehearsal may mark the plan rehearsed.
    pub passed: bool,
    /// The outcome word (`clean`, `expected_changes`, `unexplained_differences`,
    /// `invariants_failed`).
    pub outcome: String,
    /// The report as JSON.
    pub report_json: String,
    /// The report as text.
    pub text: String,
    /// Whether the scratch was fully destroyed.
    pub cleaned: bool,
}

/// One rehearsal engine, built for one plan.
pub trait RehearsalRunner: Send + Sync {
    /// Runs the rehearsal of `plan` under `ticket`. An error is `migration.rehearsal_failed`.
    fn run<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        ticket: &'a ApplyTicket,
        site: &'a SiteId,
    ) -> BoxFuture<'a, Result<RunResult, LifecycleError>>;
}

/// The runner over the real engine ([`spot_rehearsal::Rehearsal`]).
pub struct EngineRunner(pub Rehearsal);

impl RehearsalRunner for EngineRunner {
    fn run<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        ticket: &'a ApplyTicket,
        site: &'a SiteId,
    ) -> BoxFuture<'a, Result<RunResult, LifecycleError>> {
        Box::pin(async move {
            let report = self
                .0
                .run(plan, ticket, site)
                .await
                .map_err(|_| LifecycleError::RehearsalFailed)?;
            let passed = matches!(
                report.outcome,
                RehearsalOutcome::Clean | RehearsalOutcome::ExpectedChanges
            );
            let outcome = match report.outcome {
                RehearsalOutcome::Clean => "clean",
                RehearsalOutcome::ExpectedChanges => "expected_changes",
                RehearsalOutcome::UnexplainedDifferences => "unexplained_differences",
                RehearsalOutcome::InvariantsFailed => "invariants_failed",
            };
            let report_json =
                serde_json::to_string(&report).map_err(|_| LifecycleError::RehearsalFailed)?;
            Ok(RunResult {
                passed,
                outcome: outcome.to_owned(),
                report_json,
                text: report.render_text(),
                cleaned: report.clone.cleanup.cleaned(),
            })
        })
    }
}

/// What the host supplies for rehearsals of one site.
pub trait RehearsalHost: Send + Sync {
    /// The runner for `plan` (provider, applier, fingerprint source and spec, invariants, replay
    /// policy, and a clone request whose scratch is named from `hash` and `now`).
    ///
    /// # Errors
    /// `migration.port_unavailable` and similar when the host cannot build one.
    fn runner(
        &self,
        plan: &ReleasePlan,
        hash: &PlanHash,
        now: i64,
    ) -> Result<Arc<dyn RehearsalRunner>, LifecycleError>;

    /// Free bytes where the snapshot and the scratch go.
    fn free_bytes(&self) -> BoxFuture<'_, Result<u64, LifecycleError>>;

    /// The expected size of the snapshot, in bytes.
    fn snapshot_bytes(&self) -> BoxFuture<'_, Result<u64, LifecycleError>>;

    /// The wall-clock budget of one rehearsal (default 30 minutes).
    fn budget(&self) -> Duration {
        Duration::from_secs(30 * 60)
    }

    /// The rehearsal results store.
    fn ledger(&self) -> Arc<dyn RehearsalLedger>;

    /// Drops the scratch databases and snapshot directories left by killed rehearsals of this
    /// site and returns their labels. It never touches the live database.
    fn clean_leftovers(&self) -> BoxFuture<'_, Result<Vec<String>, LifecycleError>>;
}

/// The result of `rehearse`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RehearseReport {
    /// The plan hash.
    pub hash: String,
    /// Whether the rehearsal passed (and the plan is `rehearsed`).
    pub passed: bool,
    /// The outcome word.
    pub outcome: String,
    /// The ledger key: the time the result was recorded at.
    pub recorded_at: i64,
    /// Whether the scratch was fully destroyed.
    pub cleaned: bool,
    /// The plan state after the rehearsal.
    pub state: PlanState,
    /// The report as text.
    pub text: String,
}

struct Prepared {
    hash: PlanHash,
    plan: ReleasePlan,
    state: PlanState,
    version: u64,
}

fn prepare(ctx: &LifecycleContext, hash: &str) -> Result<Prepared, LifecycleError> {
    ctx.require_permission()?;
    let (hash, stored) = load(ctx, hash)?;
    let plan = parse_stored(&stored)?;
    if !matches!(stored.state, PlanState::Approved | PlanState::Rehearsed) {
        return Err(LifecycleError::NotApproved);
    }
    Ok(Prepared {
        hash,
        plan,
        state: stored.state,
        version: stored.version,
    })
}

fn ticket(ctx: &LifecycleContext, plan: &ReleasePlan) -> Result<ApplyTicket, LifecycleError> {
    ctx.approvals()
        .authorise_apply(plan, &subject(ctx.actor()), ctx.now(), &Checks::none())
        .map_err(|error| match error {
            AuthoriseError::Refused(_) => LifecycleError::NotApproved,
            AuthoriseError::Approval(inner) => map_approval(&inner),
        })
}

enum Ran {
    Done(RunResult),
    Failed,
    TimedOut,
}

fn finish(
    ctx: &LifecycleContext,
    ledger: &dyn RehearsalLedger,
    prepared: &Prepared,
    ran: Ran,
) -> Result<RehearseReport, LifecycleError> {
    let at = ctx.now();
    let (verdict, report_json, result) = match &ran {
        Ran::Done(run) => (
            if run.passed {
                VERDICT_PASS
            } else {
                VERDICT_FAIL
            },
            run.report_json.clone(),
            Some(run),
        ),
        Ran::Failed => (
            VERDICT_ERROR,
            "{\"error\":\"rehearsal_failed\"}".to_owned(),
            None,
        ),
        Ran::TimedOut => (VERDICT_TIMEOUT, "{\"error\":\"timeout\"}".to_owned(), None),
    };
    // The result is stored before anything else changes, whatever it was.
    ledger.record(RehearsalRecord {
        plan_hash: prepared.hash.clone(),
        at,
        verdict: verdict.to_owned(),
        report_json,
    })?;
    let Some(run) = result else {
        return Err(LifecycleError::RehearsalFailed);
    };
    let mut state = prepared.state;
    if run.passed && state == PlanState::Approved {
        match ctx.plans().mark_state(
            &prepared.hash,
            PlanState::Approved,
            prepared.version,
            PlanState::Rehearsed,
        ) {
            Ok(_) => state = PlanState::Rehearsed,
            // Somebody moved the plan meanwhile: report where it is, do not overwrite.
            Err(LifecycleError::PlanChanged) => {
                state = ctx
                    .plans()
                    .load(&prepared.hash)?
                    .map_or(state, |plan| plan.state);
            }
            Err(other) => return Err(other),
        }
    }
    Ok(RehearseReport {
        hash: prepared.hash.as_str().to_owned(),
        passed: run.passed,
        outcome: run.outcome.clone(),
        recorded_at: at,
        cleaned: run.cleaned,
        state,
        text: run.text.clone(),
    })
}

/// Rehearses the approved plan `hash` under a run record (permit first, refusals recorded, the
/// plan's digest on the record, the reason code on a failure). See [`LifecycleContext::audited`];
/// the errors are those of the body below.
///
/// # Errors
/// As the body; `migration.store_failed` when the run cannot be recorded.
pub async fn rehearse(
    ctx: &Arc<LifecycleContext>,
    host: &Arc<dyn RehearsalHost>,
    hash: &str,
) -> Result<RehearseReport, LifecycleError> {
    let (digest, plan) = {
        let (ctx, hash) = (Arc::clone(ctx), hash.to_owned());
        off_runtime(move || ctx.plan_audit(&hash)).await?
    };
    ctx.audited(AuditSpec::new("rehearse", digest, plan), |_| {
        rehearse_body(ctx, host, hash)
    })
    .await
}

/// Rehearses the approved plan `hash`.
///
/// # Errors
/// `migration.not_permitted`, `migration.unknown_plan`, `migration.plan_changed`,
/// `migration.not_approved`, `migration.insufficient_space`, `migration.rehearsal_failed` (the
/// engine failed or ran out of its budget; the result is recorded and the scratch cleaned),
/// `migration.store_failed`. A rehearsal that completes with a failing verdict is **not** an error:
/// `passed` is `false` and the plan stays as it was.
async fn rehearse_body(
    ctx: &Arc<LifecycleContext>,
    host: &Arc<dyn RehearsalHost>,
    hash: &str,
) -> Result<RehearseReport, LifecycleError> {
    let prepared = {
        let (ctx, hash) = (Arc::clone(ctx), hash.to_owned());
        Arc::new(off_runtime(move || prepare(&ctx, &hash)).await??)
    };
    // The disk guard comes before anything is issued or started.
    let (free, size) = (host.free_bytes().await?, host.snapshot_bytes().await?);
    if free.saturating_mul(10) < size.saturating_mul(SPACE_FACTOR_TENTHS) {
        return Err(LifecycleError::InsufficientSpace);
    }
    let proof = {
        let (ctx, prepared) = (Arc::clone(ctx), Arc::clone(&prepared));
        off_runtime(move || ticket(&ctx, &prepared.plan)).await??
    };
    let runner = host.runner(&prepared.plan, &prepared.hash, ctx.now())?;
    let ran = match tokio::time::timeout(
        host.budget(),
        runner.run(&prepared.plan, &proof, ctx.site()),
    )
    .await
    {
        Ok(Ok(run)) => Ran::Done(run),
        Ok(Err(_)) => Ran::Failed,
        // The run future was dropped: the clone's guard has destroyed the scratch.
        Err(_) => Ran::TimedOut,
    };
    let (ctx, ledger) = (Arc::clone(ctx), host.ledger());
    off_runtime(move || finish(&ctx, ledger.as_ref(), &prepared, ran)).await?
}

/// Drops the leftovers of killed rehearsals of this site.
///
/// # Errors
/// `migration.not_permitted`; `migration.store_failed` when the host cannot list or drop them.
pub async fn clean(
    ctx: &LifecycleContext,
    host: &dyn RehearsalHost,
) -> Result<Vec<String>, LifecycleError> {
    ctx.require_permission()?;
    host.clean_leftovers().await
}

struct RehearseHandler {
    ctx: Arc<LifecycleContext>,
    host: Arc<dyn RehearsalHost>,
}

impl OperationHandler for RehearseHandler {
    fn handle<'a>(
        &'a self,
        call: &'a Validated,
    ) -> BoxFuture<'a, Result<HandlerOutput, LifecycleError>> {
        Box::pin(async move {
            check_site(&self.ctx, call)?;
            let hash = param(call, "plan_hash").ok_or(LifecycleError::UnknownPlan)?;
            let report = rehearse(&self.ctx, &self.host, &hash).await?;
            let mut fields = BTreeMap::new();
            fields.insert("plan_hash".to_owned(), report.hash.clone());
            fields.insert("passed".to_owned(), report.passed.to_string());
            fields.insert("outcome".to_owned(), report.outcome.clone());
            fields.insert("state".to_owned(), report.state.as_str().to_owned());
            fields.insert("ledger_at".to_owned(), report.recorded_at.to_string());
            if !report.passed {
                // A failing verdict is a failed operation for the caller; the report is stored.
                return Err(LifecycleError::RehearsalFailed);
            }
            Ok(HandlerOutput {
                message: format!("plan {} rehearsed: {}", report.hash, report.outcome),
                fields,
            })
        })
    }
}

/// Without a host the operation stays a fail-closed stub: a rehearsal needs a snapshot provider,
/// a scratch database and an applier, which only the host can supply. See [`register_with`].
pub fn register(_ctx: &Arc<LifecycleContext>, _registry: &mut HandlerRegistry) {}

/// Registers the handler of `site.migrate.rehearse` over `host`.
pub fn register_with(
    ctx: &Arc<LifecycleContext>,
    registry: &mut HandlerRegistry,
    host: Arc<dyn RehearsalHost>,
) {
    if let Some(id) = OpId::new("site.migrate.rehearse") {
        registry.register(
            id,
            Arc::new(RehearseHandler {
                ctx: Arc::clone(ctx),
                host,
            }),
        );
    }
}
