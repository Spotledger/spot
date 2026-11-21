//! The `rollouts` module (`WP-MIG-12.T2`): a fleet rollout of a release as an operation, over
//! `spot-fleet-rollout`.
//!
//! - **Endpoints** (contract `contracts/control-plane`): `GET/POST /cp/v1/rollouts`,
//!   `GET /cp/v1/rollouts/{id}`, `POST .../{id}/pause` and `.../{id}/resume`. Creating needs the
//!   signed command (the fleet approval: it binds the body, so the release and the waves are what
//!   was signed) and an `Idempotency-Key`; every change needs an operator scoped to all tenants; a
//!   rollout is visible only to an operator who may see every tenant in it (else `cp.not_found`).
//! - **The plan.** The body names the waves as tenant lists, the first is the canary. They become
//!   a `RolloutPlan` (soak = observation window, criteria "verified and not red", pause on any
//!   failure) run by the library's `Rollout` state machine.
//! - **Each wave** issues the site lifecycle operations (`plan`, `approve`, `rehearse`, `apply`) to
//!   every tenant host through [`TenantMigrator`], in batches of the concurrency limit, stops a
//!   tenant at its first failing step, then waits for the health gate ([`HealthGate`]): a tenant
//!   that turns red and was not red before its wave is a failure. A canary failure, any failure,
//!   a new red or a missed observation pauses the whole fleet; **a pause never ends by itself**:
//!   only `resume` continues, re-running the unfinished tenants of the started waves.
//! - **Restart.** The machine lives in memory, the record in the control schema: a rollout found
//!   running after a restart is paused; `resume` rebuilds the machine and skips finished tenants.
//! - **Epoch.** It advances when the first wave starts; tenants that verified at it are recorded
//!   in an `EpochLedger`; [`RolloutsState::epoch`] and [`RolloutsState::may_sync`] serve the number
//!   and the skew decision. Outcomes and rescue pointers are in the stored record (`rollout`).
//!
//! Time is an argument; no key, token or tenant data is put in an error, log line or audit record.
use super::tenants_kv::{Kv, PgKv, from_json, to_json};
use super::tenants_ports::leaked_runtime;
use super::tenants_state::{fill_random, finish, hex, idempotency_key, parse_tenant};
use crate::auth::{OperatorCall, sha256_hex};
use crate::{CpCode, CpCtx, CpError, TaskFn};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spot_core::Clock;
use spot_fleet_rollout::{
    Action, ConcurrencyLimit, Epoch, EpochLedger, FailureKind, Fleet, FleetNode, Health, Moment,
    NodeFailure, NodeId, NodeObservation, Observations, PauseOn, ReleaseId, Rollout, RolloutPlan,
    Seed, Selector, SkewLimit, SkewRefusal, Soak, SuccessCriteria, Wave, WaveSize,
};
use spot_mgmt_api::ports::BoxFuture;
use spot_tenancy::ControlStore;
use spot_tenancy::model::TenantState;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::watch;

/// Registration order: the module reads health through a port, so it needs none against `health`.
pub const ORDER: u32 = 150;

const ROLLOUT: &str = "rollout";
const ACTIVE: &str = "rollout_active";
const COMMAND: &str = "rollout_cmd";
const LEASE_MS: i64 = 300_000;

/// The `health` module's view of tenants, as the rollout's gate.
pub trait HealthGate: Send + Sync {
    /// The colour of each tenant now (a tenant the view does not know is amber). An `Err` is
    /// treated as red: an unreadable gate never lets a wave pass.
    fn colours<'a>(
        &'a self,
        tenants: &'a [String],
    ) -> BoxFuture<'a, Result<BTreeMap<String, Health>, CpError>>;
}

/// The site lifecycle operations of `spot-migration-ops` (`site.migrate.<step>`), in order.
pub const STEPS: [&str; 4] = ["plan", "approve", "rehearse", "apply"];

/// What a site reports after one step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepReport {
    /// Whether the site finished verify (set by `apply`).
    pub verified: bool,
    /// The migration epoch the site is at.
    pub epoch: u64,
    /// The `spot migrate rescue` command that undoes the step, when the site gives one.
    pub rescue: Option<String>,
}

