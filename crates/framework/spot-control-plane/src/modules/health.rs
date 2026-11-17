//! The `health` module (`WP-CTL-03.T2`): `spot-fleet-health` hosted by the control plane.
//!
//! - `POST /cp/v1/host/heartbeat` takes a telemetry batch (at most 256 KiB and 200 observations).
//!   The tenant and the host come from the certificate, never from the body: an observation is
//!   about the calling tenant (`scope: tenant`) or the calling host (`scope: node`, subject
//!   `<tenant>:<host id>`). The only names accepted are the library's `Signal` values with their
//!   value ranges, and an item has no other field, so telemetry cannot carry business data or an
//!   unbounded label (`deny_unknown_fields`).
//! - A series budget per tenant (`SeriesBudget`): a batch that would take a tenant past its
//!   budget is refused whole with `cp.cardinality_rejected`, counted, and harms no other tenant.
//! - Evaluation is `spot_fleet_health::evaluate` with the configured `RuleSet`: on each batch (the
//!   alert manager then sees the whole view) and on a timer. A subject that stopped reporting
//!   turns amber/unknown through the freshness rule; a subject persisted in the control schema
//!   stays known across a restart, so a silent host is never shown as healthy.
//! - `GET /cp/v1/health` and `GET /cp/v1/health/tenants/{id}` serve the three colours with the
//!   sentence, the next action, the findings and the open alerts, within the operator's scope.
//! - Alert notifications go through the [`NotifySink`] port; [`WebhookNotifySink`] is the thin
//!   webhook adapter over an [`Egress`] port that the host binary implements with the egress
//!   policy.
//!
//! Observations are kept in memory for the evaluation window (plus the latest per series); the
//! control plane stores no telemetry history.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use spot_core::Clock;
use spot_fleet_health::{
    AlertManager, AlertNotification, AlertSettings, FleetHealth, NodeId, Observation, RuleSet,
    SeriesBudget, SeriesKey, Settings, Signal, Subject, SubjectHealth, TenantId, Timestamp,
    evaluate,
};
use spot_mgmt_api::ports::BoxFuture;

use super::tenants_kv::{Kv, MemoryKv, PgKv};
use super::tenants_state::finish;
use crate::auth::{HostIdentity, MAX_BODY_BYTES, OperatorCall, RequestId};
use crate::{CpCode, CpCtx, CpError};

/// Registration order: clear of the other modules.
pub const ORDER: u32 = 160;

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_tenants_kv (ns TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL, PRIMARY KEY (ns, k))",
];
/// The most observations one batch may carry.
pub const MAX_BATCH: usize = 200;
/// The default series budget per tenant.
pub const DEFAULT_MAX_SERIES: usize = 512;
const FUTURE_SKEW_SECS: i64 = 60;
const PAST_LIMIT_SECS: i64 = 86_400;

/// Delivers an alert notification (email, webhook, ...).
pub trait NotifySink: Send + Sync {
    /// Delivers `notification`.
    ///
    /// # Errors
    /// A short secret-free reason; a failure is counted and never stops the evaluation.
    fn deliver<'a>(
        &'a self,
        notification: &'a AlertNotification,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// The outbound HTTP the webhook sink uses; the host binary implements it over the egress policy
/// (destination allow-list, no private ranges), so this crate opens no socket itself.
pub trait Egress: Send + Sync {
    /// Posts `body` as JSON to `url`.
    ///
    /// # Errors
    /// A short secret-free reason.
    fn post_json<'a>(&'a self, url: &'a str, body: &'a [u8]) -> BoxFuture<'a, Result<(), String>>;
}

/// A webhook notification sink.
pub struct WebhookNotifySink {
    egress: Arc<dyn Egress>,
    url: String,
}

impl WebhookNotifySink {
    /// A sink posting every notification to `url` through `egress`.
    #[must_use]
    pub fn new(egress: Arc<dyn Egress>, url: &str) -> Self {
        Self {
            egress,
            url: url.to_owned(),
        }
    }
}

