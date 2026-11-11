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
//! service has nothing to fail. The settings are `services.report` (`enabled`, default true).
//!
//! # Audit
//!
//! Target `spot::audit::report`: a line for every run (who, report, rows, whether it was cut) and
//! for every refusal (who, report, the reason code). Never an argument value, a filter value or
//! a row.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde::Deserialize;
use serde_json::Value as Json;
use spot_api_services::ReportDelegate;
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_field_guard::{
    AuditRecord as GuardAudit, FieldAccessRecord, FieldGuard, MetaProtection, Offer, TraceSink,
};
use spot_http::api::Ports;
use spot_http::api::ports::report::{
    CatalogResult, ParameterOptionsOut, ReportCatalogPort, ReportDescriptionOut,
    ReportParameterOut, ReportSummaryOut,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes;
use spot_http::api::service::{
    ApiError, BoxFuture, Message, ReportColumn, ReportResult, RequestCtx,
};
use spot_permissions::{PermissionEngine, Principal};
use spot_postgres::PgBackend;
use spot_reports::{
    Compiler, EffectiveMetaSet, ParamOptions, ParamType, QueryExecutor, ReportArgs, ReportDef,
    ReportError, ReportRegistry, ReportRunner, Telemetry, codes,
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

/// `[services.report]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site serves reports at all. Off, `report_run` answers `report.not_found` and
    /// the catalogue routes are not mounted.
    pub enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true }
    }
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
        let label = format!("{app}/{REPORTS_DIR}/{}", entry.file_name().to_string_lossy());
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
    prepared: Mutex<Option<Arc<Prepared>>>,
    ready: AtomicBool,
}

impl Shared {
    fn guard(&self, served: &Served) -> FieldGuard {
        FieldGuard::new(
            Arc::clone(&served.decision),
            Arc::new(MetaProtection::new(&served.meta)) as Arc<dyn spot_field_guard::ProtectionSource>,
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
        let describer = self.runner(&served, Arc::new(NoRows), Arc::new(spot_reports::NoTelemetry));
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
        let result = self.run_for(&caller, report, filters, limit).await;
        match &result {
            Ok((out, _)) => tracing::info!(
                target: AUDIT_TARGET,
                site = %self.site,
                action = "run",
                who = %caller.user,
                report,
                rows = out.rows.len(),
                truncated = out.truncated,
                "report run"
            ),
            Err(error) => self.refuse("run", &caller.user, report, error),
        }
        result
    }

    async fn run_for(
        &self,
        caller: &Caller,
        report: &str,
        filters: Json,
        limit: Option<u64>,
    ) -> CatalogResult<(ReportResult, Vec<Message>)> {
        let prepared = self.prepared()?;
        let Some(registered) = prepared.registry.lookup_for(report, &caller.principal) else {
            return Err(not_found(report));
        };
        let args = ReportArgs::from_json(&filters).map_err(|e| e.to_api_error())?;
        // The request's `limit` never exceeds the report's maximum; the runner clamps to the
        // report's default when the request sends none.
        let limit = limit.map(|l| l.min(u64::from(registered.compiled.max_rows)));
        let out = prepared
            .runner
            .run(&registered.compiled, &caller.principal, &args, limit)
            .await
            .map_err(|e| e.to_api_error())?;
        Ok((out.result, out.messages))
    }

    async fn list(&self, ctx: &RequestCtx) -> CatalogResult<Vec<ReportSummaryOut>> {
        let caller = self.caller(ctx).await?;
        let prepared = self.prepared()?;
        Ok(prepared
            .registry
            .list_for(&caller.principal)
            .into_iter()
            .map(|r| ReportSummaryOut {
                name: r.compiled.name.clone(),
                label: r.compiled.label.clone(),
                app: r.app.clone(),
                base_doctype: r.compiled.base.as_str().to_owned(),
            })
            .collect())
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
            return Err(not_found(report));
        };
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
        let columns = prepared
            .describer
            .run(compiled, &caller.principal, &args, Some(1))
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
                        ParamOptions::Link(doctype) => {
                            Some(ParameterOptionsOut::Link(doctype.clone()))
                        }
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
            if !prepared.problems.is_empty() {
                return Err(prepared.problems.join("\n"));
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

/// Registers reports: the service, the engine delegate behind `report_run`, and the catalogue
/// routes on the site's router.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("report")?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let port = if ctx.assembling() {
        let installed = super::controllers::installed(env.root, env.loaded, env.site)?;
        let shipped = read_definitions(installed.into_iter().map(|a| (a.name, a.dir)))
            .map_err(|problems| Error(problems.join("\n")))?;
        let shared = Arc::new(Shared {
            site: site.clone(),
            live: Arc::clone(env.live),
            backend: env.backend.clone(),
            clock: Arc::clone(ctx.clock()),
            shipped,
            prepared: Mutex::new(None),
            ready: AtomicBool::new(false),
        });
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
    let mut ports = Ports::new();
    ports.provide::<dyn ReportCatalogPort>(port);
    // The group is mounted here, not by the route registry (see `routes/report.rs`). The host
    // authenticates every request before the router sees it, and the port authenticates again.
    let mounted = routes::mount(&[routes::report::registration()], &ports, &[]);
    let router = mounted.authenticated;
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the report routes were not mounted"
        );
    }
    ctx.router_layer(move |base| base.merge(router));
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
        assert!(second.problems[0].contains("memo_list"), "{:?}", second.problems);

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
        let mut both = shipped(&REPORT.replace("memo_list", "first").replace("\"memo\"", "\"gone\""));
        both.extend(shipped(&REPORT.replace("memo_list", "second").replace("journal", "ledger")));
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
        assert!(compiled.problems[0].contains("other"), "{:?}", compiled.problems);
    }

    #[test]
    fn definitions_are_read_per_app_in_file_order_and_every_unreadable_file_is_named() {
        let root = std::env::temp_dir().join(format!("spot_reports_read_{}", RecordId::new_v7()));
        let books = root.join("books");
        let empty = root.join("empty");
        std::fs::create_dir_all(books.join(REPORTS_DIR)).unwrap();
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(books.join(REPORTS_DIR).join("b.toml"), REPORT.replace("memo_list", "second"))
            .unwrap();
        std::fs::write(books.join(REPORTS_DIR).join("a.toml"), REPORT).unwrap();
        std::fs::write(books.join(REPORTS_DIR).join("notes.txt"), "not a report").unwrap();

        let found = read_definitions([
            ("books".to_owned(), books.clone()),
            ("empty".to_owned(), empty.clone()),
        ])
        .unwrap();
        let order: Vec<&str> = found.iter().map(|(_, d)| d.name.as_str()).collect();
        assert_eq!(order, ["memo_list", "second"], "file-name order; an app with no reports ships none");

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
        assert_eq!(placeholder(ParamType::Select, &options), Json::from("draft"));
        assert_eq!(placeholder(ParamType::Int, &ParamOptions::None), Json::from(0));
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
