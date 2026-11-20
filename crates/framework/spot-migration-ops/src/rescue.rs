//! Owned by WP-MIG-10.T2 (rescue).
//!
//! `rescue` is the way back when an apply failed or verification found a problem after the gate
//! opened: **restore** the pre-apply snapshot (the current database is quarantined, never dropped)
//! or run the declared **down** patches, then re-enter what can be replayed and supersede the plan.
//! It can destroy data written since the snapshot, so it is a two-step, two-person operation:
//!
//! 1. [`preview`] says what a rescue would do: the snapshot (present and verified), the **counted
//!    loss window** (rows per document type the restore replaces) and the **confirm token**, a digest
//!    of the plan hash, the mode, the snapshot time and those counts. The operator must hand that
//!    token back: the acknowledgement of the loss is bound to the counted loss, so a different
//!    window (more writes since the preview) is a different token and the rescue is refused.
//! 2. [`rescue`] refuses unless: the actor has `migration.run`; the plan is `failed` or `applied`
//!    (`migration.rescue_refused` otherwise); the plan is the stored plan its hash names; the
//!    maintenance gate is **closed** and the effect hold is **active** (the host proves both before
//!    anything runs); a **second person**, not the actor, confirmed; and the token matches.
//!
//! On success the plan is marked `superseded`. The host reports a precise loss window for what it
//! could not replay; this verb never reports a loss it did not count.
//!
//! The host ([`RescueHost`]) owns everything that touches the database, the snapshot provider and
//! the old version's engine; the verb is the lifecycle around it, as for apply.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_mgmt_ops::{OpId, Validated};
use spot_migration_apply::BoxFuture;
use spot_migration_plan::ReleasePlan;
use spot_rescue::{RescueJournal, RunProgress};

use crate::catalogue::{HandlerOutput, HandlerRegistry, OperationHandler};
use crate::context::{AuditSpec, LifecycleContext, PlanHash, PlanState, off_runtime};
use crate::error::LifecycleError;
use crate::plan::{check_site, load, param, parse_stored};

/// How the way back is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The declared down-patches (every step reversible).
    Down,
    /// The pre-apply snapshot, with the current database quarantined.
    Restore,
}

impl Mode {
    /// The word on the command line and in the token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Restore => "restore",
        }
    }

    /// Reads the word.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "down" => Some(Self::Down),
            "restore" => Some(Self::Restore),
            _ => None,
        }
    }
}

/// The snapshot a rescue restores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotFacts {
    /// Its id.
    pub id: String,
    /// When it was taken (unix microseconds).
    pub at: i64,
    /// Whether its checksums were verified just now.
    pub verified: bool,
}

/// What the host did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRun {
    /// What happened, in words (no row data).
    pub text: String,
    /// What could not be replayed, per document type (counts): the precise loss window.
    pub unreplayed: BTreeMap<String, u64>,
}

/// What the host supplies to rescue one site.
pub trait RescueHost: Send + Sync {
    /// The pre-apply snapshot of `plan`, or `None` when there is none.
    fn snapshot<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        hash: &'a PlanHash,
    ) -> BoxFuture<'a, Result<Option<SnapshotFacts>, LifecycleError>>;

    /// Rows per document type that a restore to `snapshot_at` replaces (the counted loss).
    fn loss_window(
        &self,
        snapshot_at: i64,
    ) -> BoxFuture<'_, Result<BTreeMap<String, u64>, LifecycleError>>;

    /// Whether the maintenance gate is closed and the effect hold is active.
    fn quiesced(&self) -> BoxFuture<'_, Result<bool, LifecycleError>>;

    /// The durable rescue journal of this site, when the host keeps one (WP-MIG-10.T3): the verb
    /// reads it to resume an unfinished rescue and to refuse starting a different one. A host
    /// without a journal (`None`) cannot resume; build one with [`pg_journal`].
    fn journal(&self) -> Option<Arc<dyn RescueJournal>> {
        None
    }

    /// Runs the rescue in `mode`: restore (or down-patches), re-entry, the invariant run, and only
    /// after it passes the gate opens. A crash is resumable.
    ///
    /// # Errors
    /// `migration.rescue_refused` (an irreversible step, a failed invariant), `migration.apply_failed`.
    fn run<'a>(
        &'a self,
        mode: Mode,
        plan: &'a ReleasePlan,
        snapshot: &'a SnapshotFacts,
        now: i64,
    ) -> BoxFuture<'a, Result<HostRun, LifecycleError>>;
}

/// What `preview` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// The plan hash.
    pub hash: String,
    /// The mode previewed.
    pub mode: Mode,
    /// The snapshot.
    pub snapshot: SnapshotFacts,
    /// The counted loss window.
    pub loss: BTreeMap<String, u64>,
    /// The token to hand back to [`rescue`].
    pub token: String,
}

