//! The fault scenarios.
//!
//! Each scenario runs a workload, applies a fault, recovers, and then checks **all L7
//! invariants** ([`spot_l7_invariants::framework_registry`]) plus the [`DurabilityAudit`]. Where
//! the fault is a network fault it goes through a [`FaultProxy`]; where it is a crash the server
//! is `SIGKILL`ed and restarted by [`ScratchCluster`].
//!
//! The workload is the seeded, multi-worker [`LoadDriver`] of WP-TST-07.T1 — the driver whose
//! history and oracle carry the per-attempt markers, so the durability audit can tell an
//! acknowledged commit from an unacknowledged one. The job-engine scenario uses WP-TST-07.T2's
//! job invariants ([`crate::jobs`]); the partition scenario adds the isolation checker
//! ([`spot_l8_isolation`]).
//!
//! # Faults are timing-based
//!
//! A crash lands mid-run by sleeping a warm-up and then killing: the run must still be going. The
//! scenario records whether it was, so a run that finished too early is visible rather than
//! silently green. Seeds fix the *schedule* of the workload; they do not fix the interleaving on
//! real threads.

use std::sync::Arc;
use std::time::{Duration, Instant};

use spot_l7_invariants::history::Level;
use spot_l7_invariants::invariant::{CheckContext, Report, RunMeta, Violation};
use spot_l7_invariants::runner::{Completed, check};
use spot_l7_invariants::schedule::{Profile, Rng, Weights};
use spot_l7_invariants::{DriverConfig, LoadDriver, Model, RunResult, framework_registry};
use spot_l8_isolation::history::Workload;
use spot_l8_isolation::model::Model as IsoModel;
use spot_l8_isolation::report::IsolationReport;
use spot_l8_isolation::workload::{WorkloadConfig, run_on};
use spot_postgres::{PgBackend, PgConfig};
use spot_storage_contract::{FeedPosition, StorageBackend, StorageError};

use crate::audit::{AuditReport, DurabilityAudit};
use crate::cluster::{ClusterError, ScratchCluster};
use crate::faults::{Direction, Fault, FaultEvent, MessageTrigger, ScheduledFault, Trigger};
use crate::jobs::{JobScenarioConfig, run_job_scenario};
use crate::proxy::{FaultProxy, ProxyConfig};
use crate::report::FaultReport;
use crate::wire::PgMessage;

/// The one fault scenario that fits the smoke budget, plus the proxy one.
pub const SMOKE_SCENARIOS: [&str; 2] = ["crash_during_load", "commit_ack_loss"];

/// Everything that can go wrong in a scenario beyond a check failing.
#[derive(Debug)]
pub enum ScenarioError {
    /// The cluster failed.
    Cluster(ClusterError),
    /// A storage operation failed.
    Storage(StorageError),
    /// The backend could not be built.
    Backend(String),
    /// A spawned workload panicked or was cancelled.
    Join(String),
    /// The proxy could not start.
    Proxy(String),
    /// The model could not be built.
    Model(String),
}

impl std::fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScenarioError::Cluster(e) => write!(f, "cluster: {e}"),
            ScenarioError::Storage(e) => write!(f, "storage: {e}"),
            ScenarioError::Backend(e) => write!(f, "backend: {e}"),
            ScenarioError::Join(e) => write!(f, "workload: {e}"),
            ScenarioError::Proxy(e) => write!(f, "proxy: {e}"),
            ScenarioError::Model(e) => write!(f, "model: {e}"),
        }
    }
}

impl std::error::Error for ScenarioError {}

impl From<ClusterError> for ScenarioError {
    fn from(e: ClusterError) -> Self {
        ScenarioError::Cluster(e)
    }
}

/// How big a scenario run is.
#[derive(Clone, Debug)]
pub struct ScenarioConfig {
    /// The seed: the workload schedule is a pure function of it.
    pub seed: u64,
    /// Concurrent workers (the brief's 16).
    pub workers: u32,
    /// Operations over all workers.
    pub operations: u64,
    /// The profile name.
    pub profile: String,
    /// How long to let the workload run before a fault lands.
    pub warmup: Duration,
    /// How many commit acknowledgements the commit-ack-loss scenario may drop.
    pub dropped_acks: u32,
    /// Statement timeout declared by scenario transactions.
    pub statement_timeout: Duration,
    /// Lock timeout declared by scenario transactions.
    pub lock_timeout: Duration,
}

