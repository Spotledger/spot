//! The control plane service (`WP-CTL-01.T6`, ADR-098, `contracts/control-plane`).
//!
//! WS18 exists as libraries and PostgreSQL stores; this crate is the deployable and the seam they
//! are served through. It has **two mutual-TLS listeners with separate CAs and separate routers**:
//!
//! - the *operator* listener (`/cp/v1/...`): client certificates from the operator CA, an allow-list
//!   with tenant scopes, and for every mutation the signed command envelope (`X-Spot-Command`) of
//!   the management API, verified by `spot-mgmt-api`'s pieces and bound to the body by its digest;
//! - the *host* listener (`/cp/v1/host/...` and `/agent/v1/...`): client certificates issued by this
//!   service's own host CA, no operator rights.
//!
//! A certificate from one CA is refused on the other listener at the handshake. Modules (tenants,
//! licensing, usage, health, moves, rollouts, links, support) are files in `src/modules/` listed by
//! `build.rs`; each registers routes, tasks and readiness checks through a [`CpCtx`].
//!
//! # Data contract
//!
//! The control plane stores registry, placement, licence, usage counters, health, support grants
//! and links. It never reads tenant business data: no module depends on the tenant data crates
//! (a test walks `cargo metadata`).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod auth;
pub mod config;
pub mod modules;
pub mod registry;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use deadpool_postgres::Pool;
use serde_json::json;
use spot_core::Clock;
use spot_mgmt_api::ports::{AuditError, AuditRecord, AuditSink, BoxFuture};
use spot_signed_command::NonceStore;
use tokio::sync::watch;

use crate::auth::{
    AuthShared, FileKeyVault, HostAuth, HostCa, KeyVault, OperatorAuth, Revocations, SignedIntake,
};
use crate::config::{Config, decode_key};
use crate::registry::{ModuleRegistry, Registration};

/// One code of the control-plane error registry (`x-spot-error-codes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpCode {
    /// No verified certificate, or an untrusted, revoked or unenrolled one.
    Unauthenticated,
    /// The principal may not call this operation.
    Forbidden,
    /// No such object in the caller's scope.
    NotFound,
    /// The request is not valid.
    Invalid,
    /// The signed command is missing, wrong or does not bind the body.
    SignatureInvalid,
    /// The nonce, command id or idempotency key was used before.
    Replayed,
    /// Another workflow holds the tenant.
    Conflict,
    /// Approval records are needed.
    ApprovalRequired,
    /// A quota or licence limit would be exceeded.
    LimitExceeded,
    /// The deprovisioning cannot be confirmed yet.
    NotConfirmable,
    /// The licence request is not valid.
    LicenceInvalid,
    /// The telemetry batch is over its cardinality budget.
    CardinalityRejected,
    /// Too many requests.
    RateLimited,
    /// The body is too large.
    BodyTooLarge,
    /// An unexpected error.
    Internal,
}

impl CpCode {
    /// Every code.
    pub const ALL: [CpCode; 15] = [
        Self::Unauthenticated,
        Self::Forbidden,
        Self::NotFound,
        Self::Invalid,
        Self::SignatureInvalid,
        Self::Replayed,
        Self::Conflict,
        Self::ApprovalRequired,
        Self::LimitExceeded,
        Self::NotConfirmable,
        Self::LicenceInvalid,
        Self::CardinalityRejected,
        Self::RateLimited,
        Self::BodyTooLarge,
        Self::Internal,
    ];

    /// The dotted wire code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthenticated => "cp.unauthenticated",
            Self::Forbidden => "cp.forbidden",
            Self::NotFound => "cp.not_found",
            Self::Invalid => "cp.invalid",
            Self::SignatureInvalid => "cp.signature_invalid",
            Self::Replayed => "cp.replayed",
            Self::Conflict => "cp.conflict",
            Self::ApprovalRequired => "cp.approval_required",
            Self::LimitExceeded => "cp.limit_exceeded",
            Self::NotConfirmable => "cp.not_confirmable",
            Self::LicenceInvalid => "cp.licence_invalid",
            Self::CardinalityRejected => "cp.cardinality_rejected",
            Self::RateLimited => "cp.rate_limited",
            Self::BodyTooLarge => "cp.body_too_large",
            Self::Internal => "cp.internal",
        }
    }

    /// The HTTP status.
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::Unauthenticated | Self::SignatureInvalid => StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::ApprovalRequired | Self::LimitExceeded => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Invalid | Self::LicenceInvalid | Self::CardinalityRejected => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            Self::Replayed | Self::Conflict | Self::NotConfirmable => StatusCode::CONFLICT,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Whether the same request may succeed later unchanged.
    #[must_use]
    pub fn retryable(self) -> bool {
        matches!(self, Self::Conflict | Self::RateLimited)
    }

    fn message(self) -> &'static str {
        match self {
            Self::Unauthenticated => "a verified client certificate is required",
            Self::Forbidden => "the principal may not call this operation",
            Self::NotFound => "no such object",
            Self::Invalid => "the request is not valid",
            Self::SignatureInvalid => {
                "the signed command is missing, wrong or does not bind the body"
            }
            Self::Replayed => "the command or key was already used",
            Self::Conflict => "another workflow holds this tenant",
            Self::ApprovalRequired => "the operation needs approval records",
            Self::LimitExceeded => "a limit would be exceeded",
            Self::NotConfirmable => "the deprovisioning cannot be confirmed yet",
            Self::LicenceInvalid => "the licence request is not valid",
            Self::CardinalityRejected => "the batch is over the cardinality budget",
            Self::RateLimited => "too many requests",
            Self::BodyTooLarge => "the body is too large",
            Self::Internal => "an internal error occurred",
        }
    }
}