/// The result of `rescue`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RescueReport {
    /// The plan hash.
    pub hash: String,
    /// The plan state afterwards (`superseded`).
    pub state: PlanState,
    /// What the host did.
    pub run: HostRun,
    /// Whether this call took an unfinished rescue over (a restart after a kill) instead of
    /// starting a new one.
    pub resumed: bool,
    /// The attempt number read from the journal after the run (`1` for a first attempt; `0` when
    /// the host keeps no journal).
    pub attempt: u32,
}

/// The PostgreSQL rescue journal of `site`, with its control schema created first (the first rescue
/// of a site creates it; a creation-time call would be cleaner, see the task report). A host
/// returns it from [`RescueHost::journal`].
///
/// # Errors
/// `migration.store_failed` when the schema cannot be created.
pub async fn pg_journal(
    pool: spot_rescue_pg::Pool,
    site: &spot_core::SiteId,
) -> Result<Arc<dyn RescueJournal>, LifecycleError> {
    spot_rescue_pg::PgRescueSchema::new(pool.clone())
        .prepare()
        .await
        .map_err(|_| LifecycleError::Store)?;
    Ok(Arc::new(spot_rescue_pg::PgRescueJournal::new(
        pool,
        site.clone(),
    )))
}

async fn progress_of(host: &Arc<dyn RescueHost>) -> Result<Option<RunProgress>, LifecycleError> {
    match host.journal() {
        None => Ok(None),
        Some(journal) => journal
            .entries()
            .await
            .map(|entries| Some(RunProgress::fold(entries)))
            .map_err(|_| LifecycleError::Store),
    }
}

/// The confirm token: a SHA-256 over the plan hash, the mode, the snapshot time and every count of
/// the loss window, in sorted order. Any change of the window is a different token.
#[must_use]
pub fn confirm_token(
    hash: &PlanHash,
    mode: Mode,
    snapshot_at: i64,
    loss: &BTreeMap<String, u64>,
) -> String {
    let mut text = format!(
        "rescue\0{}\0{}\0{snapshot_at}",
        hash.as_str(),
        mode.as_str()
    );
    for (doctype, count) in loss {
        text.push_str(&format!("\0{doctype}={count}"));
    }
    // The first 32 hex characters of the SHA-256: unguessable and short enough to type.
    spot_migration_audit::run::digest_of(text.as_bytes())
        .chars()
        .take(32)
        .collect()
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
    if !matches!(stored.state, PlanState::Failed | PlanState::Applied) {
        return Err(LifecycleError::RescueRefused);
    }
    Ok(Prepared {
        hash,
        plan,
        state: stored.state,
        version: stored.version,
    })
}

async fn facts(
    host: &Arc<dyn RescueHost>,
    prepared: &Prepared,
) -> Result<(SnapshotFacts, BTreeMap<String, u64>), LifecycleError> {
    let snapshot = host
        .snapshot(&prepared.plan, &prepared.hash)
        .await?
        .filter(|s| s.verified)
        .ok_or(LifecycleError::RescueRefused)?;
    let loss = host.loss_window(snapshot.at).await?;
    Ok((snapshot, loss))
}

/// Says what a rescue of the plan `hash` in `mode` would do, and issues the confirm token. Changes
/// nothing.
///
/// # Errors
/// `migration.not_permitted`, `migration.unknown_plan`, `migration.plan_changed`,
/// `migration.rescue_refused` (wrong state, no verified snapshot), `migration.store_failed`.
pub async fn preview(
    ctx: &Arc<LifecycleContext>,
    host: &Arc<dyn RescueHost>,
    hash: &str,
    mode: Mode,
) -> Result<Preview, LifecycleError> {
    let prepared = {
        let (ctx, hash) = (Arc::clone(ctx), hash.to_owned());
        off_runtime(move || prepare(&ctx, &hash)).await??
    };
    let (snapshot, loss) = facts(host, &prepared).await?;
    let token = confirm_token(&prepared.hash, mode, snapshot.at, &loss);
    Ok(Preview {
        hash: prepared.hash.as_str().to_owned(),
        mode,
        snapshot,
        loss,
        token,
    })
}

/// Rescues the plan `hash` in `mode` under a run record. The record's start is written before
/// anything is touched (a rescue cannot start without audit); if the **finish** cannot be written
/// the rescue's own result is returned and the finish waits for
/// [`LifecycleContext::retry_pending_finishes`], with the warning code
/// `migration_audit.finish_pending` visible through [`LifecycleContext::pending_finishes`].
///
/// # Errors
/// As the body; `migration.store_failed` when the run cannot be begun.
pub async fn rescue(
    ctx: &Arc<LifecycleContext>,
    host: &Arc<dyn RescueHost>,
    hash: &str,
    mode: Mode,
    confirm: &str,
    second_approver: &str,
) -> Result<RescueReport, LifecycleError> {
    let (digest, plan) = {
        let (ctx, hash) = (Arc::clone(ctx), hash.to_owned());
        off_runtime(move || ctx.plan_audit(&hash)).await?
    };
    let mut spec = AuditSpec::new("rescue", digest, plan);
    spec.keep_finish_on_failure = true;
    ctx.audited(spec, |_| {
        rescue_body(ctx, host, hash, mode, confirm, second_approver)
    })
    .await
}

