//! The `tenants` module (`WP-CTL-01.T7`): the tenant registry, placement and the provisioning and
//! deprovisioning workflows as `/cp/v1` endpoints, over `spot-tenancy`.
//!
//! # Shape
//!
//! - **Endpoints** (contract `contracts/control-plane`): `POST/GET /cp/v1/tenants`,
//!   `GET /cp/v1/tenants/{id}`, `POST .../{id}/suspend|resume`, the two-step
//!   `POST .../{id}/deprovision` and `.../deprovision/confirm`, `GET /cp/v1/workflows/{id}`,
//!   `GET /cp/v1/placements` and `GET /cp/v1/clusters`. Every id is checked against the
//!   operator's scope first; an out-of-scope id is `cp.not_found`, exactly like a missing one.
//! - **The workflow runner** drives `spot_tenancy::ControlPlane` (a synchronous, journaled
//!   workflow) from `spawn_blocking`. Provisioning is *registered* by the request (the tenant row
//!   exists in `Provisioning`, the answer is `202`) and *run* by a background task that also
//!   resumes every unfinished workflow at start. A workflow is driven under a lease
//!   (`cp_tenants_lease`), so two control-plane instances never run the same one; every port step
//!   is idempotent, so a crash after any journal line resumes to the same end state.
//! - **Idempotency.** Create, deprovision request and confirm need an `Idempotency-Key`. The
//!   first call stores the request digest and, for a create, the entropy and instant the tenant
//!   id derives from, *before* anything is created, so a retry after a crash finds the same id
//!   and carries on; the same key with another body is `cp.replayed`.
//! - **Licence limits** (tenants per edition, `[editions]`) are counted and the tenant
//!   registered under one gate (an advisory lock on PostgreSQL), so two parallel creates at the
//!   limit admit one.
//! - **Deprovisioning** cannot be done by one request: the request takes the verified final
//!   backup (`archive`) and records the retention end and the earliest confirmation; the
//!   confirmation, after the delay, carries a signed approval (a signed command over
//!   `"<tenant>:<request id>"`, op `cp.cp_tenant_deprovision_approve`) verified here against the
//!   administrator keys. The approvers handed to the plane are the principal who asked for the
//!   deprovisioning plus the signing keys, so the plane's own rule (two distinct approvers other
//!   than the confirming operator) holds. Nothing is deleted before the retention end: a
//!   confirmation inside the retention stays `waiting` and the runner deletes after it.
//! - **The real ports** (`production`): the control store is `spot_tenancy_pg::PgControlStore` on
//!   the control pool; schema and seed go through `spot_tenancy_adapters::{SchemaAdapter,
//!   SeedAdapter}` over a `MigrationRunner` built by a registered [`RunnerFactory`] (it needs the
//!   application catalogue, which this crate must not depend on); the licence is
//!   `LicenceIssuer`; the final backup is `SnapshotArchiver` over `PgDumpProvider`; the IAM step
//!   queues the administrator invitation; telemetry registration and the audit sink write to the
//!   control schema and to the control plane's audit sink. The provisioning credential is a
//!   separate role that may only create databases (`provisioner_url_env`); a tenant's own role
//!   and password are created by the IAM step and live in the tenant's secret store, never in
//!   the control database.
//!
//! Time is an argument (the injected clock); no secret, key, token or e-mail address is put in an
//! error, a log line or an audit record.
pub use super::tenants_kv::{Gate, Kv, MemoryKv, PgKv};
use super::tenants_kv::{from_json, store_failed, to_json};
pub use super::tenants_ports::{
    ApprovalVerifier, KvLicenceStore, ProvisionerInputs, RunnerFactory, SignedApprovals,
    leaked_runtime, set_runner_factory,
};
use super::tenants_prod::production;
pub use super::tenants_prod::{ClusterConfig, TenantsConfig};
pub use super::tenants_state::{Deps, PortSet, TenantsState};
use super::tenants_state::{
    DpRow, IdemRow, Init, Note, blocking, failed_note, finish, from_hex, hex, idempotency_key,
    load_tenant, map_err, ms_text, parse_tenant, store_op, tenant_json, workflow_id,
};
use crate::auth::{OperatorCall, sha256_hex};
use crate::{CpCode, CpCtx, CpError, TaskFn};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use spot_mgmt_api::ports::BoxFuture;
use spot_tenancy::model::TenantState;
use spot_tenancy::{JournalKind, Step, Tenant, TenantFlags, TenantId};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Registration order: the registry first, the modules that need tenants after it.
pub const ORDER: u32 = 20;

