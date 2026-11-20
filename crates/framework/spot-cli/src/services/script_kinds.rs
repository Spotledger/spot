//! API scripts and scheduled scripts, hosted (WP-EXT-06.T7; ADR-054, ADR-016).
//!
//! The host side of `spot_server_scripts::kinds` (WP-EXT-06.T6). A site that has the optional
//! `scripting` app installed serves its enabled **API scripts** as the whitelisted functions
//! `scripting/<module>/<function>` and fires its **scheduled scripts** on their cron schedules.
//! Both are TypeScript that an administrator wrote in an `api_script` or `scheduled_script`
//! document; neither needs an app to be built.
//!
//! # The shape
//!
//! - [`KindsHandle`]: the per-site, late-bound handle. The two admin controllers are installed
//!   before the document engine exists ([`register_controllers`]); `register` fills the handle
//!   with the runtime afterwards, exactly like the Server Script slot of `services::scripts`.
//! - [`KindDocs`]: reads `api_script` and `scheduled_script` documents through the live engine as
//!   the platform (`Actor::System { component: "script_kinds" }`, read-only) and writes back
//!   only run-time fields (`last_error`, `last_run_count`, and `enabled` / `disabled_reason`
//!   when a scheduled script keeps failing) through the audited path.
//! - [`KindsRuntime`]: turns the documents into the two registries of `spot-server-scripts`,
//!   keeps them current, and is the one place a script is run from.
//! - Two **seams** that exist for every script-like service, not for this one alone:
//!   [`FunctionSources`](super::function::FunctionSources) on the function service (declarations
//!   that change while the site runs, served beside the manifests' functions, never allowed to
//!   shadow one) and [`ScheduleSources`](super::jobs::ScheduleSources) on the jobs module
//!   (schedules that change while the site runs). Each is a port the module takes with
//!   `ctx.port()`. The brief's `internal schedule` port of WP-BPM-11.T3 fixes its schedules when
//!   the site is assembled, so it could not follow a script saved later; the dynamic source is
//!   the same idea with the set re-read at every scheduler tick.
//!
//! # Who a script runs as
//!
//! An API script runs as the function's **caller** with the caller's rights; a guest is refused
//! before the engine starts (`serverscript.guest_refused`). A scheduled script runs as the
//! `run_as` user and, if that user is missing or disabled, is refused with
//! `serverscript.run_as_unavailable`. Nothing here hands a script the system principal; the
//! platform principal reads the script *records* and writes their run-time summary, never the
//! script's own host calls.
//!
//! # What is live
//!
//! - **API scripts**: the call after a save, enable, disable or delete **commits** uses the new
//!   set (the committed step of the admin controller rebuilds the registry before the next
//!   call). A call already running finishes with the script it started with.
//! - **Scheduled scripts**: the schedule set is replaced at the scheduler's next tick; a run
//!   that was already enqueued runs the script as it is when the job executes (the job names the
//!   script, not its source), and a slot is never run twice (the slot's deduplication key is the
//!   job's idempotency key and the run's command id).
//! - Another node sees a change within `[services.script_kinds] refresh_secs`.
//!
//! # Level
//!
//! Optional. A site without the `scripting` app, or with `enabled = false`, gets nothing from this
//! module. While the documents have not been read yet no script function is served and no
//! schedule fires; a script whose record is invalid is skipped and listed in
//! [`KindsHandle::status`] with its reason, never a start failure; two API scripts claiming one
//! function key are both disabled and the status says so.
//!
//! # Security
//!
//! No error, log line or audit record carries script text, arguments, results or a document
//! value: a failure names the script and a stable code. Scripts cannot change another script's
//! record (`api_script` and `scheduled_script` are not scriptable DocTypes; see
//! `services::scripts::RESERVED`), and only the administrator roles hold any right on them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value as Json;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::{Document, DocumentPatch};
use spot_app_api::error::ErrorClass;
use spot_app_api::lifecycle::{HookPoint, Phase, Target};
use spot_app_api::prelude::{Name, RecordId, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, HookContext, Operation, StepDecl, StepName,
};
use spot_controllers::{ControllerRegistry, ControllerSource};
use spot_core::{Clock, SiteId};
use spot_document::engine::Engine;
use spot_functions::{FunctionDecl, FunctionHandler, RateLimit};
use spot_jobs::worker::{HandlerResult, JobContext};
use spot_scheduler::{RunSlot, SlotRef};
use spot_server_scripts::kinds::{
    ApiScript, ApiScriptRegistry, KindCompileController, KindDeps, KindRun, KindStore,
    RUN_SCHEDULED_SCRIPT_JOB, ScheduledScript, ScheduledScriptRunner, ScriptKind, UserDirectory,
    UserRecord,
};
use spot_server_scripts::{
    CompileOutcome, ScriptAccessFactory, ScriptDocumentAccess, ScriptLimits, ScriptQueries,
    ServiceAccess,
};
use spot_storage_contract::{
    Catalog, Expr, Isolation, Query, QueryExecutor, QueryPath, Rows, Selection, StorageBackend,
    TextMatch, TextMode, Transaction, TxOptions,
};
use tokio::sync::watch;

