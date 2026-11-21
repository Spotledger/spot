//! The edge invariant on a real site (WP-DAG-04.T6): the edges of a document graph equal its link
//! fields, and an operator can see and fix it when they do not.
//!
//! `spot-edge-pg` owns the scan ([`EdgeVerifyJob`]), the drift report tables and the audited
//! repair ([`EdgeRepair`]). This module is the host around them:
//!
//! - **the job**: `platform:edge_verify` is registered as a job handler on every site (the
//!   platform app schedules it weekly, Sunday 03:30 UTC, queue `long`). Each run builds the scan
//!   from the metadata the site serves *now*, so a reload or a custom field changes what is
//!   checked. The report key is `scheduled-<UTC date>-<job id>` unless the job's arguments carry
//!   `report_key`. A coordinator lease `edge_verify:<site>` keeps two runs from overlapping: the
//!   second skips with a debug line. A scan that cannot run is a `Retry` with the stable code;
//!   nothing here panics;
//! - **what it publishes**: after each run the gauges `spot_edge_drift_entries{kind}` (`missing`,
//!   `extra`, `changed`), `spot_edge_verify_last_run_seconds` and
//!   `spot_edge_verify_documents_checked`, and the counter `spot_edge_verify_runs_total{result}`
//!   (`clean`, `drift`, `error`); every series carries the site. A drift logs one `warn` with the
//!   counts and the report key, never a document id or a field value. [`EdgeIntegrity`] keeps the
//!   readiness line: `edges: clean at <time>` or `edges: <n> drift entries (report <key>)`. A
//!   drift is a data finding, not an outage: it never fails readiness;
//! - **the commands**: `spot edges verify` and `spot edges repair` (the command file only parses
//!   and prints) use [`scan`], [`load_report`] and [`dry_run`] here, so the job and the commands
//!   cannot disagree.
//!
//! The scan never changes an edge: it writes only its own report tables. A repair changes only
//! the edges a named report lists, in batches, each with one `spot_edge_repair_audit` row.
//!
//! # Settings
//!
//! `[services.edge_integrity]`: `enabled` (default true) and `verify_page_size` (documents per
//! page, default 500, 1 to 10 000). Unknown keys are refused.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use spot_coordinator::{Coordinator, error::LEASE_HELD};
use spot_core::Clock;
use spot_edge_pg::{
    DriftEntry, DriftKind, DriftReport, EdgePgError, EdgeRepair, EdgeVerifyJob, RepairReport,
    read_report,
};
use spot_edge_sync::{EdgeChanges, EdgeSchema};
use spot_jobs::worker::{HandlerResult, JobContext};
use spot_meta::EffectiveMeta;
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{Catalog, Isolation, Name, StorageBackend, Transaction, TxOptions};
use time::OffsetDateTime;
use tokio::sync::watch;

use crate::live::SiteLive;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the framework's own services (they use 100 to 900) and the function service.
pub const ORDER: u32 = 1030;

/// The job name a schedule enqueues.
pub const JOB: &str = "platform:edge_verify";

/// How many entries a command prints before it says how many more there are.
pub const PRINT_LIMIT: usize = 50;

/// A report older than this is refused by a repair: the data has moved on.
pub const REPORT_MAX_AGE: time::Duration = time::Duration::days(7);

/// How long the overlap lease lives between renewals.
const LEASE_TTL: Duration = Duration::from_secs(120);

/// `[services.edge_integrity]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the job handler is registered.
    pub enabled: bool,
    /// Documents per scan page (1 to 10 000).
    pub verify_page_size: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            verify_page_size: spot_edge_pg::DEFAULT_PAGE_SIZE,
        }
    }
}

