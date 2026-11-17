//! AI workload isolation on a running site (WP-AI-05.T4; blueprint WS13, ADR-26): MCP traffic is
//! admitted through `spot-ai-isolation`, reads on its own pool as its own database role, and shrinks
//! to nothing when people wait.
//!
//! # What it does
//!
//! - **A role and a pool of its own.** At start the `spot_ai` role is created idempotently with the
//!   crate's own statements (no DDL, no write grant, read-only by default, `statement_timeout`,
//!   `idle_in_transaction_session_timeout`, `max_parallel_workers_per_gather = 0`, a connection limit),
//!   enabled for login with the password in `services.ai_isolation.password_file`, and a second
//!   [`PgBackend`] logs in as it with `pool_size` connections (default 4). The MCP read tools
//!   (`query`, `search_*`) run on that pool through [`AiQuery`], which applies the same read policy as
//!   the engine (the principal's read rights compiled into the query, masks applied) and refuses
//!   rather than fall back to the main pool when the role cannot read.
//! - **Admission before every MCP call**: per-agent and per-tenant concurrency and rate, the call
//!   deadline, the AIMD limit and the circuit breakers of `spot-ai-isolation`, installed in the MCP
//!   service as its [`AgentAdmission`]. A call that is not admitted is `429` with `Retry-After`.
//! - **Humans first.** A sampler takes a connection from the **main** pool every `sample_ms` and times
//!   it; that wait, the pool's queue depth and the p95 of the last probes are the [`HumanLoad`] signal.
//!   Above the high thresholds AI admission shrinks (to zero), and recovers gradually.
//! - **Metrics** `ai_isolation_aimd_limit`, `ai_isolation_in_flight`, `ai_isolation_shed_total`,
//!   `ai_isolation_refused_total` and `ai_isolation_breakers_open` are published every sample.
//!
//! # Level
//!
//! Optional for the site, **required for MCP once it is switched on** (`enabled = true`, off by
//! default because it needs a role password the operator provides): the module declares that MCP needs admission
//! ([`McpHandle::require_admission`]) before anything else, so if the role, the pool or the password
//! cannot be set up, `/mcp` answers `503 server.unavailable` and nothing is served unisolated. With
//! `services.ai_isolation.enabled = false` (the default) the requirement is not made; with
//! `services.mcp.enabled = false` the module does nothing.
//!
//! # Timeouts
//!
//! The server enforces the statement deadline in the database itself (the role's `statement_timeout`),
//! so a statement past its deadline is cancelled by PostgreSQL and its connection returns to the pool.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use spot_ai_isolation::{
    AI_ROLE, AgentId, AiAdmission, AiError, AiLimits, AiPoolSettings, AiRequest, LoadThresholds,
    Outcome, TenantId,
};
use spot_api_services::ApiErrorExt;
use spot_api_services::query::QueryService;
use spot_app_api::context::Invocation;
use spot_coordinator::Coordinator;
use spot_core::Clock;
use spot_document::engine::{Authorizer, QueryPlan, apply_masks, empty_page};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture};
use spot_postgres::PgBackend;
use spot_storage_contract::{Catalog, Query, ReadSource, Rows, StorageBackend};
use tokio::sync::watch;

use crate::live::SiteLive;
use crate::services::mcp::seams::{AdmitCall, AgentAdmission, Fut, McpHandle, Permit, Shed};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "ai_isolation/load.rs"]
pub mod load;
#[path = "ai_isolation/role.rs"]
pub mod role;

pub use load::HumanLoad;

/// Start order: after the `mcp` module (1060) and the agent tier service (1070).
pub const ORDER: u32 = 1080;

fn default_pool() -> u32 {
    4
}
fn default_statement_ms() -> u64 {
    30_000
}
fn default_sample_ms() -> u64 {
    250
}
fn default_schema() -> String {
    "public".to_owned()
}
fn default_agent_concurrency() -> u32 {
    4
}
fn default_tenant_concurrency() -> u32 {
    16
}

