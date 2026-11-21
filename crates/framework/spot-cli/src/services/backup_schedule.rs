//! The scheduled backup service: due slots become deduplicated jobs, a job runs one backup under
//! a lease, failures alert, retention follows every success (WP-PRD-04.T4). Guide:
//! `docs/running-spot/backup_schedule.md`.
//!
//! # What it does
//!
//! `[services.backup_schedule]` (off by default) names a cron schedule, a target of
//! `common.toml` `[backup.targets]` and a keep policy. On every tick the service asks the
//! schedule for the latest due slot of the last 25 hours and submits it as a `spot_backup:backup`
//! job whose idempotency key is the slot's deduplication key: a second tick, a restart or a
//! second node submits the same slot and gets the same job, so one slot is one backup. A node
//! that was down at 02:00 and starts at 07:00 runs the missed backup once, not once per slot.
//!
//! The job handler runs on whichever node the job worker (itself under the jobs lease) hands the
//! job to, and holds the site's `backup` lease while it works, renewing it. A node that loses the
//! lease, or sees the site stop, drops the run: the work directory removes itself, so no partial
//! package is left, and the job is retried later. The package is written to the target only after
//! it was produced and verified, atomically, so an abandoned run never leaves half a file.
//!
//! # Failures
//!
//! A failed run (the target is absent, the key is missing, verification failed) raises a
//! [`spot_backup::BackupAlert`] and a health condition, counts a failure and leaves the previous
//! good backup alone (the runner never deletes the last verified one). After
//! [`DEGRADED_AFTER_FAILURES`] failures in a row [`BackupScheduleService::readiness_text`] says
//! `degraded`; the first success resets it. The service is **optional**: it never stops the site.
//! Retention runs inside every successful run.
//!
//! # Metrics
//!
//! `spot_backup_last_success_timestamp_seconds`, `spot_backup_last_duration_seconds`,
//! `spot_backup_failures_total` and `spot_backup_newest_verified_age_seconds` (set on every tick
//! from the newest backup this process verified; absent until there is one).
//!
//! # Tenancy and secrets
//!
//! The service is built for one site and refuses a job whose arguments name another tenant. The
//! job carries the tenant, the schedule name and the slot, never a path or a credential. No key,
//! password or connection string reaches a log line, an alert or a job's recorded error.

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value as Json, json};
use spot_backup::{
    AlertSink, BACKUP_JOB_NAME, BackupAlert, BackupSchedule, HealthCondition, HealthSeverity,
    HealthSink, RetentionPolicy, slot_instant,
};
use spot_core::Clock;
use spot_jobs::worker::HandlerResult;
use spot_secrets::KeyProvider;
use tokio::sync::watch;

use crate::config::{BackupTarget as ConfiguredTarget, Loaded};
use crate::services::ports::{ConsumerLease, JobSpec, JobSubmit, OutboxLease, PortError};
use crate::services::secrets::SecretsHost;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "backup_schedule/executor.rs"]
pub mod executor;

use executor::{Executor, ExecutorParts, FolderExecutor, Notices, RunError, RunPlan, RunSummary};

/// Start order: after the jobs ports (600), the secrets host (1000) and the processes (1100).
pub const ORDER: u32 = 1120;

/// The service's name in readiness and logs.
pub const NAME: &str = "backup_schedule";

/// The consumer name of the lease a backup run holds (`outbox:<site>:backup`).
pub const LEASE_CONSUMER: &str = "backup";

/// Consecutive failed runs before readiness says `degraded`.
pub const DEGRADED_AFTER_FAILURES: u32 = 3;

/// How far back the latest due slot is looked for.
const LOOKBACK_HOURS: i64 = 25;

/// Metric names.
pub mod metric {
    /// Unix time of the last successful backup.
    pub const LAST_SUCCESS: &str = "spot_backup_last_success_timestamp_seconds";
    /// How long the last run took.
    pub const LAST_DURATION: &str = "spot_backup_last_duration_seconds";
    /// Failed runs since the process started.
    pub const FAILURES_TOTAL: &str = "spot_backup_failures_total";
    /// Age of the newest verified backup this process made.
    pub const NEWEST_VERIFIED_AGE: &str = "spot_backup_newest_verified_age_seconds";
}

/// `[services.backup_schedule.keep]`: overrides of the keep policy of `[backup.retention]`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Keep {
    /// Daily backups to keep.
    pub keep_daily: Option<u32>,
    /// Weekly backups to keep.
    pub keep_weekly: Option<u32>,
    /// Monthly backups to keep.
    pub keep_monthly: Option<u32>,
}

