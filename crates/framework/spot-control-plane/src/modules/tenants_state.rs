//! The state, dependencies and helpers of the `tenants` module (`WP-CTL-01.T7`). Split from `tenants.rs` for the 1,000-line gate.

use super::tenants_kv::Kv;
use super::tenants_ports::ApprovalVerifier;
use super::tenants_ports::FixedApprovals;
use crate::CpCode;
use crate::CpError;
use crate::auth::OperatorCall;
use axum::Json;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use spot_core::Clock;
use spot_mgmt_api::ports::BoxFuture;
use spot_tenancy::Cluster;
use spot_tenancy::ControlPlane as Plane;
use spot_tenancy::ControlStore;
use spot_tenancy::Tenant;
use spot_tenancy::TenantId;
use spot_tenancy::model::TenantState;
use spot_tenancy::ports::ApprovalHook;
use spot_tenancy::ports::Archiver;
use spot_tenancy::ports::AuditSink as PlaneAudit;
use spot_tenancy::ports::IamPort;
use spot_tenancy::ports::LicencePort;
use spot_tenancy::ports::Ports;
use spot_tenancy::ports::SchemaPort;
use spot_tenancy::ports::SiteSeeder;
use spot_tenancy::ports::TelemetryPort;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct IdemRow {
    pub(crate) digest: String,
    pub(crate) entropy: String,
    pub(crate) at_ms: u64,
    pub(crate) status: u16,
    pub(crate) body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DpRow {
    pub(crate) request_id: String,
    pub(crate) tenant: String,
    pub(crate) requested_by: String,
    pub(crate) requested_at_ms: u64,
    pub(crate) confirmable_after_ms: u64,
    pub(crate) retention_ends_ms: u64,
    /// `requested`, `confirmed` or `done`.
    pub(crate) state: String,
    pub(crate) confirmed_by: String,
    pub(crate) approvers: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Note {
    pub(crate) code: String,
}

// ---------------------------------------------------------------------------------------------
// Dependencies and state
// ---------------------------------------------------------------------------------------------

/// The ports of the provisioning workflow.
pub struct PortSet {
    /// Schema creation.
    pub schema: Arc<dyn SchemaPort + Send + Sync>,
    /// Baseline and seed.
    pub seeder: Arc<dyn SiteSeeder + Send + Sync>,
    /// Administrator invitation.
    pub iam: Arc<dyn IamPort + Send + Sync>,
    /// Licence issuing.
    pub licence: Arc<dyn LicencePort + Send + Sync>,
    /// Telemetry registration.
    pub telemetry: Arc<dyn TelemetryPort + Send + Sync>,
    /// The final backup.
    pub archiver: Arc<dyn Archiver + Send + Sync>,
}

/// Everything the endpoints and the runner use.
pub struct Deps {
    /// The registry.
    pub store: Arc<dyn ControlStore + Send + Sync>,
    /// The ports.
    pub ports: PortSet,
    /// The plane's audit port.
    pub audit: Arc<dyn PlaneAudit + Send + Sync>,
    /// The module's rows, leases and gates.
    pub kv: Arc<dyn Kv>,
    /// Verifies the signed approval of a deprovisioning.
    pub approvals: Arc<dyn ApprovalVerifier>,
    /// The placement databases.
    pub clusters: Vec<Cluster>,
    /// The plane's policy.
    pub plane: spot_tenancy::Config,
    /// Tenants per edition.
    pub editions: BTreeMap<String, u32>,
    /// The delay before a deprovisioning may be confirmed.
    pub confirm_delay_ms: u64,
    /// The retention asked of the final backup.
    pub retention_ms: u64,
    /// Ten bytes of entropy for a tenant id (a CSPRNG in production).
    pub entropy: Arc<dyn Fn() -> [u8; 10] + Send + Sync>,
    /// The milliseconds between runner passes.
    pub poll_ms: u64,
    /// The attempts a failing workflow gets per process.
    pub max_attempts: u32,
}

impl Deps {
    fn with_plane<T>(&self, hook: &dyn ApprovalHook, f: impl FnOnce(&Plane<'_>) -> T) -> T {
        let plane = Plane {
            store: &*self.store,
            ports: Ports {
                schema: &*self.ports.schema,
                seeder: &*self.ports.seeder,
                iam: &*self.ports.iam,
                licence: &*self.ports.licence,
                telemetry: &*self.ports.telemetry,
            },
            archiver: &*self.ports.archiver,
            approvals: hook,
            audit: &*self.audit,
            clusters: &self.clusters,
            config: self.plane,
        };
        f(&plane)
    }
}

pub(crate) type Init = Box<dyn Fn() -> BoxFuture<'static, Result<Deps, CpError>> + Send + Sync>;

/// The shared state of the handlers and the runner.
pub struct TenantsState {
    clock: Arc<dyn Clock>,
    deps: tokio::sync::OnceCell<Arc<Deps>>,
    init: Option<Init>,
    pub(crate) wake: tokio::sync::Notify,
    pub(crate) attempts: Mutex<BTreeMap<String, u32>>,
    pub(crate) holder: String,
}

impl TenantsState {
    /// A state over ready dependencies (tests, embedding).
    #[must_use]
    pub fn ready(clock: Arc<dyn Clock>, deps: Deps) -> Arc<Self> {
        let cell = tokio::sync::OnceCell::new_with(Some(Arc::new(deps)));
        Arc::new(Self::build(clock, cell, None))
    }

    pub(crate) fn lazy(clock: Arc<dyn Clock>, init: Init) -> Arc<Self> {
        Arc::new(Self::build(clock, tokio::sync::OnceCell::new(), Some(init)))
    }

    fn build(
        clock: Arc<dyn Clock>,
        deps: tokio::sync::OnceCell<Arc<Deps>>,
        init: Option<Init>,
    ) -> Self {
        let mut bytes = [0u8; 8];
        fill_random(&mut bytes);
        Self {
            clock,
            deps,
            init,
            wake: tokio::sync::Notify::new(),
            attempts: Mutex::new(BTreeMap::new()),
            holder: format!("cp-{}", hex(&bytes)),
        }
    }

    pub(crate) async fn deps(&self) -> Result<Arc<Deps>, CpError> {
        let init = self.init.as_ref();
        self.deps
            .get_or_try_init(|| async {
                match init {
                    Some(init) => init().await.map(Arc::new),
                    None => Err(CpError::with_reason(
                        CpCode::Internal,
                        "tenants.not_configured",
                    )),
                }
            })
            .await
            .cloned()
    }

    pub(crate) fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now_utc().unix_timestamp())
            .unwrap_or(0)
            .saturating_mul(1000)
    }
}

pub(crate) fn fill_random(buf: &mut [u8]) {
    use rand::TryRngCore as _;
    if rand::rngs::OsRng.try_fill_bytes(buf).is_err() {
        // No entropy source: refuse to continue with predictable bytes.
        std::process::abort();
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        let high = char::from(*pair.first()?).to_digit(16)?;
        let low = char::from(*pair.get(1)?).to_digit(16)?;
        *slot = u8::try_from(high * 16 + low).ok()?;
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Helpers: errors, time, JSON views
// ---------------------------------------------------------------------------------------------

pub(crate) fn map_err(error: &spot_tenancy::Error) -> CpError {
    use spot_tenancy::Error as E;
    let reason = error.code();
    match error {
        E::NotFound { .. } => CpError::new(CpCode::NotFound),
        E::NameTaken { .. }
        | E::NameReserved { .. }
        | E::VersionConflict { .. }
        | E::InvalidTransition { .. }
        | E::WrongState { .. }
        | E::LegalHold { .. }
        | E::ArchiveUnverified { .. } => CpError::with_reason(CpCode::Conflict, reason),
        E::InvalidName { .. } => CpError::invalid(&["name"]),
        E::InvalidRequest { .. } | E::InvalidPlacement { .. } => CpError::invalid(&["body"]),
        E::NoPlacement { .. } | E::CapacityExceeded { .. } => {
            CpError::with_reason(CpCode::LimitExceeded, reason)
        }
        E::ApprovalRequired { .. } => CpError::with_reason(CpCode::ApprovalRequired, reason),
        E::RetentionPending { .. } => CpError::with_reason(CpCode::NotConfirmable, reason),
        E::StepFailed { .. }
        | E::JournalCorrupt { .. }
        | E::AuditFailed { .. }
        | E::StoreFailed { .. } => CpError::with_reason(CpCode::Internal, reason),
    }
}

pub(crate) fn rfc3339(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0) + 719_468;
    let rem = secs % 86_400;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub(crate) fn ms_text(ms: u64) -> String {
    rfc3339(ms / 1000)
}

pub(crate) fn workflow_id(kind: char, tenant: &TenantId) -> String {
    format!("wf-{kind}-{tenant}")
}

pub(crate) fn tenant_status(tenant: &Tenant, failed: bool) -> &'static str {
    match tenant.state {
        TenantState::Provisioning if failed => "failed",
        TenantState::Provisioning => "provisioning",
        TenantState::Active | TenantState::Moving => "active",
        TenantState::Suspended => "suspended",
        TenantState::Deprovisioning => "deprovisioning",
        TenantState::Deleted => "deprovisioned",
    }
}

pub(crate) fn tenant_json(tenant: &Tenant, failed: bool) -> Value {
    json!({
        "id": tenant.id.to_string(),
        "name": tenant.name.as_str(),
        "region": tenant.region.as_str(),
        "cluster": tenant.placement.cluster.as_str(),
        "status": tenant_status(tenant, failed),
        "plan": tenant.edition.as_str(),
        "created_at": ms_text(tenant.created),
        "workflow_id": workflow_id('p', &tenant.id),
    })
}

pub(crate) fn parse_tenant(text: &str) -> Result<TenantId, CpError> {
    serde_json::from_value::<TenantId>(Value::String(text.to_owned()))
        .map_err(|_| CpError::new(CpCode::NotFound))
}

pub(crate) fn idempotency_key(headers: &HeaderMap) -> Result<String, CpError> {
    let value = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| (0x21..0x7f).contains(&b))
        });
    value
        .map(str::to_owned)
        .ok_or_else(|| CpError::invalid(&["header.Idempotency-Key"]))
}

