//! Shared test support: a scratch database, a six-step catalogue, an assembled engine and the
//! test-only snapshot port and step hook the kill test drives.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use spot_core::{Clock, SiteId};
use spot_migration_apply::gate::AdmissionGate;
use spot_migration_apply::journal::Progress;
use spot_migration_apply::snapshot::RestoreOutcome;
use spot_migration_apply::step::RestorePoint;
use spot_migration_apply::testing::ticket_for;
use spot_migration_apply::{
    ApplyEngine, ApplyJournal, ApplyOptions, BoxFuture, Error as ApplyError, ProgressEvent,
    ProgressSink, Result as ApplyResult, SnapshotId, SnapshotPort, StepContext, StepDescriptor,
    StepOutcome, StepRunner,
};
use spot_migration_apply::{RecordingHold, RecordingProgress};
use spot_migration_approval::ApplyTicket;
use spot_migration_plan::{ApplyStep, Area, ReleasePlan};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_storage_contract::{
    Catalog, Collation, EntityDef, EntityKind, FieldDef, FieldType, IndexDef, Name,
};

use spot_migration_apply_pg::{
    PgAdmissionGate, PgApplyJournal, PgApplySchema, PgDriftVerifier, PgStepRunner,
    RecordingTimings, TimingPhase, TimingRecord, TimingSink,
};

/// The site every test migrates.
pub fn site() -> SiteId {
    SiteId::new("test-site").unwrap()
}

/// A scratch database, or `None` (printing `SKIPPED`) when `SPOT_TEST_PG_URL` is unset.
pub async fn db() -> Option<TestDb> {
    TestDb::create().await
}

/// The application name the migration's own connections carry, so the gate's drain never
/// waits for them.
pub const MIGRATION_APP: &str = "spot-migrate";

/// A pool on `db`'s scratch database.
pub fn pool(db: &TestDb, max_size: usize) -> Pool {
    pool_for(&db.config(), max_size, MIGRATION_APP)
}

