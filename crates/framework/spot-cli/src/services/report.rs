//! Reports of one site: `report_run`, `report_list` and `report_describe`
//! (`/api/v1/report*`, WP-PLT-14.T2), over the report engine `spot-reports`.
//!
//! `spot-reports` owns the rules (the definition, the compiler, the runner with the caller's
//! permission filter and the Report-channel field guard). `spot-http` owns the routes
//! ([`ReportCatalogPort`] for list and describe; `report_run` is the engine's
//! `ReportDelegate`). This module supplies the rest:
//!
//! - **what is served**: the report definitions the site's installed apps ship, one TOML file
//!   each in `<app>/reports/*.toml`. They are read when the site starts and compiled against the
//!   effective metadata the site serves *now*. A file that does not parse stops the site's start
//!   naming every problem; so does a definition that does not compile (a base DocType the site
//!   does not have, an unknown field, a duplicate name across apps);
//! - **who**: the site's sign-in service resolves the bearer credential; a session that has not
//!   chosen its password yet may do nothing here;
//! - **what a caller sees**: a report whose `roles` do not admit the caller is neither listed nor
//!   found (`404 report.not_found`, the same answer as a report that does not exist). The data of
//!   an admitted report is decided per run by the runner: the read filter is ANDed into the one
//!   storage query, a filter, sort, group or aggregate on a field the caller may not report on
//!   is refused, a column the caller may not report on is removed and the result says how many;
//! - **the compiled reports**: compiled once per served metadata snapshot. A metadata swap (a
//!   customisation) drops them with the snapshot and the next request compiles against the new
//!   one; a report that no longer compiles is left out of that snapshot and named in the log, so
//!   it is not found rather than run against metadata it was not written for;
//! - **describe**: the columns of `report_describe` are the columns a run for the caller would
//!   carry. They come from the runner itself, run over an executor that returns no rows, so the
//!   description and a run cannot disagree.
//!
//! # Level
//!
//! Required when the site's apps ship reports: a definition that does not compile fails the
//! start-up check, which stops the site with every problem named. With no report shipped the
//! service has nothing to fail. The settings are `services.report` (`enabled`, default true;
//! `max_concurrent`, default 4; `max_concurrent_per_user`, default 2).
//!
//! # Limits and honest failures
//!
//! A run takes a permit before it compiles or reads anything ([`ReportGate`]): at most
//! `max_concurrent` runs at once on the site and `max_concurrent_per_user` per signed-in user.
//! A run that finds either full is refused at once with `429 api.rate_limited` and
//! `Retry-After: 2`; nothing queues, so memory stays bounded. The permit is a guard, released on
//! every exit including a request cancelled mid-run. `describe` and `list` take no permit. A
//! report a service serves from its own data (a report source) is run through the same door.
//!
//! A storage failure during a run is classified by `spot-reports` (`report.timeout`,
//! `report.unavailable` seen as `server.unavailable` with `Retry-After`, `report.failed`): the
//! client sees fixed text only, and the stable storage code goes to the audit target with the
//! report name and the request id.
//!
//! # Metrics
//!
//! `spot_report_runs_total{result}` (`ok`, `timeout`, `unavailable`, `refused_busy`, `denied`,
//! `failed`), `spot_report_run_seconds` (histogram), `spot_report_rows_returned_total` and the
//! gauge `spot_report_in_flight`. No label carries a report name, a user or an argument.
//!
//! # Audit
//!
//! Target `spot::audit::report`: a line for every run (who, report, rows, whether it was cut) and
//! for every refusal (who, report, the reason code, the request id). Never an argument value, a
//! filter value or a row.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value as Json;
use spot_api_services::ReportDelegate;
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_field_guard::{
    AuditRecord as GuardAudit, FieldAccessRecord, FieldGuard, MetaProtection, Offer, TraceSink,
};
use spot_http::api::ports::report::{
    CatalogResult, ParameterOptionsOut, ReportCatalogPort, ReportDescriptionOut,
    ReportParameterOut, ReportSummaryOut,
};
use spot_http::api::registry::Code;
use spot_http::api::service::{
    ApiError, BoxFuture, Message, ReportColumn, ReportResult, RequestCtx,
};
use spot_permissions::{PermissionEngine, Principal};
use spot_postgres::PgBackend;
use spot_reports::{
    Compiler, EffectiveMetaSet, ParamOptions, ParamType, QueryExecutor, RegisteredReport,
    ReportArgs, ReportDef, ReportError, ReportRegistry, ReportRunner, Telemetry, codes,
};
use spot_storage_contract::{
    Catalog, Query, ReadSource, RecordId, Rows, StorageBackend, StorageError, Value,
};
use tokio::sync::watch;

use crate::audit::PathClass;
use crate::live::{Served, SiteLive};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the framework's own services (they use 100 to 900) and the bulk export.
pub const ORDER: u32 = 1020;

/// The audit target of this service.
const AUDIT_TARGET: &str = "spot::audit::report";

/// The directory of an app that holds its report definitions.
pub const REPORTS_DIR: &str = "reports";

/// The default for [`Settings::max_concurrent`].
pub const DEFAULT_MAX_CONCURRENT: usize = 4;

/// The default for [`Settings::max_concurrent_per_user`].
pub const DEFAULT_MAX_CONCURRENT_PER_USER: usize = 2;

/// The wait, in seconds, a refused run is told to leave before it tries again.
pub const BUSY_RETRY_AFTER_SECS: u64 = 2;

/// `[services.report]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site serves reports at all. Off, `report_run` answers `report.not_found` and
    /// the catalogue routes are not mounted.
    pub enabled: bool,
    /// The most reports that run at once on this site (at least 1). A run that finds the site at
    /// this number is refused with `429 api.rate_limited`; it is never queued.
    pub max_concurrent: usize,
    /// The most reports one signed-in user runs at once (at least 1).
    pub max_concurrent_per_user: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            max_concurrent_per_user: DEFAULT_MAX_CONCURRENT_PER_USER,
        }
    }
}

impl Settings {
    /// Refuses a limit of zero, naming the key: a site that can run no report is a mistake, and
    /// `enabled = false` is how to switch reports off.
    ///
    /// # Errors
    ///
    /// A limit is zero.
    pub fn validate(&self) -> Result<()> {
        for (key, value) in [
            ("max_concurrent", self.max_concurrent),
            ("max_concurrent_per_user", self.max_concurrent_per_user),
        ] {
            if value == 0 {
                return Err(Error(format!(
                    "services.report.{key} must be at least 1 (set services.report.enabled = false to serve no reports)"
                )));
            }
        }
        Ok(())
    }
}

// ---- admission and metrics ----

/// The metric names. Bounded labels only: a result class. A report name is app-chosen text, a
/// user is a person: neither is ever a label.
mod metric {
    /// Counter: report runs, by `result`.
    pub const RUNS_TOTAL: &str = "spot_report_runs_total";
    /// Histogram: how long a report run took.
    pub const RUN_SECONDS: &str = "spot_report_run_seconds";
    /// Counter: rows the reports returned.
    pub const ROWS_TOTAL: &str = "spot_report_rows_returned_total";
    /// Gauge: report runs in flight now.
    pub const IN_FLIGHT: &str = "spot_report_in_flight";
}