/// A refusal or failure: a code, an audit-only reason and optionally field paths (never values).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpError {
    /// The registry code.
    pub code: CpCode,
    /// A short audit-only reason (never sent in the body).
    pub reason: &'static str,
    /// Field paths for `cp.invalid` (`details.fields`).
    pub fields: Vec<String>,
}

impl CpError {
    /// An error whose audit reason is the code itself.
    #[must_use]
    pub fn new(code: CpCode) -> Self {
        Self::with_reason(code, code.as_str())
    }

    /// An error with a specific audit reason.
    #[must_use]
    pub fn with_reason(code: CpCode, reason: &'static str) -> Self {
        Self {
            code,
            reason,
            fields: Vec::new(),
        }
    }

    /// `cp.invalid` naming field paths.
    #[must_use]
    pub fn invalid(fields: &[&str]) -> Self {
        Self {
            code: CpCode::Invalid,
            reason: "cp.invalid",
            fields: fields.iter().map(|f| (*f).to_owned()).collect(),
        }
    }

    /// The HTTP response under `request_id`.
    #[must_use]
    pub fn into_response_with(self, request_id: &str) -> Response {
        let mut error = json!({
            "code": self.code.as_str(),
            "message": self.code.message(),
            "retryable": self.code.retryable(),
            "request_id": request_id,
        });
        if !self.fields.is_empty()
            && let Some(map) = error.as_object_mut()
        {
            map.insert("details".to_owned(), json!({ "fields": self.fields }));
        }
        let mut response = (self.code.status(), Json(json!({ "error": error }))).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        if self.code.retryable() {
            headers.insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
        }
        response
    }
}

/// A background task of a module: runs until the stop signal turns true.
pub type TaskFn = Box<dyn FnOnce(watch::Receiver<bool>) -> BoxFuture<'static, ()> + Send + Sync>;
/// A readiness check of a module.
pub type ReadyFn = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;

/// Idempotent DDL a module needs in the control schema, applied by `migrate`.
#[derive(Clone, Copy, Debug)]
pub struct Migration {
    /// The module that owns the statements.
    pub module: &'static str,
    /// `CREATE ... IF NOT EXISTS` statements, run in order under the migration lock.
    pub statements: &'static [&'static str],
}

/// What a module registers against.
pub struct CpCtx {
    /// The configuration.
    pub config: Arc<Config>,
    /// The control database pool (`None` only in tests that run without PostgreSQL).
    pub pool: Option<Pool>,
    /// The clock.
    pub clock: Arc<dyn Clock>,
    /// The audit sink.
    pub audit: Arc<dyn AuditSink>,
    pub(crate) current_module: &'static str,
    operator_routes: Router,
    host_routes: Router,
    tasks: Vec<(&'static str, TaskFn)>,
    readiness: Vec<(&'static str, ReadyFn)>,
    migrations: Vec<Migration>,
}

impl CpCtx {
    /// Adds operator routes (`/cp/v1/...`). They sit behind the operator middleware, so a handler
    /// reads `axum::Extension<auth::OperatorCall>`. Paths must not overlap another module's.
    pub fn operator_routes(&mut self, routes: Router) {
        let current = std::mem::take(&mut self.operator_routes);
        self.operator_routes = current.merge(routes);
    }

    /// Adds host routes (`/cp/v1/host/...`, `/agent/v1/...`), behind the host middleware: a
    /// handler reads `axum::Extension<auth::HostIdentity>`; the tenant is never taken from a body.
    pub fn host_routes(&mut self, routes: Router) {
        let current = std::mem::take(&mut self.host_routes);
        self.host_routes = current.merge(routes);
    }

    /// Adds a background task of the current module.
    pub fn add_task(&mut self, task: TaskFn) {
        self.tasks.push((self.current_module, task));
    }

    /// Adds the readiness check of the current module.
    pub fn add_readiness(&mut self, check: ReadyFn) {
        self.readiness.push((self.current_module, check));
    }

    /// Adds the current module's control-schema DDL.
    pub fn add_migration(&mut self, statements: &'static [&'static str]) {
        self.migrations.push(Migration {
            module: self.current_module,
            statements,
        });
    }
}

/// An audit sink that logs only the action and outcome (no principal data beyond the id).
#[derive(Clone, Copy, Debug, Default)]
pub struct TracingAuditSink;

impl AuditSink for TracingAuditSink {
    fn record(&self, record: AuditRecord) -> BoxFuture<'_, Result<String, AuditError>> {
        tracing::info!(action = %record.action, outcome = ?record.outcome, "control plane audit");
        Box::pin(std::future::ready(Ok(record.request_id)))
    }
}