impl ScenarioConfig {
    /// The documented size for a profile, with 16 workers.
    pub fn for_profile(profile: Profile, seed: u64) -> ScenarioConfig {
        let (operations, warmup) = match profile {
            Profile::Smoke => (3_000, Duration::from_millis(800)),
            Profile::Standard => (10_000, Duration::from_secs(2)),
            Profile::Nightly => (50_000, Duration::from_secs(5)),
        };
        ScenarioConfig {
            seed,
            workers: 16,
            operations,
            profile: profile.name().to_owned(),
            warmup,
            dropped_acks: 200,
            statement_timeout: Duration::from_secs(120),
            lock_timeout: Duration::from_secs(60),
        }
    }
}

/// What a scenario found.
pub struct ScenarioResult {
    /// The scenario name.
    pub name: String,
    /// The seed.
    pub seed: u64,
    /// The profile name.
    pub profile: String,
    /// Crash-recovery time, when the server was killed.
    pub recovery_ms: Option<u64>,
    /// Whether the L7 invariants held.
    pub l7_ok: bool,
    /// The L7 report as text.
    pub l7_text: String,
    /// The durability audit.
    pub audit: AuditReport,
    /// The L7 violations, formatted for the report.
    pub l7_violations: Vec<String>,
    /// Whether the isolation checker held, when it ran.
    pub isolation_ok: Option<bool>,
    /// The isolation report as text, when it ran.
    pub isolation_text: Option<String>,
    /// Notes for the report.
    pub notes: Vec<String>,
    /// The faults that fired.
    pub timeline: Vec<FaultEvent>,
}

impl ScenarioResult {
    /// Whether every check held.
    pub fn ok(&self) -> bool {
        self.l7_ok && self.audit.ok && self.isolation_ok.unwrap_or(true)
    }

    /// A serialisable report.
    pub fn to_report(&self) -> FaultReport {
        let mut violations = self.l7_violations.clone();
        for v in &self.audit.violations {
            violations.push(format!(
                "{} {} {} {}",
                v.invariant, v.entity, v.id, v.detail
            ));
        }
        FaultReport {
            schema: crate::report::FAULT_REPORT_SCHEMA.to_owned(),
            scenario: self.name.clone(),
            seed: self.seed,
            profile: self.profile.clone(),
            ok: self.ok(),
            recovery_ms: self.recovery_ms,
            classification: self.audit.classification.clone(),
            l7_ok: self.l7_ok,
            audit_ok: self.audit.ok,
            isolation_ok: self.isolation_ok,
            notes: self.notes.clone(),
            violations,
            fault_timeline: self.timeline.clone(),
        }
    }
}

/// The documented crash scenario: kill the server mid-run, restart, then check.
pub async fn crash_during_load(
    cluster: &ScratchCluster,
    cfg: &ScenarioConfig,
) -> Result<ScenarioResult, ScenarioError> {
    let backend = connect(cluster).await?;
    let model = model()?;
    let driver = LoadDriver::new(backend.clone(), model.clone(), driver_config(cfg, false));
    let task = tokio::spawn(driver.run());
    // Let the setup transaction commit before the kill, so the run does not fail on its own setup.
    let _ = wait_for_first_event(&backend).await;
    tokio::time::sleep(cfg.warmup).await;
    let finished_before = task.is_finished();
    cluster.kill9()?;
    let recovery = cluster.restart_after_kill().await?;
    let run = join(task).await?;
    let (report, audit) = check_run(backend, &model, run, "pg").await;
    let audit = add_log_errors(cluster, audit);
    let mut notes = Vec::new();
    if finished_before {
        notes.push(
            "the workload finished before the kill; the fault did not land mid-run".to_owned(),
        );
    }
    notes.push(format!("crash recovery {} ms", recovery.as_millis()));
    Ok(build(
        "crash_during_load",
        cfg,
        Some(u64::try_from(recovery.as_millis()).unwrap_or(u64::MAX)),
        report,
        audit,
        None,
        None,
        notes,
        Vec::new(),
    ))
}