/// How a run ended, as the `result` label of `spot_report_runs_total`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunResult {
    /// The report ran.
    Ok,
    /// The storage layer stopped it at the statement timeout.
    Timeout,
    /// The database could not run it now (`server.unavailable`).
    Unavailable,
    /// The site's or the user's concurrency cap was full.
    RefusedBusy,
    /// The caller may not run it, or may not see it (`not_found`, a denied field).
    Denied,
    /// Any other failure, including a bad argument.
    Failed,
}

impl RunResult {
    /// The label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RunResult::Ok => "ok",
            RunResult::Timeout => "timeout",
            RunResult::Unavailable => "unavailable",
            RunResult::RefusedBusy => "refused_busy",
            RunResult::Denied => "denied",
            RunResult::Failed => "failed",
        }
    }

    /// The class of an error a run returned.
    #[must_use]
    pub fn of_error(error: &ApiError) -> Self {
        match error.code.as_str() {
            codes::TIMEOUT => RunResult::Timeout,
            "server.unavailable" => RunResult::Unavailable,
            "api.rate_limited" => RunResult::RefusedBusy,
            "permission.denied" | "permission.field_denied" | codes::NOT_FOUND => RunResult::Denied,
            _ => RunResult::Failed,
        }
    }

    fn count(self) {
        metrics::counter!(metric::RUNS_TOTAL, "result" => self.as_str()).increment(1);
    }
}

#[derive(Debug, Default)]
struct GateState {
    total: usize,
    per_user: HashMap<String, usize>,
}

/// The admission of one site's report runs: a site-wide cap and a per-user cap, both refusing at
/// once. Built per site from [`Settings`].
#[derive(Debug)]
pub struct ReportGate {
    max: usize,
    max_per_user: usize,
    state: Mutex<GateState>,
}

/// A run's place under the caps. Dropping it, on any exit including a cancelled request, frees
/// the place.
#[derive(Debug)]
pub struct Permit<'a> {
    gate: &'a ReportGate,
    user: String,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut state = lock(&self.gate.state);
        state.total = state.total.saturating_sub(1);
        if let Some(n) = state.per_user.get_mut(&self.user) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                state.per_user.remove(&self.user);
            }
        }
        metrics::gauge!(metric::IN_FLIGHT).set(state.total as f64);
    }
}

/// The answer to a run the caps refuse: `429 api.rate_limited` with the wait.
fn busy() -> ApiError {
    ApiError::from_code(Code::ApiRateLimited)
        .detail("retry_after", Json::from(BUSY_RETRY_AFTER_SECS))
}

impl ReportGate {
    /// A gate with the given caps (each at least 1; [`Settings::validate`] refuses zero).
    #[must_use]
    pub fn new(max: usize, max_per_user: usize) -> Self {
        Self {
            max: max.max(1),
            max_per_user: max_per_user.max(1),
            state: Mutex::new(GateState::default()),
        }
    }

    /// A place for `user`, or the refusal. Never waits.
    ///
    /// # Errors
    ///
    /// `429 api.rate_limited` (`Retry-After` 2) when the site or the user is at its cap.
    pub fn try_acquire(&self, user: &str) -> std::result::Result<Permit<'_>, ApiError> {
        let mut state = lock(&self.state);
        let mine = state.per_user.get(user).copied().unwrap_or(0);
        if state.total >= self.max || mine >= self.max_per_user {
            return Err(busy());
        }
        state.total += 1;
        *state.per_user.entry(user.to_owned()).or_insert(0) += 1;
        metrics::gauge!(metric::IN_FLIGHT).set(state.total as f64);
        Ok(Permit {
            gate: self,
            user: user.to_owned(),
        })
    }

    /// The runs in flight now.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        lock(&self.state).total
    }

    /// The readiness detail: `in flight n/max`.
    #[must_use]
    pub fn status(&self) -> String {
        format!("in flight {}/{}", self.in_flight(), self.max)
    }

    /// Runs `body` under a permit for `user` and records the run: the result class, the seconds
    /// and the rows (`rows` reads them off a success). The permit is taken before `body` is
    /// polled for the first time, so a refused call has compiled and read nothing, and it is
    /// held by this future: dropping the future (a cancelled request) frees it.
    ///
    /// # Errors
    ///
    /// The busy refusal, or `body`'s own error.
    pub async fn run<T, F>(
        &self,
        user: &str,
        rows: fn(&T) -> usize,
        body: F,
    ) -> std::result::Result<T, ApiError>
    where
        F: Future<Output = std::result::Result<T, ApiError>>,
    {
        let permit = match self.try_acquire(user) {
            Ok(permit) => permit,
            Err(refusal) => {
                RunResult::RefusedBusy.count();
                return Err(refusal);
            }
        };
        let started = Instant::now();
        let outcome = body.await;
        metrics::histogram!(metric::RUN_SECONDS).record(started.elapsed().as_secs_f64());
        match &outcome {
            Ok(value) => {
                RunResult::Ok.count();
                metrics::counter!(metric::ROWS_TOTAL).increment(rows(value) as u64);
            }
            Err(error) => RunResult::of_error(error).count(),
        }
        drop(permit);
        outcome
    }
}

/// The client's error for a runner refusal. A refusal that hides a storage failure also writes
/// the stable storage code, with the report and the request id, to the audit target: that line
/// is how an operator finds what the client was not told.
fn run_error(error: &ReportError, report: &str, request_id: &str) -> ApiError {
    if let Some(cause) = error.cause() {
        tracing::warn!(
            target: AUDIT_TARGET,
            report,
            request_id,
            code = error.code(),
            storage_code = cause,
            "a report run failed in storage"
        );
    }
    error.to_api_error()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable).detail("retry_after", Json::from(5))
}

fn denied() -> ApiError {
    ApiError::from_code(Code::PermissionDenied)
}

/// The one answer for a report that does not exist and one the caller's roles do not admit.
fn not_found(report: &str) -> ApiError {
    ReportError::new(
        codes::NOT_FOUND,
        format!("No report named `{report}` is available."),
    )
    .to_api_error()
}

// ---- the definitions the apps ship ----

/// One report definition and the app that ships it.
type Shipped = (String, Arc<ReportDef>);

/// Reads `<dir>/reports/*.toml` of one app, in file-name order. Every file that does not parse is
/// reported; a directory that is absent ships nothing.
fn read_app(app: &str, dir: &Path, out: &mut Vec<Shipped>, problems: &mut Vec<String>) {
    let reports = dir.join(REPORTS_DIR);
    let Ok(entries) = std::fs::read_dir(&reports) else {
        return;
    };
    let mut files: Vec<_> = entries.filter_map(std::result::Result::ok).collect();
    files.sort_by_key(std::fs::DirEntry::file_name);
    for entry in files {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let label = format!(
            "{app}/{REPORTS_DIR}/{}",
            entry.file_name().to_string_lossy()
        );
        match ReportDef::load_file(&path) {
            Ok(def) => out.push((app.to_owned(), Arc::new(def))),
            Err(e) => problems.push(format!("{label}: {e}")),
        }
    }
}