/// The durable audit sink: one row per record in `cp_audit`.
pub struct PgAuditSink {
    pool: Pool,
}

/// The DDL of the core tables (audit).
pub const CORE_MIGRATION: Migration = Migration {
    module: "core",
    statements: &[
        "CREATE TABLE IF NOT EXISTS cp_audit (id BIGSERIAL PRIMARY KEY, at BIGINT NOT NULL, record TEXT NOT NULL)",
    ],
};

impl PgAuditSink {
    /// A sink over the control database.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

impl AuditSink for PgAuditSink {
    fn record(&self, record: AuditRecord) -> BoxFuture<'_, Result<String, AuditError>> {
        Box::pin(async move {
            let text = serde_json::to_string(&record).map_err(|_| AuditError)?;
            let client = self.pool.get().await.map_err(|_| AuditError)?;
            let row = client
                .query_one(
                    "INSERT INTO cp_audit (at, record) VALUES ($1, $2) RETURNING id",
                    &[&record.at, &text],
                )
                .await
                .map_err(|_| AuditError)?;
            let id: i64 = row.try_get(0).map_err(|_| AuditError)?;
            Ok(format!("cp-audit-{id}"))
        })
    }
}

/// Everything needed to assemble the service.
pub struct BuildDeps {
    /// The configuration.
    pub config: Arc<Config>,
    /// The control database pool (`None` in tests without PostgreSQL).
    pub pool: Option<Pool>,
    /// The clock.
    pub clock: Arc<dyn Clock>,
    /// The audit sink.
    pub audit: Arc<dyn AuditSink>,
    /// The durable, fail-closed nonce store.
    pub nonces: Arc<dyn NonceStore>,
    /// The CA key vault.
    pub vault: Arc<dyn KeyVault>,
    /// The module registrations (normally [`modules::REGISTRY`]).
    pub registrations: Vec<Registration>,
}

/// The assembled service.
pub struct ControlPlane {
    /// The operator listener's router (middleware included).
    pub operator_router: Router,
    /// The host and agent listener's router (middleware included).
    pub host_router: Router,
    /// The host CA.
    pub host_ca: Arc<HostCa>,
    /// The modules' background tasks.
    pub tasks: Vec<(&'static str, TaskFn)>,
    /// All control-schema DDL (core plus modules), in order.
    pub migrations: Vec<Migration>,
    /// The module names in registration order.
    pub module_names: Vec<&'static str>,
}

/// Why assembly or start failed (never carries a secret).
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// A step failed.
    #[error("{stage}: {detail}")]
    Failed {
        /// The step.
        stage: &'static str,
        /// What went wrong, without secrets.
        detail: String,
    },
}

fn fail(stage: &'static str, detail: impl Into<String>) -> StartError {
    StartError::Failed {
        stage,
        detail: detail.into(),
    }
}

async fn live() -> Response {
    (
        StatusCode::OK,
        Json(json!({ "data": { "status": "live" } })),
    )
        .into_response()
}