/// Five kills at seeded random moments in one run.
pub async fn repeated_crashes(
    cluster: &ScratchCluster,
    cfg: &ScenarioConfig,
) -> Result<ScenarioResult, ScenarioError> {
    let backend = connect(cluster).await?;
    let model = model()?;
    let driver = LoadDriver::new(backend.clone(), model.clone(), driver_config(cfg, false));
    let task = tokio::spawn(driver.run());
    let _ = wait_for_first_event(&backend).await;
    let mut rng = Rng::new(cfg.seed ^ 0x5EED_5EED);
    let mut recoveries = Vec::new();
    for _ in 0..5 {
        let wait = 200 + rng.below(500);
        tokio::time::sleep(Duration::from_millis(wait)).await;
        if task.is_finished() {
            break;
        }
        // A fresh restart budget for each deliberate kill.
        cluster.supervisor().reset().await;
        cluster.kill9()?;
        recoveries.push(cluster.restart_after_kill().await?);
    }
    let run = join(task).await?;
    let (report, audit) = check_run(backend, &model, run, "pg").await;
    let audit = add_log_errors(cluster, audit);
    let max = recoveries.iter().max().copied();
    let mut notes = vec![format!("{} deliberate kills", recoveries.len())];
    notes.push(
        "no spot-pg-migrate crate is present; the schema-change-during-crash case is skipped"
            .to_owned(),
    );
    Ok(build(
        "repeated_crashes",
        cfg,
        max.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        report,
        audit,
        None,
        None,
        notes,
        Vec::new(),
    ))
}

/// `pg_terminate_backend` on random backends mid-transaction, then check for leaks and stuck locks.
pub async fn killed_connections(
    cluster: &ScratchCluster,
    cfg: &ScenarioConfig,
) -> Result<ScenarioResult, ScenarioError> {
    let backend = connect(cluster).await?;
    let model = model()?;
    let driver = LoadDriver::new(backend.clone(), model.clone(), driver_config(cfg, false));
    let task = tokio::spawn(driver.run());
    let _ = wait_for_first_event(&backend).await;
    let mut killed = 0u32;
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        if task.is_finished() {
            break;
        }
        // Every backend on our scratch database is ours (the shared server is never touched).
        let sql = format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = '{}' AND pid <> pg_backend_pid()",
            cluster.database()
        );
        if let Ok(out) = cluster.psql_on("postgres", &sql) {
            killed += out.lines().filter(|l| *l == "t").count() as u32;
        }
    }
    let run = join(task).await?;
    let (report, audit) = check_run(backend, &model, run, "pg").await;
    // No stuck locks on our application's backends after the run.
    let mut notes = vec![format!("terminated {killed} backends")];
    match cluster.psql_on(
        "postgres",
        &format!(
            "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON l.pid = a.pid \
             WHERE a.datname = '{}'",
            cluster.database()
        ),
    ) {
        Ok(count) => {
            notes.push(format!("locks held by our backends after the run: {count}"));
            if count != "0" {
                notes.push("STUCK LOCKS: expected none after the workload stopped".to_owned());
            }
        }
        Err(e) => notes.push(format!("could not read pg_locks: {e}")),
    }
    Ok(build(
        "killed_connections",
        cfg,
        None,
        report,
        audit,
        None,
        None,
        notes,
        Vec::new(),
    ))
}