/// The definitions of every installed app, or every problem found reading them.
fn read_definitions(
    apps: impl IntoIterator<Item = (String, std::path::PathBuf)>,
) -> std::result::Result<Vec<Shipped>, Vec<String>> {
    let mut shipped = Vec::new();
    let mut problems = Vec::new();
    for (app, dir) in apps {
        read_app(&app, &dir, &mut shipped, &mut problems);
    }
    if problems.is_empty() {
        Ok(shipped)
    } else {
        Err(problems)
    }
}

// ---- compiling against a snapshot ----

/// The reports of one metadata snapshot and the problems compiling them found.
struct Compiled {
    registry: ReportRegistry,
    problems: Vec<String>,
}

/// Compiles every shipped definition against `meta` and `catalog`. A definition that does not
/// compile is left out and named; the others still serve.
fn compile_all(shipped: &[Shipped], meta: &EffectiveMetaSet, catalog: &Catalog) -> Compiled {
    let compiler = Compiler::new(meta, catalog);
    let mut registry = ReportRegistry::new();
    let mut problems = Vec::new();
    for (app, def) in shipped {
        if let Err(errors) = registry.register(app, Arc::clone(def), &compiler) {
            for error in errors {
                let at = error
                    .path()
                    .map(|p| format!(" at `{p}`"))
                    .unwrap_or_default();
                problems.push(format!("report `{}` of `{app}`{at}: {error}", def.name));
            }
        }
    }
    Compiled { registry, problems }
}

// ---- report sources: a service's own data, served by the one report engine ----

/// Who is asking, as a source's visibility hook sees it. Owned, so the hook runs on a task of its
/// own: one that panics or hangs is a denial, never a crash or a stall of the request.
#[derive(Clone, Debug)]
pub struct AdmitRequest {
    /// The site the request is for.
    pub site: String,
    /// The signed-in user's id.
    pub user: String,
    /// The caller as the permission model sees them.
    pub principal: Principal,
}

/// What a source's hook lets the caller see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Every row of the source.
    All,
    /// Only the rows whose scope key is in this set. The source's executor reads the set and
    /// applies it to its own data; the report engine never interprets a key.
    Rows(BTreeSet<String>),
}

/// A hook's refusal: a stable reason code that is audited and never shown to the caller (the
/// caller is answered `404 report.not_found`, exactly as for a report that does not exist).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Denied(pub &'static str);

/// The most a hook may take to answer. A slower hook is a denial.
pub const ADMIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Decides, per request, whether the caller may see a source's data and which rows.
///
/// Fail closed: an `Err`, a panic and a timeout all deny. The hook is called for a run, a
/// description and, per source, for the list (a source the caller is denied is not listed).
pub trait SourceVisibility: Send + Sync {
    /// The caller's admission, or the reason code of the refusal.
    fn admit<'a>(
        &'a self,
        request: &'a AdmitRequest,
    ) -> spot_reports::BoxFuture<'a, std::result::Result<Admission, Denied>>;
}

/// Runs a source report's one storage query over the source's own data.
///
/// The query is the report engine's storage query IR, built from the registered definition; the
/// executor answers it from its own data and applies `admission` to the rows. No request value
/// ever names an executor: only a registered report name resolves to one.
pub trait SourceExecutor: Send + Sync {
    /// The rows of `query` that `admission` allows.
    fn execute<'a>(
        &'a self,
        admission: &'a Admission,
        query: &'a Query,
    ) -> spot_reports::BoxFuture<'a, std::result::Result<Rows, StorageError>>;
}

/// One report a service serves from its own data.
pub struct ReportSource {
    /// The registering service (a storage-style name); it is the report's `app` in listings.
    pub service: String,
    /// The definition, already validated by the caller.
    pub def: Arc<ReportDef>,
    /// The source schema the definition compiles against: the DocType-shaped definitions of the
    /// base name (and of any joined name). It lives only in memory: it is never persisted,
    /// never added to the metadata the site serves and never visible to a `meta` route.
    pub schema: Vec<spot_meta::Definition>,
    /// Answers the query.
    pub executor: Arc<dyn SourceExecutor>,
    /// Decides who sees what.
    pub visibility: Arc<dyn SourceVisibility>,
    /// Whether a source that does not compile stops the site's start. Otherwise it is named in
    /// the log and left out.
    pub required: bool,
}

/// The registration port of the report service, provided with
/// `ctx.provide::<dyn ReportSources>(..)` (start order [`ORDER`], 1020). A service whose own
/// `ORDER` is larger fetches it with `ctx.port::<dyn ReportSources>()` from its `register`
/// (build phase) and calls [`ReportSources::register`]. The port is absent on a site with
/// `services.report.enabled = false` and on a reload build (the running service keeps what it
/// registered): a module skips quietly when it is `None`.
pub trait ReportSources: Send + Sync {
    /// Adds `source`.
    ///
    /// # Errors
    ///
    /// The service or report name is malformed, or the report name is already registered by
    /// another source (both services are named).
    fn register(&self, source: ReportSource) -> Result<()>;

    /// The names of the registered reports, in registration order.
    fn names(&self) -> Vec<String>;
}

/// The sources of one site. Built per site; nothing is shared between sites. It closes when the
/// site's [`SourceBook`] is compiled (at the start-up check): a later registration is refused, so
/// what is served never changes behind a running site.
#[derive(Default)]
pub struct SourceRegistry {
    sources: Mutex<Vec<Arc<ReportSource>>>,
    frozen: AtomicBool,
}

impl SourceRegistry {
    /// An empty, open registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ReportSources for SourceRegistry {
    fn register(&self, source: ReportSource) -> Result<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(Error(format!(
                "the report source `{}` of `{}` was registered after the site's start-up check; register from `register`",
                source.def.name, source.service
            )));
        }
        if spot_app_api::prelude::Name::new(&source.service).is_err() {
            return Err(Error(
                "a report source's service name must be a storage-style name".to_owned(),
            ));
        }
        if spot_app_api::prelude::Name::new(&source.def.name).is_err() {
            return Err(Error(format!(
                "the report source `{}` of `{}` has a name that is not a storage-style name",
                source.def.name, source.service
            )));
        }
        let mut sources = lock(&self.sources);
        if let Some(existing) = sources.iter().find(|s| s.def.name == source.def.name) {
            return Err(Error(format!(
                "the report `{}` is already served by `{}`, so `{}` cannot register it",
                source.def.name, existing.service, source.service
            )));
        }
        sources.push(Arc::new(source));
        Ok(())
    }

    fn names(&self) -> Vec<String> {
        lock(&self.sources)
            .iter()
            .map(|s| s.def.name.clone())
            .collect()
    }
}