/// A pool for an explicit config.
pub fn pool_for(config: &PgConfig, max_size: usize, app: &str) -> Pool {
    let mut driver = tokio_postgres::Config::new();
    driver
        .host(&config.host)
        .port(config.port)
        .dbname(&config.dbname)
        .user(&config.user)
        .password(&config.password)
        .application_name(app);
    let manager = Manager::from_config(
        driver,
        tokio_postgres::NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(max_size)
        .runtime(Runtime::Tokio1)
        .build()
        .expect("the pool builds")
}

/// A backend on `db`.
pub async fn backend(db: &TestDb) -> PgBackend {
    PgBackend::connect(db.config())
        .await
        .expect("the backend connects")
}

/// A valid storage name.
pub fn n(name: &str) -> Name {
    Name::new(name).unwrap()
}

/// A nullable text field called `note`.
pub fn text() -> FieldDef {
    FieldDef {
        name: n("note"),
        ty: FieldType::Text { max_chars: None },
        nullable: true,
    }
}

/// One document entity with an `id` and a `note`.
pub fn entity(name: &str) -> EntityDef {
    EntityDef {
        name: n(name),
        kind: EntityKind::Document,
        fields: vec![text()],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    }
}

/// A catalogue of `count` document entities named `branch_1` .. `branch_<count>`.
pub fn catalog(count: usize) -> Catalog {
    let entities = (1..=count)
        .map(|i| entity(&format!("branch_{i}")))
        .collect();
    Catalog::new(1, Collation::new("en").unwrap(), entities).unwrap()
}

/// A plan with `count` schema steps (one change per step).
pub fn schema_plan(count: usize) -> ReleasePlan {
    spot_migration_apply::testing::sample_plan(&vec![Area::Schema; count])
}

/// A plan with one metadata change, whose only engine step is not a schema step.
pub fn metadata_plan() -> ReleasePlan {
    spot_migration_apply::testing::sample_plan(&[Area::Metadata])
}

/// The engine steps of `plan`.
pub fn engine_steps(plan: &ReleasePlan) -> Vec<ApplyStep> {
    plan.steps()
}

/// The tables the `count`-table catalogue creates.
pub fn branch_tables(count: usize) -> Vec<String> {
    (1..=count).map(|i| format!("branch_{i}")).collect()
}

/// Everything one engine needs, kept so a test can assert on each collaborator.
pub struct Harness {
    pub db: TestDb,
    pub backend: PgBackend,
    pub pool: Pool,
    pub site: SiteId,
    pub catalog: Catalog,
    pub plan: ReleasePlan,
    pub ticket: ApplyTicket,
    pub journal: Arc<PgApplyJournal>,
    pub gate: Arc<PgAdmissionGate>,
    pub runner: Arc<PgStepRunner>,
    pub verify: Arc<PgDriftVerifier>,
    pub snapshot: Arc<TestSnapshotPort>,
    pub snapshot_id: SnapshotId,
    pub progress: Arc<RecordingProgress>,
    pub timings: Arc<RecordingTimings>,
    pub engine: ApplyEngine,
}

/// Builds a harness over a fresh scratch database with `tables` branch tables, or `None` when
/// no server is configured.
///
/// The runner is prepared (so `describe` knows which steps are non-transactional), the gate is
/// read once, and the snapshot is taken before the engine is returned, exactly as the apply
/// flow requires.
pub async fn harness(tables: usize, clock: Arc<dyn Clock>) -> Option<Harness> {
    let db = db().await?;
    let backend = backend(&db).await;
    let pool = pool(&db, 8);
    let schema = PgApplySchema::new(pool.clone());
    schema.prepare().await.expect("the control schema prepares");
    let site = site();
    let catalog = catalog(tables);
    let plan = schema_plan(tables);
    let timings = Arc::new(RecordingTimings::new());
    let mut runner = PgStepRunner::new(
        backend.clone(),
        site.clone(),
        catalog.clone(),
        engine_steps(&plan),
        clock.clone(),
    )
    .with_timings(timings.clone());
    runner.prepare().await.expect("the runner prepares");
    let runner = Arc::new(runner);
    let verify = Arc::new(PgDriftVerifier::new(backend.clone(), catalog.clone()));
    let journal = Arc::new(PgApplyJournal::new(pool.clone(), site.clone()));
    let snapshot = Arc::new(TestSnapshotPort::new(
        pool.clone(),
        site.clone(),
        branch_tables(tables),
        clock.clone(),
        timings.clone(),
    ));
    let snapshot_id = snapshot
        .take("before-apply", 1_000)
        .await
        .expect("the snapshot is taken");
    let gate = Arc::new(PgAdmissionGate::new(
        pool.clone(),
        site.clone(),
        clock,
        std::time::Duration::from_millis(50),
    ));
    gate.refresh(1_000).await.expect("the gate reads");
    let hold = Arc::new(RecordingHold::new());
    let progress = Arc::new(RecordingProgress::new());
    let ticket = ticket_for(&plan, 1_000).expect("the additive plan auto-approves");
    let engine = ApplyEngine::new(
        site.clone(),
        journal.clone() as Arc<dyn ApplyJournal>,
        gate.clone(),
        hold,
        snapshot.clone(),
        runner.clone(),
        verify.clone(),
    )
    .with_progress(progress.clone())
    .with_options(ApplyOptions {
        drain_deadline_micros: 5_000_000,
        ..ApplyOptions::default()
    });
    Some(Harness {
        db,
        backend,
        pool,
        site,
        catalog,
        plan,
        ticket,
        journal,
        gate,
        runner,
        verify,
        snapshot,
        snapshot_id,
        progress,
        timings,
        engine,
    })
}

/// The ticket for `plan` under the default policy (an additive plan auto-approves).
pub fn ticket(plan: &ReleasePlan) -> ApplyTicket {
    ticket_for(plan, 1_000).expect("the additive plan auto-approves")
}

/// The journal's fold for a site.
pub async fn progress_of(journal: &PgApplyJournal) -> Progress {
    Progress::fold(journal.read().await.expect("the journal reads"))
}

/// One request path's admission decision: `Ok(())` to serve, `Err(reason)` to refuse.
///
/// It is deliberately the fail-closed shape: a refresh that fails refuses the request.
pub async fn admit(gate: &PgAdmissionGate) -> Result<(), String> {
    if gate.refresh_if_stale(2_000).await.is_err() {
        return Err("the gate state could not be read".to_owned());
    }
    match AdmissionGate::problem(gate) {
        Some(problem) => Err(problem.detail),
        None => Ok(()),
    }
}

/// The tables the catalogue created, read from the live schema.
pub async fn live_branch_tables(backend: &PgBackend) -> Vec<String> {
    let live = spot_pg_migrate::LiveSchema::read(backend)
        .await
        .expect("the live schema reads");
    live.tables
        .keys()
        .filter(|name| name.starts_with("branch_"))
        .cloned()
        .collect()
}

/// A catalogue with one indexed document, so the planner emits a
/// `CREATE INDEX CONCURRENTLY` step on an existing table.
pub fn indexed_catalog(version: u64) -> Catalog {
    let mut doc = entity("party");
    doc.indexes.push(IndexDef {
        name: n("party_note_idx"),
        fields: vec![n("note")],
        unique: false,
        method: spot_storage_contract::IndexMethod::BTree,
        opclass: None,
        include: vec![],
    });
    Catalog::new(version, Collation::new("en").unwrap(), vec![doc]).unwrap()
}

/// Prints one phase's timings in the measurement note's format.
pub fn print_timings(label: &str, records: &[TimingRecord]) {
    for record in records {
        println!(
            "TIMING {label} phase={} micros={} snapshot={}",
            record.phase.as_str(),
            record.duration_micros,
            record.snapshot
        );
    }
}

/// The timings of one phase.
pub fn timings_of(timings: &RecordingTimings, phase: TimingPhase) -> Vec<TimingRecord> {
    timings.of(phase)
}

/// A snapshot port for the kill tests: the "snapshot" is the catalogue before apply, and a
/// restore drops every table the plan created. Its ids are recorded in the control schema's
/// snapshot index, so a **different process** (the restarted recovery) resolves them.
pub struct TestSnapshotPort {
    pool: Pool,
    site: SiteId,
    tables: Vec<String>,
    clock: Arc<dyn Clock>,
    timings: Arc<RecordingTimings>,
    next: AtomicU64,
    restores: AtomicU64,
}

impl TestSnapshotPort {
    /// A port that will drop `tables` on restore.
    pub fn new(
        pool: Pool,
        site: SiteId,
        tables: Vec<String>,
        clock: Arc<dyn Clock>,
        timings: Arc<RecordingTimings>,
    ) -> Self {
        Self {
            pool,
            site,
            tables,
            clock,
            timings,
            next: AtomicU64::new(1),
            restores: AtomicU64::new(0),
        }
    }

    /// How many restores ran.
    pub fn restores(&self) -> u64 {
        self.restores.load(Ordering::SeqCst)
    }

    fn now_micros(&self) -> i64 {
        i64::try_from(self.clock.monotonic().as_micros()).unwrap_or(i64::MAX)
    }

    async fn known(&self, id: &SnapshotId) -> bool {
        let Ok(client) = self.pool.get().await else {
            return false;
        };
        client
            .query_opt(
                "SELECT 1 FROM spot_migration_control.snapshot \
                 WHERE site = $1 AND snapshot_id = $2",
                &[&self.site.as_str(), &id.as_str()],
            )
            .await
            .ok()
            .flatten()
            .is_some()
    }
}

impl SnapshotPort for TestSnapshotPort {
    fn take<'a>(&'a self, label: &'a str, now: i64) -> BoxFuture<'a, ApplyResult<SnapshotId>> {
        Box::pin(async move {
            let started = self.now_micros();
            let n = self.next.fetch_add(1, Ordering::SeqCst);
            let id = SnapshotId::new(format!("catalog-{n}"))?;
            let client = self
                .pool
                .get()
                .await
                .map_err(|error| ApplyError::SnapshotFailed {
                    message: error.to_string(),
                })?;
            client
                .execute(
                    "INSERT INTO spot_migration_control.snapshot \
                     (site, snapshot_id, dir, label, taken_at) VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (site, snapshot_id) DO UPDATE SET dir = EXCLUDED.dir",
                    &[&self.site.as_str(), &id.as_str(), &"catalog", &label, &now],
                )
                .await
                .map_err(|error| ApplyError::SnapshotFailed {
                    message: error.to_string(),
                })?;
            self.timings
                .record(TimingRecord::new(
                    TimingPhase::Snapshot,
                    self.now_micros().saturating_sub(started),
                    id.as_str(),
                    now,
                ))
                .ok();
            Ok(id)
        })
    }

    fn exists<'a>(&'a self, id: &'a SnapshotId) -> BoxFuture<'a, ApplyResult<bool>> {
        Box::pin(async move { Ok(self.known(id).await) })
    }

    fn restore<'a>(
        &'a self,
        id: &'a SnapshotId,
        now: i64,
    ) -> BoxFuture<'a, ApplyResult<RestoreOutcome>> {
        Box::pin(async move {
            if !self.known(id).await {
                return Err(ApplyError::SnapshotFailed {
                    message: format!("snapshot {id} was not taken"),
                });
            }
            let started = self.now_micros();
            let client = self
                .pool
                .get()
                .await
                .map_err(|error| ApplyError::SnapshotFailed {
                    message: error.to_string(),
                })?;
            for table in self.tables.iter().rev() {
                let quoted = format!("\"{}\"", table.replace('"', "\"\""));
                client
                    .batch_execute(&format!("DROP TABLE IF EXISTS {quoted} CASCADE"))
                    .await
                    .map_err(|error| ApplyError::SnapshotFailed {
                        message: error.to_string(),
                    })?;
            }
            self.restores.fetch_add(1, Ordering::SeqCst);
            self.timings
                .record(TimingRecord::new(
                    TimingPhase::Restore,
                    self.now_micros().saturating_sub(started),
                    id.as_str(),
                    now,
                ))
                .ok();
            Ok(RestoreOutcome {
                snapshot: id.clone(),
                location: "catalog".to_owned(),
                at: now,
            })
        })
    }
}