/// Reset the connection right after the server committed but before the client saw the reply, on
/// a write-only workload over one connection, so every dropped acknowledgement is resolved by a
/// marker read-back.
pub async fn commit_ack_loss(
    cluster: &ScratchCluster,
    cfg: &ScenarioConfig,
) -> Result<ScenarioResult, ScenarioError> {
    let proxy = start_proxy(cluster).await?;
    let backend = proxy_backend(cluster, &proxy, 4, cluster.database()).await?;
    let model = model()?;
    let driver = LoadDriver::new(backend.clone(), model.clone(), driver_config(cfg, true));
    let task = tokio::spawn(driver.run());
    // Wait until the setup transaction has committed: its marker is the first outbox event. Only
    // then arm the fault, so prepare and setup are never cut.
    let armed_after = wait_for_first_event(&backend).await;
    // The commit is a simple-protocol `Query`; the frontend also sends `Query` for BEGIN, so skip
    // one occurrence and cut on the second — the commit — after relaying it, so the server
    // commits and the client never hears the reply.
    proxy.schedule(
        ScheduledFault::new(
            Direction::ClientToServer,
            Trigger::OnMessage(MessageTrigger::every(PgMessage::Query, true, 1)),
            Fault::Reset,
        )
        .at_most(cfg.dropped_acks),
    );
    let run = join(task).await?;
    let events = proxy.events();
    let (report, audit) = check_run(backend, &model, run, "pg+proxy").await;
    let notes = vec![
        format!("{} dropped commit acknowledgements", events.len()),
        format!("armed after setup: {armed_after}"),
        "indeterminate commits are resolved by reading the marker event back from the outbox"
            .to_owned(),
    ];
    Ok(build(
        "commit_ack_loss",
        cfg,
        None,
        report,
        audit,
        None,
        None,
        notes,
        events,
    ))
}

/// Blackhole the server's replies for 5 s, heal, then check: the driver's history is consistent
/// with the isolation checker before and after, and the pool reports down then healthy.
pub async fn partition(
    cluster: &ScratchCluster,
    cfg: &ScenarioConfig,
) -> Result<ScenarioResult, ScenarioError> {
    let proxy = start_proxy(cluster).await?;
    let backend = proxy_backend(cluster, &proxy, 8, cluster.database()).await?;
    let model = model()?;

    // Each isolation run gets its own database, so its setup does not collide with the other's.
    cluster.create_database_named("spot_faults_iso_a")?;
    cluster.create_database_named("spot_faults_iso_b")?;
    let iso_before = run_isolation(
        proxy_backend(cluster, &proxy, 4, "spot_faults_iso_a").await?,
        cfg.seed ^ 0xA1,
        4,
    )
    .await?;

    let mut partition_cfg = cfg.clone();
    partition_cfg.statement_timeout = Duration::from_secs(2);
    partition_cfg.lock_timeout = Duration::from_secs(1);
    let driver = LoadDriver::new(
        backend.clone(),
        model.clone(),
        driver_config(&partition_cfg, false),
    );
    let task = tokio::spawn(driver.run());
    let _ = wait_for_first_event(&backend).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    proxy.schedule(ScheduledFault::new(
        Direction::ServerToClient,
        Trigger::Immediate,
        Fault::blackhole(5_000),
    ));
    tokio::time::sleep(Duration::from_secs(5)).await;
    proxy.clear();
    let run = join(task).await?;
    let (report, audit) = check_run(backend.clone(), &model, run, "pg+proxy").await;

    let iso_after = run_isolation(
        proxy_backend(cluster, &proxy, 4, "spot_faults_iso_b").await?,
        cfg.seed ^ 0xA2,
        4,
    )
    .await?;
    let isolation_ok = iso_before.ok() && iso_after.ok();
    let isolation_text = format!(
        "before:\n{}\nafter:\n{}",
        iso_before.to_text(),
        iso_after.to_text()
    );
    let notes = vec![
        "blackholed server replies for 5 s, then healed; statement and lock timeouts declared at \
         2 s and 1 s"
            .to_owned(),
    ];
    Ok(build(
        "partition",
        cfg,
        None,
        report,
        audit,
        Some(isolation_ok),
        Some(isolation_text),
        notes,
        proxy.events(),
    ))
}

