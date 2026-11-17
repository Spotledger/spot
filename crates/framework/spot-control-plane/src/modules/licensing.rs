//! The `licensing` module (`WP-CTL-04.T2`): issue, renew, revoke and list signed licences, and the
//! host-side fetch of the calling tenant's current licence.
//!
//! - The licence signing key is behind [`LicenceKeyVault`] (a mode-checked file in production),
//!   never the host CA key, and never in a log or an answer. [`LicensingState::rotate`] installs a
//!   new key and key id; licences signed earlier stay valid on the hosts until they expire
//!   because hosts verify with `TrustedKeys` windows.
//! - Licences live in the control schema rows the `tenants` module also writes
//!   (`licence_active`, `licence_serial`) plus `licence_meta` (the listing facts) and
//!   `licence_revoked`. Issuance uses the control plane clock; the signed licence carries
//!   `issued_at`, which the host's `ClockGuard` compares with its own clock.
//! - A host receives only its own licence (`GET /cp/v1/host/licence`, tenant from the
//!   certificate); a revoked licence is no longer served (404) and is listed as `revoked`.
//! - Every issue, renew and revoke is audited by the operator middleware with principal and
//!   operation; the reason of a revocation is kept in the control schema, not in the audit.

use std::sync::{Arc, RwLock};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spot_core::Clock;
use spot_license::issuing::Issuer;
use spot_license::model::{
    Edition, Entitlements, Kind, Licence, Limit, PAYLOAD_VERSION, RawLicence,
};
use spot_license::{SignedLicence, apply_renewal};

use super::tenants_kv::{Kv, MemoryKv, PgKv, from_json, store_failed, to_json};
use super::tenants_state::{finish, rfc3339};
use crate::auth::{HostIdentity, OperatorCall, sha256_hex};
use crate::{CpCode, CpCtx, CpError};

/// Registration order: after `tenants`.
pub const ORDER: u32 = 30;

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_tenants_kv (ns TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL, PRIMARY KEY (ns, k))",
];
const MAX_PAGE: usize = 100;
const DEFAULT_GRACE_SECS: u64 = 7 * 86_400;

/// Where the licence signing seed comes from. The seed never leaves the vault except into the
/// [`Issuer`].
pub trait LicenceKeyVault: Send + Sync {
    /// The 32-byte Ed25519 seed of `key_id`.
    ///
    /// # Errors
    /// `cp.internal` when the key cannot be read.
    fn signing_seed(&self, key_id: &str) -> Result<[u8; 32], CpError>;
}

/// A vault over one hex seed file (64 hex characters). On Unix a file readable by group or others
/// is refused.
pub struct FileLicenceVault {
    path: std::path::PathBuf,
}

impl FileLicenceVault {
    /// A vault over `path`.
    #[must_use]
    pub fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

impl LicenceKeyVault for FileLicenceVault {
    fn signing_seed(&self, _key_id: &str) -> Result<[u8; 32], CpError> {
        let fail = || CpError::with_reason(CpCode::Internal, "licensing.key_unavailable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let meta = std::fs::metadata(&self.path).map_err(|_| fail())?;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(fail());
            }
        }
        let text = std::fs::read_to_string(&self.path).map_err(|_| fail())?;
        crate::config::decode_key(text.trim()).ok_or_else(fail)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Meta {
    id: String,
    tenant: String,
    plan: String,
    starts_at: i64,
    ends_at: i64,
    retention_days: u32,
    entitlements: Vec<String>,
}

/// The state of the module.
pub struct LicensingState {
    kv: Arc<dyn Kv>,
    clock: Arc<dyn Clock>,
    issuer: RwLock<(Arc<Issuer>, String)>,
}

impl LicensingState {
    /// A state signing with `seed` under `key_id`.
    #[must_use]
    pub fn new(kv: Arc<dyn Kv>, clock: Arc<dyn Clock>, seed: &[u8; 32], key_id: &str) -> Arc<Self> {
        let issuer = Arc::new(Issuer::new(
            ed25519_dalek::SigningKey::from_bytes(seed),
            key_id,
        ));
        Arc::new(Self {
            kv,
            clock,
            issuer: RwLock::new((issuer, key_id.to_owned())),
        })
    }