pub(crate) fn finish(
    call: &OperatorCall,
    result: Result<(StatusCode, Value), CpError>,
) -> Response {
    match result {
        Ok((status, value)) => (status, Json(value)).into_response(),
        Err(error) => error.into_response_with(&call.request_id),
    }
}

pub(crate) async fn blocking<T: Send + 'static>(
    deps: &Arc<Deps>,
    approvers: Vec<String>,
    f: impl FnOnce(&Plane<'_>) -> T + Send + 'static,
) -> Result<T, CpError> {
    let deps = Arc::clone(deps);
    tokio::task::spawn_blocking(move || {
        let hook = FixedApprovals(approvers);
        deps.with_plane(&hook, f)
    })
    .await
    .map_err(|_| CpError::with_reason(CpCode::Internal, "tenants.worker"))
}

pub(crate) async fn store_op<T: Send + 'static>(
    deps: &Arc<Deps>,
    f: impl FnOnce(&(dyn ControlStore + Send + Sync)) -> T + Send + 'static,
) -> Result<T, CpError> {
    let deps = Arc::clone(deps);
    tokio::task::spawn_blocking(move || f(&*deps.store))
        .await
        .map_err(|_| CpError::with_reason(CpCode::Internal, "tenants.worker"))
}

pub(crate) async fn load_tenant(deps: &Arc<Deps>, id: TenantId) -> Result<Tenant, CpError> {
    store_op(deps, move |s| s.get(&id))
        .await?
        .map_err(|e| map_err(&e))
}

pub(crate) async fn failed_note(deps: &Deps, id: &TenantId) -> bool {
    matches!(deps.kv.get("note", &id.to_string()).await, Ok(Some(_)))
}
