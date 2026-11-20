//! Owned by WP-MIG-07.T3 (apply).
//!
//! `apply` runs exactly the approved, rehearsed plan, once, with the site quiesced, and it is
//! recoverable. The dangerous parts (maintenance gate, effect hold, pre-apply snapshot, checkpointed
//! steps, verification, restore on failure) are the apply engine's (`spot_migration_apply`); this
//! verb is the lifecycle around it.
//!
//! # The flow
//!
//! 1. The actor needs `migration.run`. The stored plan is parsed back and its hash recomputed
//!    (`migration.plan_changed`): the plan that runs is the plan that was approved.
//! 2. The state must be `approved` or `rehearsed`. A **destructive** plan, or a site whose host
//!    says rehearsal is required, must be `rehearsed` (`migration.not_rehearsed`). `applying`
//!    and `failed` are only entered again with `resume`.
//! 3. **The plan-state compare-and-set is the lock.** `approved`/`rehearsed` to `applying`; a lost
//!    race is `migration.site_busy`. Two applies cannot both pass it.
//! 4. The approval service issues the apply ticket (audited, bound to the plan digest, two approvers
//!    for a destructive plan). A fresh run is `ApplyEngine::apply` (gate, hold, snapshot named for
//!    the plan, checkpoints, verify); `resume` is `ApplyEngine::recover`, which continues from the
//!    last checkpoint or restores.
//! 5. `Opened` verified moves the plan to `applied`. A restore, or any error, moves it to `failed`
//!    (`migration.apply_failed`); the engine decides whether the gate stays closed.
//!
//! The host supplies the engine ([`ApplyHost`]) because only it has the database, the snapshot
//! provider and the journal. Maintenance enter and leave are [`maintenance_enter`] and
//! [`maintenance_leave`] over the context's gate.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_mgmt_ops::{OpId, Validated};
use spot_migration_apply::{
    ApplyOutcome, ApplyRequest, ApplyService, BoxFuture, RecoveryAction, RecoveryOutcome, Version,
};
use spot_migration_approval::{ApplyTicket, AuthoriseError, Checks, effective_risk};
use spot_migration_plan::{ReleasePlan, Risk};

use crate::catalogue::{HandlerOutput, HandlerRegistry, OperationHandler};
use crate::context::{AuditSpec, LifecycleContext, PlanHash, PlanState, off_runtime};
use crate::error::LifecycleError;
use crate::plan::{check_site, load, map_approval, param, parse_stored, subject};

/// What the host supplies to apply one site's plans.
pub trait ApplyHost: Send + Sync {
    /// The engine for `plan` (journal, gate, hold, snapshot port, step runner, verifier).
    ///
    /// # Errors
    /// `migration.port_unavailable` when the host cannot build one.
    fn engine<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        hash: &'a PlanHash,
        now: i64,
    ) -> BoxFuture<'a, Result<Arc<dyn ApplyService>, LifecycleError>>;

    /// Whether the site policy requires a rehearsal for every plan (a destructive plan always
    /// does).
    fn rehearsal_required(&self) -> bool {
        false
    }
}

/// The result of `apply`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReport {
    /// The plan hash.
    pub hash: String,
    /// The plan state afterwards.
    pub state: PlanState,
    /// What happened, in words (no row data).
    pub text: String,
}

struct Prepared {
    hash: PlanHash,
    plan: ReleasePlan,
    state: PlanState,
    version: u64,
}