impl NotifySink for WebhookNotifySink {
    fn deliver<'a>(
        &'a self,
        notification: &'a AlertNotification,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let body = serde_json::to_vec(notification)
                .map_err(|_| "the notification cannot be encoded".to_owned())?;
            self.egress.post_json(&self.url, &body).await
        })
    }
}

static NOTIFIER: OnceLock<Arc<dyn NotifySink>> = OnceLock::new();

/// Installs the production notification sink (called by `main` before the service is built);
/// `false` when one was already set. Without one, alerts are kept and shown but not delivered.
pub fn set_notifier(sink: Arc<dyn NotifySink>) -> bool {
    NOTIFIER.set(sink).is_ok()
}

#[derive(Default)]
struct Inner {
    subjects: BTreeSet<Subject>,
    recent: Vec<Observation>,
    latest: BTreeMap<SeriesKey, Observation>,
    budgets: BTreeMap<String, SeriesBudget>,
    rejected: BTreeMap<String, u64>,
    alerts: Option<AlertManager>,
}

/// The state of the module.
pub struct HealthState {
    kv: Arc<dyn Kv>,
    clock: Arc<dyn Clock>,
    rules: RuleSet,
    settings: Settings,
    max_series: usize,
    sink: Option<Arc<dyn NotifySink>>,
    inner: Mutex<Inner>,
    loaded: tokio::sync::OnceCell<()>,
}

fn internal(reason: &'static str) -> CpError {
    CpError::with_reason(CpCode::Internal, reason)
}

fn tenant_of(subject: &Subject) -> String {
    match subject {
        Subject::Tenant(id) => id.as_str().to_owned(),
        Subject::Node(id) => id.as_str().split(':').next().unwrap_or("").to_owned(),
    }
}

fn subject_row(subject: &Subject) -> String {
    match subject {
        Subject::Tenant(id) => format!("t/{}", id.as_str()),
        Subject::Node(id) => format!("n/{}", id.as_str()),
    }
}

fn parse_row(row: &str) -> Option<Subject> {
    if let Some(id) = row.strip_prefix("t/") {
        return TenantId::try_new(id).ok().map(Subject::Tenant);
    }
    NodeId::try_new(row.strip_prefix("n/")?)
        .ok()
        .map(Subject::Node)
}

impl HealthState {
    /// A state over `kv` evaluating with `rules`, with `max_series` series per tenant and an
    /// optional notification sink.
    ///
    /// # Errors
    /// `cp.internal` when the settings or the alert settings are invalid.
    pub fn new(
        kv: Arc<dyn Kv>,
        clock: Arc<dyn Clock>,
        rules: RuleSet,
        settings: Settings,
        max_series: usize,
        sink: Option<Arc<dyn NotifySink>>,
    ) -> Result<Arc<Self>, CpError> {
        settings
            .validate()
            .map_err(|_| internal("health.settings"))?;
        let alerts = AlertManager::new(AlertSettings::defaults())
            .map_err(|_| internal("health.alert_settings"))?;
        let inner = Inner {
            alerts: Some(alerts),
            ..Inner::default()
        };
        Ok(Arc::new(Self {
            kv,
            clock,
            rules,
            settings,
            max_series,
            sink,
            inner: Mutex::new(inner),
            loaded: tokio::sync::OnceCell::new(),
        }))
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_secs(self.clock.now_utc().unix_timestamp())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, CpError> {
        self.inner.lock().map_err(|_| internal("health.state"))
    }

    /// Loads the persisted subjects once, so a host that stopped reporting before a restart is
    /// still known (and shown unknown) afterwards.
    async fn ensure_loaded(&self) -> Result<(), CpError> {
        self.loaded
            .get_or_try_init(|| async {
                let rows = self.kv.list("health_subjects").await?;
                let mut inner = self.lock()?;
                for (row, _) in rows {
                    if let Some(subject) = parse_row(&row) {
                        inner.subjects.insert(subject);
                    }
                }
                Ok::<(), CpError>(())
            })
            .await
            .map(|_| ())
    }

    /// Evaluates the whole fleet now, pruning observations outside the window.
    fn view(&self, now: Timestamp) -> Result<FleetHealth, CpError> {
        let mut inner = self.lock()?;
        let cutoff = now.saturating_sub_secs(self.settings.window_seconds);
        inner.recent.retain(|o| o.observed_at >= cutoff);
        let mut observations = inner.recent.clone();
        observations.extend(
            inner
                .latest
                .values()
                .filter(|o| o.observed_at < cutoff)
                .cloned(),
        );
        let subjects: Vec<Subject> = inner.subjects.iter().cloned().collect();
        drop(inner);
        evaluate(&subjects, &observations, &self.rules, &self.settings, now)
            .map_err(|_| internal("health.evaluate"))
    }

    /// Evaluates, lets the alert manager see the view and delivers what it decided to send.
    ///
    /// # Errors
    /// `cp.internal` when the state is unusable.
    pub async fn step(&self) -> Result<FleetHealth, CpError> {
        self.ensure_loaded().await?;
        let now = self.now();
        let view = self.view(now)?;
        let batch = {
            let mut inner = self.lock()?;
            inner
                .alerts
                .as_mut()
                .map(|manager| manager.observe(&view, now))
        };
        if let (Some(batch), Some(sink)) = (batch, &self.sink) {
            for notification in &batch.notifications {
                let _ = sink.deliver(notification).await;
            }
        }
        Ok(view)
    }
}

// ---------------------------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum Scope {
    Tenant,
    #[default]
    Node,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    #[serde(default)]
    scope: Scope,
    signal: Signal,
    value: i64,
    observed_at: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    observations: Vec<Item>,
}