impl ControlPlane {
    /// Registers the modules and builds both routers.
    ///
    /// # Errors
    /// [`StartError`] for a bad registry, a failing module, the CA key or the administrator keys.
    pub fn build(deps: BuildDeps) -> Result<Self, StartError> {
        let registry = ModuleRegistry::new(&deps.registrations)
            .map_err(|error| fail("modules", error.to_string()))?;
        let revocations = Arc::new(Revocations::load(
            deps.config.host_ca.revocation_list.clone(),
        ));
        let host_ca = Arc::new(
            HostCa::new(
                &deps.config.host_ca.common_name,
                deps.vault.as_ref(),
                deps.config.host_ca.validity_days,
                Arc::clone(&revocations),
                Arc::clone(&deps.audit),
                Arc::clone(&deps.clock),
            )
            .map_err(|error| fail("host_ca", error.to_string()))?,
        );
        let mut ctx = CpCtx {
            config: Arc::clone(&deps.config),
            pool: deps.pool.clone(),
            clock: Arc::clone(&deps.clock),
            audit: Arc::clone(&deps.audit),
            current_module: "",
            operator_routes: Router::new(),
            host_routes: Router::new(),
            tasks: Vec::new(),
            readiness: Vec::new(),
            migrations: vec![CORE_MIGRATION],
        };
        registry.register_all(&mut ctx).map_err(|error| {
            fail(
                "modules",
                format!("{}: {}", error.code.as_str(), error.reason),
            )
        })?;

        let keys: Vec<[u8; 32]> = deps
            .config
            .administrator_keys
            .iter()
            .filter_map(|text| decode_key(text))
            .collect();
        let shared = AuthShared::new(Arc::clone(&deps.clock), Arc::clone(&deps.audit));
        let operator_auth = Arc::new(OperatorAuth {
            shared: Arc::clone(&shared),
            operators: deps.config.operators.clone(),
            intake: SignedIntake::new(&keys, deps.nonces, &deps.config.audience),
        });
        let host_auth = Arc::new(HostAuth {
            shared,
            revocations,
        });

        let readiness = Arc::new(std::mem::take(&mut ctx.readiness));
        let ready_route = {
            let readiness = Arc::clone(&readiness);
            get(move || {
                let readiness = Arc::clone(&readiness);
                async move {
                    let mut modules = serde_json::Map::new();
                    let mut all = true;
                    for (name, check) in readiness.iter() {
                        let ok = check().await;
                        all &= ok;
                        modules.insert((*name).to_owned(), json!(ok));
                    }
                    let status = if all {
                        StatusCode::OK
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    };
                    (
                        status,
                        Json(json!({ "data": { "ready": all, "modules": modules } })),
                    )
                }
            })
        };
        let guarded = ctx
            .operator_routes
            .route("/cp/v1/readiness", ready_route)
            .layer(axum::middleware::from_fn_with_state(
                operator_auth,
                auth::operator_layer,
            ));
        let operator_router = Router::new().route("/cp/v1/live", get(live)).merge(guarded);
        let host_router = ctx.host_routes.layer(axum::middleware::from_fn_with_state(
            host_auth,
            auth::host_layer,
        ));
        Ok(Self {
            operator_router,
            host_router,
            host_ca,
            tasks: ctx.tasks,
            migrations: ctx.migrations,
            module_names: registry.names(),
        })
    }
}

/// One line of `check` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckItem {
    /// What was checked.
    pub name: &'static str,
    /// Whether it passed.
    pub ok: bool,
    /// What failed (never a secret).
    pub detail: String,
}

fn item(name: &'static str, result: Result<(), String>) -> CheckItem {
    match result {
        Ok(()) => CheckItem {
            name,
            ok: true,
            detail: String::new(),
        },
        Err(detail) => CheckItem {
            name,
            ok: false,
            detail,
        },
    }
}

fn load_certs(
    path: &std::path::Path,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject as _;
    let iter = CertificateDer::pem_file_iter(path)
        .map_err(|_| "the PEM file cannot be read".to_owned())?;
    let certs: Result<Vec<_>, _> = iter.collect();
    let certs = certs.map_err(|_| "the PEM file holds an invalid certificate".to_owned())?;
    if certs.is_empty() {
        return Err("the PEM file holds no certificate".to_owned());
    }
    Ok(certs)
}

fn load_key(path: &std::path::Path) -> Result<rustls::pki_types::PrivateKeyDer<'static>, String> {
    use rustls::pki_types::PrivateKeyDer;
    use rustls::pki_types::pem::PemObject as _;
    PrivateKeyDer::from_pem_file(path).map_err(|_| "the key file cannot be read".to_owned())
}

fn listener_tls(listener: &config::ListenerConfig) -> Result<rustls::ServerConfig, String> {
    let chain = load_certs(&listener.server_cert)?;
    let key = load_key(&listener.server_key)?;
    let ca = load_certs(&listener.client_ca)?;
    spot_mgmt_api::tls_config(chain, key, &ca).map_err(|error| error.to_string())
}

fn pg_config(config: &Config) -> Result<spot_postgres::PgConfig, String> {
    let password = std::env::var(&config.database.password_env)
        .map_err(|_| "the database password variable is not set".to_owned())?;
    let db = &config.database;
    let mut pg = spot_postgres::PgConfig::new(&db.host, db.port, &db.dbname, &db.user, &password);
    pg.application_name = "spot-control-plane".to_owned();
    pg.pool_max_size = db.pool_max_size;
    pg.tls = if db.tls {
        spot_postgres::Tls::Require
    } else {
        spot_postgres::Tls::Disable
    };
    Ok(pg)
}