fn prepare(
    ctx: &LifecycleContext,
    host: &dyn ApplyHost,
    hash: &str,
    resume: bool,
) -> Result<Prepared, LifecycleError> {
    ctx.require_permission()?;
    let (hash, stored) = load(ctx, hash)?;
    let plan = parse_stored(&stored)?;
    match stored.state {
        PlanState::Approved | PlanState::Rehearsed => {}
        PlanState::Applying | PlanState::Failed if resume => {}
        PlanState::Applying => return Err(LifecycleError::SiteBusy),
        _ => return Err(LifecycleError::NotApproved),
    }
    let must_rehearse = effective_risk(&plan) == Risk::Destructive || host.rehearsal_required();
    if must_rehearse && !resume && stored.state != PlanState::Rehearsed {
        return Err(LifecycleError::NotRehearsed);
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

/// Takes the plan-state lock: `approved`/`rehearsed` to `applying`. A lost race is `site_busy`.
fn lock(ctx: &LifecycleContext, prepared: &Prepared, resume: bool) -> Result<u64, LifecycleError> {
    if resume && matches!(prepared.state, PlanState::Applying) {
        return Ok(prepared.version);
    }
    ctx.plans()
        .mark_state(
            &prepared.hash,
            prepared.state,
            prepared.version,
            PlanState::Applying,
        )
        .map_err(|error| match error {
            LifecycleError::PlanChanged => LifecycleError::SiteBusy,
            other => other,
        })
}

fn settle_state(
    ctx: &LifecycleContext,
    prepared: &Prepared,
    version: u64,
    next: PlanState,
) -> Result<(), LifecycleError> {
    ctx.plans()
        .mark_state(&prepared.hash, PlanState::Applying, version, next)
        .map(|_| ())
}

fn version_word(version: Version) -> &'static str {
    match version {
        Version::Old => "the old version",
        Version::New => "the new version",
    }
}

/// Applies the approved plan `hash` (or resumes it) under a run record: permit first, a refusal
/// recorded, the plan's digest on the record, `failed` with the reason code when the apply fails.
/// See [`LifecycleContext::audited`].
///
/// # Errors
/// As the body; `migration.store_failed` when the run cannot be recorded.
pub async fn apply(
    ctx: &Arc<LifecycleContext>,
    host: &Arc<dyn ApplyHost>,
    hash: &str,
    resume: bool,
) -> Result<ApplyReport, LifecycleError> {
    let (digest, plan) = {
        let (ctx, hash) = (Arc::clone(ctx), hash.to_owned());
        off_runtime(move || ctx.plan_audit(&hash)).await?
    };
    ctx.audited(AuditSpec::new("apply", digest, plan), |_| {
        apply_body(ctx, host, hash, resume)
    })
    .await
}

/// Applies the approved plan `hash`, or with `resume` continues an unfinished apply.
///
/// # Errors
/// `migration.not_permitted`, `migration.unknown_plan`, `migration.plan_changed`,
/// `migration.not_approved`, `migration.not_rehearsed`, `migration.site_busy`,
/// `migration.apply_failed` (the plan is `failed`; the next action is `spot migrate rescue HASH`
/// or `spot migrate apply HASH --resume`), `migration.port_unavailable`, `migration.store_failed`.
async fn apply_body(
    ctx: &Arc<LifecycleContext>,
    host: &Arc<dyn ApplyHost>,
    hash: &str,
    resume: bool,
) -> Result<ApplyReport, LifecycleError> {
    let (prepared, version) = {
        let (ctx, host, hash) = (Arc::clone(ctx), Arc::clone(host), hash.to_owned());
        off_runtime(move || {
            let prepared = prepare(&ctx, host.as_ref(), &hash, resume)?;
            let version = lock(&ctx, &prepared, resume)?;
            Ok::<_, LifecycleError>((Arc::new(prepared), version))
        })
        .await??
    };
    let now = ctx.now();
    let outcome = run_engine(ctx, host, &prepared, resume, now).await;
    let next = match &outcome {
        Ok(Done::Applied(_)) => PlanState::Applied,
        _ => PlanState::Failed,
    };
    let (ctx2, prepared2) = (Arc::clone(ctx), Arc::clone(&prepared));
    off_runtime(move || settle_state(&ctx2, &prepared2, version, next)).await??;
    match outcome {
        Ok(Done::Applied(text)) => Ok(ApplyReport {
            hash: prepared.hash.as_str().to_owned(),
            state: PlanState::Applied,
            text,
        }),
        Ok(Done::Restored(_)) | Err(_) => Err(LifecycleError::ApplyFailed),
    }
}

#[allow(dead_code)]
enum Done {
    Applied(String),
    Restored(String),
}

async fn run_engine(
    ctx: &Arc<LifecycleContext>,
    host: &Arc<dyn ApplyHost>,
    prepared: &Arc<Prepared>,
    resume: bool,
    now: i64,
) -> Result<Done, LifecycleError> {
    // The ticket comes before the engine is built: a refused apply builds nothing.
    let proof = if resume {
        None
    } else {
        let (ctx, prepared) = (Arc::clone(ctx), Arc::clone(prepared));
        Some(off_runtime(move || ticket(&ctx, &prepared.plan)).await??)
    };
    let engine = host.engine(&prepared.plan, &prepared.hash, now).await?;
    if resume {
        let recovered: RecoveryOutcome = engine
            .recover(&prepared.plan, now)
            .await
            .map_err(|_| LifecycleError::ApplyFailed)?;
        return Ok(match recovered.action {
            RecoveryAction::Resumed { from_step } => {
                Done::Applied(format!("resumed from step {from_step} and applied"))
            }
            RecoveryAction::AlreadyOpen { version, verified } if verified => {
                Done::Applied(format!("already open on {}", version_word(version)))
            }
            other => Done::Restored(format!("{other:?}")),
        });
    }
    let request = ApplyRequest::new(prepared.plan.clone(), proof, None, now);
    match engine
        .apply(request)
        .await
        .map_err(|_| LifecycleError::ApplyFailed)?
    {
        ApplyOutcome::Opened { version, verified } if verified => Ok(Done::Applied(format!(
            "applied and verified; the site is open on {}",
            version_word(version)
        ))),
        ApplyOutcome::Opened { .. } => Ok(Done::Restored("opened unverified".to_owned())),
        ApplyOutcome::Restored { .. } => Ok(Done::Restored("restored".to_owned())),
    }
}

/// Closes the site's maintenance gate (reject writes, drain in-flight requests).
///
/// # Errors
/// `migration.not_permitted`; `migration.apply_failed` when the gate cannot be proven closed
/// (fail closed: the caller must not start work).
pub async fn maintenance_enter(ctx: &LifecycleContext, reason: &str) -> Result<(), LifecycleError> {
    ctx.require_permission()?;
    let now = ctx.now();
    ctx.gate()
        .close(reason, now)
        .await
        .map_err(|_| LifecycleError::ApplyFailed)?;
    ctx.gate()
        .drain(30_000_000, now)
        .await
        .map_err(|_| LifecycleError::ApplyFailed)?;
    Ok(())
}

/// Opens the site's maintenance gate.
///
/// # Errors
/// `migration.not_permitted`; `migration.apply_failed` when the gate cannot be opened.
pub async fn maintenance_leave(ctx: &LifecycleContext) -> Result<(), LifecycleError> {
    ctx.require_permission()?;
    ctx.gate()
        .open(ctx.now())
        .await
        .map_err(|_| LifecycleError::ApplyFailed)
}

struct ApplyHandler {
    ctx: Arc<LifecycleContext>,
    host: Arc<dyn ApplyHost>,
}

impl OperationHandler for ApplyHandler {
    fn handle<'a>(
        &'a self,
        call: &'a Validated,
    ) -> BoxFuture<'a, Result<HandlerOutput, LifecycleError>> {
        Box::pin(async move {
            check_site(&self.ctx, call)?;
            let hash = param(call, "plan_hash").ok_or(LifecycleError::UnknownPlan)?;
            let resume = param(call, "resume").is_some_and(|v| v == "true");
            let report = apply(&self.ctx, &self.host, &hash, resume).await?;
            let mut fields = BTreeMap::new();
            fields.insert("plan_hash".to_owned(), report.hash.clone());
            fields.insert("state".to_owned(), report.state.as_str().to_owned());
            Ok(HandlerOutput {
                message: format!("plan {}: {}", report.hash, report.text),
                fields,
            })
        })
    }
}

/// Without a host the operation stays a fail-closed stub: an apply needs the database, the
/// snapshot provider and the journal, which only the host can supply. See [`register_with`].
pub fn register(_ctx: &Arc<LifecycleContext>, _registry: &mut HandlerRegistry) {}

/// Registers the handler of `site.migrate.apply` over `host`.
pub fn register_with(
    ctx: &Arc<LifecycleContext>,
    registry: &mut HandlerRegistry,
    host: Arc<dyn ApplyHost>,
) {
    if let Some(id) = OpId::new("site.migrate.apply") {
        registry.register(
            id,
            Arc::new(ApplyHandler {
                ctx: Arc::clone(ctx),
                host,
            }),
        );
    }
}