async fn heartbeat(
    State(st): State<Arc<HealthState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
    body: Bytes,
) -> Response {
    let result = async {
        if body.len() > MAX_BODY_BYTES {
            return Err(CpError::new(CpCode::BodyTooLarge));
        }
        let batch: Batch =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["observations"]))?;
        if batch.observations.len() > MAX_BATCH {
            return Err(CpError::invalid(&["observations"]));
        }
        let now = st.now();
        let tenant =
            TenantId::try_new(host.tenant.clone()).map_err(|_| CpError::new(CpCode::Forbidden))?;
        let node = NodeId::try_new(format!("{}:{}", host.tenant, host.host_id))
            .map_err(|_| CpError::new(CpCode::Forbidden))?;
        let mut accepted = Vec::with_capacity(batch.observations.len());
        for item in &batch.observations {
            let subject = match item.scope {
                Scope::Tenant => Subject::Tenant(tenant.clone()),
                Scope::Node => Subject::Node(node.clone()),
            };
            let at = item.observed_at.unwrap_or(now.as_secs());
            if at > now.as_secs().saturating_add(FUTURE_SKEW_SECS)
                || at < now.as_secs().saturating_sub(PAST_LIMIT_SECS)
            {
                return Err(CpError::invalid(&["observed_at"]));
            }
            accepted.push(
                Observation::new(subject, item.signal, item.value, Timestamp::from_secs(at))
                    .map_err(|_| CpError::invalid(&["value"]))?,
            );
        }
        st.ensure_loaded().await?;
        let new_subjects = {
            let mut inner = st.lock()?;
            let max = st.max_series;
            let budget = inner
                .budgets
                .entry(host.tenant.clone())
                .or_insert_with(|| SeriesBudget::new(max));
            let fresh: BTreeSet<SeriesKey> = accepted
                .iter()
                .map(SeriesKey::of)
                .filter(|key| !budget.series().contains(key))
                .collect();
            if budget.distinct_series().saturating_add(fresh.len()) > budget.max_series() {
                let over = u64::try_from(fresh.len()).unwrap_or(u64::MAX);
                let counter = inner.rejected.entry(host.tenant.clone()).or_default();
                *counter = counter.saturating_add(over);
                return Err(CpError::new(CpCode::CardinalityRejected));
            }
            let mut new_subjects = Vec::new();
            for observation in &accepted {
                if let Some(budget) = inner.budgets.get_mut(&host.tenant) {
                    let _ = budget.admit(SeriesKey::of(observation));
                }
                if inner.subjects.insert(observation.subject.clone()) {
                    new_subjects.push(observation.subject.clone());
                }
                let key = SeriesKey::of(observation);
                let newer = inner
                    .latest
                    .get(&key)
                    .is_none_or(|old| old.observed_at <= observation.observed_at);
                if newer {
                    inner.latest.insert(key, observation.clone());
                }
                inner.recent.push(observation.clone());
            }
            new_subjects
        };
        for subject in &new_subjects {
            st.kv
                .put("health_subjects", &subject_row(subject), "1")
                .await?;
        }
        // Evaluate on each batch; a notification failure never fails the host's heartbeat.
        let _ = st.step().await;
        Ok((
            StatusCode::OK,
            json!({ "data": { "accepted": accepted.len() } }),
        ))
    }
    .await;
    match result {
        Ok((status, value)) => (status, Json(value)).into_response(),
        Err(error) => error.into_response_with(&request.0),
    }
}