/// `[services.ai_isolation]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Host the isolation. Default **false**: it needs a database role and a password file the
    /// operator provides (see the guide), so a site that has not set them up serves MCP as
    /// `WP-AI-04.T4` describes (still audited, rate limited and strictly as the user). When true, MCP
    /// is refused (503) unless the whole isolation starts.
    #[serde(default)]
    pub enabled: bool,
    /// Connections of the `ai` pool, 1 to 64. Default 4.
    #[serde(default = "default_pool")]
    pub pool_size: u32,
    /// The role's `statement_timeout` in milliseconds, 100 to 600,000. Default 30,000.
    #[serde(default = "default_statement_ms")]
    pub statement_timeout_ms: u64,
    /// Where the `ai` role's login password is kept: an absolute path to a file holding 16 to 128
    /// characters of `[A-Za-z0-9._~-]`. **Required**: without it the pool cannot open and MCP is
    /// refused.
    #[serde(default)]
    pub password_file: Option<PathBuf>,
    /// The schema the role is granted on. Default `public`.
    #[serde(default = "default_schema")]
    pub schema: String,
    /// The load sampling interval in milliseconds, 50 to 10,000. Default 250.
    #[serde(default = "default_sample_ms")]
    pub sample_ms: u64,
    /// The most calls one agent holds at once on this node, 1 to 256. Default 4.
    #[serde(default = "default_agent_concurrency")]
    pub per_agent_concurrency: u32,
    /// The most calls one tenant holds at once on this node, 1 to 1,024. Default 16.
    #[serde(default = "default_tenant_concurrency")]
    pub per_tenant_concurrency: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            pool_size: default_pool(),
            statement_timeout_ms: default_statement_ms(),
            password_file: None,
            schema: default_schema(),
            sample_ms: default_sample_ms(),
            per_agent_concurrency: default_agent_concurrency(),
            per_tenant_concurrency: default_tenant_concurrency(),
        }
    }
}

impl Settings {
    /// The first setting that is out of range.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if !(1..=64).contains(&self.pool_size) {
            return Err("pool_size");
        }
        if !(100..=600_000).contains(&self.statement_timeout_ms) {
            return Err("statement_timeout_ms");
        }
        if !(50..=10_000).contains(&self.sample_ms) {
            return Err("sample_ms");
        }
        if !(1..=256).contains(&self.per_agent_concurrency) {
            return Err("per_agent_concurrency");
        }
        if !(1..=1_024).contains(&self.per_tenant_concurrency) {
            return Err("per_tenant_concurrency");
        }
        if let Some(path) = &self.password_file
            && !path.is_absolute()
        {
            return Err("password_file");
        }
        let ok = !self.schema.is_empty()
            && self.schema.len() <= 63
            && self
                .schema
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !ok {
            return Err("schema");
        }
        Ok(())
    }

    fn pool(&self) -> AiPoolSettings {
        AiPoolSettings {
            pool_size: self.pool_size,
            statement_timeout: Duration::from_millis(self.statement_timeout_ms),
            ..AiPoolSettings::default()
        }
    }

    fn limits(&self) -> AiLimits {
        AiLimits {
            per_agent_concurrency: self.per_agent_concurrency,
            per_tenant_concurrency: self.per_tenant_concurrency,
            call_timeout: Duration::from_millis(self.statement_timeout_ms),
            ..AiLimits::default()
        }
    }
}

/// Reads on the `ai` pool with the engine's read policy.
///
/// The principal's read rights are compiled into the query (a query the principal may not run is
/// refused or empty exactly as on the main path), the rows come from the `ai` backend and the masks
/// are applied after. A failure of the `ai` backend is an error, never a retry on the main pool.
pub struct AiQuery {
    live: Arc<SiteLive>,
    backend: PgBackend,
}

impl QueryService for AiQuery {
    fn read<'a>(
        &'a self,
        inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a Query,
    ) -> BoxFuture<'a, std::result::Result<Rows, ApiError>> {
        Box::pin(async move {
            let authorizer = self
                .live
                .authorizer()
                .ok_or_else(|| ApiError::from_code(Code::ServerUnavailable))?;
            let principal = authorizer.principal_for(inv).await;
            let plan = authorizer
                .readable_query(&principal, query)
                .await
                .map_err(ApiErrorExt::into_api_error)?;
            let Some(QueryPlan { query, masks }) = plan else {
                return Ok(empty_page(query));
            };
            let mut rows = self
                .backend
                .read(catalog, ReadSource::Primary, &query)
                .await
                .map_err(|e| {
                    spot_app_api::error::map_storage_error(
                        &e,
                        spot_app_api::error::StorageOp::Other,
                    )
                    .into_api_error()
                })?;
            apply_masks(&mut rows, &masks);
            Ok(rows)
        })
    }
}

/// The admission the MCP service consults before every call.
pub struct Isolation {
    admission: Arc<AiAdmission>,
    coordinator: Option<Arc<dyn Coordinator>>,
    clock: Arc<dyn Clock>,
    epoch: time::OffsetDateTime,
}

impl Isolation {
    /// The monotonic-looking `now` the admission takes: time since the service started, from the
    /// site clock.
    fn now(&self) -> Duration {
        (self.clock.now_utc() - self.epoch).unsigned_abs()
    }