/// The clock is sane when it is after 1 January 2026.
const CLOCK_FLOOR: i64 = 1_767_225_600;

/// `check`: config is already parsed; reports certificates, the CA, the database and the clock,
/// each failure separately.
pub async fn check(config: &Config, clock: &dyn Clock) -> Vec<CheckItem> {
    let mut items = vec![
        item(
            "operator_listener_tls",
            listener_tls(&config.operator_listener).map(|_| ()),
        ),
        item(
            "host_listener_tls",
            listener_tls(&config.host_listener).map(|_| ()),
        ),
    ];
    let ca = FileKeyVault::new(config.host_ca.key.clone());
    let ca_result = HostCa::new(
        &config.host_ca.common_name,
        &ca,
        config.host_ca.validity_days,
        Arc::new(Revocations::load(None)),
        Arc::new(TracingAuditSink),
        Arc::new(spot_core::SystemClock::new()),
    )
    .map(|_| ())
    .map_err(|error| error.to_string());
    items.push(item("host_ca_key", ca_result));
    let database = async {
        let pg = pg_config(config)?;
        let backend = spot_postgres::PgBackend::connect(pg)
            .await
            .map_err(|_| "the database is unreachable or unsupported".to_owned())?;
        let client = backend
            .pool()
            .get()
            .await
            .map_err(|_| "no connection could be taken from the pool".to_owned())?;
        client
            .simple_query("SELECT 1")
            .await
            .map_err(|_| "the database did not answer".to_owned())?;
        Ok::<(), String>(())
    }
    .await;
    items.push(item("database", database));
    if config.wiring.deployment_key_env.is_some() {
        items.push(item(
            "deployment_key",
            wiring::deployment_signer(config, &|name| std::env::var(name).ok())
                .map(|_| ())
                .map_err(|error| error.to_string()),
        ));
    }
    let now = clock.now_utc().unix_timestamp();
    items.push(item(
        "clock",
        if now >= CLOCK_FLOOR {
            Ok(())
        } else {
            Err("the system clock is before 2026".to_owned())
        },
    ));
    items
}

/// The advisory lock key of `migrate`.
const MIGRATE_LOCK: i64 = 0x5350_4f54_4350_4d47;

/// Applies the DDL of the core and of every module, under one advisory lock.
///
/// # Errors
/// [`StartError`] naming the failing module.
pub async fn migrate(pool: &Pool, migrations: &[Migration]) -> Result<(), StartError> {
    let client = pool
        .get()
        .await
        .map_err(|_| fail("migrate", "no database connection"))?;
    client
        .execute("SELECT pg_advisory_lock($1)", &[&MIGRATE_LOCK])
        .await
        .map_err(|_| fail("migrate", "the migration lock could not be taken"))?;
    let mut result = Ok(());
    'all: for migration in migrations {
        for statement in migration.statements {
            if client.batch_execute(statement).await.is_err() {
                result = Err(fail(
                    "migrate",
                    format!("module {} failed", migration.module),
                ));
                break 'all;
            }
        }
    }
    let _ = client
        .execute("SELECT pg_advisory_unlock($1)", &[&MIGRATE_LOCK])
        .await;
    result
}

/// Runs both listeners and the module tasks until `shutdown` completes, with the providers this
/// build of the binary has (none: see [`run_with`]).
///
/// # Errors
/// [`StartError`] when a step of the start fails; a half-started service is never left running.
pub async fn run(
    config: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), StartError> {
    run_with(config, wiring::Providers::default(), shutdown).await
}

