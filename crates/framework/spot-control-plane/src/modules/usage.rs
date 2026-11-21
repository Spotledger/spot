//! The `usage` module: idempotent usage ingest from tenant hosts, period totals, quota evaluation
//! and the quota answer a host can fetch (WP-CTL-05.T5, ADR-061).
//!
//! - `POST /cp/v1/host/usage`: a batch of events keyed by event id (at most 1000 events and 256 KiB).
//!   The tenant comes from the host's certificate, never from the body: a body that names a tenant is
//!   refused. Recording is idempotent through `spot-metering-pg` (a repeated id counts once, two
//!   concurrent ingests of the same event converge). Quotas are evaluated on ingest; a soft exceed
//!   leaves a notice row (`cp_usage_notice`) for the notifier, a hard exceed is returned to the host.
//! - `GET /cp/v1/host/quota`: the host's own quota state: `allowed`, `soft_exceeded` or
//!   `hard_exceeded`, and what is limited. The host decides what to do: it **never blocks posting of
//!   accounting data on a metering outage**; when the control plane is unreachable it keeps working and
//!   queues its events (WP-CTL-05.T6).
//! - `GET /cp/v1/usage?tenant=ID[&at=UNIX]`: an operator reads one tenant's totals per meter for the
//!   period containing `at` (default now), within the operator's tenant scope.
//! - A daily job applies the tombstone retention (`spot_metering::RetentionPolicy`) to every tenant
//!   that has reported.
//!
//! Quota rules live in `cp_usage_quota` (tenant, meter, limit, mode), filled by the edition and quota
//! operations of the licensing module; this module only reads them. No personal data is accepted: an
//! active-user event carries a salted digest, never an id.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use deadpool_postgres::Pool;
use serde::Deserialize;
use serde_json::{Value, json};
use spot_metering::{
    MemberDigest, Meter, MeterEvent, MeterLimit, MeterStore, MeteringError, PeriodKey,
    PeriodResolver, PeriodSpec, QuotaDecision, QuotaRule, RetentionPolicy, TenantPeriodUsage,
    TenantPeriods, verdict_for,
};
use spot_metering_pg::PgMeterStore;
use tokio::sync::watch;

use crate::auth::{HostIdentity, OperatorCall, RequestId};
use crate::{CpCode, CpCtx, CpError};

/// Registration order.
pub const ORDER: u32 = 500;

/// Events per batch.
pub const MAX_EVENTS: usize = 1000;
/// Bytes per batch.
pub const MAX_BODY: usize = 256 * 1024;
/// How often the retention job runs.
const RETENTION_EVERY: Duration = Duration::from_secs(24 * 3600);
/// How long a soft or hard grace lasts before the verdict says so (seconds).
const GRACE: Duration = Duration::from_secs(7 * 24 * 3600);

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_usage_quota (\
        tenant text NOT NULL, meter text NOT NULL, limit_value bigint NOT NULL CHECK (limit_value >= 0), \
        mode text NOT NULL DEFAULT 'warn' CHECK (mode IN ('warn', 'soft', 'hard')), \
        PRIMARY KEY (tenant, meter))",
    "CREATE TABLE IF NOT EXISTS cp_usage_notice (\
        tenant text NOT NULL, meter text NOT NULL, period text NOT NULL, state text NOT NULL, \
        noticed_at bigint NOT NULL, PRIMARY KEY (tenant, meter, period, state))",
    "CREATE TABLE IF NOT EXISTS cp_usage_seen (tenant text PRIMARY KEY, last_seen bigint NOT NULL)",
];

struct UsageState {
    pool: Pool,
    clock: Arc<dyn spot_core::Clock>,
    periods: TenantPeriods,
    prepared: tokio::sync::OnceCell<()>,
}

/// Registers the routes, the retention task, readiness and DDL. Without a database pool the module
/// registers nothing but a failing readiness check.
///
/// # Errors
/// [`CpCode::Internal`] when the default period (UTC calendar month) cannot be built.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    ctx.add_migration(DDL);
    let Some(pool) = ctx.pool.clone() else {
        ctx.add_readiness(Arc::new(|| {
            Box::pin(async { false }) as spot_mgmt_api::ports::BoxFuture<'static, bool>
        }));
        return Ok(());
    };
    let spec = PeriodSpec::calendar_month("UTC")
        .map_err(|_| CpError::with_reason(CpCode::Internal, "cp.usage_period"))?;
    let state = Arc::new(UsageState {
        pool: pool.clone(),
        clock: Arc::clone(&ctx.clock),
        periods: TenantPeriods::new(spec),
        prepared: tokio::sync::OnceCell::new(),
    });
    ctx.host_routes(
        Router::new()
            .route("/cp/v1/host/usage", post(ingest))
            .route("/cp/v1/host/quota", get(host_quota))
            .with_state(Arc::clone(&state)),
    );
    ctx.operator_routes(
        Router::new()
            .route("/cp/v1/usage", get(operator_usage))
            .with_state(Arc::clone(&state)),
    );
    let job = Arc::clone(&state);
    ctx.add_task(Box::new(move |stop| Box::pin(retention_loop(job, stop))));
    ctx.add_readiness(Arc::new(move || {
        let pool = pool.clone();
        Box::pin(async move { pool.get().await.is_ok() })
            as spot_mgmt_api::ports::BoxFuture<'static, bool>
    }));
    Ok(())
}