impl Settings {
    /// Refuses a page size outside 1 to 10 000, naming the key.
    ///
    /// # Errors
    ///
    /// The page size is out of range.
    pub fn validate(&self) -> Result<()> {
        if !(1..=10_000).contains(&self.verify_page_size) {
            return Err(Error(
                "services.edge_integrity.verify_page_size: must be 1 to 10000".to_owned(),
            ));
        }
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------------------------
// What the scan found, for people and for metrics
// ---------------------------------------------------------------------------------------------

/// The label of a drift kind: the report calls a property mismatch `mismatch`, the metric and the
/// commands call it `changed`.
#[must_use]
pub fn kind_label(kind: DriftKind) -> &'static str {
    match kind {
        DriftKind::Missing => "missing",
        DriftKind::Extra => "extra",
        DriftKind::Mismatch => "changed",
    }
}

/// How many entries of each kind a report holds: `(missing, extra, changed)`.
#[must_use]
pub fn counts(report: &DriftReport) -> (u64, u64, u64) {
    let mut out = (0, 0, 0);
    for entry in &report.entries {
        match entry.kind {
            DriftKind::Missing => out.0 += 1,
            DriftKind::Extra => out.1 += 1,
            DriftKind::Mismatch => out.2 += 1,
        }
    }
    out
}

/// One line for one entry: `<kind> <doctype> <record id> <edge type>`. Identifiers only: an edge
/// row carries record ids and an edge type, never a field value.
#[must_use]
pub fn entry_line(entry: &DriftEntry) -> String {
    format!(
        "{} {} {} {}",
        kind_label(entry.kind),
        entry.owner.entity.as_str(),
        entry.owner.id,
        entry.edge_type
    )
}

/// The first [`PRINT_LIMIT`] lines of a report and how many entries were left out.
#[must_use]
pub fn lines(report: &DriftReport) -> (Vec<String>, usize) {
    let shown: Vec<String> = report
        .entries
        .iter()
        .take(PRINT_LIMIT)
        .map(entry_line)
        .collect();
    let more = report.entries.len().saturating_sub(shown.len());
    (shown, more)
}

/// The report key a run makes for itself: `<prefix>-<UTC date>-<run id>`.
#[must_use]
pub fn report_key(prefix: &str, now: OffsetDateTime, run: &str) -> String {
    format!(
        "{prefix}-{:04}-{:02}-{:02}-{run}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

// ---------------------------------------------------------------------------------------------
// Scanning and repairing, shared by the job and the commands
// ---------------------------------------------------------------------------------------------

/// Runs the scan over `meta` now and writes the report under `key`.
///
/// # Errors
///
/// The catalogue cannot be derived from the metadata, or the scan fails (`edge_pg.unavailable`,
/// `edge_pg.write_failed`: tables missing means `spot migrate` has not run).
pub async fn scan(
    backend: &PgBackend,
    meta: &EffectiveMeta,
    now: OffsetDateTime,
    key: &str,
    page: u32,
) -> std::result::Result<DriftReport, EdgePgError> {
    let catalog =
        Arc::new(crate::schema::catalog(meta).map_err(|e| EdgePgError::write(e.to_string()))?);
    EdgeVerifyJob::new(backend.clone(), catalog, EdgeSchema::derive(meta, &[]))
        .with_page_size(page)
        .run(now, key)
        .await
}

async fn read_tx(
    backend: &PgBackend,
    catalog: Arc<Catalog>,
) -> std::result::Result<PgTransaction, EdgePgError> {
    let label = Name::new("edge_report_read")
        .map_err(|_| EdgePgError::write("the transaction label is not a valid name"))?;
    backend
        .begin(
            catalog,
            TxOptions::new(label, Isolation::ReadCommitted).read_only(),
        )
        .await
        .map_err(|e| EdgePgError::Unavailable {
            message: e.explanation,
        })
}

/// Reads the report `key` for a repair, refusing one that is older than [`REPORT_MAX_AGE`] at
/// `now`. Changes nothing.
///
/// # Errors
///
/// [`LoadError::NotFound`] for an unknown key, [`LoadError::Stale`] for an old one,
/// [`LoadError::Failed`] when the store cannot be read.
pub async fn load_report(
    backend: &PgBackend,
    meta: &EffectiveMeta,
    key: &str,
    now: OffsetDateTime,
) -> std::result::Result<DriftReport, LoadError> {
    let catalog = Arc::new(
        crate::schema::catalog(meta)
            .map_err(|e| LoadError::Failed(EdgePgError::write(e.to_string())))?,
    );
    let mut tx = read_tx(backend, catalog).await.map_err(LoadError::Failed)?;
    let read = read_report(&mut tx, key).await;
    let _ = tx.rollback().await;
    match read {
        Ok(report) if now - report.generated_at > REPORT_MAX_AGE => Err(LoadError::Stale {
            generated_at: report.generated_at,
        }),
        Ok(report) => Ok(report),
        Err(EdgePgError::ReportNotFound { .. }) => Err(LoadError::NotFound),
        Err(other) => Err(LoadError::Failed(other)),
    }
}

/// Why a report could not be used for a repair.
#[derive(Debug)]
pub enum LoadError {
    /// No report has this key.
    NotFound,
    /// The report is older than [`REPORT_MAX_AGE`]: run `verify` again.
    Stale {
        /// When it was generated.
        generated_at: OffsetDateTime,
    },
    /// The store could not be read.
    Failed(EdgePgError),
}

/// What a repair of `report` would change: counts of `(added, removed, updated)` edges. Writes
/// nothing; it is the dry run.
///
/// # Errors
///
/// A stored entry that cannot be turned into an edge.
pub fn dry_run(report: &DriftReport) -> std::result::Result<(usize, usize, usize), EdgePgError> {
    let EdgeChanges {
        added,
        removed,
        updated,
    } = report.changes()?;
    Ok((added.len(), removed.len(), updated.len()))
}

/// Applies the report `key` in batches of `batch`, one audit row per batch.
///
/// # Errors
///
/// As [`EdgeRepair::repair`]. Batches committed before a failure stay, each with its audit row.
pub async fn repair(
    backend: &PgBackend,
    meta: &EffectiveMeta,
    key: &str,
    batch: usize,
    now: OffsetDateTime,
) -> std::result::Result<RepairReport, EdgePgError> {
    let catalog =
        Arc::new(crate::schema::catalog(meta).map_err(|e| EdgePgError::write(e.to_string()))?);
    EdgeRepair::new(backend.clone(), catalog)
        .with_batch_size(batch)
        .repair(now, key)
        .await
}

// ---------------------------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------------------------

const DRIFT_ENTRIES: &str = "spot_edge_drift_entries";
const LAST_RUN_SECONDS: &str = "spot_edge_verify_last_run_seconds";
const DOCUMENTS_CHECKED: &str = "spot_edge_verify_documents_checked";
const RUNS_TOTAL: &str = "spot_edge_verify_runs_total";

/// Publishes one finished scan.
fn publish(site: &str, report: &DriftReport, finished: OffsetDateTime) {
    let (missing, extra, changed) = counts(report);
    for (kind, n) in [("missing", missing), ("extra", extra), ("changed", changed)] {
        ::metrics::gauge!(DRIFT_ENTRIES, "site" => site.to_owned(), "kind" => kind).set(n as f64);
    }
    ::metrics::gauge!(LAST_RUN_SECONDS, "site" => site.to_owned())
        .set(finished.unix_timestamp() as f64);
    ::metrics::gauge!(DOCUMENTS_CHECKED, "site" => site.to_owned())
        .set(report.checked_documents as f64);
    let result = if report.is_clean() { "clean" } else { "drift" };
    ::metrics::counter!(RUNS_TOTAL, "site" => site.to_owned(), "result" => result).increment(1);
}

/// Counts a run that did not start because another run of this site holds the lease
/// (`spot_edge_verify_runs_total{result="skipped"}`), so overlap is visible and not only a debug
/// line.
pub fn publish_skipped(site: &str) {
    ::metrics::counter!(RUNS_TOTAL, "site" => site.to_owned(), "result" => "skipped").increment(1);
}

fn publish_error(site: &str) {
    ::metrics::counter!(RUNS_TOTAL, "site" => site.to_owned(), "result" => "error").increment(1);
}

// ---------------------------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------------------------

/// What the last finished scan found, for the readiness line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Last {
    #[default]
    Never,
    Clean(OffsetDateTime),
    Drift {
        entries: usize,
        key: String,
    },
}

/// The edge-integrity service of one site: the job's runner and the readiness line.
pub struct EdgeIntegrity {
    site: String,
    live: Arc<SiteLive>,
    backend: PgBackend,
    coordinator: Arc<dyn Coordinator>,
    clock: Arc<dyn Clock>,
    holder: String,
    page: u32,
    last: Mutex<Last>,
}

impl EdgeIntegrity {
    /// `edges: clean at <time>`, `edges: <n> drift entries (report <key>)`, or `edges: not
    /// checked yet`. A drift never makes the site not ready.
    #[must_use]
    pub fn readiness_text(&self) -> String {
        match &*lock(&self.last) {
            Last::Never => "edges: not checked yet".to_owned(),
            Last::Clean(at) => format!("edges: clean at {at}"),
            Last::Drift { entries, key } => {
                format!("edges: {entries} drift entries (report {key})")
            }
        }
    }

    /// Runs one `edge_verify` job.
    pub async fn run_job(&self, job: JobContext) -> HandlerResult {
        let lease = match self
            .coordinator
            .acquire(
                &format!("edge_verify:{}", self.site),
                &self.holder,
                LEASE_TTL,
            )
            .await
        {
            Ok(lease) => lease,
            Err(e) if e.code == LEASE_HELD => {
                tracing::debug!(site = %self.site, "edge_verify is already running here or on another node; skipped");
                publish_skipped(&self.site);
                return HandlerResult::Ok;
            }
            Err(e) => return HandlerResult::Retry(e.code.to_owned()),
        };
        // Keep the lease alive while a long scan runs; it stops when the scan is done.
        let (done, stop) = watch::channel(false);
        let keeper = self.keep(lease.clone(), stop);
        let outcome = self.scan_now(&job).await;
        let _ = done.send(true);
        keeper.await;
        let _ = self.coordinator.release(&lease).await;
        outcome
    }

    async fn keep(&self, lease: spot_coordinator::Lease, mut stop: watch::Receiver<bool>) {
        let mut lease = lease;
        loop {
            tokio::select! {
                _ = stop.changed() => return,
                () = tokio::time::sleep(LEASE_TTL / 3) => {
                    match self.coordinator.renew(&lease, LEASE_TTL).await {
                        Ok(renewed) => lease = renewed,
                        Err(e) => {
                            tracing::warn!(site = %self.site, code = e.code, "the edge_verify lease could not be renewed");
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn scan_now(&self, job: &JobContext) -> HandlerResult {
        let Some(served) = self.live.served() else {
            publish_error(&self.site);
            return HandlerResult::Retry("edge_verify.no_metadata".to_owned());
        };
        let key = job
            .job
            .args
            .get("report_key")
            .and_then(|v| v.as_str())
            .map_or_else(
                || report_key("scheduled", job.clock, &job.job.id.to_string()),
                str::to_owned,
            );
        match scan(&self.backend, &served.meta, job.clock, &key, self.page).await {
            Ok(report) => {
                let finished = self.clock.now_utc();
                publish(&self.site, &report, finished);
                if report.is_clean() {
                    *lock(&self.last) = Last::Clean(finished);
                } else {
                    let (missing, extra, changed) = counts(&report);
                    tracing::warn!(
                        site = %self.site,
                        report = %key,
                        missing,
                        extra,
                        changed,
                        "the edge set differs from the link fields: see `spot edges verify`"
                    );
                    *lock(&self.last) = Last::Drift {
                        entries: report.entries.len(),
                        key,
                    };
                }
                HandlerResult::Ok
            }
            Err(error) => {
                publish_error(&self.site);
                tracing::warn!(site = %self.site, code = error.code(), "edge_verify could not run");
                HandlerResult::Retry(error.code().to_owned())
            }
        }
    }
}

/// The service that keeps the module in the site's readiness listing. It has no loop of its own.
struct EdgeService {
    inner: Arc<EdgeIntegrity>,
}

impl Service for EdgeService {
    fn name(&self) -> &'static str {
        "edge_integrity"
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
            let _ = stop.changed().await;
            let _ = &self.inner;
            Ok(())
        })
    }
}

/// The runners built so far, by site: a reload builds the site again without building the service
/// again, and the registered handler must keep reaching the same one.
fn built() -> MutexGuard<'static, HashMap<String, Arc<EdgeIntegrity>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<EdgeIntegrity>>>> = OnceLock::new();
    lock(BUILT.get_or_init(Mutex::default))
}

/// Registers the `platform:edge_verify` job handler and the readiness line.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("edge_integrity")?;
    settings.validate()?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let runner = if ctx.assembling() {
        let clock = Arc::clone(ctx.clock());
        let holder = format!("edge-verify-{site}");
        let coordinator: Arc<dyn Coordinator> = if env.loaded.common.services.profile == "cluster" {
            // The jobs module (a smaller ORDER) prepared the coordinator's tables.
            Arc::new(spot_coordinator_pg::PgCoordinator::new(
                env.backend.pool(),
                spot_coordinator_pg::PgCoordinatorConfig::default(),
            ))
        } else {
            let node = spot_coordinator::NodeId::new(holder.clone()).map_err(|e| {
                Error(format!(
                    "services.edge_integrity: the node name: {}",
                    e.code
                ))
            })?;
            spot_coordinator::InProcessCoordinator::start(
                Arc::new(spot_coordinator::InMemoryPresence::new()),
                node,
                Arc::clone(&clock),
            )
            .await
            .map_err(|e| {
                Error(format!(
                    "services.edge_integrity: the coordinator: {}",
                    e.code
                ))
            })?
        };
        let runner = Arc::new(EdgeIntegrity {
            site: site.clone(),
            live: Arc::clone(env.live),
            backend: env.backend.clone(),
            coordinator,
            clock,
            holder,
            page: settings.verify_page_size,
            last: Mutex::new(Last::Never),
        });
        built().insert(site.clone(), Arc::clone(&runner));
        ctx.add(Arc::new(EdgeService {
            inner: Arc::clone(&runner),
        }));
        ctx.provide(Arc::clone(&runner))?;
        runner
    } else {
        match built().get(&site).cloned() {
            Some(runner) => runner,
            None => return Ok(()),
        }
    };
    ctx.job_handlers().register(JOB, move |job| {
        let runner = Arc::clone(&runner);
        Box::pin(async move { Ok(runner.run_job(job).await) })
    });
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn the_settings_default_and_refuse_a_page_of_zero_or_a_stranger_key() {
        let d = Settings::default();
        assert_eq!((d.enabled, d.verify_page_size), (true, 500));
        d.validate().unwrap();
        let zero: Settings = toml::from_str("verify_page_size = 0").unwrap();
        assert!(
            zero.validate()
                .unwrap_err()
                .to_string()
                .contains("verify_page_size")
        );
        assert!(toml::from_str::<Settings>("verify_page = 5").is_err());
    }

    #[test]
    fn a_report_key_carries_the_utc_date_and_the_run() {
        let now = datetime!(2026-11-15 03:30 UTC);
        assert_eq!(
            report_key("scheduled", now, "42"),
            "scheduled-2026-11-15-42"
        );
        assert_eq!(report_key("manual", now, "ab"), "manual-2026-11-15-ab");
    }

    #[test]
    fn the_kind_labels_are_the_metric_labels() {
        assert_eq!(kind_label(DriftKind::Missing), "missing");
        assert_eq!(kind_label(DriftKind::Extra), "extra");
        assert_eq!(kind_label(DriftKind::Mismatch), "changed");
    }

    #[test]
    fn an_empty_report_is_clean_and_prints_nothing() {
        let report = DriftReport::new("k", datetime!(2026-11-15 03:30 UTC));
        assert_eq!(counts(&report), (0, 0, 0));
        assert_eq!(lines(&report), (Vec::new(), 0));
        assert_eq!(dry_run(&report).unwrap(), (0, 0, 0));
    }
}
