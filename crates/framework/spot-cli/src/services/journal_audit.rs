//! The scheduled journal audit and the recovery report hosted (WP-RTB-01.T7, ADR-061 sections 8
//! and 9). Operator guide: `docs/running-spot/journal_audit.md`.
//!
//! A journal that has never been replayed is a hope. This module runs the audit on a schedule
//! (weekly, daily for tenants on the `always` replay policy), one pass at a time per host, on the
//! blocking pool; keeps the result rows; raises and clears `JOURNAL_AUDIT_MISMATCH`,
//! `JOURNAL_AUDIT_FAILED` and the amber `RTO_EXCEEDED` through the hosted conditions handle;
//! writes `sys_journal_admin` (`audit_run`); and serves the per-tenant recovery report and the
//! run-now operation to the management API (WP-RTB-04.T1).
//!
//! # What is built here
//!
//! The PostgreSQL parts are `spot-journal-audit-host-pg`'s ([`PgTenantDirectory`],
//! [`PgResultSink`], [`SiteAuditInputs`] with a scratch database per run). This module wires them:
//! the settings, the scratch role's connection (the password comes from an environment variable
//! the settings name, never from a file), the start-up checks (the result table exists, the
//! scratch role holds `CREATEDB` and nothing more), the leak sweep, the conditions, the audit
//! event, and the scheduler.
//!
//! # What it needs from the journal service
//!
//! [`AuditWiring`]: the tenant's id, the [`JournalSource`] (store, keys, upcasters, catalogue,
//! options with segments from the verified manifests, derived entities, fingerprints), the replay
//! policies and the recovery facts. The journal service (WP-RTB-01.T3 and T5) provides it as a
//! port. Without it the journal is not hosted on this site and this module registers nothing.
//!
//! # Level and settings
//!
//! Optional: a site whose audit cannot run still trades, and readiness names the reason
//! (`run spot migrate`, `the scratch role is too powerful`). `[services.journal_audit]`:
//! `enabled`, `scratch_role`, `scratch_password_env`, `max_parallel` (default 1), `check_seconds`
//! (how often the schedule is examined; a tenant that is not due is skipped).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use spot_conditions::{
    AuditEvent, AuditSink, ConditionDef, ConditionEngine, ConditionParams, Millis, Severity,
};
use spot_core::Clock;
use spot_jobs::worker::HandlerResult;
use spot_journal_audit_host_pg::{
    AuditHistorySync, JournalSource, PgAuditHistory, PgResultSink, PgTenantDirectory,
    RecoveryReportService, ReplayPolicies, ReportSources, RunClock, SWEEP_AFTER_MILLIS,
    ScratchFactory, SiteAuditInputs, SyncPg,
};
use spot_journal_audit_job::{
    AuditJob, AuditJobError, JOB_NAME, RunSummary, TenantAuditPlan, TenantDirectory, TenantOutcome,
    register_conditions,
};
use spot_journal_audit_run::AuditRun;
use spot_journal_store::TenantId;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::{PgBackend, PgConfig};
use spot_storage_contract::{
    Catalog, Isolation, Name, NodeRef, Outbox, OutboxEvent, RecordId, StorageBackend, Transaction,
    TxOptions,
};
use tokio::runtime::Handle;
use tokio::sync::{Semaphore, watch};

use crate::services::conditions::{ConditionRegistrations, ConditionsHandle};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the conditions service (1000) and the journal service it reads from.
pub const ORDER: u32 = 1220;

/// The amber condition raised when a rebuild would not meet the tier's recovery time objective.
pub const RTO_EXCEEDED: &str = "RTO_EXCEEDED";
/// The failure condition of this job (registered by `spot-journal-audit-job`).
pub const JOURNAL_AUDIT_FAILED: &str = spot_journal_audit_job::JOURNAL_AUDIT_FAILED;

/// One run-now per site per hour, unless the previous run failed.
pub const RUN_NOW_MIN_GAP_MILLIS: i64 = 60 * 60 * 1000;

