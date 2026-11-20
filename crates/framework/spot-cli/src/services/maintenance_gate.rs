//! `spot serve` honours the maintenance gate (WP-MIG-07.T5, ADR-22).
//!
//! `spot-migration-apply-pg` has the admission gate (one row in `spot_migration_control.gate`) and
//! `spot migrate-apply` closes and opens it. This module is the serving side: it keeps the gate
//! fresh and refuses requests while it is closed.
//!
//! # What users see
//!
//! While the gate is closed every request except liveness, readiness and `/metrics` is refused with
//! `503`, `Retry-After` (30 seconds), and the standard error body with code `server.maintenance` and
//! the request id the telemetry layer set. The refusal happens in a router layer applied **last**
//! (inside a `finally` closure, so it wraps the routes of modules that register after this one), so a
//! refused request reaches no handler, no session check and no database.
//!
//! # Failing closed
//!
//! A gate never read answers closed (so the layer is closed before the service has started); a
//! refresh that fails closes it until a later read succeeds. The one exception is a database that has
//! no control schema at all (a site created before WP-MIG-07.T4: nothing ever migrated through the
//! gate): that is **open**. "No schema" and "schema present but unreadable" are told apart by asking
//! PostgreSQL whether `spot_migration_control.gate` exists, so an unreadable gate is never mistaken
//! for an absent one. A gate that fails closed because it cannot be read (no connection, a failed
//! read) is answered `503` `server.unavailable` with the same `Retry-After`, as a database that
//! gives no connection is everywhere else: no migration closed the site, so it is not in maintenance.
//!
//! # Level
//!
//! Required: a site that cannot read its gate must not serve. `check()` does the first read.
//! The exemptions are exact method and path matches on the request: `GET /api/health/live`,
//! `GET /api/health/ready`, `GET /metrics`. No operator path is exempt (opening the gate is a
//! `spot migrate-apply` or console action done through the database, not through this listener).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use spot_core::{Clock, SiteId};
use spot_http::api::registry::Code;
use spot_http::api::service::ApiError;
use spot_http::request::RequestId;
use spot_migration_apply::gate::{AdmissionGate, GateStatus};
use spot_migration_apply_pg::{PgAdmissionGate, UNREADABLE};
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order.
pub const ORDER: u32 = 1001;

/// Seconds a refused client is told to wait.
const RETRY_AFTER_SECS: u64 = 30;

/// `[services.maintenance_gate]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site honours the gate. On by default.
    pub enabled: bool,
    /// Milliseconds between reads of the gate (100 to 30000).
    pub refresh_ms: u64,
    /// Milliseconds a read stays fresh; never below `refresh_ms`.
    pub ttl_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_ms: 1_000,
            ttl_ms: 2_000,
        }
    }
}

impl Settings {
    /// Checks the ranges.
    ///
    /// # Errors
    /// A message naming the setting.
    pub fn validate(&self) -> Result<()> {
        if !(100..=30_000).contains(&self.refresh_ms) {
            return Err(Error(
                "services.maintenance_gate.refresh_ms must be between 100 and 30000".to_owned(),
            ));
        }
        if self.ttl_ms < self.refresh_ms {
            return Err(Error(
                "services.maintenance_gate.ttl_ms must not be below refresh_ms".to_owned(),
            ));
        }
        Ok(())
    }
}

/// The seconds a refused client waits: the fixed default, never zero.
#[must_use]
pub fn retry_after_secs() -> u64 {
    RETRY_AFTER_SECS.max(1)
}

/// Whether a request is exempt from the gate: an exact method and path match.
#[must_use]
pub fn exempt(method: &Method, path: &str) -> bool {
    *method == Method::GET && matches!(path, "/api/health/live" | "/api/health/ready" | "/metrics")
}

/// The gate and what the layer needs to decide.
pub struct Shared {
    gate: Arc<PgAdmissionGate>,
    pool: deadpool_postgres::Pool,
    no_schema: AtomicBool,
    clock: Arc<dyn Clock>,
}