/// A step runner wrapper that appends a marker line before and after every step and can pause
/// inside a step, giving the kill test a window to `SIGKILL` the child.
pub struct HookedRunner {
    inner: Arc<PgStepRunner>,
    markers: Option<std::path::PathBuf>,
    pause: std::time::Duration,
}

impl HookedRunner {
    /// A runner writing markers to `markers` (when set) and pausing `pause` after a step
    /// starts.
    pub fn new(
        inner: Arc<PgStepRunner>,
        markers: Option<std::path::PathBuf>,
        pause: std::time::Duration,
    ) -> Self {
        Self {
            inner,
            markers,
            pause,
        }
    }

    fn mark(&self, line: &str) {
        if let Some(path) = &self.markers {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

impl StepRunner for HookedRunner {
    fn describe(&self, index: usize, step: &ApplyStep) -> StepDescriptor {
        self.inner.describe(index, step)
    }

    fn restore_point(&self, descriptor: &StepDescriptor) -> Option<RestorePoint> {
        self.inner.restore_point(descriptor)
    }

    fn run<'a>(
        &'a self,
        descriptor: &'a StepDescriptor,
        ctx: &'a StepContext,
    ) -> BoxFuture<'a, ApplyResult<StepOutcome>> {
        Box::pin(async move {
            self.mark(&format!("step:{}:start", descriptor.index));
            if !self.pause.is_zero() {
                tokio::time::sleep(self.pause).await;
            }
            let outcome = self.inner.run(descriptor, ctx).await;
            match &outcome {
                Ok(_) => self.mark(&format!("step:{}:done", descriptor.index)),
                Err(_) => self.mark(&format!("step:{}:failed", descriptor.index)),
            }
            outcome
        })
    }

    fn rollback<'a>(
        &'a self,
        restore_point: &'a RestorePoint,
        ctx: &'a StepContext,
    ) -> BoxFuture<'a, ApplyResult<()>> {
        Box::pin(async move { self.inner.rollback(restore_point, ctx).await })
    }
}

