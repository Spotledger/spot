//! The `moves` module (`WP-CTL-02.T3`): a per-tenant move between clusters as a controlled,
//! resumable workflow over `spot-tenant-move`.
//!
//! - **Endpoints** (contract `contracts/control-plane`): `POST /cp/v1/moves` (signed command and
//!   `Idempotency-Key`), `GET /cp/v1/moves/{id}`, `POST /cp/v1/moves/{id}/cancel`. The move id is
//!   `mv-<tenant id>` (the library's store keeps one record per tenant), so the scope check needs
//!   no lookup; an out-of-scope or malformed id is `cp.not_found`, like a missing one.
//! - **The engine is the library's** (`MoveEngine`: state machine, chunk checksums, fingerprint
//!   comparison before the placement flip, abort). This module adds the request, a background
//!   runner that resumes every unfinished move at start under a lease, cancel and the host ports.
//! - **Refusals come first.** A tenant that is not `active` or `suspended`, one with a migration or
//!   restore running ([`TenantBusy`]), an unknown or full target and a residency refusal never
//!   start a move: nothing is frozen before they pass.
//! - **Order.** The source is frozen before the first chunk is exported; the placement update is
//!   the single commit point and comes only after the fingerprints match. After the cut-over the
//!   source is not reopened: the engine's cleanup `resume` becomes
//!   [`SourceQuarantine::keep_read_only`] and the end of the quarantine is recorded.
//! - **Failure.** Steps are retried by the runner (every port step is idempotent, the engine
//!   checkpoints after each transition and chunk). A failure that cannot be retried, or the attempt
//!   limit, before the cut-over aborts the move: source resumed, placement restored. After the
//!   cut-over the target stays authoritative and the source frozen.
//! - **Cross-tenant.** [`TenantBoundTransfer`] refuses an import whose package manifest names
//!   another tenant.
//! - **Host ports** are signed management commands to the tenant host or its agent, built by a
//!   factory the host binary registers ([`set_host_ports_factory`]); without one the module is
//!   not ready: a move never starts without a way to freeze the source.
//!
//! Time is an argument; no key, token, path or tenant data is put in an error, log line or audit.
use super::tenants::TenantsConfig;
use super::tenants_kv::{Kv, PgKv, from_json, store_failed, to_json};
use super::tenants_ports::leaked_runtime;
use super::tenants_state::{IdemRow, finish, idempotency_key, ms_text, parse_tenant};
use crate::auth::{OperatorCall, sha256_hex};
use crate::{CpCode, CpCtx, CpError, TaskFn};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Router};
use deadpool_postgres::Pool;
use serde::Deserialize;
use serde_json::{Value, json};
use spot_core::Clock;
use spot_mgmt_api::ports::BoxFuture;
use spot_tenancy::model::TenantState;
use spot_tenancy::{Cluster, ClusterId, ControlStore, DatabaseId, Placement, Region, TenantId};
use spot_tenant_move::{
    ControlStorePlacement, Error as MoveError, FingerprintSource, MoveEngine, MoveId, MoveMode,
    MovePlan, MoveRecord, MoveState, MoveStore, PlacementPort, QuiescePort, ResidencyPolicy,
    RoutingCacheInvalidator, TenancyResidency, TransferPort,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::watch;

type MoveFut<'a, T> = spot_tenant_move::BoxFuture<'a, Result<T, MoveError>>;

/// Registration order: after `tenants` (20) and `licensing` (30).
pub const ORDER: u32 = 60;

/// The control-schema DDL of this module: the shared key-value and lease tables (the same
/// statements as the `tenants` module, so either may create them first).
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_tenants_kv (ns TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL, PRIMARY KEY (ns, k))",
    "CREATE TABLE IF NOT EXISTS cp_tenants_lease (id TEXT PRIMARY KEY, holder TEXT NOT NULL, until BIGINT NOT NULL)",
];

/// How long a move lease lasts (milliseconds).
const LEASE_MS: i64 = 300_000;
/// Rows of the module in the key-value store.
const ACTIVE: &str = "move_active";
const CANCEL: &str = "move_cancel";
const NOTE: &str = "move_note";
const QUARANTINE: &str = "move_quarantine";
const IDEM: &str = "move_idem";

/// Audit reason when a migration is being applied on the tenant's host.
pub const BUSY_MIGRATION: &str = "moves.migration_running";
/// Audit reason when a restore is running on the tenant's host.
pub const BUSY_RESTORE: &str = "moves.restore_running";