/// A source compiled against its own in-memory schema, with the permission engine that grants
/// the report's roles read on that schema (and nobody else).
struct CompiledSource {
    source: Arc<ReportSource>,
    registry: ReportRegistry,
    engine: Arc<PermissionEngine>,
    protection: Arc<dyn spot_field_guard::ProtectionSource>,
}

fn compile_source(source: &Arc<ReportSource>) -> std::result::Result<CompiledSource, Vec<String>> {
    let label = |why: &dyn std::fmt::Display| {
        format!(
            "report source `{}` of `{}`: {why}",
            source.def.name, source.service
        )
    };
    let registry_meta = spot_meta::Registry::new(source.schema.clone())
        .map_err(|errors| errors.iter().map(|e| label(e)).collect::<Vec<_>>())?;
    let meta = registry_meta.effective();
    let catalog = crate::schema::catalog(&meta).map_err(|e| vec![label(&e)])?;
    let set = EffectiveMetaSet::new(Arc::clone(&meta));
    let compiler = Compiler::new(&set, &catalog);
    let mut registry = ReportRegistry::new();
    registry
        .register(&source.service, Arc::clone(&source.def), &compiler)
        .map_err(|errors| errors.iter().map(|e| label(e)).collect::<Vec<_>>())?;
    // The source's schema is no DocType of the site, so the site's permission engine knows
    // nothing of it. The source gets an engine of its own: the roles the report names may read
    // its schema and no role may do anything else. Which rows is the visibility hook's answer.
    let mut grants = Vec::new();
    for view in meta.doctypes() {
        for role in &source.def.roles {
            grants.push(spot_permissions::RoleGrant {
                doctype: view.name.as_str().to_owned(),
                role: spot_permissions::Role(role.clone()),
                // A report column is a list-like channel: it needs the `report` right beside `read`.
                rights: spot_permissions::Rights::new([
                    spot_permissions::Right::Read,
                    spot_permissions::Right::Report,
                ]),
                if_owner: false,
            });
        }
    }
    grants.sort();
    let model = spot_permissions::Model {
        grants,
        ..spot_permissions::Model::default()
    };
    let snapshot = spot_permissions::Snapshot::build(&meta, &model)
        .map_err(|e| vec![label(&format!("{e:?}"))])?;
    let engine = Arc::new(PermissionEngine::new(Arc::new(snapshot)));
    let protection =
        Arc::new(MetaProtection::new(&meta)) as Arc<dyn spot_field_guard::ProtectionSource>;
    Ok(CompiledSource {
        source: Arc::clone(source),
        registry,
        engine,
        protection,
    })
}

/// Runs a source's query through its executor with the caller's admission applied.
struct Scoped {
    executor: Arc<dyn SourceExecutor>,
    admission: Admission,
}

impl QueryExecutor for Scoped {
    fn execute<'a>(
        &'a self,
        query: &'a Query,
    ) -> spot_reports::BoxFuture<'a, std::result::Result<Rows, StorageError>> {
        self.executor.execute(&self.admission, query)
    }
}

/// Calls a source's visibility hook on a task of its own, so a panic is a join error and a hook
/// that hangs is abandoned at [`ADMIT_TIMEOUT`]. Anything but an admission is a denial.
async fn admit_hook(
    visibility: &Arc<dyn SourceVisibility>,
    request: &AdmitRequest,
) -> std::result::Result<Admission, Denied> {
    let (visibility, request) = (Arc::clone(visibility), request.clone());
    let mut task = tokio::spawn(async move { visibility.admit(&request).await });
    match tokio::time::timeout(ADMIT_TIMEOUT, &mut task).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => Err(Denied("source.admit_panicked")),
        Err(_) => {
            task.abort();
            Err(Denied("source.admit_timeout"))
        }
    }
}

/// Every source of one site, compiled once (a source schema does not depend on the served
/// metadata) and served by the one report engine. Built per site from that site's registry:
/// nothing is shared between sites.
pub struct SourceBook {
    site_clock: Arc<dyn spot_core::Clock>,
    ready: Vec<Arc<CompiledSource>>,
    /// Problems that stop the start-up check (a required source that does not compile).
    fatal: Vec<String>,
}

impl SourceBook {
    /// Closes `registry` to further registration and compiles what it holds. A source that does
    /// not compile is named in the log and left out; it stops the site only when it declared
    /// itself `required`.
    pub fn compile(registry: &SourceRegistry, clock: Arc<dyn spot_core::Clock>) -> Self {
        registry.frozen.store(true, Ordering::Release);
        let sources: Vec<Arc<ReportSource>> = lock(&registry.sources).clone();
        let mut ready = Vec::new();
        let mut fatal = Vec::new();
        for source in &sources {
            match compile_source(source) {
                Ok(compiled) => ready.push(Arc::new(compiled)),
                Err(problems) => {
                    tracing::warn!(
                        service = %source.service,
                        problems = %problems.join("; "),
                        "a report source was left out: it does not compile"
                    );
                    if source.required {
                        fatal.extend(problems);
                    }
                }
            }
        }
        Self {
            site_clock: clock,
            ready,
            fatal,
        }
    }

    /// The problems that stop the start-up check.
    #[must_use]
    pub fn fatal(&self) -> &[String] {
        &self.fatal
    }

    /// The reports served, as `(report, registering service)`, in registration order.
    #[must_use]
    pub fn served(&self) -> Vec<(String, String)> {
        self.ready
            .iter()
            .map(|c| (c.source.def.name.clone(), c.source.service.clone()))
            .collect()
    }

    fn named(&self, report: &str) -> Option<&Arc<CompiledSource>> {
        self.ready.iter().find(|c| c.source.def.name == report)
    }

    fn runner(
        &self,
        compiled: &CompiledSource,
        executor: Arc<dyn QueryExecutor>,
        telemetry: Arc<dyn Telemetry>,
    ) -> ReportRunner {
        let guard = FieldGuard::new(
            Arc::clone(&compiled.engine),
            Arc::clone(&compiled.protection),
            Arc::new(GuardSinks),
            Arc::new(GuardSinks),
            Arc::clone(&self.site_clock),
        );
        ReportRunner::new(Arc::clone(&compiled.engine), guard, executor, telemetry)
    }

    /// The caller's admission to `compiled`, or the one answer for a report that does not exist
    /// (the reason code is audited and never shown).
    async fn admitted(
        &self,
        compiled: &CompiledSource,
        who: &AdmitRequest,
        action: &str,
    ) -> std::result::Result<Admission, ApiError> {
        match admit_hook(&compiled.source.visibility, who).await {
            Ok(admission) => Ok(admission),
            Err(Denied(reason)) => {
                tracing::info!(
                    target: AUDIT_TARGET,
                    site = %who.site,
                    action,
                    who = %who.user,
                    report = %compiled.source.def.name,
                    reason,
                    "report source refused"
                );
                Err(not_found(&compiled.source.def.name))
            }
        }
    }

    /// The compiled report `report` when its roles admit the caller.
    fn visible(
        &self,
        who: &AdmitRequest,
        report: &str,
    ) -> Option<(&Arc<CompiledSource>, &RegisteredReport)> {
        let compiled = self.named(report)?;
        let registered = compiled.registry.lookup_for(report, &who.principal)?;
        Some((compiled, registered))
    }