// ---- the wire --------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    events: Vec<WireEvent>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEvent {
    id: String,
    meter: String,
    #[serde(default)]
    amount: u64,
    member: Option<String>,
    at: i64,
}

fn meter_of(name: &str) -> Option<Meter> {
    match name {
        "active_users" => Some(Meter::ActiveUsers),
        "transactions" => Some(Meter::Transactions),
        "storage_bytes" => Some(Meter::StorageBytes),
        "ai_tokens" => Some(Meter::AiTokens),
        other => {
            let valid = !other.is_empty()
                && other.len() <= 64
                && other.starts_with("custom_")
                && other
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            valid.then(|| Meter::Custom(other.to_owned()))
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Turns the wire events of one tenant into metering events, or names the first bad field path.
fn events_of(tenant: &str, wire: Vec<WireEvent>) -> Result<Vec<MeterEvent>, String> {
    let mut out = Vec::with_capacity(wire.len());
    for (index, event) in wire.into_iter().enumerate() {
        let path = |field: &str| format!("events[{index}].{field}");
        if !valid_id(&event.id) {
            return Err(path("id"));
        }
        let meter = meter_of(&event.meter).ok_or_else(|| path("meter"))?;
        if event.at < 0 {
            return Err(path("at"));
        }
        let built = match (meter.kind(), event.member) {
            (spot_metering::MeterKind::DistinctSet, Some(member)) => {
                // A digest, never an id: hex of at least 16 characters.
                let digest_ok = member.len() >= 16
                    && member.len() <= 128
                    && member.bytes().all(|b| b.is_ascii_hexdigit());
                if !digest_ok {
                    return Err(path("member"));
                }
                MeterEvent::member(event.id, tenant, meter, MemberDigest::new(member), event.at)
            }
            (spot_metering::MeterKind::DistinctSet, None) => return Err(path("member")),
            (_, Some(_)) => return Err(path("member")),
            (_, None) => MeterEvent::amount(event.id, tenant, meter, event.amount, event.at),
        };
        out.push(built);
    }
    Ok(out)
}

fn fail(error: CpError, request: &RequestId) -> Response {
    error.into_response_with(&request.0)
}

fn internal(request: &RequestId, reason: &'static str) -> Response {
    fail(CpError::with_reason(CpCode::Internal, reason), request)
}

fn metering_error(error: &MeteringError, request: &RequestId) -> Response {
    match error {
        MeteringError::StoreUnavailable { .. } => internal(request, "cp.usage_store"),
        MeteringError::BatchTooLarge { .. } => fail(CpError::new(CpCode::BodyTooLarge), request),
        _ => fail(CpError::invalid(&["events"]), request),
    }
}

// ---- quota rules and verdicts ----------------------------------------------------------------

async fn rules_of(pool: &Pool, tenant: &str) -> Result<BTreeMap<Meter, QuotaRule>, ()> {
    let client = pool.get().await.map_err(|_| ())?;
    let rows = client
        .query(
            "SELECT meter, limit_value, mode FROM cp_usage_quota WHERE tenant = $1",
            &[&tenant],
        )
        .await
        .map_err(|_| ())?;
    let mut rules = BTreeMap::new();
    for row in &rows {
        let name: String = row.get(0);
        let limit: i64 = row.get(1);
        let mode: String = row.get(2);
        let (Some(meter), Ok(limit)) = (meter_of(&name), u64::try_from(limit)) else {
            continue;
        };
        let rule = QuotaRule::new(MeterLimit::UpTo(limit));
        let rule = match mode.as_str() {
            "soft" => rule.soft_blocking(),
            "hard" => rule.hard_blocking(),
            _ => rule,
        };
        rules.insert(meter, rule);
    }
    Ok(rules)
}

/// One meter's verdict as the API words it.
fn state_word(decision: &QuotaDecision) -> &'static str {
    match decision {
        QuotaDecision::Ok { .. } | QuotaDecision::Warn { .. } => "allowed",
        QuotaDecision::OverSoft { .. } => "soft_exceeded",
        QuotaDecision::Blocked { .. } => "hard_exceeded",
    }
}

fn worst(states: &[&'static str]) -> &'static str {
    if states.contains(&"hard_exceeded") {
        "hard_exceeded"
    } else if states.contains(&"soft_exceeded") {
        "soft_exceeded"
    } else {
        "allowed"
    }
}

/// The quota answer for one tenant: the worst state and what is limited. Never another tenant's
/// numbers.
fn quota_answer(
    rules: &BTreeMap<Meter, QuotaRule>,
    usage: &TenantPeriodUsage,
    period: &PeriodKey,
    now: i64,
) -> (&'static str, Value) {
    let mut states = Vec::new();
    let mut meters = Vec::new();
    for (meter, rule) in rules {
        let used = usage.total(meter);
        let decision = verdict_for(used, rule, now, GRACE);
        let word = state_word(&decision);
        states.push(word);
        let limit = match rule.limit {
            MeterLimit::UpTo(limit) => json!(limit),
            MeterLimit::Unlimited => Value::Null,
        };
        meters.push(json!({ "meter": meter.name(), "used": used, "limit": limit, "state": word }));
    }
    let overall = worst(&states);
    (
        overall,
        json!({ "state": overall, "period": period.label, "meters": meters }),
    )
}

async fn prepared(state: &UsageState, store: &PgMeterStore) -> Result<(), ()> {
    state
        .prepared
        .get_or_try_init(|| async { store.prepare().await.map_err(|_| ()) })
        .await
        .map(|_| ())
}

// ---- handlers --------------------------------------------------------------------------------

async fn ingest(
    State(state): State<Arc<UsageState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
    body: Bytes,
) -> Response {
    if body.len() > MAX_BODY {
        return fail(CpError::new(CpCode::BodyTooLarge), &request);
    }
    // A body that names a tenant is refused: the tenant is the certificate's.
    if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&body)
        && map.contains_key("tenant")
    {
        return fail(CpError::invalid(&["tenant"]), &request);
    }
    let Ok(batch) = serde_json::from_slice::<Batch>(&body) else {
        return fail(CpError::invalid(&["body"]), &request);
    };
    if batch.events.is_empty() || batch.events.len() > MAX_EVENTS {
        return fail(CpError::invalid(&["events"]), &request);
    }
    let received = batch.events.len();
    let events = match events_of(&host.tenant, batch.events) {
        Ok(events) => events,
        Err(path) => return fail(CpError::invalid(&[path.as_str()]), &request),
    };
    let store = PgMeterStore::new(state.pool.clone(), host.tenant.clone());
    if prepared(&state, &store).await.is_err() {
        return internal(&request, "cp.usage_store");
    }
    let accepted = match store.add_events(events, &state.periods).await {
        Ok(count) => count,
        Err(error) => return metering_error(&error, &request),
    };
    let now = state.clock.now_utc().unix_timestamp();
    // Remember the tenant for the retention job; a failure here does not fail the ingest.
    if let Ok(client) = state.pool.get().await {
        let _ = client
            .execute(
                "INSERT INTO cp_usage_seen (tenant, last_seen) VALUES ($1, $2) \
                 ON CONFLICT (tenant) DO UPDATE SET last_seen = EXCLUDED.last_seen",
                &[&host.tenant, &now],
            )
            .await;
    }
    let verdict = evaluate(&state, &store, &host.tenant, now).await;
    match verdict {
        Ok((overall, answer, period)) => {
            if overall == "soft_exceeded" {
                notify_soft(&state, &host.tenant, &answer, &period, now).await;
            }
            Json(json!({ "data": { "received": received, "accepted": accepted, "quota": answer } }))
                .into_response()
        }
        // The events are recorded; only the verdict could not be computed. The host keeps working.
        Err(()) => {
            Json(json!({ "data": { "received": received, "accepted": accepted, "quota": null } }))
                .into_response()
        }
    }
}

async fn evaluate(
    state: &UsageState,
    store: &PgMeterStore,
    tenant: &str,
    now: i64,
) -> Result<(&'static str, Value, String), ()> {
    let period = state.periods.resolve(tenant, now).map_err(|_| ())?;
    let usage = store
        .period_totals(tenant, &period, &state.periods)
        .await
        .map_err(|_| ())?;
    let rules = rules_of(&state.pool, tenant).await?;
    let (overall, answer) = quota_answer(&rules, &usage, &period, now);
    Ok((overall, answer, period.label))
}

/// Leaves one notice per (tenant, meter, period) for a soft exceed.
async fn notify_soft(state: &UsageState, tenant: &str, answer: &Value, period: &str, now: i64) {
    let Ok(client) = state.pool.get().await else {
        return;
    };
    for meter in answer["meters"].as_array().into_iter().flatten() {
        if meter["state"] == "soft_exceeded"
            && let Some(name) = meter["meter"].as_str()
        {
            let _ = client
                .execute(
                    "INSERT INTO cp_usage_notice (tenant, meter, period, state, noticed_at) \
                     VALUES ($1, $2, $3, 'soft_exceeded', $4) ON CONFLICT DO NOTHING",
                    &[&tenant, &name, &period, &now],
                )
                .await;
        }
    }
}

async fn host_quota(
    State(state): State<Arc<UsageState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
) -> Response {
    let store = PgMeterStore::new(state.pool.clone(), host.tenant.clone());
    if prepared(&state, &store).await.is_err() {
        return internal(&request, "cp.usage_store");
    }
    let now = state.clock.now_utc().unix_timestamp();
    match evaluate(&state, &store, &host.tenant, now).await {
        Ok((_, answer, _)) => Json(json!({ "data": answer })).into_response(),
        Err(()) => internal(&request, "cp.usage_store"),
    }
}

async fn operator_usage(
    State(state): State<Arc<UsageState>>,
    Extension(call): Extension<OperatorCall>,
    RawQuery(query): RawQuery,
) -> Response {
    let request = RequestId(call.request_id.clone());
    let mut tenant = None;
    let mut at = None;
    for pair in query.as_deref().unwrap_or("").split('&') {
        match pair.split_once('=') {
            Some(("tenant", value)) => tenant = Some(value.to_owned()),
            Some(("at", value)) => at = value.parse::<i64>().ok(),
            _ => {}
        }
    }
    let Some(tenant) = tenant.filter(|t| !t.is_empty() && t.len() <= 63) else {
        return fail(CpError::invalid(&["tenant"]), &request);
    };
    // Out of scope is the same answer as missing.
    if let Err(error) = call.scope.require(&tenant) {
        return fail(error, &request);
    }
    let now = state.clock.now_utc().unix_timestamp();
    let period = match state.periods.resolve(&tenant, at.unwrap_or(now)) {
        Ok(period) => period,
        Err(_) => return fail(CpError::invalid(&["at"]), &request),
    };
    let store = PgMeterStore::new(state.pool.clone(), tenant.clone());
    if prepared(&state, &store).await.is_err() {
        return internal(&request, "cp.usage_store");
    }
    match store.period_totals(&tenant, &period, &state.periods).await {
        Ok(usage) => {
            let meters: Vec<Value> = usage
                .by_meter
                .keys()
                .map(|meter| json!({ "meter": meter.name(), "total": usage.total(meter) }))
                .collect();
            Json(json!({ "data": { "tenant": tenant, "period": period.label, "meters": meters } }))
                .into_response()
        }
        Err(error) => metering_error(&error, &request),
    }
}

// ---- retention -------------------------------------------------------------------------------

/// Applies the tombstone retention to every tenant that has reported; returns how many tenants were
/// compacted. The cut is the period containing `now`: everything older is final.
///
/// # Errors
/// `Err(())` when the database cannot be read; a tenant whose compaction fails is skipped and the
/// next run tries it again.
#[allow(clippy::result_unit_err)]
pub async fn run_retention_once(
    pool: &Pool,
    clock: &dyn spot_core::Clock,
    periods: &TenantPeriods,
) -> Result<usize, ()> {
    let client = pool.get().await.map_err(|_| ())?;
    let rows = client
        .query("SELECT tenant FROM cp_usage_seen ORDER BY tenant", &[])
        .await
        .map_err(|_| ())?;
    let now = clock.now_utc().unix_timestamp();
    let policy = RetentionPolicy::new(RetentionPolicy::DEFAULT_HORIZON_PERIODS);
    let mut compacted = 0;
    for row in &rows {
        let tenant: String = row.get(0);
        let Ok(current) = periods.resolve(&tenant, now) else {
            continue;
        };
        let store = PgMeterStore::new(pool.clone(), tenant.clone());
        if store.prepare().await.is_ok() && store.compact(&current, &policy, periods).await.is_ok()
        {
            compacted += 1;
        }
    }
    Ok(compacted)
}

async fn retention_loop(state: Arc<UsageState>, mut stop: watch::Receiver<bool>) {
    loop {
        let _ = run_retention_once(&state.pool, state.clock.as_ref(), &state.periods).await;
        tokio::select! {
            () = tokio::time::sleep(RETENTION_EVERY) => {}
            _ = stop.changed() => return,
        }
    }
}

/// The status word a tests helper maps a decision to (kept public so the integration tests and a
/// future console share one vocabulary).
#[must_use]
pub fn quota_state_word(mode_hard: bool, used: u64, limit: u64) -> &'static str {
    let rule = {
        let rule = QuotaRule::new(MeterLimit::UpTo(limit));
        if mode_hard {
            rule.hard_blocking()
        } else {
            rule.soft_blocking()
        }
    };
    state_word(&verdict_for(used, &rule, 0, GRACE))
}