use crate::services::function::{FunctionDeclSource, FunctionSources};
use crate::services::jobs::{DynamicSchedule, ScheduleSource, ScheduleSources};
use crate::services::scripts::{ElevationLog, ElevationStep, PermissionCheck};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "script_kinds/admin.rs"]
mod admin;
#[path = "script_kinds/docs.rs"]
mod docs;
#[path = "script_kinds/runtime.rs"]
mod runtime;
#[path = "script_kinds/settings.rs"]
mod settings;

pub use admin::{KindAdmin, register_controllers};
pub use docs::{
    EngineAccessFactory, EngineQueries, KindDocs, KindSource, Loaded, NoQueries, Skipped,
};
pub use runtime::{KindsHandle, KindsRuntime, KindsStatus, handle};
pub use settings::Settings;

/// Start order: after the function service (1020) and the Server Script module (1070): it takes
/// the function and schedule sources those modules provide.
pub const ORDER: u32 = 1080;

/// The component the module acts under when it reads or writes a script record as the platform.
pub const COMPONENT: &str = "script_kinds";

/// The app that carries the two DocTypes; a site without it gets nothing from this module.
pub const SCRIPTING_APP: &str = spot_server_scripts::kinds::SCRIPTING_APP;

/// The most script rows one DocType may hold; a DocType with more is refused.
const MAX_ROWS: u32 = 5_000;

/// Joins the site: builds the runtime, provides the two sources and registers the job handler.
///
/// # Errors
///
/// A setting out of range (the error names the key, never a value), or a port the module needs
/// that the site's modules did not provide in the right order.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("script_kinds")?;
    if let Some(key) = settings.invalid_key() {
        return Err(Error(format!("services.script_kinds.{key}: out of range")));
    }
    let (site_name, has_app) = {
        let env = *ctx.env()?;
        (
            env.site.site.clone(),
            env.site.apps.iter().any(|app| app == SCRIPTING_APP),
        )
    };
    let handle = handle(&site_name);
    if !settings.enabled || !has_app || !ctx.assembling() {
        if !ctx.assembling() {
            // A reload keeps the running runtime; the next refresh sees the new metadata.
            return Ok(());
        }
        handle.set_off();
        return Ok(());
    }
    let script_settings: crate::services::scripts::Settings = ctx.settings_or_default("scripts")?;
    let limits = script_settings.script_limits();
    let (docs, site) = {
        let env = *ctx.env()?;
        let site = SiteId::new(&env.site.site).map_err(crate::error)?;
        let live = Arc::clone(env.live);
        (
            KindDocs::new(
                site.clone(),
                Arc::new(move || live.engine()),
                Arc::clone(ctx.clock()),
                env.permission.catalog.clone(),
            ),
            site,
        )
    };
    let live = Arc::clone(ctx.env()?.live);
    let permissions: Arc<dyn PermissionCheck> =
        ctx.port::<dyn PermissionCheck>().unwrap_or_else(|| {
            Arc::new(crate::services::scripts::LivePermissionCheck::new(
                Arc::clone(&live),
            ))
        });
    let engine_of = Arc::clone(&live);
    let access = Arc::new(EngineAccessFactory::new(Arc::new(move || {
        engine_of.engine()
    })));
    let mut runtime = KindsRuntime::new(
        site,
        settings,
        limits,
        Arc::new(docs),
        Arc::clone(&access) as Arc<dyn ScriptAccessFactory>,
        Arc::clone(ctx.clock()),
        crate::services::scripts::slot(&site_name)
            .script_engine()
            .clone(),
    )
    .with_permissions(permissions);
    // Elevation (WP-EXT-06.T10) needs somewhere to audit: the port `ElevationLog`, bound to the
    // audit chain appender by the module that owns the chain. Without it no API script is ever
    // elevated (default deny), whatever its record says.
    match ctx.port::<dyn ElevationLog>() {
        Some(log) => {
            runtime = runtime.with_elevation(spot_server_scripts::elevation::ElevationWiring {
                factory: access,
                audit: Arc::new(crate::services::scripts::ChainElevationAudit::new(log)),
            });
        }
        None => {
            tracing::info!(site = %site_name, "no elevation log is provided: API scripts are never elevated");
        }
    }
    let runtime = Arc::new(runtime);
    handle.set_ready(Arc::clone(&runtime));

    // The two seams. Both are optional ports: a site with the function service or the jobs
    // module off simply serves or schedules nothing from here, and says so in the status.
    if let Some(functions) = ctx.port::<FunctionSources>() {
        let source: Arc<dyn FunctionDeclSource> = Arc::clone(&runtime.api_source()) as _;
        functions
            .set(SCRIPTING_APP, source)
            .map_err(|conflicts| Error(format!("script functions: {conflicts}")))?;
        runtime.attach_functions(functions);
    } else {
        tracing::warn!(site = %site_name, "no function service: API scripts are not served");
    }
    if let Some(schedules) = ctx.port::<ScheduleSources>() {
        let source: Arc<dyn ScheduleSource> = Arc::clone(&runtime.schedule_source()) as _;
        schedules.set(SCRIPTING_APP, source);
    } else {
        tracing::warn!(site = %site_name, "no jobs module: scheduled scripts do not run");
    }
    // The job a scheduled slot enqueues. A script failure never retries by itself (ADR-072): the
    // job answers `Fail`, the next slot is the next attempt.
    let job_runtime = Arc::clone(&runtime);
    ctx.job_handlers()
        .register(RUN_SCHEDULED_SCRIPT_JOB, move |job: JobContext| {
            let runtime = Arc::clone(&job_runtime);
            Box::pin(async move { Ok(runtime.run_job(&job.job).await) })
        });
    ctx.add(Arc::new(KindsService {
        runtime,
        refresh: Duration::from_secs(settings.refresh_secs),
    }));
    Ok(())
}