/// Asks the tenant's host whether a migration or restore is running.
pub trait TenantBusy: Send + Sync {
    /// `BUSY_MIGRATION` or `BUSY_RESTORE` while the host runs one; `Err` when the host cannot say
    /// (the request is then refused: an unknown state is never treated as idle).
    fn busy<'a>(
        &'a self,
        tenant: &'a TenantId,
    ) -> BoxFuture<'a, Result<Option<&'static str>, CpError>>;
}

/// Keeps the source of a finished cut-over frozen and read-only for the quarantine period.
pub trait SourceQuarantine: Send + Sync {
    /// Keeps the source of `plan` read-only until `until_ms` (Unix milliseconds): the gate stays
    /// closed and the side-effect hold stays begun. Idempotent.
    ///
    /// # Errors
    /// A fixed secret-free reason.
    fn keep_read_only<'a>(
        &'a self,
        plan: &'a MovePlan,
        move_id: &'a MoveId,
        until_ms: u64,
    ) -> BoxFuture<'a, Result<(), &'static str>>;
}

/// The host-facing ports, built over the signed management channel by the host binary.
pub struct HostPorts {
    /// The persisted move records (`spot_tenant_move_pg::PgMoveStore` over the shared bridge in
    /// production: it is synchronous and its construction belongs to the host binary).
    pub moves: Arc<dyn MoveStore>,
    /// Closes and opens the source's maintenance gate and side-effect hold.
    pub quiesce: Arc<dyn QuiescePort>,
    /// Exports the source and imports into the target.
    pub transfer: Arc<dyn TransferPort>,
    /// Measures the invariant fingerprint on either side.
    pub fingerprints: Arc<dyn FingerprintSource>,
    /// The migration and restore check.
    pub busy: Arc<dyn TenantBusy>,
    /// Keeps the source read-only after the cut-over.
    pub quarantine: Arc<dyn SourceQuarantine>,
}

/// What a [`HostPortsFactory`] builds the ports from.
pub struct HostPortInputs {
    /// The control database pool.
    pub pool: Pool,
    /// The placement databases (cluster, database and region of every possible target).
    pub clusters: Vec<Cluster>,
    /// The runtime the synchronous parts block on.
    pub runtime: Arc<tokio::runtime::Runtime>,
}

/// Builds the host ports (they need the management-API client and the host trust store, which
/// this module does not own).
pub type HostPortsFactory = fn(HostPortInputs) -> Result<HostPorts, CpError>;

static HOST_PORTS_FACTORY: OnceLock<HostPortsFactory> = OnceLock::new();

/// Registers the factory the production wiring builds the host ports with; `false` when one is
/// already registered.
pub fn set_host_ports_factory(factory: HostPortsFactory) -> bool {
    HOST_PORTS_FACTORY.set(factory).is_ok()
}

/// The file name of the snapshot manifest, the first file of every package.
const MANIFEST_FILE: &str = "snapshot.toml";

/// Refuses the restore of a package whose manifest names another tenant. The chunks only reach a
/// staging area; the check runs in `finish_import`, before anything is restored into the target:
/// it reads the manifest chunks back through the inner port (`export_chunk` from the start until
/// the first non-manifest file) and compares the `tenant` the package names with the plan's.
pub struct TenantBoundTransfer {
    inner: Arc<dyn TransferPort>,
}

/// The most manifest chunks read when checking a package (a manifest is far smaller).
const MAX_MANIFEST_CHUNKS: u64 = 64;

impl TenantBoundTransfer {
    /// Wraps `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn TransferPort>) -> Self {
        Self { inner }
    }

    async fn check(&self, plan: &MovePlan) -> Result<(), MoveError> {
        let refuse = |reason: &'static str| MoveError::TransferFailed {
            tenant: plan.tenant.to_string(),
            reason,
        };
        let mut bytes: Vec<u8> = Vec::new();
        let mut after: Option<u64> = None;
        for _ in 0..MAX_MANIFEST_CHUNKS {
            let Some(chunk) = self.inner.export_chunk(plan, after).await? else {
                break;
            };
            if chunk.file != MANIFEST_FILE {
                break;
            }
            bytes.extend_from_slice(&chunk.bytes);
            after = Some(chunk.sequence);
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| refuse("the package manifest is not text"))?;
        let manifest: toml::Table =
            toml::from_str(text).map_err(|_| refuse("the package manifest cannot be read"))?;
        let named = manifest.get("tenant").and_then(toml::Value::as_str);
        if named == Some(plan.tenant.to_string().as_str()) {
            Ok(())
        } else {
            Err(refuse("the package names another tenant"))
        }
    }
}