/// `[services.journal_audit]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `false` leaves the audit off.
    pub enabled: bool,
    /// The dedicated role that creates and drops scratch databases (`CREATEDB` and nothing else).
    /// The installer creates it; the service only checks it.
    pub scratch_role: String,
    /// The name of the environment variable that holds the scratch role's password. The password
    /// is never in a file, a log or an error.
    pub scratch_password_env: String,
    /// Audits that may run at once on this host (a scratch rebuild is heavy).
    pub max_parallel: u32,
    /// How often the schedule is examined, in seconds.
    pub check_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            scratch_role: "spot_audit_scratch".to_owned(),
            scratch_password_env: "SPOT_AUDIT_SCRATCH_PASSWORD".to_owned(),
            max_parallel: 1,
            check_seconds: 3600,
        }
    }
}

/// What the journal service provides for the audit. A port; its absence means no journal here.
pub trait AuditWiring: Send + Sync + 'static {
    /// The site's tenant id.
    fn tenant(&self) -> TenantId;
    /// Opens one run's journal data.
    fn source(&self) -> Arc<dyn JournalSource>;
    /// Which tenants are on the `always` replay policy (audited daily).
    fn policies(&self) -> Arc<dyn ReplayPolicies>;
    /// The recovery report's sources; `history` is this module's audit history.
    fn report_sources(&self, history: Arc<dyn AuditHistorySync>) -> ReportSources;
}

/// Allows one run-now per site per hour unless the previous run failed.
#[derive(Default)]
pub struct RunNowGate {
    state: Mutex<(Option<i64>, bool)>,
}

impl RunNowGate {
    /// Takes the slot at `now_millis`, or says how many seconds to wait.
    ///
    /// # Errors
    /// The seconds until the next run-now is allowed.
    pub fn try_start(&self, now_millis: i64) -> std::result::Result<(), u64> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let (Some(last), false) = *state {
            let wait = last.saturating_add(RUN_NOW_MIN_GAP_MILLIS) - now_millis;
            if wait > 0 {
                return Err(u64::try_from(wait / 1000).unwrap_or(0).max(1));
            }
        }
        *state = (Some(now_millis), false);
        Ok(())
    }

    /// Records how the run that took the slot ended; a failure frees the next run-now.
    pub fn finished(&self, failed: bool) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).1 = failed;
    }
}

/// Shows every tenant as never audited, so a pass audits the one asked for now.
struct ForceDirectory(Arc<dyn TenantDirectory>);

impl TenantDirectory for ForceDirectory {
    fn tenants(&self) -> std::result::Result<Vec<TenantAuditPlan>, AuditJobError> {
        let mut plans = self.0.tenants()?;
        for plan in &mut plans {
            plan.last_run_millis = None;
        }
        Ok(plans)
    }
}

/// Collects the events the audit's local engine decided, to forward to the hosted conditions.
#[derive(Clone, Default)]
struct CollectSink(Arc<Mutex<Vec<AuditEvent>>>);

impl CollectSink {
    fn drain(&self) -> Vec<AuditEvent> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

impl AuditSink for CollectSink {
    fn record(&mut self, event: AuditEvent) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(event);
    }
}

/// Run ids and time for the scratch databases.
struct SiteRunClock {
    clock: Arc<dyn Clock>,
    counter: AtomicU64,
}

impl RunClock for SiteRunClock {
    fn run_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        format!("r{}n{n}", self.now_millis())
    }

    fn now_millis(&self) -> i64 {
        millis_of(self.clock.as_ref())
    }
}