// ---------------------------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------------------------

fn subject_json(entry: &SubjectHealth) -> Value {
    let (kind, id) = match &entry.subject {
        Subject::Tenant(id) => ("tenant", id.as_str()),
        Subject::Node(id) => ("node", id.as_str()),
    };
    json!({
        "kind": kind, "id": id, "tenant_id": tenant_of(&entry.subject),
        "health": entry.health.as_str(), "sentence": entry.sentence, "action": entry.action,
        "stale": entry.stale, "no_data": entry.no_data,
        "last_observed_at": entry.last_observed_at.map(Timestamp::as_secs),
        "findings": entry.findings.iter().map(|f| json!({
            "rule": f.rule_id.as_str(), "health": f.health.as_str(),
            "sentence": f.sentence, "since": f.since.as_secs(),
        })).collect::<Vec<_>>(),
    })
}

impl HealthState {
    fn alerts_json(&self, call: &OperatorCall, only: Option<&str>) -> Result<Vec<Value>, CpError> {
        let inner = self.lock()?;
        let Some(manager) = inner.alerts.as_ref() else {
            return Ok(Vec::new());
        };
        Ok(manager
            .open_alerts()
            .filter(|alert| {
                let tenant = tenant_of(&alert.key.subject);
                call.scope.allows(&tenant) && only.is_none_or(|t| t == tenant)
            })
            .map(|alert| {
                json!({
                    "tenant_id": tenant_of(&alert.key.subject), "rule": alert.key.rule_id.as_str(),
                    "health": alert.health.as_str(), "sentence": alert.sentence,
                    "silenced": alert.silenced, "opened_at": alert.opened_at.as_secs(),
                })
            })
            .collect())
    }

    fn rejected_total(&self, call: &OperatorCall, only: Option<&str>) -> Result<u64, CpError> {
        let inner = self.lock()?;
        Ok(inner
            .rejected
            .iter()
            .filter(|(tenant, _)| call.scope.allows(tenant) && only.is_none_or(|t| t == *tenant))
            .map(|(_, n)| *n)
            .sum())
    }
}

async fn list_health(
    State(st): State<Arc<HealthState>>,
    Extension(call): Extension<OperatorCall>,
) -> Response {
    let result = async {
        let view = st.step().await?;
        let counts = view
            .subjects
            .iter()
            .filter(|s| call.scope.allows(&tenant_of(&s.subject)))
            .fold([0usize; 3], |mut acc, s| {
                match s.health.as_str() {
                    "green" => acc[0] += 1,
                    "amber" => acc[1] += 1,
                    _ => acc[2] += 1,
                }
                acc
            });
        let subjects: Vec<Value> = view
            .subjects
            .iter()
            .filter(|s| call.scope.allows(&tenant_of(&s.subject)))
            .map(subject_json)
            .collect();
        Ok((
            StatusCode::OK,
            json!({ "data": {
                "counts": { "green": counts[0], "amber": counts[1], "red": counts[2] },
                "subjects": subjects, "alerts": st.alerts_json(&call, None)?,
                "cardinality_rejected": st.rejected_total(&call, None)?,
            }}),
        ))
    }
    .await;
    finish(&call, result)
}