impl TransferPort for TenantBoundTransfer {
    fn export_chunk<'a>(
        &'a self,
        plan: &'a MovePlan,
        after: Option<u64>,
    ) -> MoveFut<'a, Option<spot_tenant_move::Chunk>> {
        self.inner.export_chunk(plan, after)
    }

    fn import_chunk<'a>(
        &'a self,
        plan: &'a MovePlan,
        chunk: &'a spot_tenant_move::Chunk,
    ) -> MoveFut<'a, ()> {
        self.inner.import_chunk(plan, chunk)
    }

    fn finish_import<'a>(&'a self, plan: &'a MovePlan) -> MoveFut<'a, ()> {
        Box::pin(async move {
            self.check(plan).await?;
            self.inner.finish_import(plan).await
        })
    }

    fn cleanup<'a>(&'a self, plan: &'a MovePlan) -> MoveFut<'a, ()> {
        self.inner.cleanup(plan)
    }
}

/// Turns the engine's cleanup `resume` after a cut-over into a quarantine of the source: before
/// the cut-over (abort) it resumes the source for real.
struct QuarantineQuiesce {
    inner: Arc<dyn QuiescePort>,
    moves: Arc<dyn MoveStore>,
    quarantine: Arc<dyn SourceQuarantine>,
    kv: Arc<dyn Kv>,
    quarantine_ms: u64,
}

impl QuiescePort for QuarantineQuiesce {
    fn quiesce<'a>(&'a self, plan: &'a MovePlan, move_id: &'a MoveId, now: u64) -> MoveFut<'a, ()> {
        self.inner.quiesce(plan, move_id, now)
    }

    fn resume<'a>(&'a self, plan: &'a MovePlan, move_id: &'a MoveId, now: u64) -> MoveFut<'a, ()> {
        Box::pin(async move {
            let after_cut_over = matches!(
                self.moves.get(&plan.tenant),
                Ok(Some(record)) if record.state == MoveState::CutOver
            );
            if !after_cut_over {
                return self.inner.resume(plan, move_id, now).await;
            }
            let until = now.saturating_add(self.quarantine_ms);
            self.quarantine
                .keep_read_only(plan, move_id, until)
                .await
                .map_err(|reason| MoveError::QuiesceFailed {
                    tenant: plan.tenant.to_string(),
                    reason,
                })?;
            self.kv
                .put(QUARANTINE, &plan.tenant.to_string(), &until.to_string())
                .await
                .map_err(|_| MoveError::QuiesceFailed {
                    tenant: plan.tenant.to_string(),
                    reason: "the quarantine end could not be recorded",
                })
        })
    }
}

/// The control plane keeps no routing cache of its own: hosts read the placement from the
/// registry, so the invalidation after a flip has nothing to do.
struct NoCaches;

impl RoutingCacheInvalidator for NoCaches {
    fn invalidate(&self, _tenant: &TenantId) -> Result<(), &'static str> {
        Ok(())
    }
}

/// Everything the endpoints and the runner use.
pub struct MoveDeps {
    /// The tenant registry (reads the tenant and its placement).
    pub tenants: Arc<dyn ControlStore + Send + Sync>,
    /// The persisted move records.
    pub moves: Arc<dyn MoveStore>,
    /// The source's gate and side-effect hold.
    pub quiesce: Arc<dyn QuiescePort>,
    /// The export and import.
    pub transfer: Arc<dyn TransferPort>,
    /// The placement map: the single commit point.
    pub placement: Arc<dyn PlacementPort>,
    /// The residency policy.
    pub residency: Arc<dyn ResidencyPolicy>,
    /// The fingerprint comparison before the cut-over.
    pub fingerprints: Arc<dyn FingerprintSource>,
    /// The migration and restore check.
    pub busy: Arc<dyn TenantBusy>,
    /// Keeps the source read-only after the cut-over.
    pub quarantine: Arc<dyn SourceQuarantine>,
    /// The module's rows, leases and flags.
    pub kv: Arc<dyn Kv>,
    /// The possible targets.
    pub clusters: Vec<Cluster>,
    /// Whether the source is closed for the whole move or only until the dump.
    pub mode: MoveMode,
    /// How long the source stays frozen and read-only after the cut-over (milliseconds).
    pub quarantine_ms: u64,
    /// The milliseconds between runner passes.
    pub poll_ms: u64,
    /// The attempts a failing move gets per process.
    pub max_attempts: u32,
}