    /// Runs `report` for the caller.
    ///
    /// # Errors
    ///
    /// `404 report.not_found` for a name no source registered, a report the caller's roles do
    /// not admit and a caller the visibility hook denied (an error, a panic and a timeout
    /// included); otherwise the runner's own refusals.
    pub async fn run(
        &self,
        who: &AdmitRequest,
        report: &str,
        filters: &Json,
        limit: Option<u64>,
    ) -> CatalogResult<(ReportResult, Vec<Message>)> {
        self.run_with_request(who, report, filters, limit, "").await
    }

    /// As [`SourceBook::run`], naming the request for the log line a storage failure writes.
    ///
    /// # Errors
    ///
    /// As [`SourceBook::run`].
    pub async fn run_with_request(
        &self,
        who: &AdmitRequest,
        report: &str,
        filters: &Json,
        limit: Option<u64>,
        request_id: &str,
    ) -> CatalogResult<(ReportResult, Vec<Message>)> {
        let (compiled, registered) = self.visible(who, report).ok_or_else(|| not_found(report))?;
        let admission = self.admitted(compiled, who, "run").await?;
        let args = ReportArgs::from_json(filters).map_err(|e| e.to_api_error())?;
        let limit = limit.map(|l| l.min(u64::from(registered.compiled.max_rows)));
        let runner = self.runner(
            compiled,
            Arc::new(Scoped {
                executor: Arc::clone(&compiled.source.executor),
                admission,
            }),
            Arc::new(Timing),
        );
        let out = runner
            .run(&registered.compiled, &who.principal, &args, limit)
            .await
            .map_err(|e| run_error(&e, report, request_id))?;
        Ok((out.result, out.messages))
    }

    /// The source reports the caller may see, unordered. A source the hook denies is absent.
    pub async fn list(&self, who: &AdmitRequest) -> Vec<ReportSummaryOut> {
        let mut out = Vec::new();
        for compiled in &self.ready {
            let Some(registered) = compiled
                .registry
                .lookup_for(&compiled.source.def.name, &who.principal)
            else {
                continue;
            };
            if self.admitted(compiled, who, "list").await.is_err() {
                continue;
            }
            out.push(ReportSummaryOut {
                name: registered.compiled.name.clone(),
                label: registered.compiled.label.clone(),
                app: registered.app.clone(),
                base_doctype: registered.compiled.base.as_str().to_owned(),
            });
        }
        out
    }

    /// Describes `report` for the caller.
    ///
    /// # Errors
    ///
    /// As [`SourceBook::run`].
    pub async fn describe(
        &self,
        who: &AdmitRequest,
        report: &str,
    ) -> CatalogResult<ReportDescriptionOut> {
        let (compiled, registered) = self.visible(who, report).ok_or_else(|| not_found(report))?;
        // The description needs no rows, but the same hook decides whether the report exists
        // for this caller.
        self.admitted(compiled, who, "describe").await?;
        let describer = self.runner(
            compiled,
            Arc::new(NoRows),
            Arc::new(spot_reports::NoTelemetry),
        );
        describe_registered(&describer, registered, &who.principal).await
    }
}

// ---- running ----

/// Runs a report's query on the site's database.
struct Live {
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl QueryExecutor for Live {
    fn execute<'a>(
        &'a self,
        query: &'a Query,
    ) -> spot_reports::BoxFuture<'a, std::result::Result<Rows, StorageError>> {
        Box::pin(async move {
            self.backend
                .read(Arc::clone(&self.catalog), ReadSource::Primary, query)
                .await
        })
    }
}

/// An executor that finds no row: the runner then reduces the columns for the caller exactly as
/// it would for a run, which is what a description of the report states.
struct NoRows;

impl QueryExecutor for NoRows {
    fn execute<'a>(
        &'a self,
        _query: &'a Query,
    ) -> spot_reports::BoxFuture<'a, std::result::Result<Rows, StorageError>> {
        Box::pin(async move {
            Ok(Rows {
                columns: Vec::new(),
                rows: Vec::new(),
            })
        })
    }
}

/// The runner's timing line: the report, the rows and the milliseconds. Never an argument.
struct Timing;

impl Telemetry for Timing {
    fn report_run(&self, report: &str, rows: usize, elapsed_ms: u64) {
        tracing::debug!(report, rows, elapsed_ms, "report ran");
    }
}

/// The guard's sinks: a record names who, what and through which door, never a value.
struct GuardSinks;

impl TraceSink for GuardSinks {
    fn offer(&self, record: FieldAccessRecord) -> Offer {
        tracing::info!(
            target: "spot::audit::field_access",
            who = %record.who,
            doctype = %record.doctype,
            field = %record.field,
            via = record.via.as_str(),
            authorised = record.authorised,
            "field access"
        );
        Offer::Accepted
    }
}

impl spot_field_guard::AuditSink for GuardSinks {
    fn offer(&self, _record: GuardAudit) -> Offer {
        tracing::info!(target: "spot::audit::field_guard", "field guard record");
        Offer::Accepted
    }
}

/// Everything derived from one served snapshot: rebuilt when the snapshot is swapped.
struct Prepared {
    served: Arc<Served>,
    registry: ReportRegistry,
    problems: Vec<String>,
    /// Runs reports.
    runner: ReportRunner,
    /// Runs reports over no rows, to state a description.
    describer: ReportRunner,
}

struct Caller {
    user: String,
    principal: Principal,
}

struct Shared {
    site: String,
    live: Arc<SiteLive>,
    backend: PgBackend,
    clock: Arc<dyn spot_core::Clock>,
    shipped: Vec<Shipped>,
    sources: Arc<SourceRegistry>,
    book: OnceLock<SourceBook>,
    prepared: Mutex<Option<Arc<Prepared>>>,
    ready: AtomicBool,
    /// The caps on concurrent runs.
    gate: ReportGate,
}

impl Shared {
    fn guard(&self, served: &Served) -> FieldGuard {
        FieldGuard::new(
            Arc::clone(&served.decision),
            Arc::new(MetaProtection::new(&served.meta))
                as Arc<dyn spot_field_guard::ProtectionSource>,
            Arc::new(GuardSinks),
            Arc::new(GuardSinks),
            Arc::clone(&self.clock),
        )
    }

    fn runner(
        &self,
        served: &Served,
        executor: Arc<dyn QueryExecutor>,
        telemetry: Arc<dyn Telemetry>,
    ) -> ReportRunner {
        let engine: Arc<PermissionEngine> = Arc::clone(&served.decision);
        ReportRunner::new(engine, self.guard(served), executor, telemetry)
    }

    /// The report sources of this site, compiled at first use (the start-up check) and closed to
    /// registration from then on.
    fn book(&self) -> &SourceBook {
        self.book
            .get_or_init(|| SourceBook::compile(&self.sources, Arc::clone(&self.clock)))
    }