    /// The typed health snapshot.
    #[must_use]
    pub fn health(&self) -> spot_ai_isolation::AiHealthReport {
        self.admission.health()
    }
}

fn shed_of(error: &AiError) -> Shed {
    let secs = error
        .retry_after
        .map_or(1, |d| d.as_secs() + u64::from(d.subsec_nanos() > 0));
    Shed {
        retry_after_secs: secs.max(1),
    }
}

impl AgentAdmission for Isolation {
    fn admit<'a>(&'a self, call: &'a AdmitCall) -> Fut<'a, std::result::Result<Permit, Shed>> {
        Box::pin(async move {
            let (Ok(agent), Ok(tenant)) = (
                AgentId::new(call.actor.clone()),
                TenantId::new(call.tenant.clone()),
            ) else {
                return Err(Shed {
                    retry_after_secs: 1,
                });
            };
            let request = AiRequest::new(agent, tenant);
            let now = self.now();
            let admitted = match &self.coordinator {
                Some(coordinator) => {
                    self.admission
                        .admit_cluster(coordinator.as_ref(), &request, now)
                        .await
                }
                None => self.admission.admit(&request, now),
            };
            let admission = match admitted {
                Ok(admission) => admission,
                Err(error) => return Err(shed_of(&error)),
            };
            // The admission is released when the permit drops; `finish` feeds the breakers.
            let slot = Arc::new(Mutex::new(Some(admission)));
            let holder = Arc::clone(&slot);
            let clock = Arc::clone(&self.clock);
            let epoch = self.epoch;
            Ok(Permit::holding(slot).on_finish(move |ok| {
                let taken = holder.lock().unwrap_or_else(|p| p.into_inner()).take();
                if let Some(admission) = taken {
                    let now = (clock.now_utc() - epoch).unsigned_abs();
                    admission.complete(
                        now,
                        if ok {
                            Outcome::Success
                        } else {
                            Outcome::Failure
                        },
                    );
                }
            }))
        })
    }
}

/// The service: runs the load sampler and publishes the metrics.
struct IsolationService {
    isolation: Arc<Isolation>,
    load: Arc<HumanLoad>,
    main: PgBackend,
    ai: PgBackend,
    sample: Duration,
}

impl IsolationService {
    async fn probe(&self) {
        let started = tokio::time::Instant::now();
        let got = tokio::time::timeout(Duration::from_secs(2), self.main.pool().get()).await;
        let waited = started.elapsed();
        let status = self.main.pool().status();
        let waiting = u32::try_from(status.waiting).unwrap_or(u32::MAX);
        // A failed or timed-out probe is the worst wait there is.
        let wait = if matches!(got, Ok(Ok(_))) {
            waited
        } else {
            Duration::from_secs(2)
        };
        drop(got);
        self.load.record(wait, waiting);
    }

    fn publish(&self) {
        let report = self.isolation.health();
        metrics::gauge!("ai_isolation_aimd_limit").set(f64::from(report.aimd_limit));
        metrics::gauge!("ai_isolation_in_flight").set(f64::from(report.in_flight));
        metrics::gauge!("ai_isolation_shed_total").set(f64::from(
            u32::try_from(report.shed_total).unwrap_or(u32::MAX),
        ));
        metrics::gauge!("ai_isolation_refused_total").set(f64::from(
            u32::try_from(report.refused_total).unwrap_or(u32::MAX),
        ));
        let open = report
            .breakers
            .iter()
            .filter(|b| b.state != spot_ai_isolation::BreakerState::Closed)
            .count();
        metrics::gauge!("ai_isolation_breakers_open")
            .set(f64::from(u32::try_from(open).unwrap_or(u32::MAX)));
    }
}

impl Service for IsolationService {
    fn name(&self) -> &'static str {
        "ai_isolation"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            // The pool must open as the role, and the role must not be able to write.
            let client =
                self.ai.pool().get().await.map_err(|_| {
                    "the ai pool cannot open a connection as the ai role".to_owned()
                })?;
            let row = client
                .query_one("SELECT current_user::text", &[])
                .await
                .map_err(|_| "the ai pool cannot read".to_owned())?;
            let user: String = row.get(0);
            if user != AI_ROLE {
                return Err("the ai pool is not logged in as the ai role".to_owned());
            }
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                self.probe().await;
                self.publish();
                tokio::select! {
                    _ = stop.changed() => break,
                    () = tokio::time::sleep(self.sample) => {}
                }
                if *stop.borrow() {
                    break;
                }
            }
            Ok(())
        })
    }
}