/// The control-schema DDL of this module.
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_tenants_kv (ns TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL, PRIMARY KEY (ns, k))",
    "CREATE TABLE IF NOT EXISTS cp_tenants_lease (id TEXT PRIMARY KEY, holder TEXT NOT NULL, until BIGINT NOT NULL)",
];

/// How long a workflow lease lasts (milliseconds); the runner renews by re-acquiring.
const LEASE_MS: i64 = 120_000;
const MAX_PAGE: usize = 100;

// ---------------------------------------------------------------------------------------------
// Workflow views
// ---------------------------------------------------------------------------------------------

async fn workflow_view(
    deps: &Arc<Deps>,
    id: TenantId,
    kind: char,
    now_ms: u64,
) -> Result<Value, CpError> {
    let id_text = id.to_string();
    let (tenant, journal) = store_op(deps, move |s| (s.get(&id), s.journal(&id))).await?;
    let tenant = tenant.map_err(|e| map_err(&e))?;
    let journal = journal.map_err(|e| map_err(&e))?;
    let note: Option<Note> = match deps.kv.get("note", &id_text).await? {
        Some(text) => Some(from_json(&text)?),
        None => None,
    };
    let last = |step: Step| journal.iter().rev().find(|e| e.step == step).copied();
    let activated = last(Step::Activate).is_some_and(|e| e.kind == JournalKind::Done);
    let mut steps = Vec::new();
    let mut blamed = false;
    if kind == 'p' {
        for step in Step::ALL {
            let entry = last(step);
            let mut state = match entry.map(|e| e.kind) {
                Some(JournalKind::Done) => "done",
                Some(JournalKind::Started) => "running",
                Some(JournalKind::Compensated) => "skipped",
                None => "pending",
            };
            let mut code = Value::Null;
            if let Some(note) = &note
                && !blamed
                && state != "done"
                && state != "skipped"
            {
                blamed = true;
                state = "failed";
                code = json!(note.code);
            }
            steps.push(json!({ "name": step.name(), "state": state, "at": entry.map(|e| ms_text(e.at)), "code": code }));
        }
    } else {
        for step in Step::ALL.iter().rev() {
            let entry = last(*step).filter(|e| e.kind == JournalKind::Compensated);
            steps.push(json!({ "name": step.name(), "state": if entry.is_some() { "done" } else { "pending" }, "at": entry.map(|e| ms_text(e.at)), "code": Value::Null }));
        }
    }
    let state = if kind == 'p' {
        match tenant.state {
            TenantState::Provisioning if note.is_some() => "failed",
            TenantState::Provisioning if journal.is_empty() => "pending",
            TenantState::Provisioning => "running",
            TenantState::Deleted if !activated => "cancelled",
            _ => "succeeded",
        }
    } else {
        let row = dp_row(deps, &id_text)
            .await?
            .ok_or_else(|| CpError::new(CpCode::NotFound))?;
        match (tenant.state, row.state.as_str()) {
            (TenantState::Deleted, _) => "succeeded",
            (_, "confirmed") if note.is_some() => "failed",
            (_, "confirmed") if now_ms < row.retention_ends_ms => "waiting",
            (_, "confirmed") => "running",
            (TenantState::Suspended, _) => "cancelled",
            _ => "waiting",
        }
    };
    let resumable = matches!(state, "pending" | "running" | "failed");
    Ok(json!({
        "id": workflow_id(kind, &id),
        "kind": if kind == 'p' { "provision" } else { "deprovision" },
        "state": state,
        "steps": steps,
        "resumable": resumable,
    }))
}