/// The module's service: it loads the scripts once the engine exists, then keeps them current.
struct KindsService {
    runtime: Arc<KindsRuntime>,
    refresh: Duration,
}

impl Service for KindsService {
    fn name(&self) -> &'static str {
        "script_kinds"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut since_load = Duration::ZERO;
            let mut loaded = false;
            let tick = Duration::from_secs(1);
            while !*stop.borrow() {
                let due = !loaded || self.runtime.take_dirty() || since_load >= self.refresh;
                if due {
                    match self.runtime.refresh().await {
                        Ok(()) => {
                            loaded = true;
                            since_load = Duration::ZERO;
                        }
                        // The engine is not there yet, or storage is down: try again at the next
                        // tick; scripts stay as they were.
                        Err(error) => {
                            tracing::debug!(code = %error.code, "script kinds not loaded yet");
                        }
                    }
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    () = tokio::time::sleep(tick) => {
                        since_load += tick;
                    }
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_defaults_are_in_range() {
        assert_eq!(Settings::default().invalid_key(), None);
        assert!(Settings::default().enabled);
        assert_eq!(Settings::default().max_scripts, 200);
        assert_eq!(Settings::default().default_rate_limit_per_minute, 60);
    }

    #[test]
    fn a_setting_out_of_range_names_its_key() {
        let settings = Settings {
            max_scripts: 0,
            ..Settings::default()
        };
        assert_eq!(settings.invalid_key(), Some("max_scripts"));
        let settings = Settings {
            default_rate_limit_per_minute: 0,
            ..Settings::default()
        };
        assert_eq!(
            settings.invalid_key(),
            Some("default_rate_limit_per_minute")
        );
        let settings = Settings {
            refresh_secs: 0,
            ..Settings::default()
        };
        assert_eq!(settings.invalid_key(), Some("refresh_secs"));
    }

    #[test]
    fn unknown_settings_keys_are_refused() {
        let table: toml::Value = toml::from_str("enabled = true\nnope = 1").unwrap();
        assert!(table.try_into::<Settings>().is_err());
    }

    #[test]
    fn the_job_name_is_pinned() {
        assert_eq!(RUN_SCHEDULED_SCRIPT_JOB, "scripting:run_scheduled_script");
    }
}