/// Rescues the plan `hash` in `mode`.
///
/// An unfinished rescue of the site (its journal says it started and neither opened the site nor
/// halted) is **resumed** when it restores the same snapshot (`resumed: true`, `attempt` one more),
/// and refuses a different one with `migration.rescue_refused` before the host runs anything.
///
/// `second_approver` is the person who confirmed besides the actor; it must be named and differ
/// from the actor (`migration.self_approval`). `confirm` must equal the token [`preview`] issues for
/// the loss window as it is now (`migration.rescue_refused`).
///
/// # Errors
/// As [`preview`], plus `migration.self_approval`, `migration.rescue_refused` (token, gate or hold),
/// `migration.apply_failed` (the host's run failed; the plan stays as it was).
async fn rescue_body(
    ctx: &Arc<LifecycleContext>,
    host: &Arc<dyn RescueHost>,
    hash: &str,
    mode: Mode,
    confirm: &str,
    second_approver: &str,
) -> Result<RescueReport, LifecycleError> {
    let prepared = {
        let (ctx, hash) = (Arc::clone(ctx), hash.to_owned());
        Arc::new(off_runtime(move || prepare(&ctx, &hash)).await??)
    };
    let second = second_approver.trim();
    if second.is_empty() || ctx.actor().principal().is_system {
        return Err(LifecycleError::NotPermitted);
    }
    if second == ctx.actor().principal().user {
        return Err(LifecycleError::SelfApproval);
    }
    let (snapshot, loss) = facts(host, &prepared).await?;
    let token = confirm_token(&prepared.hash, mode, snapshot.at, &loss);
    if token != confirm.trim() {
        return Err(LifecycleError::RescueRefused);
    }
    // Fail closed: the gate must be closed and the hold active before anything is touched.
    if !host.quiesced().await? {
        return Err(LifecycleError::RescueRefused);
    }
    // An unfinished rescue of this site is resumed when it is the same snapshot, and a different one
    // is refused: two rescues must never interleave on one site.
    let before = progress_of(host).await?;
    let unfinished = before.as_ref().filter(|p| p.in_progress());
    let resumed = match unfinished {
        Some(progress) if progress.snapshot.as_deref() == Some(snapshot.id.as_str()) => true,
        Some(_) => return Err(LifecycleError::RescueRefused),
        None => false,
    };
    let run = host.run(mode, &prepared.plan, &snapshot, ctx.now()).await?;
    let attempt = progress_of(host)
        .await?
        .map_or(0, |progress| progress.attempt);
    let (ctx2, prepared2) = (Arc::clone(ctx), Arc::clone(&prepared));
    off_runtime(move || {
        ctx2.plans().mark_state(
            &prepared2.hash,
            prepared2.state,
            prepared2.version,
            PlanState::Superseded,
        )
    })
    .await??;
    Ok(RescueReport {
        hash: prepared.hash.as_str().to_owned(),
        state: PlanState::Superseded,
        run,
        resumed,
        attempt,
    })
}

struct RescueHandler {
    ctx: Arc<LifecycleContext>,
    host: Arc<dyn RescueHost>,
}

impl OperationHandler for RescueHandler {
    fn handle<'a>(
        &'a self,
        call: &'a Validated,
    ) -> BoxFuture<'a, Result<HandlerOutput, LifecycleError>> {
        Box::pin(async move {
            check_site(&self.ctx, call)?;
            let hash = param(call, "plan_hash").ok_or(LifecycleError::UnknownPlan)?;
            let mode = param(call, "mode")
                .and_then(|m| Mode::parse(&m))
                .ok_or(LifecycleError::RescueRefused)?;
            let confirm = param(call, "confirm").ok_or(LifecycleError::RescueRefused)?;
            let second = param(call, "second_approver").ok_or(LifecycleError::NotPermitted)?;
            let report = rescue(&self.ctx, &self.host, &hash, mode, &confirm, &second).await?;
            let mut fields = BTreeMap::new();
            fields.insert("plan_hash".to_owned(), report.hash.clone());
            fields.insert("state".to_owned(), report.state.as_str().to_owned());
            Ok(HandlerOutput {
                message: format!("plan {} rescued: {}", report.hash, report.run.text),
                fields,
            })
        })
    }
}

/// Without a host the operation stays a fail-closed stub. See [`register_with`].
pub fn register(_ctx: &Arc<LifecycleContext>, _registry: &mut HandlerRegistry) {}

/// Registers the handler of `site.migrate.rescue` over `host`.
pub fn register_with(
    ctx: &Arc<LifecycleContext>,
    registry: &mut HandlerRegistry,
    host: Arc<dyn RescueHost>,
) {
    if let Some(id) = OpId::new("site.migrate.rescue") {
        registry.register(
            id,
            Arc::new(RescueHandler {
                ctx: Arc::clone(ctx),
                host,
            }),
        );
    }
}