async fn tenant_health(
    State(st): State<Arc<HealthState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = async {
        call.scope.require(&id)?;
        let view = st.step().await?;
        let mine: Vec<&SubjectHealth> = view
            .subjects
            .iter()
            .filter(|s| tenant_of(&s.subject) == id)
            .collect();
        if mine.is_empty() {
            return Err(CpError::new(CpCode::NotFound));
        }
        let tenant = mine
            .iter()
            .find(|s| matches!(s.subject, Subject::Tenant(_)));
        let nodes: Vec<Value> = mine
            .iter()
            .filter(|s| matches!(s.subject, Subject::Node(_)))
            .map(|s| subject_json(s))
            .collect();
        Ok((
            StatusCode::OK,
            json!({ "data": {
                "tenant": tenant.map(|s| subject_json(s)), "nodes": nodes,
                "alerts": st.alerts_json(&call, Some(&id))?,
                "cardinality_rejected": st.rejected_total(&call, Some(&id))?,
            }}),
        ))
    }
    .await;
    finish(&call, result)
}

/// The routes of the module: operator routes and host routes.
pub fn routes(state: Arc<HealthState>) -> (Router, Router) {
    let operator = Router::new()
        .route("/cp/v1/health", get(list_health))
        .route("/cp/v1/health/tenants/{id}", get(tenant_health))
        .with_state(Arc::clone(&state));
    let host = Router::new()
        .route("/cp/v1/host/heartbeat", post(heartbeat))
        .with_state(state);
    (operator, host)
}

/// Mounts the module over a prepared state and starts the evaluation timer.
pub fn register_with(ctx: &mut CpCtx, state: Arc<HealthState>) {
    let (operator, host) = routes(Arc::clone(&state));
    ctx.operator_routes(operator);
    ctx.host_routes(host);
    ctx.add_migration(DDL);
    let interval = state.settings.evaluation_interval_seconds;
    ctx.add_task(Box::new(move |mut stop| -> BoxFuture<'static, ()> {
        Box::pin(async move {
            loop {
                let stopped = tokio::select! {
                    changed = stop.changed() => changed.is_err() || *stop.borrow(),
                    () = tokio::time::sleep(std::time::Duration::from_secs(interval)) => false,
                };
                if stopped {
                    break;
                }
                let _ = state.step().await;
            }
        })
    }));
}

/// Registers the module over the control database with the built-in rule set, the default
/// settings and the sink installed by [`set_notifier`] (alerts are shown but not delivered when
/// there is none).
///
/// # Errors
/// `cp.internal` when there is no control database or the rules cannot be loaded.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let pool = ctx
        .pool
        .clone()
        .ok_or_else(|| internal("health.no_control_database"))?;
    let rules = RuleSet::builtin().map_err(|_| internal("health.rules"))?;
    let kv: Arc<dyn Kv> = Arc::new(PgKv::new(pool));
    let state = HealthState::new(
        kv,
        Arc::clone(&ctx.clock),
        rules,
        Settings::defaults(),
        DEFAULT_MAX_SERIES,
        NOTIFIER.get().cloned(),
    )?;
    register_with(ctx, state);
    Ok(())
}

/// An in-memory state for tests and the development profile.
///
/// # Errors
/// As [`HealthState::new`].
pub fn memory_state(
    clock: Arc<dyn Clock>,
    max_series: usize,
    sink: Option<Arc<dyn NotifySink>>,
) -> Result<Arc<HealthState>, CpError> {
    let rules = RuleSet::builtin().map_err(|_| internal("health.rules"))?;
    HealthState::new(
        Arc::new(MemoryKv::new()),
        clock,
        rules,
        Settings::defaults(),
        max_series,
        sink,
    )
}