    /// Signs new licences with `seed` under `key_id` from now on; earlier licences are untouched
    /// and stay valid until they expire.
    ///
    /// # Errors
    /// `cp.internal` when the state is poisoned.
    pub fn rotate(&self, seed: &[u8; 32], key_id: &str) -> Result<(), CpError> {
        let issuer = Arc::new(Issuer::new(
            ed25519_dalek::SigningKey::from_bytes(seed),
            key_id,
        ));
        *self.issuer.write().map_err(|_| store_failed())? = (issuer, key_id.to_owned());
        Ok(())
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn current(&self) -> Result<(Arc<Issuer>, String), CpError> {
        let guard = self.issuer.read().map_err(|_| store_failed())?;
        Ok((Arc::clone(&guard.0), guard.1.clone()))
    }
}

fn parse_time(text: &str) -> Option<i64> {
    // `YYYY-MM-DDTHH:MM:SSZ`
    let b = text.as_bytes();
    if b.len() != 20 || b.get(10) != Some(&b'T') || b.get(19) != Some(&b'Z') {
        return None;
    }
    let num = |a: usize, z: usize| text.get(a..z)?.parse::<i64>().ok();
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hh, mm, ss) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn time_text(secs: i64) -> String {
    rfc3339(u64::try_from(secs).unwrap_or(0))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueBody {
    tenant_id: String,
    plan: String,
    starts_at: String,
    ends_at: String,
    retention_days: Option<u32>,
    #[serde(default)]
    entitlements: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenewBody {
    ends_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeBody {
    reason: String,
}

#[derive(Deserialize, Default)]
struct ListQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

fn mint(
    state: &LicensingState,
    tenant: &str,
    plan: &str,
    window: (i64, i64),
    features: &[String],
    serial: u64,
) -> Result<(Licence, SignedLicence), CpError> {
    let (issuer, key_id) = state.current()?;
    let trial = plan.starts_with("trial");
    let raw = RawLicence {
        v: PAYLOAD_VERSION,
        licence_id: format!("lic-{tenant}"),
        tenant: Some(tenant.to_owned()),
        installation_id: None,
        edition: Edition::new(plan).map_err(|_| CpError::invalid(&["plan"]))?,
        ent: Entitlements {
            users: Some(Limit::Unlimited),
            apps: Vec::new(),
            features: features.to_vec(),
            extra: std::collections::BTreeMap::new(),
        },
        kind: if trial {
            Kind::Trial
        } else {
            Kind::Subscription
        },
        issued_at: state.now(),
        not_before: window.0,
        expires_at: Some(window.1),
        renew_after: None,
        grace: if trial { 0 } else { DEFAULT_GRACE_SECS },
        serial,
        key_id,
    };
    let licence = Licence::new(raw).map_err(|_| CpError::invalid(&["ends_at"]))?;
    let signed = issuer
        .sign(&licence)
        .map_err(|_| CpError::with_reason(CpCode::Internal, "licensing.sign"))?;
    Ok((licence, signed))
}

async fn serial_of(state: &LicensingState, tenant: &str) -> Result<u64, CpError> {
    let text = state.kv.get("licence_serial", tenant).await?;
    Ok(text.and_then(|t| t.parse().ok()).unwrap_or(0))
}

async fn save(
    state: &LicensingState,
    meta: &Meta,
    signed: &SignedLicence,
    serial: u64,
) -> Result<(), CpError> {
    let text = signed.to_text().map_err(|_| store_failed())?;
    state.kv.put("licence_active", &meta.tenant, &text).await?;
    state
        .kv
        .put("licence_serial", &meta.tenant, &serial.to_string())
        .await?;
    state
        .kv
        .put("licence_meta", &meta.tenant, &to_json(meta)?)
        .await
}

async fn meta_of(state: &LicensingState, tenant: &str) -> Result<Meta, CpError> {
    let text = state
        .kv
        .get("licence_meta", tenant)
        .await?
        .ok_or_else(|| CpError::new(CpCode::NotFound))?;
    from_json(&text)
}

async fn view(state: &LicensingState, meta: &Meta) -> Result<Value, CpError> {
    let revoked = state
        .kv
        .get("licence_revoked", &meta.tenant)
        .await?
        .is_some();
    let status = if revoked {
        "revoked"
    } else if state.now() > meta.ends_at {
        "expired"
    } else {
        "active"
    };
    Ok(json!({
        "id": meta.id, "tenant_id": meta.tenant, "plan": meta.plan, "status": status,
        "starts_at": time_text(meta.starts_at), "ends_at": time_text(meta.ends_at),
        "retention_days": meta.retention_days, "entitlements": meta.entitlements,
    }))
}

fn valid_tenant(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

async fn issue(
    State(st): State<Arc<LicensingState>>,
    Extension(call): Extension<OperatorCall>,
    body: Bytes,
) -> Response {
    let result = async {
        let req: IssueBody =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]))?;
        if !valid_tenant(&req.tenant_id) {
            return Err(CpError::invalid(&["tenant_id"]));
        }
        call.scope.require(&req.tenant_id)?;
        let starts = parse_time(&req.starts_at).ok_or_else(|| CpError::invalid(&["starts_at"]))?;
        let ends = parse_time(&req.ends_at).ok_or_else(|| CpError::invalid(&["ends_at"]))?;
        if st.kv.get("licence_active", &req.tenant_id).await?.is_some() {
            return Err(CpError::with_reason(
                CpCode::Conflict,
                "licensing.already_issued",
            ));
        }
        let serial = serial_of(&st, &req.tenant_id).await?.saturating_add(1);
        let (licence, signed) = mint(
            &st,
            &req.tenant_id,
            &req.plan,
            (starts, ends),
            &req.entitlements,
            serial,
        )?;
        let meta = Meta {
            id: licence.licence_id.clone(),
            tenant: req.tenant_id,
            plan: req.plan,
            starts_at: starts,
            ends_at: ends,
            retention_days: req.retention_days.unwrap_or(30),
            entitlements: req.entitlements,
        };
        st.kv.del("licence_revoked", &meta.tenant).await?;
        save(&st, &meta, &signed, serial).await?;
        Ok((
            StatusCode::CREATED,
            json!({ "data": view(&st, &meta).await? }),
        ))
    }
    .await;
    finish(&call, result)
}

async fn renew(
    State(st): State<Arc<LicensingState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let result = async {
        call.scope.require(&id)?;
        let req: RenewBody =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]))?;
        let ends = parse_time(&req.ends_at).ok_or_else(|| CpError::invalid(&["ends_at"]))?;
        let mut meta = meta_of(&st, &id).await?;
        if st.kv.get("licence_revoked", &id).await?.is_some() {
            return Err(CpError::with_reason(CpCode::Conflict, "licensing.revoked"));
        }
        let text = st
            .kv
            .get("licence_active", &id)
            .await?
            .ok_or_else(|| CpError::new(CpCode::NotFound))?;
        let current = SignedLicence::from_text(&text)
            .and_then(|s| s.claimed_licence())
            .map_err(|_| store_failed())?;
        let serial = serial_of(&st, &id).await?.saturating_add(1);
        let (incoming, signed) = mint(
            &st,
            &id,
            &meta.plan,
            (meta.starts_at, ends),
            &meta.entitlements,
            serial,
        )?;
        apply_renewal(&current, &incoming)
            .map_err(|_| CpError::with_reason(CpCode::Conflict, "licensing.stale_renewal"))?;
        meta.ends_at = ends;
        save(&st, &meta, &signed, serial).await?;
        Ok((StatusCode::OK, json!({ "data": view(&st, &meta).await? })))
    }
    .await;
    finish(&call, result)
}

async fn revoke(
    State(st): State<Arc<LicensingState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let result = async {
        call.scope.require(&id)?;
        let req: RevokeBody =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]))?;
        if req.reason.is_empty() || req.reason.len() > 500 {
            return Err(CpError::invalid(&["reason"]));
        }
        let meta = meta_of(&st, &id).await?;
        let record =
            json!({ "at": st.now(), "by": call.principal, "reason": req.reason }).to_string();
        st.kv.put("licence_revoked", &id, &record).await?;
        Ok((StatusCode::OK, json!({ "data": view(&st, &meta).await? })))
    }
    .await;
    finish(&call, result)
}