fn millis_of(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// The audit as the management API uses it: the recovery report and run-now.
pub struct AuditService {
    site: String,
    report: Arc<RecoveryReportService>,
    scheduled: Arc<AuditJob<CollectSink>>,
    forced: Arc<AuditJob<CollectSink>>,
    scheduled_sink: CollectSink,
    forced_sink: CollectSink,
    conditions: Option<Arc<ConditionsHandle>>,
    admin: AdminAudit,
    clock: Arc<dyn Clock>,
    slots: Arc<Semaphore>,
    gate: RunNowGate,
    settings: Settings,
    scratch: Arc<ScratchFactory>,
    tenant: TenantId,
    db: Arc<SyncPg>,
}

/// Why a run-now did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunNowError {
    /// Another site's name.
    WrongSite,
    /// Too soon after the last run; retry after this many seconds.
    RateLimited(u64),
    /// A pass is already running on this host.
    Busy,
    /// The pass could not be completed.
    Failed(&'static str),
}

impl AuditService {
    /// The recovery report of `site`.
    ///
    /// # Errors
    /// The report service's refusal for another site.
    pub async fn report(
        &self,
        site: &str,
    ) -> std::result::Result<spot_journal_recovery::Assessment, spot_journal_audit_host_pg::HostError>
    {
        self.report
            .report(site, millis_of(self.clock.as_ref()))
            .await
    }

    /// Audits the site now. One per site per hour unless the previous run failed.
    ///
    /// # Errors
    /// [`RunNowError`].
    pub async fn run_now(
        self: &Arc<Self>,
        site: &str,
    ) -> std::result::Result<RunSummary, RunNowError> {
        if site != self.site {
            return Err(RunNowError::WrongSite);
        }
        let now = millis_of(self.clock.as_ref());
        self.gate.try_start(now).map_err(RunNowError::RateLimited)?;
        let outcome = self.pass(true).await;
        self.gate.finished(!matches!(&outcome, Ok(summary) if summary.results.iter().all(|r| matches!(r.outcome, TenantOutcome::Clean))));
        outcome
    }

    /// One pass: runs on the blocking pool, then forwards what it decided.
    async fn pass(self: &Arc<Self>, forced: bool) -> std::result::Result<RunSummary, RunNowError> {
        let Ok(permit) = Arc::clone(&self.slots).try_acquire_owned() else {
            return Err(RunNowError::Busy);
        };
        let (job, sink) = if forced {
            (Arc::clone(&self.forced), self.forced_sink.clone())
        } else {
            (Arc::clone(&self.scheduled), self.scheduled_sink.clone())
        };
        let now = millis_of(self.clock.as_ref());
        let exec = Handle::current();
        let joined = tokio::task::spawn_blocking(move || job.run_at(&exec, now)).await;
        drop(permit);
        let summary = match joined {
            Ok(Ok(summary)) => summary,
            Ok(Err(error)) => return Err(RunNowError::Failed(error.code())),
            Err(_) => return Err(RunNowError::Failed("journal_audit_job.pass_aborted")),
        };
        self.forward(&sink, &summary).await;
        Ok(summary)
    }

    async fn forward(&self, sink: &CollectSink, summary: &RunSummary) {
        if let Some(handle) = &self.conditions {
            for event in sink.drain() {
                let sent = match &event {
                    AuditEvent::Raised {
                        id,
                        subject,
                        params,
                        at_millis,
                        ..
                    } => handle
                        .raise(id.as_str(), subject.as_str(), params.clone(), *at_millis)
                        .await
                        .map(|_| ()),
                    AuditEvent::Cleared {
                        id,
                        subject,
                        at_millis,
                        ..
                    } => handle
                        .observe_cleared(id.as_str(), subject.as_str(), *at_millis)
                        .await
                        .map(|_| ()),
                    // The hosted engine escalates on its own tick.
                    AuditEvent::Escalated { .. } => Ok(()),
                };
                if sent.is_err() {
                    tracing::warn!("journal_audit: a condition could not be forwarded");
                }
            }
            for result in &summary.results {
                let at = Millis::new(u64::try_from(result.finished_millis).unwrap_or(0));
                let over = result
                    .run
                    .as_ref()
                    .and_then(AuditRun::report)
                    .and_then(|r| match r.rto {
                        spot_journal_audit::RtoVerdict::Breached { over_by_millis } => {
                            Some(over_by_millis)
                        }
                        spot_journal_audit::RtoVerdict::Within => None,
                    });
                let subject = result.tenant.as_str();
                let _ = match over {
                    Some(over_by) => {
                        let mut params = ConditionParams::new();
                        params.set("over_by_millis", over_by.to_string());
                        handle
                            .raise(RTO_EXCEEDED, subject, params, at)
                            .await
                            .map(|_| ())
                    }
                    None => handle
                        .observe_cleared(RTO_EXCEEDED, subject, at)
                        .await
                        .map(|_| ()),
                };
            }
        }
        for result in &summary.results {
            let (outcome, code) = match result.outcome {
                TenantOutcome::Clean => ("ok", None),
                TenantOutcome::Mismatch => ("failed", Some("journal_audit.mismatch")),
                TenantOutcome::Failed(info) => ("failed", Some(info.code)),
            };
            let position = result
                .verified_position()
                .map(|v| format!("{}:{}", v.position.generation, v.position.seq));
            self.admin.write("audit_run", position, outcome, code).await;
        }
    }
}

/// Writes `sys_journal_admin` (`audit_run`) to the outbox.
struct AdminAudit {
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl AdminAudit {
    async fn write(&self, action: &str, target: Option<String>, outcome: &str, code: Option<&str>) {
        let mut payload = json!({
            "action": action,
            "actor": "system",
            "outcome": outcome,
            "dry_run": false,
        });
        if let Some(map) = payload.as_object_mut() {
            if let Some(target) = target {
                map.insert("target".to_owned(), json!(target));
            }
            if let Some(code) = code {
                map.insert("code".to_owned(), json!(code));
            }
        }
        let written: std::result::Result<(), ()> = async {
            let options = TxOptions::new(
                Name::new("journal_admin_audit").map_err(|_| ())?,
                Isolation::ReadCommitted,
            );
            let mut tx = self
                .backend
                .begin(Arc::clone(&self.catalog), options)
                .await
                .map_err(|_| ())?;
            tx.enqueue(&OutboxEvent {
                id: RecordId::new_v7(),
                topic: Name::new("sys_journal_admin").map_err(|_| ())?,
                subject: NodeRef {
                    entity: Name::new("journal_admin").map_err(|_| ())?,
                    id: RecordId::new_v7(),
                },
                subject_version: None,
                schema_version: 1,
                payload: payload.to_string(),
            })
            .await
            .map_err(|_| ())?;
            tx.commit().await.map_err(|_| ())
        }
        .await;
        if written.is_err() {
            tracing::warn!("journal_audit: the audit record could not be written");
        }
    }
}

/// The hosted service: start-up checks and the schedule.
struct Hosted {
    service: Arc<AuditService>,
}

impl Service for Hosted {
    fn name(&self) -> &'static str {
        "journal_audit"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        let service = Arc::clone(&self.service);
        Box::pin(async move {
            // The calls block on a worker thread: keep them off the executor.
            tokio::task::spawn_blocking(move || {
                service
                    .db
                    .run(|pool| async move {
                        let client = pool
                            .get()
                            .await
                            .map_err(|_| spot_journal_audit_host_pg::HostError::Worker)?;
                        let rows = client
                            .query(
                                "SELECT to_regclass('spot_journal_audit_result') IS NOT NULL",
                                &[],
                            )
                            .await
                            .map_err(|_| {
                                spot_journal_audit_host_pg::HostError::Db("check".to_owned())
                            })?;
                        let present = rows
                            .first()
                            .and_then(|r| r.try_get::<_, bool>(0).ok())
                            .unwrap_or(false);
                        if present {
                            Ok(())
                        } else {
                            Err(spot_journal_audit_host_pg::HostError::Db("missing".to_owned()))
                        }
                    })
                    .map_err(|_| {
                        "the table spot_journal_audit_result is missing or unreachable: run `spot migrate`"
                            .to_owned()
                    })?;
                service.scratch.check_role().map_err(|e| match e {
                    spot_journal_audit_host_pg::HostError::RoleTooPowerful(what) => format!(
                        "journal_audit_host.role_too_powerful: the scratch role holds {what}; it may hold CREATEDB only"
                    ),
                    _ => "the scratch role does not exist or cannot create databases (the installer creates it)".to_owned(),
                })?;
                // A crashed run's leak: sweep what is more than a day old.
                let cutoff = millis_of(service.clock.as_ref()) - SWEEP_AFTER_MILLIS;
                let _ = service.scratch.sweep(service.tenant.as_str(), cutoff);
                Ok(())
            })
            .await
            .unwrap_or_else(|_| Err("the audit start-up check did not finish".to_owned()))
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        let service = Arc::clone(&self.service);
        Box::pin(async move {
            let mut stop = stop;
            let every = Duration::from_secs(service.settings.check_seconds.max(60));
            loop {
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    () = tokio::time::sleep(every) => {}
                }
                // One pass at a time per host; a tenant that is not due is skipped by the job.
                match service.pass(false).await {
                    Ok(_) | Err(RunNowError::Busy) => {}
                    Err(error) => tracing::warn!(?error, "journal_audit: the pass did not finish"),
                }
            }
        })
    }
}

fn built() -> &'static Mutex<HashMap<String, Arc<AuditService>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<AuditService>>>> = OnceLock::new();
    BUILT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn rto_definition() -> ConditionDef {
    ConditionDef::new(
        RTO_EXCEEDED,
        Severity::Amber,
        spot_journal_audit_job::SUBJECT_KIND,
        concat!("sync", ".condition", ".rto_exceeded", ".explanation"),
    )
    .with_param("over_by_millis")
}

fn failed_definition() -> ConditionDef {
    ConditionDef::new(
        JOURNAL_AUDIT_FAILED,
        Severity::Red,
        spot_journal_audit_job::SUBJECT_KIND,
        concat!(
            "sync",
            ".condition",
            ".journal_audit_failed",
            ".explanation"
        ),
    )
    .with_param("stage")
    .with_param("code")
    .with_action("re_verify")
}

fn engine(sink: CollectSink) -> Result<ConditionEngine<CollectSink>> {
    let mut registry = spot_sync_conditions::catalogue().map_err(|e| Error(e.to_string()))?;
    register_conditions(&mut registry).map_err(|e| Error(e.to_string()))?;
    registry
        .register(rto_definition())
        .map_err(|e| Error(e.to_string()))?;
    Ok(ConditionEngine::new(registry, sink))
}

/// Registers the audit when the journal service provided its wiring.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("journal_audit")?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let service = if ctx.assembling() {
        let Some(wiring) = ctx.port::<dyn AuditWiring>() else {
            tracing::debug!("journal_audit: the journal is not hosted on this site");
            return Ok(());
        };
        // The audit's own definitions join the hosted conditions registry.
        if let Some(registrations) = ctx.port::<ConditionRegistrations>() {
            registrations.register(failed_definition());
            registrations.register(rto_definition());
        }
        let tenant = wiring.tenant();
        let site_config: PgConfig = env.loaded.admin.clone().with_dbname(&env.site.database);
        let mut scratch_config = env.loaded.admin.clone().with_dbname("postgres");
        scratch_config.user = settings.scratch_role.clone();
        scratch_config.password = std::env::var(&settings.scratch_password_env).unwrap_or_default();
        // The audit is optional: a worker or a bridge that cannot start leaves it off (and says
        // why), never the site down.
        let (db, scratch_admin) = match (
            SyncPg::start(site_config),
            SyncPg::start(scratch_config.clone()),
        ) {
            (Ok(db), Ok(admin)) => (Arc::new(db), Arc::new(admin)),
            (first, second) => {
                let code = first.err().or(second.err()).map_or("", |e| e.code());
                tracing::warn!(
                    code,
                    "journal_audit: the audit's database workers did not start; the audit is off"
                );
                return Ok(());
            }
        };
        let scratch = Arc::new(ScratchFactory::new(scratch_admin, scratch_config));
        let bridge = match Bridge::for_backend(
            BridgeConfig {
                name: "journal_audit".to_owned(),
                ..BridgeConfig::default()
            },
            env.backend,
        ) {
            Ok(bridge) => Arc::new(bridge),
            Err(_) => {
                tracing::warn!("journal_audit: the bridge did not start; the audit is off");
                return Ok(());
            }
        };
        let clock = Arc::clone(ctx.clock());
        let run_clock: Arc<dyn RunClock> = Arc::new(SiteRunClock {
            clock: Arc::clone(&clock),
            counter: AtomicU64::new(0),
        });
        let inputs = Arc::new(SiteAuditInputs::new(
            tenant.clone(),
            bridge,
            wiring.source(),
            Arc::clone(&scratch),
            run_clock,
        ));
        let directory: Arc<dyn TenantDirectory> = Arc::new(PgTenantDirectory::new(
            Arc::clone(&db),
            vec![tenant.clone()],
            wiring.policies(),
        ));
        let sink_db: Arc<PgResultSink> = Arc::new(PgResultSink::new(Arc::clone(&db)));
        let audit_clock: Arc<dyn spot_journal_audit_job::SyncClock> =
            Arc::new(AuditClock(Arc::clone(&clock)));
        let scheduled_sink = CollectSink::default();
        let forced_sink = CollectSink::default();
        let scheduled = Arc::new(AuditJob::new(
            Arc::clone(&directory),
            Arc::clone(&inputs) as Arc<dyn spot_journal_audit_job::AuditInputs>,
            Arc::clone(&sink_db) as Arc<dyn spot_journal_audit_job::ResultSink>,
            Arc::clone(&audit_clock),
            engine(scheduled_sink.clone())?,
        ));
        let forced = Arc::new(AuditJob::new(
            Arc::new(ForceDirectory(directory)),
            inputs as Arc<dyn spot_journal_audit_job::AuditInputs>,
            sink_db as Arc<dyn spot_journal_audit_job::ResultSink>,
            audit_clock,
            engine(forced_sink.clone())?,
        ));
        let history: Arc<dyn AuditHistorySync> =
            Arc::new(PgAuditHistory::new(Arc::clone(&db), &tenant));
        let report = Arc::new(RecoveryReportService::new(wiring.report_sources(history)));
        let service = Arc::new(AuditService {
            site: site.clone(),
            report,
            scheduled,
            forced,
            scheduled_sink,
            forced_sink,
            conditions: ctx.port::<ConditionsHandle>(),
            admin: AdminAudit {
                backend: env.backend.clone(),
                catalog: Arc::clone(&env.permission.catalog),
            },
            clock,
            slots: Arc::new(Semaphore::new(settings.max_parallel.max(1) as usize)),
            gate: RunNowGate::default(),
            settings,
            scratch,
            tenant,
            db,
        });
        ctx.add(Arc::new(Hosted {
            service: Arc::clone(&service),
        }));
        built()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(site, Arc::clone(&service));
        service
    } else {
        let built = built().lock().unwrap_or_else(|p| p.into_inner());
        let Some(service) = built.get(&site).cloned() else {
            return Ok(());
        };
        service
    };
    // The platform job: a schedule entry or the management API can enqueue it by name.
    let for_job = Arc::clone(&service);
    ctx.job_handlers().register(JOB_NAME, move |_context| {
        let service = Arc::clone(&for_job);
        Box::pin(async move {
            Ok(match service.pass(false).await {
                Ok(_) | Err(RunNowError::Busy) => HandlerResult::Ok,
                Err(RunNowError::Failed(code)) => HandlerResult::Retry(code.to_owned()),
                Err(_) => HandlerResult::Ok,
            })
        })
    });
    ctx.provide::<AuditService>(service)?;
    Ok(())
}

/// The job's clock: the site clock.
struct AuditClock(Arc<dyn Clock>);

impl spot_journal_audit_run::Clock for AuditClock {
    fn now_millis(&self) -> i64 {
        millis_of(self.0.as_ref())
    }
}