/// Runs both listeners and the module tasks until `shutdown` completes, with the providers the
/// host process supplies installed first ([`wiring::install`]). A module whose provider is not
/// supplied reports not ready; each is logged by name.
///
/// # Errors
/// [`StartError`] when a step of the start fails, among them an unusable deployment key; a
/// half-started service is never left running.
pub async fn run_with(
    config: Config,
    providers: wiring::Providers,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), StartError> {
    let config = Arc::new(config);
    let wired = wiring::install(&config, providers, &|name| std::env::var(name).ok())?;
    for (module, missing) in &wired.unwired {
        tracing::warn!(
            module = *module,
            missing = *missing,
            "the module is not wired: it reports not ready"
        );
    }
    let backend =
        spot_postgres::PgBackend::connect(pg_config(&config).map_err(|d| fail("database", d))?)
            .await
            .map_err(|_| fail("database", "the database is unreachable or unsupported"))?;
    let pool = backend.pool();
    let nonces = spot_signed_command_pg::PgNonceStore::with_skew(
        pool.clone(),
        spot_mgmt_api::MAX_CLOCK_SKEW_SECS,
    );
    nonces
        .prepare()
        .await
        .map_err(|_| fail("nonce_store", "the nonce table could not be prepared"))?;
    let clock: Arc<dyn Clock> = Arc::new(spot_core::SystemClock::new());
    let audit: Arc<dyn AuditSink> = Arc::new(PgAuditSink::new(pool.clone()));
    let service = ControlPlane::build(BuildDeps {
        config: Arc::clone(&config),
        pool: Some(pool.clone()),
        clock: Arc::clone(&clock),
        audit,
        nonces: Arc::new(nonces),
        vault: Arc::new(FileKeyVault::new(config.host_ca.key.clone())),
        registrations: wiring::registrations(),
    })?;
    migrate(&pool, &service.migrations).await?;

    let operator_tls =
        listener_tls(&config.operator_listener).map_err(|d| fail("operator_listener", d))?;
    let host_tls = listener_tls(&config.host_listener).map_err(|d| fail("host_listener", d))?;
    let operator_socket = tokio::net::TcpListener::bind(&config.operator_listener.listen)
        .await
        .map_err(|_| fail("operator_listener", "the address cannot be bound"))?;
    let host_socket = tokio::net::TcpListener::bind(&config.host_listener.listen)
        .await
        .map_err(|_| fail("host_listener", "the address cannot be bound"))?;

    let (stop_tx, stop_rx) = watch::channel(false);
    let stopper = |mut rx: watch::Receiver<bool>| async move {
        let _stopped = rx.wait_for(|stop| *stop).await.is_ok();
    };
    let limiter = Arc::new(spot_mgmt_api::tls::HandshakeLimiter::standard());
    let operator = tokio::spawn(spot_mgmt_api::tls::serve(
        operator_socket,
        Arc::new(operator_tls),
        service.operator_router,
        Arc::clone(&clock),
        Arc::clone(&limiter),
        stopper(stop_rx.clone()),
    ));
    let host = tokio::spawn(spot_mgmt_api::tls::serve(
        host_socket,
        Arc::new(host_tls),
        service.host_router,
        Arc::clone(&clock),
        limiter,
        stopper(stop_rx.clone()),
    ));
    let mut tasks = Vec::new();
    for (_name, task) in service.tasks {
        tasks.push(tokio::spawn(task(stop_rx.clone())));
    }
    shutdown.await;
    let _ = stop_tx.send(true);
    let _ = operator.await;
    let _ = host.await;
    for task in tasks {
        let _ = task.await;
    }
    Ok(())
}

/// A pinned future, for module authors building [`TaskFn`] values.
pub type PinFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The production wiring of the modules (`WP-CTL-01.T9`).
///
/// The modules are libraries: each one needs providers that only the host process can supply
/// (a provisioning executor over the application catalogue, a client for the tenant hosts' signed
/// management channel, the deployment signing key, the notification sink) and reports itself
/// **not ready** until they are installed. [`install`] installs them, once, before the service is
/// built; [`registrations`] is the module table the service is built from (the registry table of
/// `modules::REGISTRY` with the `health` registration replaced by one that shares its state with
/// `rollouts`).
///
/// # Rules
///
/// - A key is read from the environment variable the configuration names; a variable that is
///   named and missing, unreadable or not a key refuses the start (`cp.wiring.*`), and **no
///   default key exists**.
/// - The deployment key signs deployment steps and nothing else: it must differ from every
///   administrator key (it is never the licence key, which the `tenants` module reads from its
///   own variable).
/// - A provider that is not supplied is not faked: its module stays not ready and is listed in
///   [`Wired::unwired`], logged at start and visible in `/cp/v1/readiness`.
pub mod wiring {
    use std::collections::BTreeMap;
    use std::sync::{Arc, OnceLock};

    use spot_deploy_wizard::EnrolledAgents;
    use spot_fleet_health::{Health as FleetColour, RuleSet, Settings, TenantId as HealthTenant};
    use spot_fleet_rollout::Health as RolloutColour;
    use spot_mgmt_api::ports::BoxFuture;
    use spot_signed_command::{Signer, SoftwareSigner};

    use super::{CpCode, CpCtx, CpError, StartError, fail};
    use crate::config::{Config, decode_key};
    use crate::modules::deploy::{self, AgentLink};
    use crate::modules::health::{self, DEFAULT_MAX_SERIES, HealthState, NotifySink};
    use crate::modules::moves::{self, HostPortsFactory};
    use crate::modules::rollouts::{self, HealthGate, TenantMigrator};
    use crate::modules::tenants::{self, Kv, PgKv, RunnerFactory};
    use crate::registry::Registration;