    fn admit_request(&self, caller: &Caller) -> AdmitRequest {
        AdmitRequest {
            site: self.site.clone(),
            user: caller.user.clone(),
            principal: caller.principal.clone(),
        }
    }

    /// The reports of the snapshot served now, compiled once per snapshot.
    fn prepared(&self) -> CatalogResult<Arc<Prepared>> {
        let served = self.live.served().ok_or_else(unavailable)?;
        let mut cached = lock(&self.prepared);
        if let Some(p) = cached.as_ref()
            && Arc::ptr_eq(&p.served, &served)
        {
            return Ok(Arc::clone(p));
        }
        let catalog = Arc::new(crate::schema::catalog(&served.meta).map_err(|_| unavailable())?);
        let meta = EffectiveMetaSet::new(Arc::clone(&served.meta));
        let Compiled { registry, problems } = compile_all(&self.shipped, &meta, &catalog);
        if !problems.is_empty() {
            tracing::warn!(
                site = %self.site,
                problems = %problems.join("; "),
                "reports left out of the served metadata"
            );
        }
        let runner = self.runner(
            &served,
            Arc::new(Live {
                backend: self.backend.clone(),
                catalog,
            }),
            Arc::new(Timing),
        );
        let describer = self.runner(
            &served,
            Arc::new(NoRows),
            Arc::new(spot_reports::NoTelemetry),
        );
        let prepared = Arc::new(Prepared {
            served,
            registry,
            problems,
            runner,
            describer,
        });
        *cached = Some(Arc::clone(&prepared));
        Ok(prepared)
    }

    async fn caller(&self, ctx: &RequestCtx) -> CatalogResult<Caller> {
        if !self.ready.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        if ctx.site.as_str() != self.site {
            return Err(denied());
        }
        let auth = self.live.auth().ok_or_else(unavailable)?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        if auth.is_restricted(&who) {
            return Err(denied());
        }
        let principals = self.live.principals().ok_or_else(unavailable)?;
        let principal = DocPrincipals::resolve(&*principals, &who.user_id).await;
        Ok(Caller {
            user: who.user_id.to_string(),
            principal,
        })
    }

    /// One refused request, audited with its reason code and nothing else.
    fn refuse(&self, action: &str, who: &str, report: &str, error: &ApiError) {
        tracing::info!(
            target: AUDIT_TARGET,
            site = %self.site,
            action,
            who,
            report,
            code = %error.code.as_str(),
            "report refused"
        );
    }

    async fn run(
        &self,
        ctx: &RequestCtx,
        report: &str,
        filters: Json,
        limit: Option<u64>,
    ) -> CatalogResult<(ReportResult, Vec<Message>)> {
        let caller = self.caller(ctx).await?;
        let request_id = ctx.request_id.as_str();
        let started = Instant::now();
        // The permit is taken here, before the report is looked up, compiled or read, and is
        // held until the run ends or this future is dropped.
        let result = self
            .gate
            .run(
                &caller.user,
                |v: &(ReportResult, Vec<Message>)| v.0.rows.len(),
                self.run_for(&caller, report, filters, limit, request_id),
            )
            .await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match &result {
            Ok((out, _)) => tracing::info!(
                target: AUDIT_TARGET,
                site = %self.site,
                action = "run",
                who = %caller.user,
                report,
                request_id,
                result = RunResult::Ok.as_str(),
                rows = out.rows.len(),
                truncated = out.truncated,
                elapsed_ms,
                "report run"
            ),
            Err(error) => {
                tracing::info!(
                    target: AUDIT_TARGET,
                    site = %self.site,
                    action = "run",
                    who = %caller.user,
                    report,
                    request_id,
                    code = %error.code.as_str(),
                    result = RunResult::of_error(error).as_str(),
                    elapsed_ms,
                    "report refused"
                );
            }
        }
        result
    }

    async fn run_for(
        &self,
        caller: &Caller,
        report: &str,
        filters: Json,
        limit: Option<u64>,
        request_id: &str,
    ) -> CatalogResult<(ReportResult, Vec<Message>)> {
        let prepared = self.prepared()?;
        let Some(registered) = prepared.registry.lookup_for(report, &caller.principal) else {
            // Not a shipped report: a service's source, or nothing (`404 report.not_found`).
            let who = self.admit_request(caller);
            return self
                .book()
                .run_with_request(&who, report, &filters, limit, request_id)
                .await;
        };
        let args = ReportArgs::from_json(&filters).map_err(|e| e.to_api_error())?;
        // The request's `limit` never exceeds the report's maximum; the runner clamps to the
        // report's default when the request sends none.
        let limit = limit.map(|l| l.min(u64::from(registered.compiled.max_rows)));
        let out = prepared
            .runner
            .run(&registered.compiled, &caller.principal, &args, limit)
            .await
            .map_err(|e| run_error(&e, report, request_id))?;
        Ok((out.result, out.messages))
    }