/// Issues one site lifecycle operation to a tenant's host (a signed management command to its
/// management API or agent: built by the host binary).
pub trait TenantMigrator: Send + Sync {
    /// Runs `step` of the migration to `release` at `epoch` on `tenant`'s host.
    ///
    /// # Errors
    /// A short secret-free sentence (never a payload).
    fn run_step<'a>(
        &'a self,
        tenant: &'a str,
        release: &'a str,
        epoch: u64,
        step: &'static str,
    ) -> BoxFuture<'a, Result<StepReport, String>>;
}

static HEALTH_GATE: OnceLock<Arc<dyn HealthGate>> = OnceLock::new();
static MIGRATOR: OnceLock<Arc<dyn TenantMigrator>> = OnceLock::new();
static SHARED: OnceLock<Arc<RolloutsState>> = OnceLock::new();

/// Installs the health gate (`main` builds it over the `health` module's state before the service
/// is built); `false` when one is already set. Without one the module is not ready.
pub fn set_health_gate(gate: Arc<dyn HealthGate>) -> bool {
    HEALTH_GATE.set(gate).is_ok()
}

/// Installs the tenant-host migrator; `false` when one is already set. Without one the module is
/// not ready: a rollout never starts without a way to reach the hosts.
pub fn set_migrator(migrator: Arc<dyn TenantMigrator>) -> bool {
    MIGRATOR.set(migrator).is_ok()
}