    /// The environment variable that names the tenants and moves configuration file.
    pub const TENANTS_CONFIG_ENV: &str = "SPOT_CP_TENANTS_CONFIG";
    /// The environment variable that names the cloud-backup folder (the filesystem store).
    pub const BACKUP_DIR_ENV: &str = "SPOT_CP_BACKUP_DIR";

    /// What the host process supplies. A `None` leaves its module not ready; nothing is faked.
    #[derive(Default)]
    pub struct Providers {
        /// Builds the provisioning executor (`tenants`): it needs the application catalogue, which
        /// this crate must not depend on.
        pub runner_factory: Option<RunnerFactory>,
        /// Builds the ports to the tenant hosts for a move (`moves`).
        pub host_ports_factory: Option<HostPortsFactory>,
        /// Runs one site lifecycle step on a tenant host (`rollouts`).
        pub migrator: Option<Arc<dyn TenantMigrator>>,
        /// The enrolled agents and their identity keys (`deploy`).
        pub directory: Option<Arc<dyn EnrolledAgents + Send + Sync>>,
        /// The agent side of a deployment (`deploy`).
        pub agent_link: Option<Arc<dyn AgentLink>>,
        /// Delivers alert notifications (`health`; without one alerts are kept and shown).
        pub notifier: Option<Arc<dyn NotifySink>>,
    }