/// `[services.backup_schedule]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the service runs. Off by default.
    pub enabled: bool,
    /// The six-field cron expression of the schedule (`sec min hour day month weekday`).
    pub cron: String,
    /// The IANA zone the cron is read in.
    pub timezone: String,
    /// The target of `[backup.targets]`; absent means `backup.default_target`, or the only one.
    pub target: Option<String>,
    /// `logical`; `stopped` backups need the bundled PostgreSQL layout and are refused.
    pub kind: String,
    /// Keep-policy overrides.
    pub keep: Keep,
    /// Verify every Nth backup in full (restore into a scratch database); `0` never.
    pub verify_full_every: u32,
    /// Accept a site without a key provider and write in clear.
    pub allow_unencrypted: bool,
    /// Seconds between ticks; at least 5.
    pub tick_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            cron: "0 0 2 * * *".to_owned(),
            timezone: "UTC".to_owned(),
            target: None,
            kind: "logical".to_owned(),
            keep: Keep::default(),
            verify_full_every: 0,
            allow_unencrypted: false,
            tick_seconds: 30,
        }
    }
}

impl Settings {
    /// The interval between ticks, never below five seconds.
    pub fn tick(&self) -> Duration {
        Duration::from_secs(self.tick_seconds.max(5))
    }

    /// The schedule the settings describe.
    ///
    /// # Errors
    ///
    /// The key that is wrong, in the words of the setting.
    pub fn schedule(&self) -> std::result::Result<spot_scheduler::Schedule, String> {
        let cron = spot_manifest::Cron::from_str(&self.cron).map_err(|_| {
            "services.backup_schedule.cron is not a valid cron expression".to_owned()
        })?;
        let step = spot_app_api::StepName::new(BACKUP_JOB_NAME)
            .map_err(|_| "the backup step name is invalid".to_owned())?;
        let zone = spot_scheduler::TimeZone::parse(&self.timezone)
            .map_err(|_| "services.backup_schedule.timezone is not a known zone".to_owned())?;
        spot_scheduler::ScheduleBuilder::new("backup")
            .cron(cron)
            .step(step)
            .zone(zone)
            .build()
            .map_err(|_| "services.backup_schedule describes no valid schedule".to_owned())
    }

    /// Refuses what cannot run.
    ///
    /// # Errors
    ///
    /// [`Error`] naming the key.
    pub fn validate(&self) -> Result<()> {
        if self.kind != "logical" {
            return Err(Error(
                "services.backup_schedule.kind must be `logical`; stopped backups are not available in this build".to_owned(),
            ));
        }
        if self.verify_full_every > 10_000 {
            return Err(Error(
                "services.backup_schedule.verify_full_every is too large".to_owned(),
            ));
        }
        self.schedule().map(|_| ()).map_err(Error)
    }
}

#[derive(Default)]
struct State {
    failures: u32,
    failures_total: u64,
    successes: u64,
    aborted: u64,
    last_success: Option<i64>,
    newest_verified: Option<i64>,
    last_error: Option<&'static str>,
    running: bool,
}

/// What a service is built from. The folder executor and the real ports come from `register`; a
/// test gives fakes.
pub struct ServiceParts {
    /// The site.
    pub site: String,
    /// The settings.
    pub settings: Settings,
    /// The schedule (from [`Settings::schedule`]).
    pub schedule: spot_scheduler::Schedule,
    /// Creates the slot's job.
    pub submit: Arc<dyn JobSubmit>,
    /// Holds the run's lease.
    pub outbox: Arc<dyn OutboxLease>,
    /// Takes the backup.
    pub executor: Arc<dyn Executor>,
    /// The site clock.
    pub clock: Arc<dyn Clock>,
    /// How often a run renews its lease and looks at the stop flag.
    pub renew_every: Duration,
}

/// The scheduled backups of one site.
pub struct BackupScheduleService {
    site: String,
    schedule: BackupSchedule,
    settings: Settings,
    submit: Arc<dyn JobSubmit>,
    outbox: Arc<dyn OutboxLease>,
    executor: Arc<dyn Executor>,
    clock: Arc<dyn Clock>,
    renew_every: Duration,
    notices: Notices,
    stopping: AtomicBool,
    state: Mutex<State>,
}

/// Builds the service. Exposed so a test drives it without a `SiteCtx`.
pub fn build(parts: ServiceParts) -> Arc<BackupScheduleService> {
    Arc::new(BackupScheduleService {
        site: parts.site,
        schedule: BackupSchedule::new(parts.schedule),
        settings: parts.settings,
        submit: parts.submit,
        outbox: parts.outbox,
        executor: parts.executor,
        clock: parts.clock,
        renew_every: parts.renew_every,
        notices: Notices::default(),
        stopping: AtomicBool::new(false),
        state: Mutex::new(State::default()),
    })
}