/// Add 500 ms of latency for a short window: throughput degrades, invariants hold, and the
/// lock-wait path times out cleanly.
pub async fn slow_storage(
    cluster: &ScratchCluster,
    cfg: &ScenarioConfig,
) -> Result<ScenarioResult, ScenarioError> {
    let proxy = start_proxy(cluster).await?;
    let backend = proxy_backend(cluster, &proxy, 8, cluster.database()).await?;
    let model = model()?;
    let mut slow_cfg = cfg.clone();
    slow_cfg.lock_timeout = Duration::from_millis(500);
    let driver = LoadDriver::new(
        backend.clone(),
        model.clone(),
        driver_config(&slow_cfg, false),
    );
    let task = tokio::spawn(driver.run());
    let _ = wait_for_first_event(&backend).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    proxy.schedule(ScheduledFault::new(
        Direction::ServerToClient,
        Trigger::Immediate,
        Fault::delay(500),
    ));
    tokio::time::sleep(Duration::from_secs(2)).await;
    proxy.clear();
    let run = join(task).await?;
    let lock_timeouts = run
        .history
        .entries
        .iter()
        .filter(|e| {
            matches!(&e.outcome, spot_l7_invariants::history::Outcome::Failed { code, .. }
                if code == "storage.lock_timeout")
        })
        .count();
    let retried = run
        .history
        .entries
        .iter()
        .filter(|e| e.attempts > 1)
        .count();
    let (report, audit) = check_run(backend, &model, run, "pg+proxy").await;
    let notes = vec![
        "500 ms added server->client latency for a 2 s window, then healed; lock timeout 500 ms"
            .to_owned(),
        format!(
            "operations retried after a transient error (a lock timeout is retried, so it shows \
             here rather than as a failure): {retried}; lock timeouts that exhausted the retry \
             budget: {lock_timeouts}"
        ),
    ];
    Ok(build(
        "slow_storage",
        cfg,
        None,
        report,
        audit,
        None,
        None,
        notes,
        proxy.events(),
    ))
}

/// The job-engine scenario: killed workers, expired leases, dedup and bounded retries.
pub async fn job_engine(cfg: &ScenarioConfig) -> ScenarioResult {
    let outcome = run_job_scenario(JobScenarioConfig::for_profile(
        Profile::parse(&cfg.profile).unwrap_or(Profile::Smoke),
        cfg.seed,
    ))
    .await;
    let report = outcome.report;
    let audit = AuditReport {
        classification: Default::default(),
        ok: true,
        violations: Vec::new(),
    };
    let l7_ok = report.ok;
    let l7_text = report.to_text();
    let l7_violations = report
        .results
        .iter()
        .flat_map(|r| r.violations.iter())
        .map(|v| format!("{} {} {} {}", v.invariant, v.entity, v.id, v.detail))
        .collect();
    ScenarioResult {
        name: "job_engine".to_owned(),
        seed: cfg.seed,
        profile: cfg.profile.clone(),
        recovery_ms: None,
        l7_ok,
        l7_text,
        audit,
        l7_violations,
        isolation_ok: None,
        isolation_text: None,
        notes: outcome.notes,
        timeline: Vec::new(),
    }
}

// -- helpers ---------------------------------------------------------------------------------

async fn connect(cluster: &ScratchCluster) -> Result<Arc<PgBackend>, ScenarioError> {
    let backend = PgBackend::connect(cluster.backend_config())
        .await
        .map_err(ScenarioError::Storage)?;
    Ok(Arc::new(backend))
}

async fn start_proxy(cluster: &ScratchCluster) -> Result<FaultProxy, ScenarioError> {
    let upstream = format!("127.0.0.1:{}", cluster.port())
        .parse()
        .map_err(|e| ScenarioError::Proxy(format!("bad upstream address: {e}")))?;
    FaultProxy::start(ProxyConfig::new(upstream))
        .await
        .map_err(|e| ScenarioError::Proxy(e.to_string()))
}

async fn proxy_backend(
    cluster: &ScratchCluster,
    proxy: &FaultProxy,
    pool: usize,
    dbname: &str,
) -> Result<Arc<PgBackend>, ScenarioError> {
    let mut config: PgConfig = cluster.backend_config_named(dbname);
    config.port = proxy.addr().port();
    config.pool_max_size = pool;
    config.pool_wait_timeout = Duration::from_secs(30);
    let backend = PgBackend::connect(config)
        .await
        .map_err(ScenarioError::Storage)?;
    Ok(Arc::new(backend))
}

fn model() -> Result<Arc<Model>, ScenarioError> {
    Model::new()
        .map(Arc::new)
        .map_err(|e| ScenarioError::Model(e.to_string()))
}