impl Shared {
    /// Whether the control schema's gate table exists.
    async fn schema_present(&self) -> std::result::Result<bool, ()> {
        let client = self.pool.get().await.map_err(|_| ())?;
        let row = client
            .query_one(
                "SELECT to_regclass('spot_migration_control.gate') IS NOT NULL",
                &[],
            )
            .await
            .map_err(|_| ())?;
        row.try_get::<_, bool>(0).map_err(|_| ())
    }

    /// One read. `Ok` when the gate was read, or when the database has no control schema at all
    /// (open); `Err` (closed) otherwise.
    ///
    /// # Errors
    /// A fixed sentence when the gate cannot be read.
    pub async fn refresh(&self) -> std::result::Result<(), String> {
        let now = self.clock.now_utc().unix_timestamp();
        match self.gate.refresh(now).await {
            Ok(_) => {
                self.no_schema.store(false, Ordering::SeqCst);
                Ok(())
            }
            Err(_) => match self.schema_present().await {
                Ok(false) => {
                    self.no_schema.store(true, Ordering::SeqCst);
                        Ok(())
                }
                _ => {
                    self.no_schema.store(false, Ordering::SeqCst);
                    Err("maintenance_gate: unreadable".to_owned())
                }
            },
        }
    }

    /// Whether the gate is closed because it cannot be read (never read, or the last read failed:
    /// the database is out of reach or out of connections), which a client is told as
    /// `server.unavailable`, not as maintenance: no migration closed it. The gate itself says so
    /// with its [`UNREADABLE`] reason, whoever made the read that failed.
    #[must_use]
    pub fn unreadable(&self) -> bool {
        matches!(self.gate.status(), GateStatus::Closed { reason, .. } if reason == UNREADABLE)
    }

    /// The closed state, when the layer must refuse: `Some(reason)`.
    #[must_use]
    pub fn closed(&self) -> Option<String> {
        if self.no_schema.load(Ordering::SeqCst) {
            return None;
        }
        match self.gate.status() {
            GateStatus::Open => None,
            GateStatus::Closed { reason, since } => {
                Some(format!("maintenance_gate: closed since {since}: {reason}"))
            }
        }
    }
}

/// The refusal while the gate is closed: `503`, `Retry-After` and the standard envelope.
fn refusal(id: &str, unreadable: bool, path: &str) -> Response {
    // A gate that cannot be read fails closed, but the answer is the one a database that gives no
    // connection gets everywhere else (`server.unavailable`, retryable): nothing is in
    // maintenance.
    let error = if unreadable {
        // A sign-in is told what the sign-in service says when it has no connection, so the
        // answer does not depend on which of the two noticed first.
        let error = if path == "/api/v1/auth/login" {
            ApiError::new(
                Code::ServerUnavailable,
                "The sign-in service is temporarily unavailable.",
            )
        } else {
            ApiError::from_code(Code::ServerUnavailable)
        };
        error
            .detail("retry_after", serde_json::Value::from(RETRY_AFTER_SECS))
    } else {
        ApiError::from_code(Code::ServerMaintenance)
    };
    let status =
        StatusCode::from_u16(error.status().as_u16()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    let mut response = (status, axum::Json(error.to_envelope_json(id))).into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs()));
    response
}

/// The host's check, made before anything else of a request (sign-in, the open paths, the
/// realtime upgrade and the router are all behind it): `Some(refusal)` while the site's gate is
/// closed and the request is not an exempt probe. `None` when the site has no gate (disabled, or
/// not assembled) or the gate is open.
#[must_use]
pub fn refuse(site: &str, method: &Method, path: &str, request_id: &str) -> Option<Response> {
    let shared = provided()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(site)
        .cloned()?;
    shared.closed()?;
    if exempt(method, path) {
        return None;
    }
    Some(refusal(request_id, shared.unreadable(), path))
}