/// Marks the node as not running a backup again, on every way out (a dropped future too).
struct Running<'a>(&'a Mutex<State>);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .running = false;
    }
}

/// How a run ended.
enum Ended {
    Done(std::result::Result<RunSummary, RunError>),
    Lost,
    Stopping,
}

impl BackupScheduleService {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One tick: submits the latest due slot of the last 25 hours as a job (deduplicated by the
    /// slot's key) and refreshes the age gauge. The number of jobs that were new.
    ///
    /// # Errors
    ///
    /// [`PortError`] when the job store or the slot's instant fails.
    pub async fn tick(&self) -> std::result::Result<usize, PortError> {
        let now = self.clock.now_utc();
        self.publish_age(now.unix_timestamp());
        let to = jiff::Timestamp::from_second(now.unix_timestamp())
            .map_err(|_| PortError::Invalid("the clock is out of range"))?;
        let Ok(from) = to.checked_sub(jiff::Span::new().hours(LOOKBACK_HOURS)) else {
            return Ok(0);
        };
        let slots = self.schedule.due_slots(from, to, 500);
        let Some(slot) = slots.last() else {
            return Ok(0);
        };
        let not_before = slot_instant(slot)
            .map_err(|_| PortError::Invalid("the slot instant is out of range"))?;
        let job = JobSpec {
            name: BACKUP_JOB_NAME.to_owned(),
            queue: self.schedule.queue().to_owned(),
            args: json!({
                "tenant": self.site,
                "schedule": self.schedule.schedule().name,
                "scheduled_for": slot.scheduled_for.to_string(),
                "deduplication_key": slot.deduplication_key,
            }),
            idempotency_key: slot.deduplication_key.clone(),
            not_before: Some(not_before),
        };
        let fresh = self.submit.submit(job).await?;
        Ok(usize::from(fresh))
    }

    /// Runs the job `args` describe, under the lease. `Retry` when the lease is another node's, the
    /// run was abandoned or the target may come back; `Fail` when trying again cannot help.
    pub async fn run_job(&self, args: &Json) -> HandlerResult {
        if args.get("tenant").and_then(Json::as_str) != Some(self.site.as_str()) {
            return HandlerResult::Fail(
                "backup.tenant_mismatch: the job names another tenant".to_owned(),
            );
        }
        if self.stopping.load(Ordering::SeqCst) {
            return HandlerResult::Retry("backup.stopping: the site is stopping".to_owned());
        }
        {
            let mut state = self.state();
            if state.running {
                return HandlerResult::Retry(
                    "backup.busy: a backup is already running on this node".to_owned(),
                );
            }
            state.running = true;
        }
        let _running = Running(&self.state);
        let lease = match self.outbox.acquire(LEASE_CONSUMER).await {
            Ok(lease) => lease,
            Err(PortError::LeaseUnavailable) => {
                return HandlerResult::Retry(
                    "backup.lease_held: another node holds the backup lease".to_owned(),
                );
            }
            Err(PortError::Invalid(_)) => {
                return HandlerResult::Fail(
                    "backup.lease_unusable: the lease request is invalid".to_owned(),
                );
            }
            Err(_) => {
                return HandlerResult::Retry(
                    "backup.lease_unavailable: the lease cannot be taken now".to_owned(),
                );
            }
        };
        let scheduled_for: String = args
            .get("scheduled_for")
            .and_then(Json::as_str)
            .unwrap_or("")
            .chars()
            .filter(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '-' | ':' | '.' | '+' | 'T' | 'Z' | '[' | ']' | '/')
            })
            .take(64)
            .collect();
        let plan = RunPlan {
            tenant: self.site.clone(),
            scheduled_for,
            full: self.next_is_full(),
            now: self.clock.now_utc(),
        };
        let started = self.clock.monotonic();
        let (ended, lease) = self.supervise(&plan, lease).await;
        let _ = self.outbox.release(&lease).await;
        let took = self.clock.monotonic().saturating_sub(started);
        match ended {
            Ended::Done(Ok(summary)) => {
                self.succeeded(&summary, took);
                HandlerResult::Ok
            }
            Ended::Done(Err(error)) => {
                self.failed(&plan, &error);
                let text = format!("{}: {}", error.code, error.message);
                if error.transient {
                    HandlerResult::Retry(text)
                } else {
                    HandlerResult::Fail(text)
                }
            }
            Ended::Lost => {
                self.state().aborted += 1;
                self.notices.push("the backup lease was lost; the run was abandoned and its partial package removed".to_owned());
                HandlerResult::Retry("backup.lease_lost: the run was abandoned".to_owned())
            }
            Ended::Stopping => {
                self.state().aborted += 1;
                self.notices.push(
                    "the site is stopping; the run was abandoned and its partial package removed"
                        .to_owned(),
                );
                HandlerResult::Retry("backup.stopping: the run was abandoned".to_owned())
            }
        }
    }

    /// Drives the executor while renewing the lease; the run is dropped (its work directory
    /// removes itself) when the lease is lost or the site stops.
    async fn supervise(&self, plan: &RunPlan, mut lease: ConsumerLease) -> (Ended, ConsumerLease) {
        let ended = {
            let run = self.executor.run(plan, &self.notices);
            tokio::pin!(run);
            let mut ticker = tokio::time::interval(self.renew_every.max(Duration::from_millis(10)));
            ticker.tick().await;
            loop {
                tokio::select! {
                    done = &mut run => break Ended::Done(done),
                    _ = ticker.tick() => {
                        if self.stopping.load(Ordering::SeqCst) {
                            break Ended::Stopping;
                        }
                        match self.outbox.renew(&lease).await {
                            Ok(next) => lease = next,
                            Err(_) => break Ended::Lost,
                        }
                        if !matches!(self.outbox.still_held(&lease).await, Ok(true)) {
                            break Ended::Lost;
                        }
                    }
                }
            }
        };
        (ended, lease)
    }

    fn next_is_full(&self) -> bool {
        let every = u64::from(self.settings.verify_full_every);
        every > 0 && (self.state().successes + 1).is_multiple_of(every)
    }

    fn succeeded(&self, summary: &RunSummary, took: Duration) {
        {
            let mut state = self.state();
            state.failures = 0;
            state.last_error = None;
            state.successes += 1;
            state.last_success = Some(summary.created_at);
            if summary.verified {
                state.newest_verified = Some(summary.created_at);
            }
        }
        metrics::gauge!(metric::LAST_SUCCESS).set(summary.created_at as f64);
        metrics::gauge!(metric::LAST_DURATION).set(took.as_secs_f64());
        tracing::info!(site = %self.site, id = %summary.id, bytes = summary.bytes, pruned = summary.deleted.len(), "scheduled backup done");
    }

    fn failed(&self, plan: &RunPlan, error: &RunError) {
        {
            let mut state = self.state();
            state.failures = state.failures.saturating_add(1);
            state.failures_total += 1;
            state.last_error = Some(error.code);
        }
        metrics::counter!(metric::FAILURES_TOTAL).increment(1);
        self.notices.raise(HealthCondition {
            code: error.code.to_owned(),
            severity: HealthSeverity::Warning,
            message: format!("the scheduled backup failed: {}", error.message),
        });
        self.notices.alert(BackupAlert {
            backup_id: plan.scheduled_for.clone(),
            code: error.code.to_owned(),
            message: format!(
                "the scheduled backup failed and the previous good backup was kept: {}",
                error.message
            ),
        });
    }

    fn publish_age(&self, now: i64) {
        let newest = self.state().newest_verified;
        if let Some(newest) = newest {
            metrics::gauge!(metric::NEWEST_VERIFIED_AGE)
                .set(now.saturating_sub(newest).max(0) as f64);
        }
    }

    /// `backup: ok (...)`, or `backup: degraded (...)` after [`DEGRADED_AFTER_FAILURES`] failed
    /// runs in a row. Never names a path, a key or a record.
    pub fn readiness_text(&self) -> String {
        let state = self.state();
        if state.failures >= DEGRADED_AFTER_FAILURES {
            return format!(
                "backup: degraded ({} runs in a row failed, last code {}; the last good backup is kept)",
                state.failures,
                state.last_error.unwrap_or("unknown")
            );
        }
        match state.last_success {
            Some(at) => format!(
                "backup: ok (last success at unix {at}, {} failures in a row)",
                state.failures
            ),
            None => format!(
                "backup: ok (no backup yet, {} failures in a row)",
                state.failures
            ),
        }
    }

    /// Whether readiness says `degraded`.
    pub fn degraded(&self) -> bool {
        self.state().failures >= DEGRADED_AFTER_FAILURES
    }

    /// Consecutive failed runs.
    pub fn failures(&self) -> u32 {
        self.state().failures
    }

    /// Runs abandoned for a lost lease or a stopping site.
    pub fn aborted(&self) -> u64 {
        self.state().aborted
    }

    /// Successful runs since the process started.
    pub fn successes(&self) -> u64 {
        self.state().successes
    }

    /// The health conditions and alerts raised so far, oldest first.
    pub fn notices(&self) -> Vec<String> {
        self.notices.lines()
    }

    /// The lease port the runs hold their lease through (a test or an operator tool may hold the
    /// lease from outside to keep a node from running).
    pub fn outbox(&self) -> Arc<dyn OutboxLease> {
        Arc::clone(&self.outbox)
    }

    /// Tells a running backup the site is stopping; it is abandoned within one renew interval.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }
}

