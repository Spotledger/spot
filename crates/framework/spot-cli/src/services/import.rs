//! Data import of one site: `import_limits`, `import_create`, `import_get`, `import_errors`,
//! `import_run` and `import_cancel` (`/api/v1/import*`, contract 0.21.0, WP-DOC-09.T5), over
//! `spot-import` and the files service.
//!
//! `spot-http` owns the routes ([`ImportPort`]); `spot-import` owns the reading, mapping,
//! conversion and writing. This module is the host half, split in three files under
//! `services/import/`:
//!
//! - `flow.rs`: the two-phase state machine (dry run, digest, run), the limits, creator-only
//!   access, idempotent create and run, audit and the runner. No database, no files: it is
//!   tested without them (`tests/import_service.rs`).
//! - `host.rs`: the production [`ImportHost`](flow::ImportHost): who the caller is, the import
//!   and submit rights, the uploaded file released by the files service, and `spot-import`.
//! - `records.rs`: the job record store seam and an in-memory store.
//!
//! # Level
//!
//! Optional. A failed start-up check (no metadata served, no files service, an unusable spool
//! directory) degrades the site; the import routes then answer `503`. Settings are
//! `[services.import]`: `enabled`, `max_file_bytes`, `max_rows`, `max_columns`,
//! `max_concurrent_per_site`, `batch_rows`, `max_errors`, `spool_dir`.
//!
//! # Audit
//!
//! Target `spot::audit::import`: one line per event (created, dry run, run started, finished,
//! cancelled, refused): who, job, DocType, outcome. Never a cell, a file name or a token.

#[path = "import/flow.rs"]
pub mod flow;
#[path = "import/host.rs"]
mod host;
#[path = "import/records.rs"]
pub mod records;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use spot_http::api::Ports;
use spot_http::api::ports::import::ImportPort;
use spot_http::api::router::routes;
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

use self::flow::{AuditEvent, ImportAudit, ImportCore, Limits};
use self::host::SiteHost;
use self::records::MemoryRecords;

/// Start order: after the framework's own services.
pub const ORDER: u32 = 1060;

/// `[services.import]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site serves imports at all.
    pub enabled: bool,
    /// The largest file in bytes.
    pub max_file_bytes: u64,
    /// The most rows.
    pub max_rows: u64,
    /// The most columns.
    pub max_columns: u64,
    /// The most dry runs and runs at once on the site.
    pub max_concurrent_per_site: u64,
    /// Rows per batch.
    pub batch_rows: u64,
    /// Failed rows after which a run stops.
    pub max_errors: u64,
    /// Where the uploaded files are spooled for a job (default: the system temporary
    /// directory, under `spot-import`).
    pub spool_dir: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        let limits = Limits::default();
        Settings {
            enabled: true,
            max_file_bytes: limits.max_file_bytes,
            max_rows: limits.max_rows,
            max_columns: limits.max_columns,
            max_concurrent_per_site: limits.max_concurrent_per_site,
            batch_rows: limits.batch_rows,
            max_errors: limits.max_errors,
            spool_dir: None,
        }
    }
}

impl Settings {
    /// `Err(key)` for a value out of range.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        let ranges: [(&'static str, u64, u64, u64); 6] = [
            ("max_file_bytes", self.max_file_bytes, 1, 1 << 32),
            ("max_rows", self.max_rows, 1, 10_000_000),
            ("max_columns", self.max_columns, 1, 500),
            (
                "max_concurrent_per_site",
                self.max_concurrent_per_site,
                1,
                64,
            ),
            ("batch_rows", self.batch_rows, 1, 10_000),
            ("max_errors", self.max_errors, 1, 100_000),
        ];
        for (key, value, low, high) in ranges {
            if value < low || value > high {
                return Err(key);
            }
        }
        Ok(())
    }

    fn limits(&self) -> Limits {
        Limits {
            max_file_bytes: self.max_file_bytes,
            max_rows: self.max_rows,
            max_columns: self.max_columns,
            max_concurrent_per_site: self.max_concurrent_per_site,
            batch_rows: self.batch_rows,
            max_errors: self.max_errors,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// The audit target of this service.
const AUDIT_TARGET: &str = "spot::audit::import";

struct TracingAudit;

impl ImportAudit for TracingAudit {
    fn record(&self, event: &AuditEvent) {
        tracing::info!(
            target: "spot::audit::import",
            site = %event.site,
            user = %event.user,
            job = event.job.as_deref().unwrap_or(""),
            doctype = event.doctype.as_deref().unwrap_or(""),
            action = event.action,
            outcome = event.outcome,
            "import"
        );
        let _ = AUDIT_TARGET;
    }
}

struct ImportService {
    core: Arc<ImportCore>,
    live: Arc<crate::live::SiteLive>,
    spool: PathBuf,
}

impl Service for ImportService {
    fn name(&self) -> &'static str {
        "import"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.live
                .served()
                .ok_or_else(|| "no metadata is served".to_owned())?;
            self.live.files().ok_or_else(|| {
                "the files service is not running; imports read uploaded files".to_owned()
            })?;
            tokio::fs::create_dir_all(&self.spool).await.map_err(|_| {
                "the import spool directory cannot be created; set services.import.spool_dir"
                    .to_owned()
            })?;
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        self.core.pump().await;
                    }
                }
            }
            Ok(())
        })
    }
}

/// The ports of the sites built so far: a reload builds the site again without building the
/// service again, and its routes must reach the same port.
fn built() -> &'static Mutex<HashMap<String, Arc<ImportCore>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<ImportCore>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// Registers data import: its service and its port for the route registry.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("import")?;
    settings
        .validate()
        .map_err(|key| Error(format!("services.import.{key}: out of range")))?;
    if !settings.enabled {
        return Ok(());
    }
    let site = ctx.env()?.site.site.as_str().to_owned();
    let core = if ctx.assembling() {
        let live = Arc::clone(ctx.env()?.live);
        let spool = settings
            .spool_dir
            .as_deref()
            .map_or_else(|| std::env::temp_dir().join("spot-import"), PathBuf::from);
        let host = Arc::new(SiteHost {
            site: site.clone(),
            live: Arc::clone(&live),
            clock: Arc::clone(ctx.clock()),
            spool: spool.clone(),
            parallel: host::parallel_for_pool(ctx.env()?.backend.config().pool_max_size),
        });
        // Job records are held in memory until the platform declares the `import_job` and
        // `import_job_error` DocTypes (see import.md): a restart ends running jobs.
        let core = ImportCore::new(
            site.clone(),
            host,
            Arc::new(MemoryRecords::new()),
            Arc::new(TracingAudit),
            Arc::clone(ctx.clock()),
            settings.limits(),
            format!("import-{site}"),
        );
        ctx.add(Arc::new(ImportService {
            core: Arc::clone(&core),
            live,
            spool,
        }));
        lock(built()).insert(site, Arc::clone(&core));
        core
    } else {
        match lock(built()).get(&site).cloned() {
            Some(core) => core,
            None => return Ok(()),
        }
    };
    let port: Arc<dyn ImportPort> = core;
    ctx.provide::<dyn ImportPort>(Arc::clone(&port))?;
    // The group is mounted here, not by the route registry (as `services/export.rs` does): the
    // host authenticates every request before the router sees it, and the port authenticates
    // again.
    let mut ports = Ports::new();
    ports.provide::<dyn ImportPort>(port);
    let mounted = routes::mount(&[routes::import::registration()], &ports, &[]);
    let router = mounted.authenticated;
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the import routes were not mounted"
        );
    }
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}