async fn list(
    State(st): State<Arc<LicensingState>>,
    Extension(call): Extension<OperatorCall>,
    Query(q): Query<ListQuery>,
) -> Response {
    let result = async {
        let limit = q.limit.unwrap_or(50).clamp(1, MAX_PAGE);
        let mut page = Vec::new();
        let mut more = false;
        for (tenant, text) in st.kv.list("licence_meta").await? {
            if !call.scope.allows(&tenant)
                || q.cursor.as_deref().is_some_and(|c| tenant.as_str() <= c)
            {
                continue;
            }
            if page.len() == limit {
                more = true;
                break;
            }
            page.push(view(&st, &from_json::<Meta>(&text)?).await?);
        }
        let next = if more {
            page.last()
                .and_then(|v| v["tenant_id"].as_str())
                .map(str::to_owned)
        } else {
            None
        };
        Ok((StatusCode::OK, json!({ "data": page, "next_cursor": next })))
    }
    .await;
    finish(&call, result)
}

async fn host_licence(
    State(st): State<Arc<LicensingState>>,
    Extension(host): Extension<HostIdentity>,
) -> Response {
    let result = async {
        let tenant = host.tenant.as_str();
        if st.kv.get("licence_revoked", tenant).await?.is_some() {
            return Err(CpError::new(CpCode::NotFound));
        }
        let text = st.kv.get("licence_active", tenant).await?.ok_or_else(|| CpError::new(CpCode::NotFound))?;
        let meta = meta_of(&st, tenant).await?;
        let proof = sha256_hex(text.as_bytes());
        Ok((StatusCode::OK, json!({ "data": { "licence": text, "proof": proof, "valid_until": time_text(meta.ends_at) } })))
    }
    .await;
    match result {
        Ok((status, body)) => (status, axum::Json(body)).into_response(),
        Err(error) => error.into_response_with("host"),
    }
}