/// The dependencies with the wrappers built once.
struct Wired {
    deps: MoveDeps,
    transfer: TenantBoundTransfer,
    quiesce: QuarantineQuiesce,
}

impl Wired {
    fn new(deps: MoveDeps) -> Self {
        let transfer = TenantBoundTransfer::new(Arc::clone(&deps.transfer));
        let quiesce = QuarantineQuiesce {
            inner: Arc::clone(&deps.quiesce),
            moves: Arc::clone(&deps.moves),
            quarantine: Arc::clone(&deps.quarantine),
            kv: Arc::clone(&deps.kv),
            quarantine_ms: deps.quarantine_ms,
        };
        Self {
            deps,
            transfer,
            quiesce,
        }
    }

    fn engine(&self) -> MoveEngine<'_> {
        MoveEngine {
            store: &*self.deps.moves,
            transfer: &self.transfer,
            quiesce: &self.quiesce,
            placement: &*self.deps.placement,
            residency: &*self.deps.residency,
            fingerprints: &*self.deps.fingerprints,
        }
    }
}

type Init = Box<dyn Fn() -> BoxFuture<'static, Result<MoveDeps, CpError>> + Send + Sync>;

/// The shared state of the handlers and the runner.
pub struct MovesState {
    clock: Arc<dyn Clock>,
    wired: tokio::sync::OnceCell<Arc<Wired>>,
    init: Option<Init>,
    wake: tokio::sync::Notify,
    attempts: Mutex<BTreeMap<String, u32>>,
    holder: String,
}

impl MovesState {
    /// A state over ready dependencies (tests, embedding).
    #[must_use]
    pub fn ready(clock: Arc<dyn Clock>, deps: MoveDeps) -> Arc<Self> {
        let cell = tokio::sync::OnceCell::new_with(Some(Arc::new(Wired::new(deps))));
        Arc::new(Self::build(clock, cell, None))
    }

    fn lazy(clock: Arc<dyn Clock>, init: Init) -> Arc<Self> {
        Arc::new(Self::build(clock, tokio::sync::OnceCell::new(), Some(init)))
    }

    fn build(
        clock: Arc<dyn Clock>,
        wired: tokio::sync::OnceCell<Arc<Wired>>,
        init: Option<Init>,
    ) -> Self {
        let mut bytes = [0u8; 8];
        super::tenants_state::fill_random(&mut bytes);
        Self {
            clock,
            wired,
            init,
            wake: tokio::sync::Notify::new(),
            attempts: Mutex::new(BTreeMap::new()),
            holder: format!("cp-moves-{}", super::tenants_state::hex(&bytes)),
        }
    }