async fn dp_row(deps: &Deps, tenant: &str) -> Result<Option<DpRow>, CpError> {
    match deps.kv.get("dp", tenant).await? {
        Some(text) => Ok(Some(from_json(&text)?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: String,
    region: String,
    plan: String,
    admin_email: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeprovisionBody {
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmBody {
    request_id: String,
    approval: String,
}

#[derive(Deserialize, Default)]
struct ListQuery {
    status: Option<String>,
    region: Option<String>,
    q: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

fn status_of(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::OK)
}

/// Stores the request digest (and the seed of the tenant id) under the key, or returns the first
/// call's row; the same key with another body is `cp.replayed`.
async fn idem_begin(deps: &Deps, key: &str, row: IdemRow) -> Result<IdemRow, CpError> {
    if deps.kv.put_new("idem", key, &to_json(&row)?).await? {
        return Ok(row);
    }
    let text = deps.kv.get("idem", key).await?.ok_or_else(store_failed)?;
    let existing: IdemRow = from_json(&text)?;
    if existing.digest == row.digest {
        Ok(existing)
    } else {
        Err(CpError::new(CpCode::Replayed))
    }
}

async fn idem_done(
    deps: &Deps,
    key: &str,
    mut row: IdemRow,
    status: StatusCode,
    body: &Value,
) -> Result<(), CpError> {
    row.status = status.as_u16();
    row.body = body.to_string();
    deps.kv.put("idem", key, &to_json(&row)?).await
}

fn replayed(row: &IdemRow) -> Option<(StatusCode, Value)> {
    (row.status != 0).then(|| {
        (
            status_of(row.status),
            serde_json::from_str(&row.body).unwrap_or(Value::Null),
        )
    })
}

async fn create(
    State(st): State<Arc<TenantsState>>,
    Extension(call): Extension<OperatorCall>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let result = create_inner(&st, &call, &headers, &body).await;
    finish(&call, result)
}

async fn create_inner(
    st: &Arc<TenantsState>,
    call: &OperatorCall,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(StatusCode, Value), CpError> {
    if !call.scope.is_all() {
        return Err(CpError::with_reason(CpCode::Forbidden, "cp.scope_not_all"));
    }
    let deps = st.deps().await?;
    let key = idempotency_key(headers)?;
    let req: CreateBody = serde_json::from_slice(body).map_err(|_| CpError::invalid(&["body"]))?;
    let email_ok = req.admin_email.len() <= 320
        && req.admin_email.contains('@')
        && !req
            .admin_email
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control());
    if !email_ok {
        return Err(CpError::invalid(&["admin_email"]));
    }
    let idem_key = format!("{}|create|{key}", call.principal);
    let seed = IdemRow {
        digest: sha256_hex(body),
        entropy: hex(&(deps.entropy)()),
        at_ms: st.now_ms(),
        status: 0,
        body: String::new(),
    };
    let row = idem_begin(&deps, &idem_key, seed).await?;
    if let Some(done) = replayed(&row) {
        return Ok(done);
    }
    let entropy: [u8; 10] = from_hex(&row.entropy).ok_or_else(store_failed)?;
    let id = TenantId::new_v7(row.at_ms, entropy);
    deps.kv
        .put("invite", &id.to_string(), &req.admin_email)
        .await?;
    let gate = deps.kv.gate(&format!("edition:{}", req.plan)).await?;
    let limit = deps.editions.get(&req.plan).copied();
    let (at, actor, edition) = (row.at_ms, call.principal.clone(), req.plan.clone());
    let request = spot_tenancy::ProvisionRequest {
        name: req.name,
        region: req.region,
        edition: req.plan,
        entropy,
        flags: TenantFlags::NONE,
    };
    let result = blocking(&deps, Vec::new(), move |plane| -> Result<Tenant, CpError> {
        if let Ok(existing) = plane.store.get(&id) {
            return Ok(existing);
        }
        if let Some(max) = limit {
            let live = plane.store.list().map_err(|e| map_err(&e))?;
            let used = live
                .iter()
                .filter(|t| t.state != TenantState::Deleted && t.edition.as_str() == edition)
                .count();
            if used >= usize::try_from(max).unwrap_or(usize::MAX) {
                return Err(CpError::with_reason(
                    CpCode::LimitExceeded,
                    "cp.edition_limit",
                ));
            }
        }
        plane
            .provision(&request, &actor, at)
            .map_err(|e| map_err(&e))?;
        plane.store.get(&id).map_err(|e| map_err(&e))
    })
    .await
    .and_then(|r| r);
    deps.kv.ungate(gate).await;
    let tenant = match result {
        Ok(tenant) => tenant,
        Err(error) => {
            // The request was refused before anything was created: free the key and the invitation.
            let _ = deps.kv.del("idem", &idem_key).await;
            let _ = deps.kv.del("invite", &id.to_string()).await;
            return Err(error);
        }
    };
    let answer = json!({ "data": { "tenant": tenant_json(&tenant, false), "workflow_id": workflow_id('p', &tenant.id) } });
    idem_done(&deps, &idem_key, row, StatusCode::ACCEPTED, &answer).await?;
    st.wake.notify_one();
    Ok((StatusCode::ACCEPTED, answer))
}

fn in_scope(call: &OperatorCall, id: &str) -> Result<TenantId, CpError> {
    call.scope.require(id)?;
    parse_tenant(id)
}

async fn list(
    State(st): State<Arc<TenantsState>>,
    Extension(call): Extension<OperatorCall>,
    Query(q): Query<ListQuery>,
) -> Response {
    let result = async {
        let deps = st.deps().await?;
        let tenants = store_op(&deps, |s| s.list())
            .await?
            .map_err(|e| map_err(&e))?;
        let limit = q.limit.unwrap_or(50).clamp(1, MAX_PAGE);
        let mut page = Vec::new();
        let mut more = false;
        for tenant in tenants {
            let id = tenant.id.to_string();
            if !call.scope.allows(&id) || q.cursor.as_deref().is_some_and(|c| id.as_str() <= c) {
                continue;
            }
            let failed = failed_note(&deps, &tenant.id).await;
            let json = tenant_json(&tenant, failed);
            let matches = q.status.as_deref().is_none_or(|s| json["status"] == s)
                && q.region
                    .as_deref()
                    .is_none_or(|r| tenant.region.as_str() == r)
                && q.q
                    .as_deref()
                    .is_none_or(|needle| tenant.name.as_str().contains(needle));
            if !matches {
                continue;
            }
            if page.len() == limit {
                more = true;
                break;
            }
            page.push(json);
        }
        let next = if more {
            page.last()
                .and_then(|t| t["id"].as_str())
                .map(str::to_owned)
        } else {
            None
        };
        Ok((StatusCode::OK, json!({ "data": page, "next_cursor": next })))
    }
    .await;
    finish(&call, result)
}

async fn get_one(
    State(st): State<Arc<TenantsState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = async {
        let tenant_id = in_scope(&call, &id)?;
        let deps = st.deps().await?;
        let tenant = load_tenant(&deps, tenant_id).await?;
        let failed = failed_note(&deps, &tenant.id).await;
        Ok((
            StatusCode::OK,
            json!({ "data": tenant_json(&tenant, failed) }),
        ))
    }
    .await;
    finish(&call, result)
}

async fn transition(
    st: &Arc<TenantsState>,
    call: &OperatorCall,
    id: &str,
    suspend: bool,
) -> Result<(StatusCode, Value), CpError> {
    let tenant_id = in_scope(call, id)?;
    let deps = st.deps().await?;
    let (now, actor) = (st.now_ms(), call.principal.clone());
    let tenant = blocking(&deps, Vec::new(), move |plane| {
        if suspend {
            plane.suspend(&tenant_id, &actor, now)
        } else {
            plane.reactivate(&tenant_id, &actor, now)
        }
    })
    .await?
    .map_err(|e| map_err(&e))?;
    Ok((
        StatusCode::OK,
        json!({ "data": tenant_json(&tenant, false) }),
    ))
}

async fn suspend(
    State(st): State<Arc<TenantsState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = transition(&st, &call, &id, true).await;
    finish(&call, result)
}

async fn resume(
    State(st): State<Arc<TenantsState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = transition(&st, &call, &id, false).await;
    finish(&call, result)
}

async fn deprovision(
    State(st): State<Arc<TenantsState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let result = deprovision_inner(&st, &call, &id, &headers, &body).await;
    finish(&call, result)
}

async fn deprovision_inner(
    st: &Arc<TenantsState>,
    call: &OperatorCall,
    id: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(StatusCode, Value), CpError> {
    let tenant_id = in_scope(call, id)?;
    let deps = st.deps().await?;
    let key = idempotency_key(headers)?;
    let req: DeprovisionBody =
        serde_json::from_slice(body).map_err(|_| CpError::invalid(&["body"]))?;
    if req.reason.is_empty() || req.reason.len() > 500 {
        return Err(CpError::invalid(&["reason"]));
    }
    let idem_key = format!("{}|deprovision|{id}|{key}", call.principal);
    let seed = IdemRow {
        digest: sha256_hex(body),
        entropy: String::new(),
        at_ms: st.now_ms(),
        status: 0,
        body: String::new(),
    };
    let row = idem_begin(&deps, &idem_key, seed).await?;
    if let Some(done) = replayed(&row) {
        return Ok(done);
    }
    let (now, actor, retention_ms) = (row.at_ms, call.principal.clone(), deps.retention_ms);
    let outcome = blocking(&deps, Vec::new(), move |plane| -> Result<Tenant, CpError> {
        let tenant = plane.store.get(&tenant_id).map_err(|e| map_err(&e))?;
        if tenant.state == TenantState::Deprovisioning {
            return Ok(tenant);
        }
        plane
            .archive(&tenant_id, &actor, now, retention_ms)
            .map_err(|e| map_err(&e))
    })
    .await
    .and_then(|r| r);
    let tenant = match outcome {
        Ok(tenant) => tenant,
        Err(error) => {
            let _ = deps.kv.del("idem", &idem_key).await;
            return Err(error);
        }
    };
    let retention_ends = tenant.retention.as_ref().map_or(now, |r| r.delete_after);
    let dp = DpRow {
        request_id: format!("dp-{}", &sha256_hex(idem_key.as_bytes())[..16]),
        tenant: id.to_owned(),
        requested_by: call.principal.clone(),
        requested_at_ms: now,
        confirmable_after_ms: now.saturating_add(deps.confirm_delay_ms),
        retention_ends_ms: retention_ends,
        state: "requested".to_owned(),
        confirmed_by: String::new(),
        approvers: Vec::new(),
    };
    deps.kv.put("dp", id, &to_json(&dp)?).await?;
    let answer = json!({ "data": {
        "request_id": dp.request_id,
        "confirmable_after": ms_text(dp.confirmable_after_ms),
        "retention_ends": ms_text(dp.retention_ends_ms),
    } });
    idem_done(&deps, &idem_key, row, StatusCode::ACCEPTED, &answer).await?;
    Ok((StatusCode::ACCEPTED, answer))
}

async fn confirm(
    State(st): State<Arc<TenantsState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let result = confirm_inner(&st, &call, &id, &headers, &body).await;
    finish(&call, result)
}

async fn confirm_inner(
    st: &Arc<TenantsState>,
    call: &OperatorCall,
    id: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(StatusCode, Value), CpError> {
    let tenant_id = in_scope(call, id)?;
    let deps = st.deps().await?;
    let key = idempotency_key(headers)?;
    let req: ConfirmBody = serde_json::from_slice(body).map_err(|_| CpError::invalid(&["body"]))?;
    let idem_key = format!("{}|confirm|{id}|{key}", call.principal);
    let seed = IdemRow {
        digest: sha256_hex(body),
        entropy: String::new(),
        at_ms: st.now_ms(),
        status: 0,
        body: String::new(),
    };
    let idem = idem_begin(&deps, &idem_key, seed).await?;
    if let Some(done) = replayed(&idem) {
        return Ok(done);
    }
    let result = confirm_steps(st, &deps, call, id, tenant_id, &req).await;
    if result.is_err() {
        let _ = deps.kv.del("idem", &idem_key).await;
    }
    let answer = json!({ "data": result? });
    idem_done(&deps, &idem_key, idem, StatusCode::ACCEPTED, &answer).await?;
    Ok((StatusCode::ACCEPTED, answer))
}

async fn confirm_steps(
    st: &Arc<TenantsState>,
    deps: &Arc<Deps>,
    call: &OperatorCall,
    id: &str,
    tenant_id: TenantId,
    req: &ConfirmBody,
) -> Result<Value, CpError> {
    let mut row = dp_row(deps, id)
        .await?
        .filter(|r| r.request_id == req.request_id)
        .ok_or_else(|| CpError::new(CpCode::NotFound))?;
    if row.state == "done" {
        return Err(CpError::with_reason(
            CpCode::Conflict,
            "cp.already_deprovisioned",
        ));
    }
    let now = st.now_ms();
    if now < row.confirmable_after_ms {
        return Err(CpError::new(CpCode::NotConfirmable));
    }
    if row.state == "requested" {
        let secs = i64::try_from(now / 1000).unwrap_or(0);
        let signers = deps
            .approvals
            .verify(&req.approval, id, &req.request_id, secs)
            .await?;
        row.state = "confirmed".to_owned();
        row.confirmed_by = call.principal.clone();
        row.approvers = std::iter::once(row.requested_by.clone())
            .chain(signers)
            .collect();
        deps.kv.put("dp", id, &to_json(&row)?).await?;
    }
    if now >= row.retention_ends_ms {
        drive_delete(st, deps, row).await?;
    }
    workflow_view(deps, tenant_id, 'd', st.now_ms()).await
}

/// Deletes a confirmed tenant when the retention has ended and no other instance holds the
/// workflow. `Ok(false)` means it is still waiting.
async fn drive_delete(
    st: &Arc<TenantsState>,
    deps: &Arc<Deps>,
    mut row: DpRow,
) -> Result<bool, CpError> {
    let id = parse_tenant(&row.tenant)?;
    let lease = workflow_id('d', &id);
    if !deps
        .kv
        .lease(
            &lease,
            &st.holder,
            i64::try_from(st.now_ms()).unwrap_or(0),
            LEASE_MS,
        )
        .await?
    {
        return Ok(false);
    }
    let (now, actor, approvers) = (st.now_ms(), row.confirmed_by.clone(), row.approvers.clone());
    let outcome = blocking(deps, approvers, move |plane| plane.delete(&id, &actor, now)).await;
    let _ = deps.kv.unlease(&lease, &st.holder).await;
    match outcome? {
        Ok(_) => {
            row.state = "done".to_owned();
            deps.kv.put("dp", &row.tenant, &to_json(&row)?).await?;
            deps.kv.del("note", &row.tenant).await?;
            Ok(true)
        }
        Err(spot_tenancy::Error::RetentionPending { .. }) => Ok(false),
        Err(error) => {
            note_failure(st, deps, &row.tenant, error.code()).await;
            Err(map_err(&error))
        }
    }
}

async fn note_failure(st: &TenantsState, deps: &Deps, tenant: &str, code: &str) {
    if let Ok(text) = to_json(&Note {
        code: code.to_owned(),
    }) {
        let _ = deps.kv.put("note", tenant, &text).await;
    }
    if let Ok(mut attempts) = st.attempts.lock() {
        *attempts.entry(tenant.to_owned()).or_insert(0) += 1;
    }
}

async fn workflow_get(
    State(st): State<Arc<TenantsState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = async {
        let (kind, rest) = match id.strip_prefix("wf-p-") {
            Some(rest) => ('p', rest),
            None => (
                'd',
                id.strip_prefix("wf-d-")
                    .ok_or_else(|| CpError::new(CpCode::NotFound))?,
            ),
        };
        let tenant_id = in_scope(&call, rest)?;
        let deps = st.deps().await?;
        Ok((
            StatusCode::OK,
            json!({ "data": workflow_view(&deps, tenant_id, kind, st.now_ms()).await? }),
        ))
    }
    .await;
    finish(&call, result)
}

fn page_of(query: &ListQuery) -> (usize, Option<&str>) {
    (
        query.limit.unwrap_or(50).clamp(1, MAX_PAGE),
        query.cursor.as_deref(),
    )
}

async fn placements(
    State(st): State<Arc<TenantsState>>,
    Extension(call): Extension<OperatorCall>,
    Query(q): Query<ListQuery>,
) -> Response {
    let result = async {
        let deps = st.deps().await?;
        let tenants = store_op(&deps, |s| s.list()).await?.map_err(|e| map_err(&e))?;
        let (limit, cursor) = page_of(&q);
        let rows: Vec<Value> = tenants
            .iter()
            .filter(|t| t.state != TenantState::Deleted && call.scope.allows(&t.id.to_string()))
            .filter(|t| cursor.is_none_or(|c| t.id.to_string().as_str() > c))
            .take(limit + 1)
            .map(|t| json!({ "tenant_id": t.id.to_string(), "cluster": t.placement.cluster.as_str(), "node": t.placement.database.as_str(), "region": t.region.as_str() }))
            .collect();
        let next = (rows.len() > limit).then(|| rows.get(limit.saturating_sub(1)).and_then(|r| r["tenant_id"].as_str()).map(str::to_owned)).flatten();
        let data: Vec<Value> = rows.into_iter().take(limit).collect();
        Ok((StatusCode::OK, json!({ "data": data, "next_cursor": next })))
    }
    .await;
    finish(&call, result)
}

async fn clusters(
    State(st): State<Arc<TenantsState>>,
    Extension(call): Extension<OperatorCall>,
    Query(q): Query<ListQuery>,
) -> Response {
    let result = async {
        let deps = st.deps().await?;
        let tenants = store_op(&deps, |s| s.list()).await?.map_err(|e| map_err(&e))?;
        // One row per cluster: its databases are its nodes.
        let mut grouped: BTreeMap<String, (String, u32, u32, u32, bool)> = BTreeMap::new();
        for cluster in &deps.clusters {
            let used = tenants
                .iter()
                .filter(|t| t.state != TenantState::Deleted && t.placement.cluster == cluster.cluster && t.placement.database == cluster.database)
                .filter(|t| call.scope.allows(&t.id.to_string()))
                .count();
            let entry = grouped.entry(cluster.cluster.to_string()).or_insert((cluster.region.to_string(), 0, 0, 0, true));
            entry.1 = entry.1.saturating_add(1);
            entry.2 = entry.2.saturating_add(u32::try_from(used).unwrap_or(u32::MAX));
            entry.3 = entry.3.saturating_add(cluster.capacity);
            entry.4 &= cluster.accepting;
        }
        let (limit, cursor) = page_of(&q);
        let rows: Vec<Value> = grouped
            .iter()
            .filter(|(name, _)| cursor.is_none_or(|c| name.as_str() > c))
            .take(limit + 1)
            .map(|(name, (region, nodes, used, capacity, accepting))| {
                let state = if *used >= *capacity { "red" } else if !*accepting || used.saturating_mul(10) >= capacity.saturating_mul(8) { "amber" } else { "green" };
                json!({ "name": name, "region": region, "nodes": nodes, "tenants": used, "state": state })
            })
            .collect();
        let next = (rows.len() > limit).then(|| rows.get(limit.saturating_sub(1)).and_then(|r| r["name"].as_str()).map(str::to_owned)).flatten();
        let data: Vec<Value> = rows.into_iter().take(limit).collect();
        Ok((StatusCode::OK, json!({ "data": data, "next_cursor": next })))
    }
    .await;
    finish(&call, result)
}

// ---------------------------------------------------------------------------------------------
// The workflow runner
// ---------------------------------------------------------------------------------------------

fn attempts_left(st: &TenantsState, deps: &Deps, tenant: &str) -> bool {
    st.attempts
        .lock()
        .is_ok_and(|a| a.get(tenant).copied().unwrap_or(0) < deps.max_attempts)
}

async fn drive_provision(st: &Arc<TenantsState>, deps: &Arc<Deps>, id: TenantId) {
    let text = id.to_string();
    if !attempts_left(st, deps, &text) {
        return;
    }
    let lease = workflow_id('p', &id);
    let now = i64::try_from(st.now_ms()).unwrap_or(0);
    if !matches!(
        deps.kv.lease(&lease, &st.holder, now, LEASE_MS).await,
        Ok(true)
    ) {
        return;
    }
    let at = st.now_ms();
    let outcome = blocking(deps, Vec::new(), move |plane| {
        plane.run(&id, "control-plane", at)
    })
    .await;
    match outcome {
        Ok(Ok(_)) => {
            let _ = deps.kv.del("note", &text).await;
        }
        Ok(Err(error)) => note_failure(st, deps, &text, error.code()).await,
        Err(_) => {}
    }
    let _ = deps.kv.unlease(&lease, &st.holder).await;
}

/// One runner pass: runs every unfinished provisioning and every confirmed deletion whose
/// retention has ended. Returns the number of workflows it drove.
pub async fn run_once(st: &Arc<TenantsState>) -> usize {
    let Ok(deps) = st.deps().await else { return 0 };
    let Ok(Ok(tenants)) = store_op(&deps, |s| s.list()).await else {
        return 0;
    };
    let mut driven = 0;
    for tenant in tenants {
        let pending = match tenant.state {
            TenantState::Provisioning => true,
            TenantState::Active => {
                let id = tenant.id;
                let journal = store_op(&deps, move |s| s.journal(&id))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                journal
                    .iter()
                    .rev()
                    .find(|e| e.step == Step::Activate)
                    .is_some_and(|e| e.kind == JournalKind::Started)
            }
            _ => false,
        };
        if pending {
            drive_provision(st, &deps, tenant.id).await;
            driven += 1;
        }
    }
    if let Ok(rows) = deps.kv.list("dp").await {
        for (tenant, text) in rows {
            let Ok(row) = from_json::<DpRow>(&text) else {
                continue;
            };
            if row.state == "confirmed"
                && row.retention_ends_ms <= st.now_ms()
                && attempts_left(st, &deps, &tenant)
            {
                let _ = drive_delete(st, &deps, row).await;
                driven += 1;
            }
        }
    }
    driven
}

fn runner_task(st: Arc<TenantsState>) -> TaskFn {
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

/// The routes of the module, for a state.
pub fn routes(state: Arc<TenantsState>) -> Router {
    Router::new()
        .route("/cp/v1/tenants", post(create).get(list))
        .route("/cp/v1/tenants/{id}", get(get_one))
        .route("/cp/v1/tenants/{id}/suspend", post(suspend))
        .route("/cp/v1/tenants/{id}/resume", post(resume))
        .route("/cp/v1/tenants/{id}/deprovision", post(deprovision))
        .route("/cp/v1/tenants/{id}/deprovision/confirm", post(confirm))
        .route("/cp/v1/workflows/{id}", get(workflow_get))
        .route("/cp/v1/placements", get(placements))
        .route("/cp/v1/clusters", get(clusters))
        .with_state(state)
}

/// Mounts the module over a prepared state: routes, the runner and the readiness check.
pub fn register_with(ctx: &mut CpCtx, state: Arc<TenantsState>) {
    ctx.operator_routes(routes(Arc::clone(&state)));
    ctx.add_task(runner_task(Arc::clone(&state)));
    ctx.add_migration(DDL);
    let ready = Arc::clone(&state);
    ctx.add_readiness(Arc::new(move || -> BoxFuture<'static, bool> {
        let ready = Arc::clone(&ready);
        Box::pin(async move { ready.deps().await.is_ok() })
    }));
}

/// Registers the module from the file `SPOT_CP_TENANTS_CONFIG` names. Without it the module is
/// not mounted and readiness reports it as not ready.
///
/// # Errors
/// `cp.internal` when the control database is missing or `cp.invalid` for a bad configuration.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let Ok(path) = std::env::var("SPOT_CP_TENANTS_CONFIG") else {
        tracing::warn!("SPOT_CP_TENANTS_CONFIG is not set: the tenants module is not mounted");
        ctx.add_readiness(Arc::new(|| -> BoxFuture<'static, bool> {
            Box::pin(async { false })
        }));
        return Ok(());
    };
    let text = std::fs::read_to_string(path)
        .map_err(|_| CpError::with_reason(CpCode::Invalid, "tenants.config_unreadable"))?;
    let config = TenantsConfig::from_toml(&text)?;
    let pool = ctx
        .pool
        .clone()
        .ok_or_else(|| CpError::with_reason(CpCode::Internal, "tenants.no_control_database"))?;
    let (clock, audit, cp_config) = (
        Arc::clone(&ctx.clock),
        Arc::clone(&ctx.audit),
        Arc::clone(&ctx.config),
    );
    let init_clock = Arc::clone(&clock);
    let init: Init = Box::new(move || -> BoxFuture<'static, Result<Deps, CpError>> {
        let (config, pool, clock, audit, cp_config) = (
            config.clone(),
            pool.clone(),
            Arc::clone(&init_clock),
            Arc::clone(&audit),
            Arc::clone(&cp_config),
        );
        Box::pin(async move { production(config, pool, clock, audit, &cp_config).await })
    });
    register_with(ctx, TenantsState::lazy(clock, init));
    Ok(())
}