    async fn list(&self, ctx: &RequestCtx) -> CatalogResult<Vec<ReportSummaryOut>> {
        let caller = self.caller(ctx).await?;
        let prepared = self.prepared()?;
        let mut out: Vec<ReportSummaryOut> = prepared
            .registry
            .list_for(&caller.principal)
            .into_iter()
            .map(|r| ReportSummaryOut {
                name: r.compiled.name.clone(),
                label: r.compiled.label.clone(),
                app: r.app.clone(),
                base_doctype: r.compiled.base.as_str().to_owned(),
            })
            .collect();
        out.extend(self.book().list(&self.admit_request(&caller)).await);
        out.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    async fn describe(
        &self,
        ctx: &RequestCtx,
        report: &str,
    ) -> CatalogResult<ReportDescriptionOut> {
        let caller = self.caller(ctx).await?;
        let result = self.describe_for(&caller, report).await;
        if let Err(error) = &result {
            self.refuse("describe", &caller.user, report, error);
        }
        result
    }

    async fn describe_for(
        &self,
        caller: &Caller,
        report: &str,
    ) -> CatalogResult<ReportDescriptionOut> {
        let prepared = self.prepared()?;
        let Some(registered) = prepared.registry.lookup_for(report, &caller.principal) else {
            return self
                .book()
                .describe(&self.admit_request(caller), report)
                .await;
        };
        describe_registered(&prepared.describer, registered, &caller.principal).await
    }
}

/// Describes one registered report as a run for `principal` over no rows states it: the columns
/// are the ones a run for this caller carries.
async fn describe_registered(
    describer: &ReportRunner,
    registered: &RegisteredReport,
    principal: &Principal,
) -> CatalogResult<ReportDescriptionOut> {
    let compiled = &registered.compiled;
    // The columns are the ones a run for this caller carries. Only the required parameters
    // are bound, so a filter on an optional parameter is not judged: a description is
    // refused only where a run with the same arguments would be.
    let args = ReportArgs::from_map(
        compiled
            .parameters
            .iter()
            .filter(|p| p.required)
            .map(|p| (p.name.clone(), placeholder(p.ty, &p.options)))
            .collect::<BTreeMap<_, _>>(),
    );
    let columns = describer
        .run(compiled, principal, &args, Some(1))
        .await
        .map_err(|e| e.to_api_error())?
        .result
        .columns;
    Ok(ReportDescriptionOut {
        name: compiled.name.clone(),
        label: compiled.label.clone(),
        app: registered.app.clone(),
        base_doctype: compiled.base.as_str().to_owned(),
        parameters: compiled
            .parameters
            .iter()
            .map(|p| ReportParameterOut {
                name: p.name.clone(),
                label: p.label.clone(),
                kind: p.ty.as_str().to_owned(),
                required: p.required,
                default: p.default.as_ref().map(|v| default_json(p.ty, v)),
                options: match &p.options {
                    ParamOptions::None => None,
                    ParamOptions::Values(values) => {
                        Some(ParameterOptionsOut::Values(values.clone()))
                    }
                    ParamOptions::Link(doctype) => Some(ParameterOptionsOut::Link(doctype.clone())),
                },
            })
            .collect(),
        columns: columns
            .into_iter()
            .map(|c| ReportColumn {
                name: c.name,
                label: c.label,
                field_type: c.field_type,
            })
            .collect(),
        max_rows: compiled.max_rows,
    })
}

/// A value of a parameter's type that binds: what a description passes for a required parameter.
fn placeholder(ty: ParamType, options: &ParamOptions) -> Json {
    match ty {
        ParamType::Text => Json::String(String::new()),
        ParamType::Int => Json::from(0),
        ParamType::Decimal => Json::String("0".to_owned()),
        ParamType::Date => Json::String("1970-01-01".to_owned()),
        ParamType::Link => Json::String(RecordId::new_v7().to_string()),
        ParamType::Select => Json::String(match options {
            ParamOptions::Values(values) => values.first().cloned().unwrap_or_default(),
            _ => String::new(),
        }),
    }
}

/// A parameter's default in the contract's value encoding.
fn default_json(ty: ParamType, value: &Value) -> Json {
    use spot_reports::encode::{FieldType, encode_value};
    let field_type = match ty {
        ParamType::Text | ParamType::Select => FieldType::Text,
        ParamType::Int => FieldType::Int,
        ParamType::Decimal => FieldType::Decimal { scale: 0 },
        ParamType::Date => FieldType::Date,
        ParamType::Link => FieldType::Link,
    };
    encode_value(value, &field_type)
}

/// The report port of one site: the engine delegate behind `report_run` and the catalogue
/// behind `report_list` and `report_describe`.
struct Reports(Arc<Shared>);

impl ReportDelegate for Reports {
    fn run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, std::result::Result<(ReportResult, Vec<Message>), ApiError>> {
        Box::pin(async move { self.0.run(ctx, report, filters, limit).await })
    }
}

impl ReportCatalogPort for Reports {
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, CatalogResult<Vec<ReportSummaryOut>>> {
        Box::pin(async move { self.0.list(ctx).await })
    }

    fn describe<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
    ) -> BoxFuture<'a, CatalogResult<ReportDescriptionOut>> {
        Box::pin(async move { self.0.describe(ctx, report).await })
    }
}

struct ReportService {
    shared: Arc<Shared>,
}

impl Service for ReportService {
    fn name(&self) -> &'static str {
        "report"
    }

    fn level(&self) -> Level {
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.shared
                .live
                .served()
                .ok_or_else(|| "no metadata is served".to_owned())?;
            let prepared = self
                .shared
                .prepared()
                .map_err(|_| "the reports cannot be compiled".to_owned())?;
            let mut problems = prepared.problems.clone();
            let book = self.shared.book();
            problems.extend(book.fatal().iter().cloned());
            // A source report named like a shipped one stops the check, naming both.
            for (name, service) in book.served() {
                if let Some(shipped) = prepared.registry.lookup(&name) {
                    problems.push(format!(
                        "the report `{name}` is shipped by the app `{}` and also registered by the service `{service}`",
                        shipped.app
                    ));
                }
            }
            if !problems.is_empty() {
                return Err(problems.join("\n"));
            }
            self.shared.ready.store(true, Ordering::Release);
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            self.shared.ready.store(false, Ordering::Release);
            Ok(())
        })
    }
}

/// The ports of the sites built so far, by site name: a reload builds the site again without
/// building the service again, and its routes must reach the same port.
fn built() -> &'static Mutex<HashMap<String, Arc<Reports>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<Reports>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// The readiness line of the reports (WP-PLT-14.T7): always passing, and its detail is `in
/// flight n/max`, the runs in flight against the site's cap. `n` never exceeds `max` (the gate
/// refuses at the cap) and the text holds two numbers, no report, user or site data.
///
/// The service registry's own check has no channel for an ok-detail, so this is a
/// [`ReadinessCheck`] of its own: the module provides it as the port `ReportReadiness`, and the
/// host adds it to the site's readiness list next to the service health.
pub struct ReportReadiness {
    shared: Arc<Shared>,
}

impl ReportReadiness {
    /// The detail text now.
    #[must_use]
    pub fn detail(&self) -> String {
        self.shared.gate.status()
    }
}

impl spot_http::health::ReadinessCheck for ReportReadiness {
    fn name(&self) -> &str {
        "reports"
    }

    fn check(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = spot_http::health::CheckResult> + Send + '_>> {
        Box::pin(async move {
            spot_http::health::CheckResult {
                ok: true,
                detail: self.detail(),
            }
        })
    }
}

/// Registers reports: the service, the engine delegate behind `report_run`, and the catalogue
/// port the host gives the router (the registry mounts the routes).
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("report")?;
    settings.validate()?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let port = if ctx.assembling() {
        // Services that start later register reports of their own data through this port.
        let sources = Arc::new(SourceRegistry::new());
        ctx.provide::<dyn ReportSources>(Arc::clone(&sources) as Arc<dyn ReportSources>)?;
        let installed = super::controllers::installed(env.root, env.loaded, env.site)?;
        let shipped = read_definitions(installed.into_iter().map(|a| (a.name, a.dir)))
            .map_err(|problems| Error(problems.join("\n")))?;
        let shared = Arc::new(Shared {
            site: site.clone(),
            live: Arc::clone(env.live),
            backend: env.backend.clone(),
            clock: Arc::clone(ctx.clock()),
            shipped,
            sources: Arc::clone(&sources),
            book: OnceLock::new(),
            prepared: Mutex::new(None),
            ready: AtomicBool::new(false),
            gate: ReportGate::new(settings.max_concurrent, settings.max_concurrent_per_user),
        });
        ctx.provide(Arc::new(ReportReadiness {
            shared: Arc::clone(&shared),
        }))?;
        ctx.add(Arc::new(ReportService {
            shared: Arc::clone(&shared),
        }));
        let port = Arc::new(Reports(shared));
        lock(built()).insert(site, Arc::clone(&port));
        port
    } else {
        match lock(built()).get(&site).cloned() {
            Some(port) => port,
            None => return Ok(()),
        }
    };
    // `report_run` reaches the engine through the delegate the host hands the resource adapter.
    ctx.provide::<dyn ReportDelegate>(Arc::clone(&port) as Arc<dyn ReportDelegate>)?;
    // The catalogue port goes to the host, which hands it to the router's `ApiServices`
    // (`with_port`): the route registry then mounts `report_list` and `report_describe` once,
    // behind the router's bearer layer (WP-PLT-14.T5). An absent port leaves the group
    // unmounted, never half served.
    ctx.provide::<dyn ReportCatalogPort>(port as Arc<dyn ReportCatalogPort>)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use spot_meta::{Definition, FieldSpec, FieldType, Registry};

    const REPORT: &str = r#"