    /// What [`install`] did.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Wired {
        /// The modules whose providers were installed.
        pub wired: Vec<&'static str>,
        /// The modules left without a provider, each with what is missing. They report not ready.
        pub unwired: Vec<(&'static str, &'static str)>,
    }

    impl Wired {
        fn record(&mut self, module: &'static str, present: bool, missing: &'static str) {
            if present {
                if !self.wired.contains(&module) {
                    self.wired.push(module);
                }
            } else {
                self.unwired.push((module, missing));
            }
        }
    }

    static HEALTH_STATE: OnceLock<Arc<HealthState>> = OnceLock::new();
    static NOTIFIER: OnceLock<Arc<dyn NotifySink>> = OnceLock::new();

    fn health_colour(colour: FleetColour) -> RolloutColour {
        match colour {
            FleetColour::Green => RolloutColour::Green,
            FleetColour::Amber => RolloutColour::Amber,
            FleetColour::Red => RolloutColour::Red,
        }
    }

    /// The rollouts' health gate over the `health` module's state, bound when `health` registers
    /// (rollouts register first). A tenant the view does not know is amber; a state that is not
    /// bound or cannot be read is an error, which the rollout treats as red.
    struct StateGate;

    impl HealthGate for StateGate {
        fn colours<'a>(
            &'a self,
            tenants: &'a [String],
        ) -> BoxFuture<'a, Result<BTreeMap<String, RolloutColour>, CpError>> {
            Box::pin(async move {
                let state = HEALTH_STATE.get().ok_or_else(|| {
                    CpError::with_reason(CpCode::Internal, "wiring.health_unbound")
                })?;
                let view = state.step().await?;
                let mut out = BTreeMap::new();
                for tenant in tenants {
                    let colour = HealthTenant::try_new(tenant.clone())
                        .ok()
                        .and_then(|id| view.tenant(&id).map(|found| health_colour(found.health)))
                        .unwrap_or(RolloutColour::Amber);
                    out.insert(tenant.clone(), colour);
                }
                Ok(out)
            })
        }
    }

    /// The `health` registration of the service: the module's own, except that the state it
    /// builds is kept for the rollouts' gate and the notification sink comes from [`install`].
    fn register_health(ctx: &mut CpCtx) -> Result<(), CpError> {
        let state = if let Some(state) = HEALTH_STATE.get() {
            Arc::clone(state)
        } else {
            let pool = ctx.pool.clone().ok_or_else(|| {
                CpError::with_reason(CpCode::Internal, "health.no_control_database")
            })?;
            let rules = RuleSet::builtin()
                .map_err(|_| CpError::with_reason(CpCode::Internal, "health.rules"))?;
            let kv: Arc<dyn Kv> = Arc::new(PgKv::new(pool));
            let state = HealthState::new(
                kv,
                Arc::clone(&ctx.clock),
                rules,
                Settings::defaults(),
                DEFAULT_MAX_SERIES,
                NOTIFIER.get().cloned(),
            )?;
            let _ = HEALTH_STATE.set(Arc::clone(&state));
            state
        };
        health::register_with(ctx, state);
        Ok(())
    }

    /// The module table the service is built from: every module of `modules::REGISTRY`, with the
    /// `health` registration of this file.
    #[must_use]
    pub fn registrations() -> Vec<Registration> {
        crate::modules::REGISTRY
            .iter()
            .map(|registration| {
                if registration.name == "health" {
                    Registration {
                        register: register_health,
                        ..*registration
                    }
                } else {
                    *registration
                }
            })
            .collect()
    }

    /// The deployment signer named by `[wiring] deployment_key_env`, or `None` when the section
    /// does not name one.
    ///
    /// # Errors
    /// [`StartError`] `cp.wiring.deployment_key_missing` (named variable unset or empty),
    /// `cp.wiring.deployment_key_invalid` (not 64 hex characters) or
    /// `cp.wiring.deployment_key_not_separate` (equal to an administrator key). The text never
    /// holds the key.
    pub fn deployment_signer(
        config: &Config,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Arc<dyn Signer + Send + Sync>>, StartError> {
        let Some(name) = config.wiring.deployment_key_env.as_deref() else {
            return Ok(None);
        };
        let text = env(name)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| fail("wiring", "cp.wiring.deployment_key_missing"))?;
        let seed = decode_key(text.trim())
            .ok_or_else(|| fail("wiring", "cp.wiring.deployment_key_invalid"))?;
        let signer = SoftwareSigner::from_seed(&seed)
            .map_err(|_| fail("wiring", "cp.wiring.deployment_key_invalid"))?;
        let public = signer.public_key_bytes();
        if config
            .administrator_keys
            .iter()
            .any(|key| decode_key(key) == Some(public))
        {
            return Err(fail("wiring", "cp.wiring.deployment_key_not_separate"));
        }
        Ok(Some(Arc::new(signer)))
    }

    /// Installs every provider, once, before the service is built. `env` reads the environment
    /// (a parameter so a test needs no process-wide variable).
    ///
    /// Order: the deployment key first (a bad key refuses the start before anything is
    /// installed), then `tenants`, `moves`, the health gate and `rollouts`, the notification
    /// sink, `deploy`. `desired_state`, `licensing` and `cloud_backup` need no provider: they
    /// build themselves over the control database (the backup folder from `SPOT_CP_BACKUP_DIR`).
    ///
    /// # Errors
    /// [`StartError`] for a deployment key that is named and unusable (see
    /// [`deployment_signer`]).
    pub fn install(
        config: &Config,
        providers: Providers,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Wired, StartError> {
        let signer = deployment_signer(config, env)?;
        let mut wired = Wired::default();

        let tenants_config = env(TENANTS_CONFIG_ENV).is_some();
        if let Some(factory) = providers.runner_factory {
            let _ = tenants::set_runner_factory(factory);
        }
        wired.record(
            "tenants",
            providers.runner_factory.is_some() && tenants_config,
            if providers.runner_factory.is_none() {
                "a RunnerFactory over the application catalogue"
            } else {
                "the SPOT_CP_TENANTS_CONFIG file"
            },
        );

        if let Some(factory) = providers.host_ports_factory {
            let _ = moves::set_host_ports_factory(factory);
        }
        wired.record(
            "moves",
            providers.host_ports_factory.is_some() && tenants_config,
            if providers.host_ports_factory.is_none() {
                "a HostPortsFactory over the signed management channel"
            } else {
                "the SPOT_CP_TENANTS_CONFIG file"
            },
        );

        // The gate reads the health module's state when it registers; it is always installed.
        let _ = rollouts::set_health_gate(Arc::new(StateGate));
        if let Some(migrator) = providers.migrator.clone() {
            let _ = rollouts::set_migrator(migrator);
        }
        wired.record(
            "rollouts",
            providers.migrator.is_some(),
            "a TenantMigrator over the signed management channel",
        );

        if let Some(sink) = providers.notifier {
            let _ = NOTIFIER.set(sink);
        }
        wired.record("health", true, "");

        let deploy_ready = match (&signer, &providers.directory, &providers.agent_link) {
            (Some(signer), Some(directory), Some(link)) => {
                let _ = deploy::set_wiring(deploy::Wiring {
                    directory: Arc::clone(directory),
                    link: Arc::clone(link),
                    signer: Arc::clone(signer),
                });
                true
            }
            _ => false,
        };
        wired.record(
            "deploy",
            deploy_ready,
            if signer.is_none() {
                "[wiring] deployment_key_env"
            } else if providers.directory.is_none() {
                "an EnrolledAgents directory"
            } else {
                "an AgentLink to the agents"
            },
        );

        wired.record(
            "cloud_backup",
            env(BACKUP_DIR_ENV).is_some(),
            "the SPOT_CP_BACKUP_DIR folder",
        );
        wired.record("desired_state", true, "");
        wired.record("licensing", true, "");
        Ok(wired)
    }
}