    async fn wired(&self) -> Result<Arc<Wired>, CpError> {
        let init = self.init.as_ref();
        self.wired
            .get_or_try_init(|| async {
                match init {
                    Some(init) => init().await.map(|deps| Arc::new(Wired::new(deps))),
                    None => Err(CpError::with_reason(
                        CpCode::Internal,
                        "moves.not_configured",
                    )),
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

    fn bump_attempts(&self, tenant: &str) -> u32 {
        match self.attempts.lock() {
            Ok(mut map) => {
                let slot = map.entry(tenant.to_owned()).or_insert(0);
                *slot = slot.saturating_add(1);
                *slot
            }
            Err(_) => u32::MAX,
        }
    }

    fn clear_attempts(&self, tenant: &str) {
        if let Ok(mut map) = self.attempts.lock() {
            map.remove(tenant);
        }
    }
}

fn internal(reason: &'static str) -> CpError {
    CpError::with_reason(CpCode::Internal, reason)
}

fn move_id_of(tenant: &TenantId) -> String {
    format!("mv-{tenant}")
}

fn lease_id(tenant: &str) -> String {
    format!("move:{tenant}")
}

/// The tenant a move id names, checked against the operator's scope first.
fn tenant_of_move(call: &OperatorCall, id: &str) -> Result<TenantId, CpError> {
    let tenant = id
        .strip_prefix("mv-")
        .ok_or_else(|| CpError::new(CpCode::NotFound))?;
    call.scope.require(tenant)?;
    parse_tenant(tenant)
}

fn map_move_err(error: &MoveError) -> CpError {
    let reason = error.code();
    match error {
        MoveError::NotFound { .. } => CpError::new(CpCode::NotFound),
        MoveError::AlreadyExists { .. }
        | MoveError::WrongState { .. }
        | MoveError::AbortTooLate { .. } => CpError::with_reason(CpCode::Conflict, reason),
        MoveError::ResidencyRefused { .. } => CpError::with_reason(CpCode::Forbidden, reason),
        MoveError::InvalidPlan { .. } | MoveError::InvalidMoveId { .. } => {
            CpError::invalid(&["body"])
        }
        _ => CpError::with_reason(CpCode::Internal, reason),
    }
}

/// Errors a retry cannot fix.
fn permanent(error: &MoveError) -> bool {
    matches!(
        error,
        MoveError::FingerprintMismatch { .. }
            | MoveError::ChunkCorrupt { .. }
            | MoveError::ResidencyRefused { .. }
            | MoveError::InvalidPlan { .. }
            | MoveError::WrongState { .. }
    )
}

fn api_state(record: &MoveRecord, noted: bool) -> &'static str {
    if noted && record.state != MoveState::Cleaned {
        return "failed";
    }
    match record.state {
        MoveState::Planned => "requested",
        MoveState::Quiesced => "preparing",
        MoveState::Exported | MoveState::Imported => "copying",
        MoveState::Verified | MoveState::CutOver => "cutover",
        MoveState::Cleaned => "done",
        MoveState::Aborted => "cancelled",
    }
}

async fn view(deps: &MoveDeps, record: &MoveRecord) -> Result<Value, CpError> {
    let tenant = record.plan.tenant.to_string();
    let noted = deps.kv.get(NOTE, &tenant).await?.is_some();
    Ok(json!({
        "id": move_id_of(&record.plan.tenant),
        "tenant_id": tenant,
        "to_cluster": record.plan.to.cluster.as_str(),
        "state": api_state(record, noted),
        "workflow_id": move_id_of(&record.plan.tenant),
        "created_at": ms_text(record.created_at),
    }))
}

/// Runs `f` on a blocking thread with the module's runtime: the PostgreSQL stores reach their
/// bridge from synchronous code and must not run on an async worker.
async fn blocking<T: Send + 'static>(
    wired: &Arc<Wired>,
    f: impl FnOnce(&Wired, &tokio::runtime::Runtime) -> T + Send + 'static,
) -> Result<T, CpError> {
    let wired = Arc::clone(wired);
    let runtime = leaked_runtime();
    tokio::task::spawn_blocking(move || f(&wired, &runtime))
        .await
        .map_err(|_| internal("moves.worker"))
}

fn status_of(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::OK)
}

/// Stores the request digest under the key, or returns the first call's row; the same key with
/// another body is `cp.replayed`.
async fn idem_begin(deps: &MoveDeps, key: &str, row: IdemRow) -> Result<IdemRow, CpError> {
    if deps.kv.put_new(IDEM, key, &to_json(&row)?).await? {
        return Ok(row);
    }
    let text = deps.kv.get(IDEM, key).await?.ok_or_else(store_failed)?;
    let existing: IdemRow = from_json(&text)?;
    if existing.digest == row.digest {
        Ok(existing)
    } else {
        Err(CpError::new(CpCode::Replayed))
    }
}

/// Runs one move forward from its checkpoint to a terminal state, honouring a cancel request or a
/// recorded failure between transitions. Called on a blocking thread.
fn drive(
    wired: &Wired,
    runtime: &tokio::runtime::Runtime,
    tenant: &TenantId,
    now: u64,
) -> Result<MoveRecord, MoveError> {
    let engine = wired.engine();
    let text = tenant.to_string();
    loop {
        let record = engine.load(tenant)?;
        if record.is_terminal() {
            return Ok(record);
        }
        let flagged = runtime
            .block_on(async {
                let cancel = wired.deps.kv.get(CANCEL, &text).await?.is_some();
                let failed = wired.deps.kv.get(NOTE, &text).await?.is_some();
                Ok::<bool, CpError>(cancel || failed)
            })
            .unwrap_or(false);
        if flagged {
            if record.state.is_before_cut_over() {
                return runtime.block_on(engine.abort(tenant, now, "operator_or_failure"));
            }
            // Too late to cancel: the flag is stale.
            let _ = runtime.block_on(wired.deps.kv.del(CANCEL, &text));
        }
        runtime.block_on(engine.advance(tenant, now))?;
    }
}

/// Records the outcome of one drive in the module's rows.
async fn settle(
    st: &MovesState,
    wired: &Arc<Wired>,
    tenant: &TenantId,
    result: Result<Result<MoveRecord, MoveError>, CpError>,
) {
    let text = tenant.to_string();
    let deps = &wired.deps;
    match result {
        Ok(Ok(_record)) => {
            let _ = deps.kv.del(ACTIVE, &text).await;
            let _ = deps.kv.del(CANCEL, &text).await;
            st.clear_attempts(&text);
        }
        Ok(Err(error)) => {
            let tries = st.bump_attempts(&text);
            if permanent(&error) || tries >= deps.max_attempts {
                tracing::warn!(code = error.code(), "a tenant move failed");
                let _ = deps.kv.put(NOTE, &text, error.code()).await;
                // The next pass aborts a move that is still before the cut-over.
                st.wake.notify_one();
            }
        }
        Err(_) => {
            let _ = st.bump_attempts(&text);
        }
    }
}

/// One runner pass: drives every unfinished move this instance can lease. Returns how many it
/// drove (used by tests; the background task ignores it).
pub async fn run_once(st: &Arc<MovesState>) -> usize {
    let Ok(wired) = st.wired().await else {
        return 0;
    };
    let Ok(rows) = wired.deps.kv.list(ACTIVE).await else {
        return 0;
    };
    let mut driven = 0;
    for (text, _) in rows {
        let Ok(tenant) = parse_tenant(&text) else {
            continue;
        };
        let now = st.now_ms();
        let now_i = i64::try_from(now).unwrap_or(i64::MAX);
        match wired
            .deps
            .kv
            .lease(&lease_id(&text), &st.holder, now_i, LEASE_MS)
            .await
        {
            Ok(true) => {}
            _ => continue,
        }
        driven += 1;
        let result = blocking(&wired, move |w, rt| drive(w, rt, &tenant, now)).await;
        settle(st, &wired, &tenant, result).await;
        let _ = wired.deps.kv.unlease(&lease_id(&text), &st.holder).await;
    }
    driven
}

fn runner_task(st: Arc<MovesState>) -> TaskFn {
    Box::new(
        move |mut stop: watch::Receiver<bool>| -> BoxFuture<'static, ()> {
            Box::pin(async move {
                loop {
                    run_once(&st).await;
                    let poll = match st.wired().await {
                        Ok(wired) => Duration::from_millis(wired.deps.poll_ms),
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
struct StartBody {
    tenant_id: String,
    to_cluster: String,
}

async fn start(
    State(st): State<Arc<MovesState>>,
    Extension(call): Extension<OperatorCall>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let result = start_inner(&st, &call, &headers, &body).await;
    finish(&call, result)
}

async fn start_inner(
    st: &Arc<MovesState>,
    call: &OperatorCall,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(StatusCode, Value), CpError> {
    let wired = st.wired().await?;
    let deps = &wired.deps;
    let key = idempotency_key(headers)?;
    let req: StartBody = serde_json::from_slice(body).map_err(|_| CpError::invalid(&["body"]))?;
    let tenant_id = {
        call.scope.require(&req.tenant_id)?;
        parse_tenant(&req.tenant_id)?
    };
    let idem_key = format!("{}|move|{key}", call.principal);
    let seed = IdemRow {
        digest: sha256_hex(body),
        entropy: String::new(),
        at_ms: st.now_ms(),
        status: 0,
        body: String::new(),
    };
    let row = idem_begin(deps, &idem_key, seed).await?;
    if row.status != 0 {
        let replay = serde_json::from_str(&row.body).unwrap_or(Value::Null);
        return Ok((status_of(row.status), replay));
    }
    let outcome = begin_move(st, &wired, &tenant_id, &req.to_cluster, row.at_ms).await;
    let record = match outcome {
        Ok(record) => record,
        Err(error) => {
            // Refused before anything was frozen: free the key so the corrected request can be sent.
            let _ = deps.kv.del(IDEM, &idem_key).await;
            return Err(error);
        }
    };
    let answer = json!({ "data": view(deps, &record).await? });
    let mut stored = row;
    stored.status = StatusCode::ACCEPTED.as_u16();
    stored.body = answer.to_string();
    deps.kv.put(IDEM, &idem_key, &to_json(&stored)?).await?;
    st.wake.notify_one();
    Ok((StatusCode::ACCEPTED, answer))
}

/// The checks, the plan and the first record: nothing is frozen until the runner takes it.
async fn begin_move(
    st: &Arc<MovesState>,
    wired: &Arc<Wired>,
    tenant_id: &TenantId,
    to_cluster: &str,
    now: u64,
) -> Result<MoveRecord, CpError> {
    let deps = &wired.deps;
    let id = *tenant_id;
    let registry = Arc::clone(&deps.tenants);
    let tenant = tokio::task::spawn_blocking(move || registry.get(&id))
        .await
        .map_err(|_| internal("moves.worker"))?
        .map_err(|e| match e {
            spot_tenancy::Error::NotFound { .. } => CpError::new(CpCode::NotFound),
            _ => internal("moves.registry"),
        })?;
    if !matches!(tenant.state, TenantState::Active | TenantState::Suspended) {
        return Err(CpError::with_reason(CpCode::Conflict, "moves.tenant_state"));
    }
    if let Some(busy) = deps.busy.busy(tenant_id).await? {
        return Err(CpError::with_reason(CpCode::Conflict, busy));
    }
    let target = deps
        .clusters
        .iter()
        .find(|c| c.cluster.as_str() == to_cluster)
        .ok_or_else(|| CpError::invalid(&["to_cluster"]))?;
    if !target.accepting {
        return Err(CpError::with_reason(
            CpCode::Conflict,
            "moves.target_not_accepting",
        ));
    }
    let to = Placement {
        cluster: target.cluster.clone(),
        database: target.database.clone(),
        schema: tenant.id.schema_name(),
    };
    if to == tenant.placement {
        return Err(CpError::invalid(&["to_cluster"]));
    }
    let registry = Arc::clone(&deps.tenants);
    let load = tokio::task::spawn_blocking(move || registry.load())
        .await
        .map_err(|_| internal("moves.worker"))?
        .map_err(|_| internal("moves.registry"))?;
    let used = load
        .get(&(target.cluster.clone(), target.database.clone()))
        .copied()
        .unwrap_or(0);
    if used >= target.capacity {
        return Err(CpError::with_reason(
            CpCode::LimitExceeded,
            "moves.target_full",
        ));
    }
    let plan = MovePlan::new(
        tenant.id,
        tenant.placement.clone(),
        to,
        target.region.clone(),
        deps.mode,
    )
    .map_err(|e| map_move_err(&e))?;
    let move_id = MoveId::new(move_id_of(tenant_id)).map_err(|e| map_move_err(&e))?;
    let record = blocking(wired, move |w, _| w.engine().start(plan, move_id, now))
        .await?
        .map_err(|e| map_move_err(&e))?;
    deps.kv.put(ACTIVE, &tenant_id.to_string(), "1").await?;
    st.clear_attempts(&tenant_id.to_string());
    Ok(record)
}

async fn get_one(
    State(st): State<Arc<MovesState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = async {
        let tenant = tenant_of_move(&call, &id)?;
        let wired = st.wired().await?;
        let record = blocking(&wired, move |w, _| w.engine().load(&tenant))
            .await?
            .map_err(|e| map_move_err(&e))?;
        Ok((
            StatusCode::OK,
            json!({ "data": view(&wired.deps, &record).await? }),
        ))
    }
    .await;
    finish(&call, result)
}

async fn cancel(
    State(st): State<Arc<MovesState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = cancel_inner(&st, &call, &id).await;
    finish(&call, result)
}

async fn cancel_inner(
    st: &Arc<MovesState>,
    call: &OperatorCall,
    id: &str,
) -> Result<(StatusCode, Value), CpError> {
    let tenant = tenant_of_move(call, id)?;
    let wired = st.wired().await?;
    let text = tenant.to_string();
    let record = blocking(&wired, move |w, _| w.engine().load(&tenant))
        .await?
        .map_err(|e| map_move_err(&e))?;
    if !record.state.is_before_cut_over() && record.state != MoveState::Aborted {
        return Err(CpError::with_reason(
            CpCode::Conflict,
            "moves.after_cutover",
        ));
    }
    wired.deps.kv.put(CANCEL, &text, &call.principal).await?;
    // When no runner holds the move, abort here; otherwise the runner sees the flag between two
    // transitions and the answer shows the state at this moment.
    let now = st.now_ms();
    let now_i = i64::try_from(now).unwrap_or(i64::MAX);
    if wired
        .deps
        .kv
        .lease(&lease_id(&text), &st.holder, now_i, LEASE_MS)
        .await?
    {
        let result = blocking(&wired, move |w, rt| drive(w, rt, &tenant, now)).await;
        settle(st, &wired, &tenant, result).await;
        let _ = wired.deps.kv.unlease(&lease_id(&text), &st.holder).await;
    }
    st.wake.notify_one();
    let record = blocking(&wired, move |w, _| w.engine().load(&tenant))
        .await?
        .map_err(|e| map_move_err(&e))?;
    Ok((
        StatusCode::OK,
        json!({ "data": view(&wired.deps, &record).await? }),
    ))
}

/// The routes of the module, for a state.
pub fn routes(state: Arc<MovesState>) -> Router {
    Router::new()
        .route("/cp/v1/moves", post(start))
        .route("/cp/v1/moves/{id}", get(get_one))
        .route("/cp/v1/moves/{id}/cancel", post(cancel))
        .with_state(state)
}

/// Mounts the module over a prepared state: routes, the runner and the readiness check.
pub fn register_with(ctx: &mut CpCtx, state: Arc<MovesState>) {
    ctx.operator_routes(routes(Arc::clone(&state)));
    ctx.add_task(runner_task(Arc::clone(&state)));
    ctx.add_migration(DDL);
    let ready = Arc::clone(&state);
    ctx.add_readiness(Arc::new(move || -> BoxFuture<'static, bool> {
        let ready = Arc::clone(&ready);
        Box::pin(async move { ready.wired().await.is_ok() })
    }));
}

/// Days the source stays frozen and read-only after the cut-over.
const QUARANTINE_DAYS: u64 = 30;
/// The seconds between runner passes.
const POLL_SECS: u64 = 5;
/// The attempts a failing move gets per process.
const MAX_ATTEMPTS: u32 = 5;

async fn production(config: TenantsConfig, pool: Pool) -> Result<MoveDeps, CpError> {
    let runtime = leaked_runtime();
    let store = Arc::new(spot_tenancy_pg::PgControlStore::new(
        pool.clone(),
        Arc::clone(&runtime),
        Arc::new(spot_tenancy_pg::Faults::new()),
    ));
    store
        .prepare()
        .await
        .map_err(|_| internal("moves.control_schema"))?;
    let mut clusters = Vec::new();
    for entry in &config.clusters {
        clusters.push(Cluster {
            cluster: ClusterId::new(entry.cluster.clone())
                .map_err(|_| internal("moves.cluster_id"))?,
            database: DatabaseId::new(entry.database.clone())
                .map_err(|_| internal("moves.cluster_id"))?,
            region: Region::new(entry.region.clone())
                .map_err(|_| internal("moves.cluster_region"))?,
            capacity: entry.capacity,
            accepting: entry.accepting,
        });
    }
    let factory = HOST_PORTS_FACTORY
        .get()
        .ok_or_else(|| internal("moves.no_host_ports"))?;
    let hosts = factory(HostPortInputs {
        clusters: clusters.clone(),
        pool: pool.clone(),
        runtime,
    })?;
    let tenants: Arc<dyn ControlStore + Send + Sync> = store.clone();
    Ok(MoveDeps {
        tenants,
        moves: hosts.moves,
        quiesce: hosts.quiesce,
        transfer: hosts.transfer,
        placement: Arc::new(ControlStorePlacement::new(
            Arc::clone(&store),
            Arc::new(NoCaches),
        )),
        residency: Arc::new(TenancyResidency::new(store)),
        fingerprints: hosts.fingerprints,
        busy: hosts.busy,
        quarantine: hosts.quarantine,
        kv: Arc::new(PgKv::new(pool)),
        clusters,
        mode: MoveMode::Offline,
        quarantine_ms: QUARANTINE_DAYS * 86_400_000,
        poll_ms: POLL_SECS * 1000,
        max_attempts: MAX_ATTEMPTS,
    })
}

/// Registers the module from the file `SPOT_CP_TENANTS_CONFIG` names (the same placement databases
/// as the `tenants` module). Without it the module is not mounted and readiness reports it as not
/// ready.
///
/// # Errors
/// `cp.internal` when the control database is missing or `cp.invalid` for a bad configuration.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let Ok(path) = std::env::var("SPOT_CP_TENANTS_CONFIG") else {
        tracing::warn!("SPOT_CP_TENANTS_CONFIG is not set: the moves module is not mounted");
        ctx.add_readiness(Arc::new(|| -> BoxFuture<'static, bool> {
            Box::pin(async { false })
        }));
        return Ok(());
    };
    let text = std::fs::read_to_string(path)
        .map_err(|_| CpError::with_reason(CpCode::Invalid, "moves.config_unreadable"))?;
    let config = TenantsConfig::from_toml(&text)?;
    let pool = ctx
        .pool
        .clone()
        .ok_or_else(|| internal("moves.no_control_database"))?;
    let clock = Arc::clone(&ctx.clock);
    let init: Init = Box::new(move || -> BoxFuture<'static, Result<MoveDeps, CpError>> {
        let (config, pool) = (config.clone(), pool.clone());
        Box::pin(async move { production(config, pool).await })
    });
    register_with(ctx, MovesState::lazy(clock, init));
    Ok(())
}