/// The state of the registered module, for the heartbeat answer (epoch and skew decision).
#[must_use]
pub fn shared_state() -> Option<Arc<RolloutsState>> {
    SHARED.get().cloned()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Outcome {
    ok: bool,
    step: String,
    detail: String,
    rescue: Option<String>,
    at_ms: u64,
}

impl Outcome {
    fn new(ok: bool, step: &str, detail: &str, rescue: Option<String>, at_ms: u64) -> Self {
        let (step, detail) = (step.to_owned(), detail.to_owned());
        Self {
            ok,
            step,
            detail,
            rescue,
            at_ms,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Row {
    id: String,
    release: String,
    epoch: u64,
    seed: u64,
    digest: String,
    plan: RolloutPlan,
    waves: Vec<Vec<String>>,
    /// `planned`, `running`, `paused`, `done` or `failed`.
    state: String,
    /// What the machine said when it paused.
    pause: Option<String>,
    outcomes: BTreeMap<String, Outcome>,
    /// Tenants that were red before their wave started.
    red_before: BTreeSet<String>,
    /// Failures already handed to the state machine.
    reported: BTreeSet<String>,
    epoch_started: bool,
}

impl Row {
    fn machine(&self) -> Result<Rollout, CpError> {
        let mut nodes = Vec::new();
        for (index, wave) in self.waves.iter().enumerate() {
            for tenant in wave {
                let id = NodeId::new(tenant.as_str()).map_err(|_| internal("rollouts.node_id"))?;
                nodes.push(FleetNode::new(id, format!("wave-{index}")));
            }
        }
        Rollout::new(self.plan.clone(), &Fleet::new(nodes), Seed::new(self.seed))
            .map_err(|e| map_rollout_err(&e))
    }

    fn started(&self, index: usize) -> bool {
        let wave = self.waves.get(index);
        wave.is_some_and(|w| w.iter().any(|t| self.outcomes.contains_key(t)))
    }

    /// `pending` until a tenant ran, `failed` while one failed, `done` once all finished and the
    /// next wave started (or the rollout is done), `running` otherwise.
    fn wave_state(&self, index: usize) -> &'static str {
        let wave = self.waves.get(index).map_or(&[][..], Vec::as_slice);
        let ran: Vec<&Outcome> = wave.iter().filter_map(|t| self.outcomes.get(t)).collect();
        if ran.is_empty() {
            "pending"
        } else if ran.iter().any(|o| !o.ok) {
            "failed"
        } else if ran.len() == wave.len() && (self.started(index + 1) || self.state == "done") {
            "done"
        } else {
            "running"
        }
    }

    fn view(&self) -> Value {
        let waves: Vec<Value> = (0..self.waves.len())
            .zip(&self.waves)
            .map(|(i, w)| json!({ "index": i, "tenants": w, "state": self.wave_state(i) }))
            .collect();
        json!({ "id": self.id, "target_version": self.release, "state": self.state,
            "waves": waves, "workflow_id": self.id })
    }

    fn visible(&self, call: &OperatorCall) -> bool {
        self.waves.iter().flatten().all(|t| call.scope.allows(t))
    }
}

/// Everything the endpoints and the runner use.
pub struct RolloutDeps {
    /// The tenant registry (checks the tenants of a new rollout).
    pub tenants: Arc<dyn ControlStore + Send + Sync>,
    /// The health gate.
    pub health: Arc<dyn HealthGate>,
    /// Reaches the tenant hosts.
    pub migrator: Arc<dyn TenantMigrator>,
    /// The module's rows, leases and flags.
    pub kv: Arc<dyn Kv>,
    /// The observation window of every wave, in seconds.
    pub soak_secs: u64,
    /// The epochs behind the hub an edge may be at and still sync.
    pub skew_limit: u32,
    /// Tenants migrated at once.
    pub concurrency: usize,
    /// The milliseconds between runner passes.
    pub poll_ms: u64,
}

type Init = Box<dyn Fn() -> BoxFuture<'static, Result<RolloutDeps, CpError>> + Send + Sync>;

/// The shared state of the handlers and the runner.
pub struct RolloutsState {
    clock: Arc<dyn Clock>,
    deps: tokio::sync::OnceCell<Arc<RolloutDeps>>,
    init: Option<Init>,
    wake: tokio::sync::Notify,
    machines: Mutex<BTreeMap<String, Rollout>>,
    holder: String,
}

impl RolloutsState {
    /// A state over ready dependencies (tests, embedding).
    #[must_use]
    pub fn ready(clock: Arc<dyn Clock>, deps: RolloutDeps) -> Arc<Self> {
        let cell = tokio::sync::OnceCell::new_with(Some(Arc::new(deps)));
        Arc::new(Self::build(clock, cell, None))
    }

    fn build(
        clock: Arc<dyn Clock>,
        deps: tokio::sync::OnceCell<Arc<RolloutDeps>>,
        init: Option<Init>,
    ) -> Self {
        let mut bytes = [0u8; 8];
        fill_random(&mut bytes);
        Self {
            clock,
            deps,
            init,
            wake: tokio::sync::Notify::new(),
            machines: Mutex::new(BTreeMap::new()),
            holder: format!("cp-rollouts-{}", hex(&bytes)),
        }
    }

    async fn deps(&self) -> Result<Arc<RolloutDeps>, CpError> {
        let init = self.init.as_ref();
        self.deps
            .get_or_try_init(|| async {
                match init {
                    Some(init) => init().await.map(Arc::new),
                    None => Err(internal("rollouts.not_configured")),
                }
            })
            .await
            .cloned()
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now_utc().unix_timestamp())
            .unwrap_or(0)
            .saturating_mul(1000)
    }

    /// The migration epoch the control plane publishes (0 before the first rollout started).
    ///
    /// # Errors
    /// `cp.internal` when the module is not configured or its rows cannot be read.
    pub async fn epoch(&self) -> Result<u64, CpError> {
        let deps = self.deps().await?;
        Ok(load_ledger(&deps).await?.current().get())
    }

    /// Whether `tenant`'s host may sync at the published epoch (the library's rule over its
    /// recorded verified epoch and the configured skew limit). The inner `Err` is the refusal.
    ///
    /// # Errors
    /// `cp.internal` when the module is not configured or its rows cannot be read.
    pub async fn may_sync(&self, tenant: &str) -> Result<Result<(), SkewRefusal>, CpError> {
        let deps = self.deps().await?;
        let ledger = load_ledger(&deps).await?;
        let node = NodeId::new(tenant).map_err(|_| internal("rollouts.node_id"))?;
        Ok(ledger.may_sync(&node, ledger.current(), SkewLimit::new(deps.skew_limit)))
    }
}

fn internal(reason: &'static str) -> CpError {
    CpError::with_reason(CpCode::Internal, reason)
}

fn map_rollout_err(error: &spot_fleet_rollout::Error) -> CpError {
    use spot_fleet_rollout::Error as E;
    match error {
        E::NotPaused | E::NotRunning => CpError::with_reason(CpCode::Conflict, error.code()),
        E::InvalidPlan { .. } | E::EmptyWave { .. } | E::InvalidId { .. } => {
            CpError::invalid(&["waves"])
        }
        _ => CpError::with_reason(CpCode::Internal, error.code()),
    }
}

async fn load_row(deps: &RolloutDeps, id: &str) -> Result<Row, CpError> {
    match deps.kv.get(ROLLOUT, id).await? {
        Some(text) => from_json(&text),
        None => Err(CpError::new(CpCode::NotFound)),
    }
}

async fn save_row(deps: &RolloutDeps, row: &Row) -> Result<(), CpError> {
    deps.kv.put(ROLLOUT, &row.id, &to_json(row)?).await
}

async fn load_ledger(deps: &RolloutDeps) -> Result<EpochLedger, CpError> {
    match deps.kv.get("rollout_epoch", "ledger").await? {
        Some(text) => from_json(&text),
        None => Ok(EpochLedger::new()),
    }
}

/// Runs the four lifecycle steps on one tenant, stopping at the first failing one.
async fn migrate_one(
    migrator: Arc<dyn TenantMigrator>,
    tenant: String,
    release: String,
    epoch: u64,
    at_ms: u64,
) -> (String, Outcome) {
    let mut rescue = None;
    for step in STEPS {
        let report = match migrator.run_step(&tenant, &release, epoch, step).await {
            Ok(report) => report,
            Err(reason) => return (tenant, Outcome::new(false, step, &reason, rescue, at_ms)),
        };
        rescue = report.rescue.or(rescue);
        if step == "apply" {
            let ok = report.verified && report.epoch == epoch;
            let text = if ok {
                "migrated and verified"
            } else {
                "the site did not verify at the target epoch"
            };
            return (tenant, Outcome::new(ok, step, text, rescue, at_ms));
        }
    }
    (
        tenant.clone(),
        Outcome::new(false, "apply", "no step ran", rescue, at_ms),
    )
}

/// Dispatches `tenants` in batches of the concurrency limit and records every outcome. A tenant
/// that verified at the target epoch is recorded in the ledger.
async fn dispatch(
    st: &RolloutsState,
    deps: &RolloutDeps,
    row: &mut Row,
    tenants: Vec<String>,
) -> Result<(), CpError> {
    let now_ms = st.now_ms();
    let colours = deps.health.colours(&tenants).await.unwrap_or_default();
    for tenant in &tenants {
        if colours.get(tenant) == Some(&Health::Red) {
            row.red_before.insert(tenant.clone());
        } else {
            row.red_before.remove(tenant);
        }
    }
    let mut ledger = load_ledger(deps).await?;
    for batch in tenants.chunks(row.plan.concurrency.get()) {
        let mut set = tokio::task::JoinSet::new();
        for tenant in batch {
            let migrator = Arc::clone(&deps.migrator);
            set.spawn(migrate_one(
                migrator,
                tenant.clone(),
                row.release.clone(),
                row.epoch,
                now_ms,
            ));
        }
        while let Some(joined) = set.join_next().await {
            let (tenant, outcome) = joined.map_err(|_| internal("rollouts.worker"))?;
            if outcome.ok {
                if let Ok(node) = NodeId::new(tenant.as_str()) {
                    let _ = ledger.record_at_current(node, Moment::from_millis(now_ms));
                }
                row.reported.remove(&tenant);
            }
            row.outcomes.insert(tenant, outcome);
        }
    }
    deps.kv
        .put("rollout_epoch", "ledger", &to_json(&ledger)?)
        .await
}

/// The observations of one tick: every tenant with an outcome, with the health gate's colour, and
/// the failures not yet handed to the state machine.
async fn observations(deps: &RolloutDeps, row: &mut Row) -> Observations {
    let done: Vec<String> = row.outcomes.keys().cloned().collect();
    let colours = deps.health.colours(&done).await;
    let mut obs = Observations::new();
    for (tenant, outcome) in &row.outcomes {
        let Ok(node) = NodeId::new(tenant.as_str()) else {
            continue;
        };
        let health = match (&colours, row.red_before.contains(tenant)) {
            // An unreadable gate never lets a wave pass.
            (Err(_), _) => Health::Red,
            (Ok(map), before) => match (map.get(tenant).copied(), before) {
                (Some(Health::Red), false) => Health::Red,
                (Some(Health::Green), _) => Health::Green,
                _ => Health::Amber,
            },
        };
        obs.nodes
            .push(NodeObservation::new(node.clone(), health, 0, outcome.ok));
        if !outcome.ok && row.reported.insert(tenant.clone()) {
            let kind = if outcome.step == "apply" {
                FailureKind::Verify
            } else {
                FailureKind::Migrate
            };
            obs.failures
                .push(NodeFailure::new(node, kind, outcome.detail.clone()));
        }
    }
    obs
}

fn manual(note: &str) -> Option<String> {
    Some(format!("manual: {note}"))
}

/// Applies an operator command (and the restart pause) to the machine and the record.
async fn apply_command(
    st: &RolloutsState,
    deps: &RolloutDeps,
    row: &mut Row,
    machine: &mut Rollout,
    command: Option<&str>,
) -> Result<(), CpError> {
    let moment = Moment::from_millis(st.now_ms());
    match command {
        Some("pause") if row.state != "paused" => {
            let _ = machine.pause_now(moment, "paused by an operator");
            row.state = "paused".to_owned();
            row.pause = manual("paused by an operator");
        }
        Some("resume") if row.state == "paused" => {
            machine.resume(moment).map_err(|e| map_rollout_err(&e))?;
            row.state = "running".to_owned();
            row.pause = None;
            // The operator has acted: run again every unfinished tenant of a started wave, and let
            // a new failure of the same tenant pause again.
            let again: Vec<String> = (0..row.waves.len())
                .zip(&row.waves)
                .filter(|(i, _)| row.started(*i))
                .flat_map(|(_, w)| w.clone())
                .filter(|t| !row.outcomes.get(t).is_some_and(|o| o.ok))
                .collect();
            for tenant in &again {
                row.reported.remove(tenant);
                row.outcomes.remove(tenant);
            }
            dispatch(st, deps, row, again).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Advances one rollout by one tick: applies an operator command, asks the machine what to do,
/// dispatches the wave it starts and asks again with the new observations (so a canary failure
/// pauses in the same tick). Returns whether the rollout still needs the runner.
async fn tick(st: &RolloutsState, deps: &RolloutDeps, id: &str) -> Result<bool, CpError> {
    let mut row = load_row(deps, id).await?;
    if matches!(row.state.as_str(), "done" | "failed") {
        return Ok(false);
    }
    let known = st.machines.lock().ok().and_then(|mut m| m.remove(id));
    let restarted = known.is_none() && row.state != "planned";
    let mut machine = match known {
        Some(machine) => machine,
        None => row.machine()?,
    };
    let moment = Moment::from_millis(st.now_ms());
    if restarted {
        let _ = machine.pause_now(moment, "the control plane restarted");
        row.state = "paused".to_owned();
        row.pause = manual("the control plane restarted");
    }
    let command = deps.kv.get(COMMAND, id).await?;
    deps.kv.del(COMMAND, id).await?;
    apply_command(st, deps, &mut row, &mut machine, command.as_deref()).await?;
    if row.state == "planned" {
        row.state = "running".to_owned();
    }
    let mut needs_runner = true;
    for _ in 0..=row.waves.len() + 1 {
        if row.state == "paused" {
            break;
        }
        let obs = observations(deps, &mut row).await;
        match machine.advance(&obs, moment) {
            Action::StartWave { index, .. } => {
                if !row.epoch_started {
                    let mut ledger = load_ledger(deps).await?;
                    ledger.advance().map_err(|e| map_rollout_err(&e))?;
                    deps.kv
                        .put("rollout_epoch", "ledger", &to_json(&ledger)?)
                        .await?;
                    row.epoch_started = true;
                }
                // Tenants that already finished (before a restart) are not migrated again.
                let tenants: Vec<String> = row.waves.get(index).cloned().unwrap_or_default();
                let todo = tenants
                    .into_iter()
                    .filter(|t| !row.outcomes.get(t).is_some_and(|o| o.ok));
                let todo: Vec<String> = todo.collect();
                dispatch(st, deps, &mut row, todo).await?;
            }
            Action::Soak { .. } | Action::Resume { .. } => break,
            Action::Pause { pause } => {
                row.state = "paused".to_owned();
                row.pause = Some(format!("{}: {}", pause.reason.as_str(), pause.detail));
            }
            Action::Complete { .. } | Action::Abort { .. } => {
                row.state = if matches!(machine.state(), spot_fleet_rollout::State::Complete) {
                    "done"
                } else {
                    "failed"
                }
                .to_owned();
                needs_runner = false;
                break;
            }
        }
    }
    save_row(deps, &row).await?;
    if needs_runner {
        if let Ok(mut machines) = st.machines.lock() {
            machines.insert(id.to_owned(), machine);
        }
    } else {
        deps.kv.del(ACTIVE, id).await?;
    }
    Ok(needs_runner)
}

fn lease_id(id: &str) -> String {
    format!("rollout:{id}")
}

/// One runner pass: ticks every active rollout this instance can lease. Returns how many it
/// ticked.
pub async fn run_once(st: &Arc<RolloutsState>) -> usize {
    let Ok(deps) = st.deps().await else {
        return 0;
    };
    let Ok(rows) = deps.kv.list(ACTIVE).await else {
        return 0;
    };
    let mut ticked = 0;
    for (id, _) in rows {
        let now = i64::try_from(st.now_ms()).unwrap_or(i64::MAX);
        let Ok(true) = deps
            .kv
            .lease(&lease_id(&id), &st.holder, now, LEASE_MS)
            .await
        else {
            continue;
        };
        ticked += 1;
        if let Err(error) = tick(st, &deps, &id).await {
            tracing::warn!(reason = error.reason, "a rollout tick failed");
        }
        let _ = deps.kv.unlease(&lease_id(&id), &st.holder).await;
    }
    ticked
}

fn runner_task(st: Arc<RolloutsState>) -> TaskFn {
    Box::new(
        move |mut stop: watch::Receiver<bool>| -> BoxFuture<'static, ()> {
            Box::pin(async move {
                loop {
                    run_once(&st).await;
                    let poll = match st.deps().await {
                        Ok(deps) => Duration::from_millis(deps.poll_ms),
                        Err(_) => Duration::from_secs(5),
                    };
                    tokio::select! {
                        changed = stop.changed() => {
                            if changed.is_err() || *stop.borrow() {
                                break;
                            }
                        }
                        () = st.wake.notified() => {}
                        () = tokio::time::sleep(poll) => {}
                    }
                }
            })
        },
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaveBody {
    tenants: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    target_version: String,
    waves: Vec<WaveBody>,
}

fn require_all(call: &OperatorCall) -> Result<(), CpError> {
    if call.scope.is_all() {
        Ok(())
    } else {
        Err(CpError::with_reason(CpCode::Forbidden, "cp.scope_not_all"))
    }
}

/// The plan of a body: the first wave is the canary, every wave soaks for the observation window.
fn build_plan(
    release: &str,
    epoch: u64,
    waves: usize,
    deps: &RolloutDeps,
) -> Result<RolloutPlan, CpError> {
    let release = ReleaseId::new(release).map_err(|_| CpError::invalid(&["target_version"]))?;
    let criteria = SuccessCriteria {
        min_health: Health::Amber,
        ..SuccessCriteria::default()
    };
    let planned: Vec<Wave> = (0..waves)
        .map(|i| {
            let selector = Selector::all().with_groups([format!("wave-{i}")]);
            let wave = Wave::new(
                format!("wave-{i}"),
                selector,
                WaveSize::All,
                Soak::from_secs(deps.soak_secs),
            );
            let wave = wave.with_criteria(criteria);
            if i == 0 { wave.as_canary() } else { wave }
        })
        .collect();
    let mut plan = RolloutPlan::new(release, Epoch::new(epoch), planned);
    plan.pause_on = PauseOn::default();
    plan.skew_limit = SkewLimit::new(deps.skew_limit);
    plan.concurrency = ConcurrencyLimit::new(deps.concurrency).map_err(|e| map_rollout_err(&e))?;
    plan.validate().map_err(|e| map_rollout_err(&e))?;
    Ok(plan)
}

async fn create(
    State(st): State<Arc<RolloutsState>>,
    Extension(call): Extension<OperatorCall>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let result = create_inner(&st, &call, &headers, &body).await;
    finish(&call, result)
}

async fn create_inner(
    st: &Arc<RolloutsState>,
    call: &OperatorCall,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(StatusCode, Value), CpError> {
    require_all(call)?;
    let deps = st.deps().await?;
    let key = idempotency_key(headers)?;
    let req: CreateBody = serde_json::from_slice(body).map_err(|_| CpError::invalid(&["body"]))?;
    let waves: Vec<Vec<String>> = req.waves.into_iter().map(|w| w.tenants).collect();
    let mut seen = BTreeSet::new();
    let shaped = (1..=50).contains(&waves.len())
        && waves.iter().all(|w| (1..=500).contains(&w.len()))
        && waves.iter().flatten().all(|t| seen.insert(t.clone()));
    if !shaped {
        return Err(CpError::invalid(&["waves"]));
    }
    // The id derives from the principal and the key, so a retry (also after a crash) finds the
    // same record; the same key with another body is `cp.replayed`.
    let digest = sha256_hex(body);
    let id = format!(
        "ro-{}",
        &sha256_hex(format!("{}|{key}", call.principal).as_bytes())[..16]
    );
    if let Some(text) = deps.kv.get(ROLLOUT, &id).await? {
        let existing: Row = from_json(&text)?;
        if existing.digest != digest {
            return Err(CpError::new(CpCode::Replayed));
        }
        return Ok((StatusCode::CREATED, json!({ "data": existing.view() })));
    }
    let view = create_record(&deps, call, &req.target_version, waves, (&id, digest)).await?;
    st.wake.notify_one();
    Ok((StatusCode::CREATED, json!({ "data": view })))
}

async fn create_record(
    deps: &Arc<RolloutDeps>,
    _call: &OperatorCall,
    target_version: &str,
    waves: Vec<Vec<String>>,
    (id, digest): (&str, String),
) -> Result<Value, CpError> {
    for (_, text) in deps.kv.list(ROLLOUT).await? {
        let other: Row = from_json(&text)?;
        if matches!(other.state.as_str(), "planned" | "running" | "paused") {
            return Err(CpError::with_reason(
                CpCode::Conflict,
                "rollouts.one_at_a_time",
            ));
        }
    }
    let registry = Arc::clone(&deps.tenants);
    let wanted: Vec<String> = waves.iter().flatten().cloned().collect();
    let states = tokio::task::spawn_blocking(move || {
        let bad = || CpError::invalid(&["waves"]);
        wanted
            .iter()
            .map(|t| {
                registry
                    .get(&parse_tenant(t).map_err(|_| bad())?)
                    .map(|x| x.state)
                    .map_err(|_| bad())
            })
            .collect::<Result<Vec<TenantState>, CpError>>()
    })
    .await
    .map_err(|_| internal("rollouts.worker"))??;
    if states.iter().any(|s| *s != TenantState::Active) {
        return Err(CpError::invalid(&["waves"]));
    }
    let epoch = load_ledger(deps).await?.current().get().saturating_add(1);
    let row = Row {
        id: id.to_owned(),
        release: target_version.to_owned(),
        epoch,
        seed: u64::from_str_radix(id.get(3..).unwrap_or("0"), 16).unwrap_or(0),
        digest,
        plan: build_plan(target_version, epoch, waves.len(), deps)?,
        waves,
        state: "planned".to_owned(),
        pause: None,
        outcomes: BTreeMap::new(),
        red_before: BTreeSet::new(),
        reported: BTreeSet::new(),
        epoch_started: false,
    };
    // The machine must build from the plan before anything is stored.
    row.machine()?;
    save_row(deps, &row).await?;
    deps.kv.put(ACTIVE, id, "1").await?;
    Ok(row.view())
}

async fn list(
    State(st): State<Arc<RolloutsState>>,
    Extension(call): Extension<OperatorCall>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    let result = async {
        let deps = st.deps().await?;
        let limit = q
            .get("limit")
            .and_then(|v| v.parse().ok())
            .unwrap_or(50)
            .clamp(1, 100);
        let mut page = Vec::new();
        let mut more = false;
        for (id, text) in deps.kv.list(ROLLOUT).await? {
            if q.get("cursor").is_some_and(|c| id.as_str() <= c.as_str()) {
                continue;
            }
            let row: Row = from_json(&text)?;
            if !row.visible(&call) {
                continue;
            }
            if page.len() == limit {
                more = true;
                break;
            }
            page.push(row.view());
        }
        let next = if more {
            page.last()
                .and_then(|r| r["id"].as_str())
                .map(str::to_owned)
        } else {
            None
        };
        Ok((StatusCode::OK, json!({ "data": page, "next_cursor": next })))
    }
    .await;
    finish(&call, result)
}

async fn visible_row(
    st: &RolloutsState,
    call: &OperatorCall,
    id: &str,
) -> Result<(Arc<RolloutDeps>, Row), CpError> {
    let deps = st.deps().await?;
    let row = load_row(&deps, id).await?;
    if row.visible(call) {
        Ok((deps, row))
    } else {
        Err(CpError::new(CpCode::NotFound))
    }
}

async fn get_one(
    State(st): State<Arc<RolloutsState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = async {
        let (_, row) = visible_row(&st, &call, &id).await?;
        Ok((StatusCode::OK, json!({ "data": row.view() })))
    }
    .await;
    finish(&call, result)
}

/// Queues `pause` or `resume`, ticks the rollout here when no runner holds it, and answers with
/// the record as it is then.
async fn command(
    st: &Arc<RolloutsState>,
    call: &OperatorCall,
    id: &str,
    verb: &'static str,
) -> Result<(StatusCode, Value), CpError> {
    require_all(call)?;
    let (deps, row) = visible_row(st, call, id).await?;
    let allowed = if verb == "pause" {
        matches!(row.state.as_str(), "running" | "planned")
    } else {
        row.state == "paused"
    };
    if !allowed {
        return Err(CpError::with_reason(
            CpCode::Conflict,
            "rollouts.wrong_state",
        ));
    }
    deps.kv.put(COMMAND, id, verb).await?;
    let now = i64::try_from(st.now_ms()).unwrap_or(i64::MAX);
    if deps
        .kv
        .lease(&lease_id(id), &st.holder, now, LEASE_MS)
        .await?
    {
        let result = tick(st, &deps, id).await;
        let _ = deps.kv.unlease(&lease_id(id), &st.holder).await;
        result?;
    }
    st.wake.notify_one();
    let row = load_row(&deps, id).await?;
    Ok((StatusCode::OK, json!({ "data": row.view() })))
}

async fn pause(
    State(st): State<Arc<RolloutsState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = command(&st, &call, &id, "pause").await;
    finish(&call, result)
}

async fn resume(
    State(st): State<Arc<RolloutsState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = command(&st, &call, &id, "resume").await;
    finish(&call, result)
}

/// The routes of the module, for a state.
pub fn routes(state: Arc<RolloutsState>) -> Router {
    Router::new()
        .route("/cp/v1/rollouts", post(create).get(list))
        .route("/cp/v1/rollouts/{id}", get(get_one))
        .route("/cp/v1/rollouts/{id}/pause", post(pause))
        .route("/cp/v1/rollouts/{id}/resume", post(resume))
        .with_state(state)
}

/// Mounts the module over a prepared state: routes, the runner and the readiness check.
pub fn register_with(ctx: &mut CpCtx, state: Arc<RolloutsState>) {
    ctx.operator_routes(routes(Arc::clone(&state)));
    ctx.add_task(runner_task(Arc::clone(&state)));
    let _ = SHARED.set(Arc::clone(&state));
    let ready = Arc::clone(&state);
    ctx.add_readiness(Arc::new(move || -> BoxFuture<'static, bool> {
        let ready = Arc::clone(&ready);
        Box::pin(async move { ready.deps().await.is_ok() })
    }));
}

/// Registers the module over the control database. Without the health gate and the migrator
/// ([`set_health_gate`], [`set_migrator`]) it mounts nothing and readiness reports it not ready.
///
/// # Errors
/// `cp.internal` when the control database is missing.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let (Some(health), Some(migrator)) = (HEALTH_GATE.get().cloned(), MIGRATOR.get().cloned())
    else {
        tracing::warn!(
            "no health gate or migrator is installed: the rollouts module is not mounted"
        );
        ctx.add_readiness(Arc::new(|| -> BoxFuture<'static, bool> {
            Box::pin(async { false })
        }));
        return Ok(());
    };
    let pool = ctx
        .pool
        .clone()
        .ok_or_else(|| internal("rollouts.no_control_database"))?;
    let clock = Arc::clone(&ctx.clock);
    let init: Init = Box::new(
        move || -> BoxFuture<'static, Result<RolloutDeps, CpError>> {
            let (pool, health, migrator) =
                (pool.clone(), Arc::clone(&health), Arc::clone(&migrator));
            Box::pin(async move {
                let faults = Arc::new(spot_tenancy_pg::Faults::new());
                let store =
                    spot_tenancy_pg::PgControlStore::new(pool.clone(), leaked_runtime(), faults);
                store
                    .prepare()
                    .await
                    .map_err(|_| internal("rollouts.control_schema"))?;
                Ok(RolloutDeps {
                    tenants: Arc::new(store),
                    health,
                    migrator,
                    kv: Arc::new(PgKv::new(pool)),
                    soak_secs: 300,
                    skew_limit: 1,
                    concurrency: 8,
                    poll_ms: 5_000,
                })
            })
        },
    );
    let state = Arc::new(RolloutsState::build(
        clock,
        tokio::sync::OnceCell::new(),
        Some(init),
    ));
    register_with(ctx, state);
    Ok(())
}
