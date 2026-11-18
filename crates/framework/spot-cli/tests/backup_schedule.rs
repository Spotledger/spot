//! WP-PRD-04.T4: the scheduled backup service. The scheduling, lease and failure rules run on an
//! injected clock, the in-process coordinator and the in-memory job store with a fake executor;
//! the folder executor runs against a real PostgreSQL through `SPOT_TEST_PG_URL` and the client
//! tools (`SPOT_TEST_PG_BIN`, else `pg_dump` on `PATH`): those tests print one `SKIPPED` line and
//! pass when either is missing, and never fake them.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use spot_app_api::context::Invocation;
use spot_app_api::document::Document;
use spot_app_api::prelude::RecordId;
use spot_backup::{BACKUP_JOB_NAME, BackupLedger, FileLedger, RetentionPolicy};
use spot_cli::services::backup_schedule::executor::{
    ExecFuture, Executor, ExecutorParts, FolderExecutor, Notices, RunError, RunPlan, RunSummary,
};
use spot_cli::services::backup_schedule::{
    BackupScheduleService, DEGRADED_AFTER_FAILURES, ORDER, ServiceParts, Settings, build,
};
use spot_cli::services::ports::{
    DocumentLoader, JobSubmit, OutboxLease, PortFuture, PortParts, provide,
};
use spot_cli::services::{Service, SiteCtx};
use spot_coordinator::{Coordinator, InMemoryPresence, InProcessCoordinator, NodeId};
use spot_core::{ManualClock, SiteId};
use spot_jobs::store::JobFilter;
use spot_jobs::worker::HandlerResult;
use spot_jobs::{InMemoryJobStore, JobStore};
use spot_storage_contract::{Change, ChangeFeed, FeedPosition, OutboxRecord, StorageResult};
use time::macros::datetime;
use tokio::sync::Notify;

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(datetime!(2026-11-06 10:00:00 UTC)))
}

#[derive(Clone, Default)]
struct EmptyFeed;

impl ChangeFeed for EmptyFeed {
    async fn read_outbox(
        &self,
        _after: FeedPosition,
        _limit: u32,
    ) -> StorageResult<Vec<OutboxRecord>> {
        Ok(Vec::new())
    }

    async fn read_changes(&self, _after: FeedPosition, _limit: u32) -> StorageResult<Vec<Change>> {
        Ok(Vec::new())
    }
}

struct NoDocs;

impl DocumentLoader for NoDocs {
    fn load<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
    ) -> PortFuture<'a, Option<Document>> {
        Box::pin(async { Ok(None) })
    }
}

fn summary(id: &str) -> RunSummary {
    RunSummary {
        id: id.to_owned(),
        bytes: 10,
        verified: true,
        created_at: 1_793_000_000,
        deleted: Vec::new(),
    }
}

fn error(code: &'static str, transient: bool) -> RunError {
    RunError {
        code,
        message: "the target is not there".to_owned(),
        transient,
    }
}

/// A partial package: a file that exists while the run is alive and is gone once it is dropped.
struct Partial(PathBuf);

impl Partial {
    fn create(dir: &Path) -> Partial {
        let path = dir.join("partial.spotbak.tmp");
        std::fs::write(&path, b"half a package").unwrap();
        Partial(path)
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// An executor that answers from a script, or hangs with a partial package until it is dropped.
struct Fake {
    calls: AtomicUsize,
    script: Mutex<VecDeque<Result<RunSummary, RunError>>>,
    hang_in: Option<PathBuf>,
    started: Notify,
}

impl Fake {
    fn new(script: Vec<Result<RunSummary, RunError>>) -> Arc<Fake> {
        Arc::new(Fake {
            calls: AtomicUsize::new(0),
            script: Mutex::new(script.into()),
            hang_in: None,
            started: Notify::new(),
        })
    }

    fn hanging(dir: &Path) -> Arc<Fake> {
        Arc::new(Fake {
            calls: AtomicUsize::new(0),
            script: Mutex::new(VecDeque::new()),
            hang_in: Some(dir.to_owned()),
            started: Notify::new(),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Executor for Fake {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }

    fn run<'a>(&'a self, _plan: &'a RunPlan, _notes: &'a Notices) -> ExecFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(dir) = &self.hang_in {
                let _partial = Partial::create(dir);
                self.started.notify_one();
                std::future::pending::<()>().await;
            }
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(summary("acme-main-1")))
        })
    }
}