async fn layer(State(shared): State<Arc<Shared>>, request: Request, next: Next) -> Response {
    let Some(state) = shared.closed() else {
        return next.run(request).await;
    };
    if !exempt(request.method(), request.uri().path()) {
        let id = request
            .extensions()
            .get::<RequestId>()
            .map(|id| id.as_str().to_owned())
            .unwrap_or_default();
        return refusal(&id, shared.unreadable(), request.uri().path());
    }
    // An exempt probe still tells the operator why the site is closed.
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&state) {
        response
            .headers_mut()
            .insert("x-spot-maintenance-gate", value);
    }
    response
}

struct GateService {
    shared: Arc<Shared>,
    settings: Settings,
}

impl Service for GateService {
    fn name(&self) -> &'static str {
        "maintenance_gate"
    }

    fn level(&self) -> Level {
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move { self.shared.refresh().await })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let every = Duration::from_millis(self.settings.refresh_ms);
            let mut was_ok = true;
            loop {
                tokio::select! {
                    () = tokio::time::sleep(every) => {}
                    _ = stop.changed() => return Ok(()),
                }
                if *stop.borrow() {
                    return Ok(());
                }
                let ok = self.shared.refresh().await.is_ok();
                // One line per transition, never per tick, never a connection string.
                if ok != was_ok {
                    if ok {
                        tracing::info!("maintenance_gate: readable again");
                    } else {
                        tracing::warn!("maintenance_gate: unreadable, the site is closed");
                    }
                    was_ok = ok;
                }
            }
        })
    }
}

/// The gate a running site refreshes, by site name, kept across a reload (the running service is
/// not rebuilt, so the reloaded router must layer the same instance).
fn provided() -> &'static Mutex<HashMap<String, Arc<Shared>>> {
    static PROVIDED: OnceLock<Mutex<HashMap<String, Arc<Shared>>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// Registers the gate: provides it as a port and refuses requests while it is closed.
///
/// # Errors
/// A setting out of range, or a context without a site (`ctx.env()`).
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("maintenance_gate")?;
    if !settings.enabled {
        return Ok(());
    }
    settings.validate()?;
    // A reload builds a new router but not new services: the running service keeps refreshing the
    // gate it was built with, so the reloaded router must layer that same gate. A gate of its own
    // would never be read (nothing refreshes it) and answer closed for ever.
    let running = if ctx.assembling() {
        None
    } else {
        provided()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(ctx.label())
            .cloned()
    };
    // No running gate to share (the site never assembled): a fresh one, never read, so it stays
    // closed (fail closed).
    let shared = if let Some(shared) = running {
        shared
    } else {
        let env = ctx.env()?;
        let site = SiteId::new(&env.site.site).map_err(|e| Error(e.to_string()))?;
        let pool = env.backend.pool();
        let clock = Arc::clone(ctx.clock());
        // The cluster coordinator is not offered as a port by any module yet, so the gate polls
        // (second nodes see a change within refresh_ms).
        let gate = Arc::new(PgAdmissionGate::new(
            pool.clone(),
            site,
            Arc::clone(&clock),
            Duration::from_millis(settings.ttl_ms),
        ));
        let shared = Arc::new(Shared {
            gate,
            pool,
            no_schema: AtomicBool::new(false),
            clock,
        });
        provided()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(ctx.label().to_owned(), Arc::clone(&shared));
        shared
    };
    let gate = Arc::clone(&shared.gate);
    ctx.provide::<PgAdmissionGate>(Arc::clone(&gate))?;
    ctx.provide::<dyn AdmissionGate>(gate)?;
    let for_layer = Arc::clone(&shared);
    // Applied after every module's own router edits, so it wraps routes registered later too.
    ctx.finally(move |ctx| {
        ctx.router_layer(move |router| {
            router.layer(axum::middleware::from_fn_with_state(for_layer, layer))
        });
        Ok(())
    });
    ctx.add(Arc::new(GateService { shared, settings }));
    Ok(())
}