/// The routes of the module.
pub fn routes(state: Arc<LicensingState>) -> (Router, Router) {
    let operator = Router::new()
        .route("/cp/v1/licences", post(issue).get(list))
        .route("/cp/v1/licences/{id}/renew", post(renew))
        .route("/cp/v1/licences/{id}/revoke", post(revoke))
        .with_state(Arc::clone(&state));
    let host = Router::new()
        .route("/cp/v1/host/licence", get(host_licence))
        .with_state(state);
    (operator, host)
}

/// Mounts the module over a prepared state.
pub fn register_with(ctx: &mut CpCtx, state: Arc<LicensingState>) {
    let (operator, host) = routes(state);
    ctx.operator_routes(operator);
    ctx.host_routes(host);
    ctx.add_migration(DDL);
}

/// Registers the module with the seed file named by `SPOT_CP_LICENCE_SEED_FILE` (key id in
/// `SPOT_CP_LICENCE_KEY_ID`, default `licence-1`). Without the file the module is not mounted and
/// readiness reports it as not ready.
///
/// # Errors
/// `cp.internal` when the key cannot be read or there is no control database.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let Ok(path) = std::env::var("SPOT_CP_LICENCE_SEED_FILE") else {
        ctx.add_readiness(Arc::new(
            || -> spot_mgmt_api::ports::BoxFuture<'static, bool> { Box::pin(async { false }) },
        ));
        return Ok(());
    };
    let key_id = std::env::var("SPOT_CP_LICENCE_KEY_ID").unwrap_or_else(|_| "licence-1".to_owned());
    let seed = FileLicenceVault::new(path.into()).signing_seed(&key_id)?;
    let pool = ctx
        .pool
        .clone()
        .ok_or_else(|| CpError::with_reason(CpCode::Internal, "licensing.no_control_database"))?;
    let kv: Arc<dyn Kv> = Arc::new(PgKv::new(pool));
    register_with(
        ctx,
        LicensingState::new(kv, Arc::clone(&ctx.clock), &seed, &key_id),
    );
    Ok(())
}

/// An in-memory state for tests and the development profile.
#[must_use]
pub fn memory_state(clock: Arc<dyn Clock>, seed: &[u8; 32], key_id: &str) -> Arc<LicensingState> {
    LicensingState::new(Arc::new(MemoryKv::new()), clock, seed, key_id)
}