impl Service for BackupScheduleService {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        // A failing backup is loud, but it never takes the site out of rotation.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move { self.executor.check() })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut ticker = tokio::time::interval(self.settings.tick());
            loop {
                if *stop.borrow() {
                    break;
                }
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(error) = self.tick().await {
                            tracing::warn!(site = %self.site, %error, "the backup schedule could not enqueue its slot");
                            self.notices.push(format!("the backup slot could not be enqueued: {error}"));
                        }
                    }
                    changed = stop.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                }
            }
            self.stop();
            Ok(())
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            self.stop();
            Ok(())
        })
    }
}

/// The folder target of `[backup.targets]` the settings name, as a path, or why it cannot be used.
fn target_path(
    loaded: &Loaded,
    wanted: Option<&str>,
) -> std::result::Result<std::path::PathBuf, String> {
    let (_, target) = loaded
        .common
        .backup
        .target(wanted)
        .map_err(|e| e.to_string())?;
    match target {
        ConfiguredTarget::Folder { path } => Ok(path.clone()),
        _ => Err("backup.unsupported: cloud targets are not available yet (WP-PRD-04.T5); use a folder target".to_owned()),
    }
}

/// Registers the service and its job handler. Inert unless `[services.backup_schedule]` enables
/// it, and on a reload (which keeps the running service).
///
/// # Errors
///
/// A setting that does not decode or describes no schedule; a missing job port.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("backup_schedule")?;
    settings.validate()?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let submit = ctx.require_port::<dyn JobSubmit>()?;
    let outbox = ctx.require_port::<dyn OutboxLease>()?;
    let keys: Option<Arc<dyn KeyProvider>> = ctx.port::<SecretsHost>().map(|host| {
        let provider: Arc<dyn KeyProvider> = host.keyring().clone();
        provider
    });
    let retention = &env.loaded.common.backup.retention;
    let policy = RetentionPolicy {
        keep_daily: settings.keep.keep_daily.unwrap_or(retention.keep_daily),
        keep_weekly: settings.keep.keep_weekly.unwrap_or(retention.keep_weekly),
        keep_monthly: settings.keep.keep_monthly.unwrap_or(retention.keep_monthly),
        protect_newer_than_days: RetentionPolicy::default().protect_newer_than_days,
        keep_last: RetentionPolicy::default().keep_last,
    };
    let mut forbidden = vec![env.root.join("sites")];
    forbidden.extend(env.loaded.common.apps.iter().map(|app| env.root.join(app)));
    if let Some(pgdata) = std::env::var_os("PGDATA").filter(|v| !v.is_empty()) {
        forbidden.push(std::path::PathBuf::from(pgdata));
    }
    let schedule = settings.schedule().map_err(Error)?;
    let executor = FolderExecutor::new(ExecutorParts {
        tenant: env.site.site.clone(),
        database: env.site.database.clone(),
        apps: env.site.apps.clone(),
        cfg: env.loaded.admin.clone().with_dbname(&env.site.database),
        backend: env.backend.clone(),
        target: target_path(env.loaded, settings.target.as_deref()),
        forbidden,
        schedule: schedule.clone(),
        retention: policy,
        keys,
        allow_unencrypted: settings.allow_unencrypted,
        bin: None,
        clock: Arc::clone(ctx.clock()),
    });
    let service = build(ServiceParts {
        site: env.site.site.clone(),
        settings,
        schedule,
        submit,
        outbox,
        executor: Arc::new(executor),
        clock: Arc::clone(ctx.clock()),
        renew_every: Duration::from_secs(5),
    });
    let handler = Arc::clone(&service);
    ctx.job_handlers().register(BACKUP_JOB_NAME, move |job| {
        let service = Arc::clone(&handler);
        Box::pin(async move { Ok(service.run_job(&job.job.args).await) })
    });
    ctx.provide::<BackupScheduleService>(Arc::clone(&service))?;
    ctx.add(service);
    Ok(())
}