/// A progress sink that also appends one line per event to a file, for the kill test's parent
/// to watch.
pub struct FileProgress {
    inner: Arc<RecordingProgress>,
    path: Option<std::path::PathBuf>,
}

impl FileProgress {
    /// A sink writing to `path`.
    pub fn new(inner: Arc<RecordingProgress>, path: Option<std::path::PathBuf>) -> Self {
        Self { inner, path }
    }
}

impl ProgressSink for FileProgress {
    fn on(&self, event: ProgressEvent) {
        if let Some(path) = &self.path {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let tag = event_tag(&event);
                let _ = writeln!(file, "progress:{tag}");
            }
        }
        self.inner.on(event);
    }
}

/// The stable tag of a progress event.
pub fn event_tag(event: &ProgressEvent) -> &'static str {
    match event {
        ProgressEvent::Started { .. } => "started",
        ProgressEvent::GateClosing { .. } => "gate_closing",
        ProgressEvent::GateClosed { .. } => "gate_closed",
        ProgressEvent::HoldBegun { .. } => "hold_begun",
        ProgressEvent::StepStarted { .. } => "step_started",
        ProgressEvent::StepCompleted { .. } => "step_completed",
        ProgressEvent::VerifyStarted { .. } => "verify_started",
        ProgressEvent::VerifyPassed { .. } => "verify_passed",
        ProgressEvent::VerifyFailed { .. } => "verify_failed",
        ProgressEvent::RestoreStarted { .. } => "restore_started",
        ProgressEvent::Restored { .. } => "restored",
        ProgressEvent::HoldReleased { .. } => "hold_released",
        ProgressEvent::HoldDiscarded { .. } => "hold_discarded",
        ProgressEvent::Opened { .. } => "opened",
        ProgressEvent::Halted { .. } => "halted",
        ProgressEvent::Resumed { .. } => "resumed",
    }
}