fn settings() -> Settings {
    Settings {
        enabled: true,
        ..Settings::default()
    }
}

/// The job and lease ports of one node of the site `acme`, built from the shared store and
/// coordinator.
fn ports_of(
    holder: &str,
    store: &Arc<InMemoryJobStore>,
    coordinator: Arc<dyn Coordinator>,
    clock: &Arc<ManualClock>,
) -> (Arc<dyn JobSubmit>, Arc<dyn OutboxLease>) {
    let mut ctx = SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        clock.clone(),
        BTreeMap::new(),
        true,
    );
    ctx.as_module("jobs", 600);
    provide(
        &mut ctx,
        PortParts {
            site: SiteId::new("acme").unwrap(),
            store: store.clone() as Arc<dyn JobStore>,
            queues: vec!["default".to_owned()],
            clock: clock.clone(),
            coordinator,
            feed: EmptyFeed,
            node: holder.to_owned(),
            ttl: Duration::from_secs(30),
            loader: Arc::new(NoDocs),
        },
    )
    .unwrap();
    ctx.as_module("backup_schedule", ORDER);
    (
        ctx.require_port::<dyn JobSubmit>().unwrap(),
        ctx.require_port::<dyn OutboxLease>().unwrap(),
    )
}

fn service(
    submit: Arc<dyn JobSubmit>,
    outbox: Arc<dyn OutboxLease>,
    clock: &Arc<ManualClock>,
    executor: Arc<dyn Executor>,
    settings: Settings,
) -> Arc<BackupScheduleService> {
    build(ServiceParts {
        site: "acme".to_owned(),
        schedule: settings.schedule().unwrap(),
        settings,
        submit,
        outbox,
        executor,
        clock: clock.clone(),
        renew_every: Duration::from_millis(20),
    })
}

async fn node(
    holder: &str,
    store: &Arc<InMemoryJobStore>,
    coordinator: Arc<dyn Coordinator>,
    clock: &Arc<ManualClock>,
    executor: Arc<dyn Executor>,
    settings: Settings,
) -> Arc<BackupScheduleService> {
    let (submit, outbox) = ports_of(holder, store, coordinator, clock);
    service(submit, outbox, clock, executor, settings)
}

/// A lease port that can be made to lose its lease: the next renewal fails and it no longer holds.
struct Flaky {
    inner: Arc<dyn OutboxLease>,
    lost: std::sync::atomic::AtomicBool,
}

impl OutboxLease for Flaky {
    fn acquire<'a>(
        &'a self,
        consumer: &'a str,
    ) -> PortFuture<'a, spot_cli::services::ports::ConsumerLease> {
        self.inner.acquire(consumer)
    }

    fn renew<'a>(
        &'a self,
        lease: &'a spot_cli::services::ports::ConsumerLease,
    ) -> PortFuture<'a, spot_cli::services::ports::ConsumerLease> {
        if self.lost.load(Ordering::SeqCst) {
            return Box::pin(async { Err(spot_cli::services::ports::PortError::LeaseUnavailable) });
        }
        self.inner.renew(lease)
    }

    fn still_held<'a>(
        &'a self,
        lease: &'a spot_cli::services::ports::ConsumerLease,
    ) -> PortFuture<'a, bool> {
        if self.lost.load(Ordering::SeqCst) {
            return Box::pin(async { Ok(false) });
        }
        self.inner.still_held(lease)
    }

    fn release<'a>(
        &'a self,
        lease: &'a spot_cli::services::ports::ConsumerLease,
    ) -> PortFuture<'a, ()> {
        self.inner.release(lease)
    }

    fn read<'a>(&'a self, after: FeedPosition, limit: u32) -> PortFuture<'a, Vec<OutboxRecord>> {
        self.inner.read(after, limit)
    }
}

async fn coordinator(holder: &str, clock: &Arc<ManualClock>) -> Arc<dyn Coordinator> {
    InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new(holder).unwrap(),
        clock.clone(),
    )
    .await
    .unwrap()
}