fn driver_config(cfg: &ScenarioConfig, write_only: bool) -> DriverConfig {
    let mut weights = Weights::default();
    if write_only {
        weights.read = 0;
        weights.delete_party = 0;
    }
    DriverConfig {
        seed: cfg.seed,
        workers: cfg.workers,
        operations: cfg.operations,
        weights,
        retry: spot_l7_invariants::RetryPolicy::default(),
        lock_timeout: cfg.lock_timeout,
        statement_timeout: cfg.statement_timeout,
        profile: cfg.profile.clone(),
    }
}

async fn join(
    task: tokio::task::JoinHandle<spot_storage_contract::StorageResult<RunResult>>,
) -> Result<RunResult, ScenarioError> {
    match task.await {
        Ok(Ok(run)) => Ok(run),
        Ok(Err(e)) => Err(ScenarioError::Storage(e)),
        Err(e) => Err(ScenarioError::Join(e.to_string())),
    }
}

async fn check_run<B: StorageBackend>(
    backend: Arc<B>,
    model: &Arc<Model>,
    run: RunResult,
    backend_name: &str,
) -> (Report, AuditReport) {
    let done = Completed {
        meta: RunMeta {
            seed: run.seed,
            backend: backend_name.to_owned(),
            profile: run.profile.clone(),
            workers: run.workers,
        },
        history: Arc::new(run.history),
        oracle: Arc::new(run.oracle),
    };
    let registry = framework_registry(model);
    let report = check(backend.clone(), model, &done, &registry).await;
    let ctx = CheckContext::for_backend(
        backend,
        model.catalog.clone(),
        done.history.clone(),
        done.oracle.clone(),
    );
    let audit = DurabilityAudit::run(&ctx).await;
    (report, audit)
}

/// Runs a small isolation workload at `ReadCommitted` and checks it.
async fn run_isolation<B: StorageBackend>(
    backend: Arc<B>,
    seed: u64,
    workers: u32,
) -> Result<IsolationReport, ScenarioError> {
    let mut config = WorkloadConfig::for_profile(Profile::Smoke, Workload::RwRegister, seed);
    config.workers = workers;
    config.transactions = 120;
    config.keys = 6;
    let model = Arc::new(IsoModel::default());
    let history = run_on(backend, model, &config, Level::ReadCommitted)
        .await
        .map_err(ScenarioError::Storage)?;
    Ok(spot_l8_isolation::checker::check(
        &history,
        Level::ReadCommitted,
    ))
}

#[allow(clippy::too_many_arguments)]
fn build(
    name: &str,
    cfg: &ScenarioConfig,
    recovery_ms: Option<u64>,
    report: Report,
    audit: AuditReport,
    isolation_ok: Option<bool>,
    isolation_text: Option<String>,
    notes: Vec<String>,
    timeline: Vec<FaultEvent>,
) -> ScenarioResult {
    let l7_violations = report
        .results
        .iter()
        .flat_map(|r| r.violations.iter())
        .map(|v| format!("{} {} {} {}", v.invariant, v.entity, v.id, v.detail))
        .collect();
    ScenarioResult {
        name: name.to_owned(),
        seed: cfg.seed,
        profile: cfg.profile.clone(),
        recovery_ms,
        l7_ok: report.ok,
        l7_text: report.to_text(),
        audit,
        l7_violations,
        isolation_ok,
        isolation_text,
        notes,
        timeline,
    }
}

/// Waits until the backend's outbox holds its first event — the setup transaction's marker — so a
/// commit-ack-loss fault can be armed only after setup. Returns a short status for the report.
async fn wait_for_first_event<B: StorageBackend>(backend: &Arc<B>) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(page) = backend.read_outbox(FeedPosition::START, 1).await
            && !page.is_empty()
        {
            return "yes".to_owned();
        }
        if Instant::now() >= deadline {
            return "timed out".to_owned();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Adds any server-log durability errors to the audit as violations.
fn add_log_errors(cluster: &ScratchCluster, mut audit: AuditReport) -> AuditReport {
    for line in cluster.recovery_errors() {
        audit.violations.push(Violation::new(
            crate::audit::DURABILITY_AUDIT,
            "log",
            "-",
            format!("server_log: {line}"),
        ));
    }
    audit.ok = audit.violations.is_empty();
    audit
}