/// Registers AI isolation: the requirement first, then the role, the pool, the admission, the query
/// routing and the service. Any failure leaves MCP closed.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("ai_isolation")?;
    settings.validate().map_err(|key| {
        Error(format!(
            "services.ai_isolation.{key}: out of range or invalid"
        ))
    })?;
    if !settings.enabled {
        return Ok(());
    }
    let Some(handle) = ctx.port::<McpHandle>() else {
        return Ok(());
    };
    // MCP must never be served unisolated: the requirement stands until the admission is installed.
    handle.require_admission();
    if !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    match build(ctx, &settings).await {
        Ok(service) => {
            let isolation = Arc::clone(&service.isolation);
            handle.install_query_service(Arc::new(AiQuery {
                live: Arc::clone(env.live),
                backend: service.ai.clone(),
            }));
            handle.install_admission(Arc::clone(&isolation) as Arc<dyn AgentAdmission>, true);
            ctx.add(Arc::new(service));
            ctx.provide(isolation)
                .map_err(|e| Error(format!("services.ai_isolation: {e}")))?;
        }
        Err(reason) => {
            tracing::error!(site = %env.site.site, %reason, "ai isolation could not start; MCP is refused");
            handle.close("ai isolation is not available");
            ctx.add(crate::services::Idle::new(
                "ai_isolation",
                Level::Optional,
                move || {
                    let reason = reason.clone();
                    Box::pin(async move { Err(reason) })
                },
            ));
        }
    }
    Ok(())
}

async fn build(
    ctx: &mut SiteCtx<'_>,
    settings: &Settings,
) -> std::result::Result<IsolationService, String> {
    let env = *ctx.env().map_err(|e| e.to_string())?;
    let path = settings
        .password_file
        .as_ref()
        .ok_or("services.ai_isolation.password_file is not set")?;
    let password = role::read_password(path)?;
    let pool = settings.pool();
    let statements =
        role::provisioning_sql(&pool, &env.site.database, &settings.schema, &password)?;
    role::provision(env.backend, &statements).await?;
    let ai = PgBackend::connect(role::ai_config(env.backend.config(), &password, &pool))
        .await
        .map_err(|_| "the ai pool could not connect as the ai role".to_owned())?;
    let load = Arc::new(HumanLoad::default());
    let admission = AiAdmission::new(
        settings.limits(),
        pool,
        Arc::clone(&load) as Arc<dyn spot_ai_isolation::LoadSignal>,
    )
    .map_err(|e| format!("the admission limits are not valid: {}", e.code))?
    .with_thresholds(LoadThresholds::default());
    let isolation = Arc::new(Isolation {
        admission: Arc::new(admission),
        coordinator: ctx.port::<dyn Coordinator>(),
        clock: Arc::clone(ctx.clock()),
        epoch: ctx.clock().now_utc(),
    });
    Ok(IsolationService {
        isolation,
        load,
        main: env.backend.clone(),
        ai,
        sample: Duration::from_millis(settings.sample_ms),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_off_with_a_pool_of_four_and_need_a_password_file() {
        let s = Settings::default();
        assert!(!s.enabled);
        assert!(!toml::from_str::<Settings>("").unwrap().enabled);
        assert!(
            toml::from_str::<Settings>("enabled = true")
                .unwrap()
                .enabled
        );
        assert_eq!(s.pool_size, 4);
        assert!(s.password_file.is_none());
        assert!(s.validate().is_ok());
        assert_eq!(s.pool().pool_size, 4);
        assert_eq!(s.limits().per_agent_concurrency, 4);
    }

    #[test]
    fn out_of_range_values_and_relative_paths_are_refused() {
        let bad = |f: fn(&mut Settings)| {
            let mut s = Settings::default();
            f(&mut s);
            s.validate().unwrap_err()
        };
        assert_eq!(bad(|s| s.pool_size = 0), "pool_size");
        assert_eq!(bad(|s| s.pool_size = 65), "pool_size");
        assert_eq!(bad(|s| s.sample_ms = 1), "sample_ms");
        assert_eq!(
            bad(|s| s.password_file = Some("relative/pw".into())),
            "password_file"
        );
        assert_eq!(bad(|s| s.schema = "Public; DROP".into()), "schema");
        assert!(toml::from_str::<Settings>("nope = 1").is_err());
    }

    #[test]
    fn a_retry_hint_rounds_up_and_is_at_least_one_second() {
        let e = AiError::rate_limited(Duration::from_millis(1_200));
        assert_eq!(shed_of(&e).retry_after_secs, 2);
        let none = AiError::busy("tenant");
        assert_eq!(shed_of(&none).retry_after_secs, 1);
    }
}