name = "memo_list"
label = "Memos"
base = "journal"
roles = ["Reader"]

[[columns]]
field = "memo"
"#;

    fn meta(fields: Vec<FieldSpec>) -> (EffectiveMetaSet, Catalog) {
        let mut journal = Definition::new("journal");
        journal.fields = fields;
        let effective = Registry::new(vec![journal]).expect("metadata").effective();
        let catalog = crate::schema::catalog(&effective).expect("catalog");
        (EffectiveMetaSet::new(effective), catalog)
    }

    fn shipped(text: &str) -> Vec<Shipped> {
        vec![(
            "books".to_owned(),
            Arc::new(ReportDef::from_toml(text).expect("definition")),
        )]
    }

    #[test]
    fn a_definition_compiles_against_the_metadata_that_has_its_fields() {
        let (set, catalog) = meta(vec![FieldSpec::new("memo", FieldType::Text)]);
        let compiled = compile_all(&shipped(REPORT), &set, &catalog);
        assert!(compiled.problems.is_empty(), "{:?}", compiled.problems);
        assert_eq!(compiled.registry.len(), 1);
    }

    #[test]
    fn a_metadata_swap_that_drops_a_field_leaves_the_report_out_and_names_it() {
        let (before, catalog) = meta(vec![FieldSpec::new("memo", FieldType::Text)]);
        let first = compile_all(&shipped(REPORT), &before, &catalog);
        let digest_before = first
            .registry
            .lookup("memo_list")
            .expect("compiled before the swap")
            .compiled
            .meta_digest
            .clone();

        // The same definitions against a snapshot without the field: the report is not served
        // against metadata it was not written for.
        let (after, catalog) = meta(vec![FieldSpec::new("note", FieldType::Text)]);
        let second = compile_all(&shipped(REPORT), &after, &catalog);
        assert!(second.registry.lookup("memo_list").is_none());
        assert_eq!(second.problems.len(), 1, "{:?}", second.problems);
        assert!(
            second.problems[0].contains("memo_list"),
            "{:?}",
            second.problems
        );

        // And against a snapshot that still has it but differs elsewhere, it is compiled afresh
        // for that snapshot: the compiled form carries the digest of the metadata it saw.
        let (changed, catalog) = meta(vec![
            FieldSpec::new("memo", FieldType::Text),
            FieldSpec::new("note", FieldType::Text),
        ]);
        let third = compile_all(&shipped(REPORT), &changed, &catalog);
        let digest_after = third
            .registry
            .lookup("memo_list")
            .expect("compiled after the swap")
            .compiled
            .meta_digest
            .clone();
        assert_ne!(digest_before, digest_after);
    }

    #[test]
    fn every_problem_of_every_definition_is_named() {
        let (set, catalog) = meta(vec![FieldSpec::new("memo", FieldType::Text)]);
        let mut both = shipped(
            &REPORT
                .replace("memo_list", "first")
                .replace("\"memo\"", "\"gone\""),
        );
        both.extend(shipped(
            &REPORT
                .replace("memo_list", "second")
                .replace("journal", "ledger"),
        ));
        let compiled = compile_all(&both, &set, &catalog);
        assert_eq!(compiled.problems.len(), 2, "{:?}", compiled.problems);
        assert!(compiled.problems.iter().any(|p| p.contains("`first`")));
        assert!(compiled.problems.iter().any(|p| p.contains("`second`")));
    }

    #[test]
    fn the_same_name_from_two_apps_is_refused() {
        let (set, catalog) = meta(vec![FieldSpec::new("memo", FieldType::Text)]);
        let mut both = shipped(REPORT);
        both.push((
            "other".to_owned(),
            Arc::new(ReportDef::from_toml(REPORT).expect("definition")),
        ));
        let compiled = compile_all(&both, &set, &catalog);
        assert_eq!(compiled.registry.len(), 1);
        assert_eq!(compiled.problems.len(), 1, "{:?}", compiled.problems);
        assert!(
            compiled.problems[0].contains("other"),
            "{:?}",
            compiled.problems
        );
    }

    #[test]
    fn definitions_are_read_per_app_in_file_order_and_every_unreadable_file_is_named() {
        let root = std::env::temp_dir().join(format!("spot_reports_read_{}", RecordId::new_v7()));
        let books = root.join("books");
        let empty = root.join("empty");
        std::fs::create_dir_all(books.join(REPORTS_DIR)).unwrap();
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(
            books.join(REPORTS_DIR).join("b.toml"),
            REPORT.replace("memo_list", "second"),
        )
        .unwrap();
        std::fs::write(books.join(REPORTS_DIR).join("a.toml"), REPORT).unwrap();
        std::fs::write(books.join(REPORTS_DIR).join("notes.txt"), "not a report").unwrap();

        let found = read_definitions([
            ("books".to_owned(), books.clone()),
            ("empty".to_owned(), empty.clone()),
        ])
        .unwrap();
        let order: Vec<&str> = found.iter().map(|(_, d)| d.name.as_str()).collect();
        assert_eq!(
            order,
            ["memo_list", "second"],
            "file-name order; an app with no reports ships none"
        );

        std::fs::write(books.join(REPORTS_DIR).join("c.toml"), "name = ").unwrap();
        std::fs::write(books.join(REPORTS_DIR).join("d.toml"), "bogus = 1").unwrap();
        let problems = read_definitions([("books".to_owned(), books)]).unwrap_err();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("books/reports/c.toml"), "{problems:?}");
        assert!(problems[1].contains("books/reports/d.toml"), "{problems:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_description_binds_required_parameters_only_with_values_of_their_type() {
        let options = ParamOptions::Values(vec!["draft".to_owned(), "posted".to_owned()]);
        assert_eq!(
            placeholder(ParamType::Select, &options),
            Json::from("draft")
        );
        assert_eq!(
            placeholder(ParamType::Int, &ParamOptions::None),
            Json::from(0)
        );
        assert_eq!(
            placeholder(ParamType::Date, &ParamOptions::None),
            Json::from("1970-01-01")
        );
        assert!(
            placeholder(ParamType::Link, &ParamOptions::None)
                .as_str()
                .is_some_and(|id| id.len() == 36)
        );
    }

    #[test]
    fn a_default_is_encoded_as_the_contract_encodes_a_value_of_its_type() {
        assert_eq!(
            default_json(ParamType::Text, &Value::Text("posted".to_owned())),
            Json::from("posted")
        );
        assert_eq!(default_json(ParamType::Int, &Value::Int(7)), Json::from(7));
    }
}