fn args() -> serde_json::Value {
    json!({"tenant": "acme", "schedule": "backup", "scheduled_for": "2026-11-06T02:00:00Z"})
}

fn temp(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spot_backup_t4_{label}_{}", RecordId::new_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_due_slot_enqueues_one_job_even_with_two_nodes() {
    let clock = clock();
    let store = Arc::new(InMemoryJobStore::new());
    let a = node(
        "n1",
        &store,
        coordinator("n1", &clock).await,
        &clock,
        Fake::new(vec![]),
        settings(),
    )
    .await;
    let b = node(
        "n2",
        &store,
        coordinator("n2", &clock).await,
        &clock,
        Fake::new(vec![]),
        settings(),
    )
    .await;
    // 10:00: the latest due slot is the 02:00 of today, found by both nodes.
    assert_eq!(a.tick().await.unwrap(), 1);
    assert_eq!(b.tick().await.unwrap(), 0, "the same slot is the same job");
    assert_eq!(a.tick().await.unwrap(), 0);
    let jobs = store.list(JobFilter::default()).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].name, BACKUP_JOB_NAME);
    assert_eq!(jobs[0].args["tenant"], "acme");
    assert!(
        jobs[0].args["scheduled_for"]
            .as_str()
            .unwrap()
            .starts_with("2026-11-06T02:00:00"),
        "{}",
        jobs[0].args
    );
    assert!(
        jobs[0]
            .idempotency_key
            .as_deref()
            .unwrap()
            .starts_with("spot_backup:backup:")
    );
    // The job names the tenant, the schedule and the slot, never a path or a credential.
    let mut keys: Vec<String> = jobs[0].args.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["deduplication_key", "schedule", "scheduled_for", "tenant"]
    );
    // The next slot, a day later, is a new job.
    clock.advance(Duration::from_secs(17 * 3600));
    assert_eq!(b.tick().await.unwrap(), 1);
    assert_eq!(a.tick().await.unwrap(), 0);
    assert_eq!(store.list(JobFilter::default()).await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_was_down_runs_the_missed_slot_once() {
    let clock = clock();
    let store = Arc::new(InMemoryJobStore::new());
    let svc = node(
        "n1",
        &store,
        coordinator("n1", &clock).await,
        &clock,
        Fake::new(vec![]),
        Settings {
            cron: "0 0 * * * *".to_owned(),
            ..settings()
        },
    )
    .await;
    // Hourly: 25 slots lie in the 25-hour window, and one job is made, not one per slot.
    assert_eq!(svc.tick().await.unwrap(), 1);
    assert_eq!(svc.tick().await.unwrap(), 0);
    assert_eq!(store.list(JobFilter::default()).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_backup_runs_under_the_lease_and_a_foreign_tenant_or_holder_is_refused() {
    let clock = clock();
    let store = Arc::new(InMemoryJobStore::new());
    let shared = coordinator("n1", &clock).await;
    let fake = Fake::new(vec![Ok(summary("acme-main-1"))]);
    let a = node(
        "n1",
        &store,
        shared.clone(),
        &clock,
        fake.clone(),
        settings(),
    )
    .await;
    let b = node("n2", &store, shared, &clock, Fake::new(vec![]), settings()).await;
    assert_eq!(a.run_job(&args()).await, HandlerResult::Ok);
    assert_eq!((fake.calls(), a.successes(), a.failures()), (1, 1, 0));
    assert!(
        a.readiness_text().starts_with("backup: ok"),
        "{}",
        a.readiness_text()
    );
    // A job that names another tenant is never run, whatever the lease.
    let foreign = json!({"tenant": "other", "scheduled_for": "x"});
    assert!(matches!(a.run_job(&foreign).await, HandlerResult::Fail(m) if m.contains("tenant")));
    assert_eq!(fake.calls(), 1);
    // While another node holds the backup lease, this node does not run and retries later.
    let port = b.outbox();
    let held = port.acquire("backup").await.unwrap();
    let retry = a.run_job(&args()).await;
    assert!(
        matches!(&retry, HandlerResult::Retry(m) if m.contains("lease_held")),
        "{retry:?}"
    );
    assert_eq!(fake.calls(), 1);
    port.release(&held).await.unwrap();
    assert_eq!(a.run_job(&args()).await, HandlerResult::Ok);
    assert_eq!(a.successes(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_target_raises_an_alert_counts_a_failure_and_retries_an_absent_drive() {
    let clock = clock();
    let store = Arc::new(InMemoryJobStore::new());
    let fake = Fake::new(vec![
        Err(error("backup.target_unavailable", true)),
        Err(error("backup.verify_failed", false)),
    ]);
    let svc = node(
        "n1",
        &store,
        coordinator("n1", &clock).await,
        &clock,
        fake,
        settings(),
    )
    .await;
    let first = svc.run_job(&args()).await;
    assert!(
        matches!(&first, HandlerResult::Retry(m) if m.starts_with("backup.target_unavailable")),
        "{first:?}"
    );
    let second = svc.run_job(&args()).await;
    assert!(
        matches!(&second, HandlerResult::Fail(m) if m.starts_with("backup.verify_failed")),
        "{second:?}"
    );
    assert_eq!(svc.failures(), 2);
    let notices = svc.notices().join("\n");
    assert!(
        notices.contains("ALERT backup.target_unavailable"),
        "{notices}"
    );
    assert!(notices.contains("ALERT backup.verify_failed"), "{notices}");
    assert!(
        notices.contains("previous good backup was kept"),
        "{notices}"
    );
    assert!(!svc.degraded());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_degrades_after_three_failures_and_recovers_after_a_success() {
    let clock = clock();
    let store = Arc::new(InMemoryJobStore::new());
    let mut script: Vec<_> = (0..DEGRADED_AFTER_FAILURES)
        .map(|_| Err(error("backup.target_unavailable", true)))
        .collect();
    script.push(Ok(summary("acme-main-4")));
    let svc = node(
        "n1",
        &store,
        coordinator("n1", &clock).await,
        &clock,
        Fake::new(script),
        settings(),
    )
    .await;
    for failed in 1..=DEGRADED_AFTER_FAILURES {
        svc.run_job(&args()).await;
        assert_eq!(svc.failures(), failed);
        assert_eq!(svc.degraded(), failed >= DEGRADED_AFTER_FAILURES);
    }
    let text = svc.readiness_text();
    assert!(text.starts_with("backup: degraded"), "{text}");
    assert!(text.contains("backup.target_unavailable"), "{text}");
    // It is an optional service: degraded readiness never stops the site.
    assert_eq!(svc.level(), spot_cli::services::Level::Optional);
    assert_eq!(svc.run_job(&args()).await, HandlerResult::Ok);
    assert!(!svc.degraded());
    assert!(
        svc.readiness_text().starts_with("backup: ok"),
        "{}",
        svc.readiness_text()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn losing_the_lease_aborts_the_run_and_removes_its_partial_package() {
    let clock = clock();
    let store = Arc::new(InMemoryJobStore::new());
    let dir = temp("lost");
    let (submit, inner) = ports_of("n1", &store, coordinator("n1", &clock).await, &clock);
    let flaky = Arc::new(Flaky {
        inner,
        lost: std::sync::atomic::AtomicBool::new(false),
    });
    let fake = Fake::hanging(&dir);
    let a = service(submit, flaky.clone(), &clock, fake.clone(), settings());
    let running = {
        let a = Arc::clone(&a);
        tokio::spawn(async move { a.run_job(&args()).await })
    };
    fake.started.notified().await;
    assert!(
        dir.join("partial.spotbak.tmp").exists(),
        "the run is writing"
    );
    // Another node takes the lease: this node's next renewal fails.
    flaky.lost.store(true, Ordering::SeqCst);
    let ended = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the run ended")
        .unwrap();
    assert!(
        matches!(&ended, HandlerResult::Retry(m) if m.contains("lease_lost")),
        "{ended:?}"
    );
    assert!(
        !dir.join("partial.spotbak.tmp").exists(),
        "the partial package is gone"
    );
    assert_eq!(a.aborted(), 1);
    assert_eq!(a.failures(), 0, "an abandoned run is not a failed backup");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_a_run_drains_within_the_deadline_and_leaves_no_partial_package() {
    let clock = clock();
    let store = Arc::new(InMemoryJobStore::new());
    let dir = temp("stop");
    let fake = Fake::hanging(&dir);
    let svc = node(
        "n1",
        &store,
        coordinator("n1", &clock).await,
        &clock,
        fake.clone(),
        settings(),
    )
    .await;
    let running = {
        let svc = Arc::clone(&svc);
        tokio::spawn(async move { svc.run_job(&args()).await })
    };
    fake.started.notified().await;
    assert!(dir.join("partial.spotbak.tmp").exists());
    // The service's own shutdown tells the run; it is abandoned within one renew interval.
    svc.shutdown().await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("drained within the deadline")
        .unwrap();
    assert!(
        matches!(&ended, HandlerResult::Retry(m) if m.contains("stopping")),
        "{ended:?}"
    );
    assert!(!dir.join("partial.spotbak.tmp").exists());
    // A stopped site starts no new backup.
    let after = svc.run_job(&args()).await;
    assert!(
        matches!(&after, HandlerResult::Retry(m) if m.contains("stopping")),
        "{after:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_settings_refuse_unknown_keys_a_bad_cron_a_bad_zone_and_a_stopped_backup() {
    assert!(toml::from_str::<Settings>("enabeld = true").is_err());
    assert!(Settings::default().validate().is_ok());
    assert!(!Settings::default().enabled, "off by default");
    let bad = |text: &str| {
        toml::from_str::<Settings>(text)
            .unwrap()
            .validate()
            .is_err()
    };
    assert!(bad("cron = \"every day\""));
    assert!(bad("timezone = \"Mars/Olympus\""));
    assert!(bad("kind = \"stopped\""));
    assert!(bad("verify_full_every = 100000"));
    let ok: Settings = toml::from_str(
        "enabled = true\ncron = \"0 30 1 * * *\"\ntimezone = \"Asia/Karachi\"\ntarget = \"usb\"\n[keep]\nkeep_daily = 3\n",
    )
    .unwrap();
    assert!(ok.validate().is_ok());
    assert_eq!(ok.keep.keep_daily, Some(3));
    assert_eq!(
        Settings {
            tick_seconds: 1,
            ..Settings::default()
        }
        .tick(),
        Duration::from_secs(5)
    );
}

// The folder executor against a real database

fn pg_tools() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("SPOT_TEST_PG_BIN") {
        let root = PathBuf::from(root);
        return [root.join("bin"), root]
            .into_iter()
            .find(|d| d.join("pg_dump").is_file());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|d| d.join("pg_dump").is_file())
}

struct Rig {
    env: common::Env,
    target: PathBuf,
    bin: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(
            &self.target,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        );
        let _ = std::fs::remove_dir_all(&self.target);
    }
}

impl Rig {
    async fn new() -> Option<Rig> {
        let Some(bin) = pg_tools() else {
            println!("SKIPPED: no pg_dump client tools (set SPOT_TEST_PG_BIN)");
            return None;
        };
        let env = common::Env::new("").await?;
        env.site();
        Some(Rig {
            env,
            target: temp("target"),
            bin,
        })
    }

    async fn executor(&self, target: PathBuf, clock: &Arc<ManualClock>) -> Arc<FolderExecutor> {
        let loaded = spot_cli::config::load(&self.env.root.0, false).unwrap();
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == self.env.site)
            .unwrap();
        let cfg = loaded.admin.clone().with_dbname(&site.database);
        let backend = spot_postgres::PgBackend::connect(cfg.clone())
            .await
            .unwrap();
        Arc::new(FolderExecutor::new(ExecutorParts {
            tenant: site.site.clone(),
            database: site.database.clone(),
            apps: site.apps.clone(),
            cfg,
            backend,
            target: Ok(target),
            forbidden: vec![self.env.root.0.join("sites")],
            schedule: Settings::default().schedule().unwrap(),
            retention: RetentionPolicy::default(),
            keys: None,
            allow_unencrypted: true,
            bin: Some(self.bin.clone()),
            clock: clock.clone(),
        }))
    }

    async fn scratch_databases(&self) -> i64 {
        let loaded = spot_cli::config::load(&self.env.root.0, false).unwrap();
        let backend =
            spot_postgres::PgBackend::connect(loaded.admin.clone().with_dbname("postgres"))
                .await
                .unwrap();
        let client = backend.pool().get().await.unwrap();
        let sql =
            "SELECT count(*) FROM pg_database WHERE datname LIKE 'spot\\_scratch\\_sched\\_%'";
        client.query_one(sql, &[]).await.unwrap().get::<_, i64>(0)
    }

    fn packages(&self) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.target)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "spotbak"))
            .collect();
        found.sort();
        found
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_backup_runs_and_verifies_in_full_and_a_failing_target_keeps_the_prior_backup() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    let clock = clock();
    let store = Arc::new(InMemoryJobStore::new());
    let executor = rig.executor(rig.target.clone(), &clock).await;
    // Every backup is verified in full: restored into a scratch database that is dropped again.
    let svc = node(
        "n1",
        &store,
        coordinator("n1", &clock).await,
        &clock,
        executor,
        Settings {
            verify_full_every: 1,
            ..settings()
        },
    )
    .await;
    let done = svc.run_job(&args()).await;
    assert_eq!(done, HandlerResult::Ok, "{:?}", svc.notices());
    let packages = rig.packages();
    assert_eq!(packages.len(), 1);
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&packages[0]).unwrap().permissions(),
    );
    assert_eq!(
        mode & 0o077,
        0,
        "the package is private to its owner: {mode:o}"
    );
    let ledger = FileLedger::open(&rig.target).unwrap();
    assert!(
        ledger.records().iter().any(|r| r.verified),
        "the ledger shows a verified backup"
    );
    assert_eq!(
        rig.scratch_databases().await,
        0,
        "the scratch database was dropped"
    );
    assert!(
        std::fs::read_dir(&rig.target).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".work-")),
        "no work directory is left in the target"
    );
    // A target this user cannot write to fails the run; the first backup is untouched.
    let probe = rig.target.join(".probe");
    std::fs::set_permissions(
        &rig.target,
        std::os::unix::fs::PermissionsExt::from_mode(0o500),
    )
    .unwrap();
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        println!(
            "SKIPPED: this user can write to a read-only folder (root); the failing-target half does not run"
        );
        return;
    }
    std::fs::set_permissions(
        &rig.target,
        std::os::unix::fs::PermissionsExt::from_mode(0o500),
    )
    .unwrap();
    let failed = svc.run_job(&args()).await;
    assert!(
        matches!(&failed, HandlerResult::Retry(m) if m.starts_with("backup.target_unavailable")),
        "{failed:?}"
    );
    assert_eq!(svc.failures(), 1);
    std::fs::set_permissions(
        &rig.target,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    assert_eq!(
        rig.packages(),
        packages,
        "the previous good backup was kept"
    );
    let notices = svc.notices().join("\n");
    assert!(
        notices.contains("ALERT backup.target_unavailable"),
        "{notices}"
    );
    let password = std::env::var("SPOT_TEST_PG_URL")
        .ok()
        .and_then(|u| url::Url::parse(&u).ok())
        .and_then(|u| u.password().map(str::to_owned))
        .unwrap_or_default();
    assert!(
        !common::leaks_password(&notices, &password),
        "no secret in the alert"
    );
    // A site without a key provider and without the override writes nothing.
    let strict = {
        let loaded = spot_cli::config::load(&rig.env.root.0, false).unwrap();
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == rig.env.site)
            .unwrap();
        let cfg = loaded.admin.clone().with_dbname(&site.database);
        FolderExecutor::new(ExecutorParts {
            tenant: site.site.clone(),
            database: site.database.clone(),
            apps: Vec::new(),
            backend: spot_postgres::PgBackend::connect(cfg.clone())
                .await
                .unwrap(),
            cfg,
            target: Ok(rig.target.clone()),
            forbidden: Vec::new(),
            schedule: Settings::default().schedule().unwrap(),
            retention: RetentionPolicy::default(),
            keys: None,
            allow_unencrypted: false,
            bin: Some(rig.bin.clone()),
            clock: clock.clone(),
        })
    };
    assert!(
        strict
            .check()
            .unwrap_err()
            .contains("backup.encryption_unavailable")
    );
    assert_eq!(rig.packages(), packages, "nothing more was written");
}