/// A descriptor of one scratch database a child process may use, as `host|port|db|user|pass`.
pub fn child_database(db: &TestDb) -> String {
    let config = db.config();
    format!(
        "{}|{}|{}|{}|{}",
        config.host, config.port, config.dbname, config.user, config.password
    )
}

/// Reads a child database descriptor back.
pub fn config_from_child(text: &str) -> PgConfig {
    let parts: Vec<&str> = text.split('|').collect();
    PgConfig::new(
        parts[0],
        parts[1].parse().unwrap(),
        parts[2],
        parts[3],
        parts[4],
    )
}

/// A probe that records every request-path decision while a migration runs.
#[derive(Debug, Default)]
pub struct ProbeLog {
    served: Mutex<Vec<String>>,
    refused: Mutex<Vec<String>>,
    violations: Mutex<Vec<String>>,
    seen_closed: std::sync::atomic::AtomicBool,
    observations: AtomicU64,
}

impl ProbeLog {
    /// Records one observation; `decision` is `Ok(())` when the path was admitted.
    pub fn observe(&self, decision: Result<(), String>, at: &str) {
        self.observations.fetch_add(1, Ordering::SeqCst);
        match decision {
            Ok(()) => self.served.lock().unwrap().push(at.to_owned()),
            Err(reason) => self.refused.lock().unwrap().push(reason),
        }
    }

    /// How many observations were made.
    pub fn observations(&self) -> u64 {
        self.observations.load(Ordering::SeqCst)
    }

    /// The observations in which a request was served.
    pub fn served(&self) -> Vec<String> {
        self.served.lock().unwrap().clone()
    }

    /// The refusals, one reason per observation.
    pub fn refusals(&self) -> Vec<String> {
        self.refused.lock().unwrap().clone()
    }

    /// Notes that a refusal was observed: from then on a served request is a violation.
    pub fn note_closed(&self) {
        self.seen_closed.store(true, Ordering::SeqCst);
    }

    /// Whether the probe has seen the gate closed.
    pub fn has_seen_closed(&self) -> bool {
        self.seen_closed.load(Ordering::SeqCst)
    }

    /// Records one request that was served while the gate was closed.
    pub fn note_violation(&self, what: &str) {
        self.violations.lock().unwrap().push(what.to_owned());
    }

    /// Every served-while-closed violation.
    pub fn violations(&self) -> Vec<String> {
        self.violations.lock().unwrap().clone()
    }
}

/// The gate's cached status as a label, for a concise assertion message.
pub fn gate_label(gate: &PgAdmissionGate) -> &'static str {
    match gate.status() {
        spot_migration_apply::GateStatus::Open => "open",
        spot_migration_apply::GateStatus::Closed { .. } => "closed",
    }
}
