//! What one scheduled backup does: the [`Executor`] port, and [`FolderExecutor`], which takes,
//! seals, writes and verifies the backup exactly as `spot backup run` does (WP-PRD-04.T2) and
//! applies retention after the write.
//!
//! The port exists so the service's scheduling, lease and failure rules are tested without a
//! database; the folder executor is tested against a real PostgreSQL.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use spot_backup::{
    AlertSink, BackupAlert, BackupError, BackupJob, BackupProducer, BackupRunOutcome, BackupRunner,
    BoxFuture, FileLedger, FolderTarget, HealthCondition, HealthSink, InvariantReport,
    InvariantRunner, KeyRef, MediaNotifier, PackageFile, RestoredSite, RetentionPolicy,
    SchemaCheck, ScratchSite, SnapshotProducer, SystemSpaceProbe, TargetRef, VerifyMode,
    encrypting,
};
use spot_core::Clock;
use spot_postgres::{PgBackend, PgConfig, Tls};
use spot_secrets::KeyProvider;
use spot_snapshot::{
    PgDumpProvider, Progress, ProgressSink, RestoreOptions, RestoreTarget, ScratchMarker, Snapshot,
    SnapshotProvider, SnapshotTarget, drop_scratch, shipped_checks,
};
use time::OffsetDateTime;

/// The one backup chain of a site (ids read `<site>-main-<n>`).
const CHAIN: &str = "main";

/// What a run is asked to do.
#[derive(Clone, Debug)]
pub struct RunPlan {
    /// The tenant (the site) the backup covers.
    pub tenant: String,
    /// The slot the job was enqueued for, as text (for the alert and the log; never a path).
    pub scheduled_for: String,
    /// Whether this run is verified in full (restored into a scratch database).
    pub full: bool,
    /// The instant the run starts (the site clock).
    pub now: OffsetDateTime,
}

/// What a finished run produced.
#[derive(Clone, Debug)]
pub struct RunSummary {
    /// The backup's id.
    pub id: String,
    /// The package's size in bytes.
    pub bytes: u64,
    /// Whether it passed its verification.
    pub verified: bool,
    /// When the backup was taken, Unix seconds.
    pub created_at: i64,
    /// The ids retention removed after this run.
    pub deleted: Vec<String>,
}

/// Why a run failed.
#[derive(Clone, Debug)]
pub struct RunError {
    /// The stable `backup.*` code.
    pub code: &'static str,
    /// A plain-language reason with every secret scrubbed out.
    pub message: String,
    /// Whether trying again later may succeed (an absent drive, a held lock) or not (a damaged
    /// package, a missing key).
    pub transient: bool,
}

/// A run's future.
pub type ExecFuture<'a> = Pin<Box<dyn Future<Output = Result<RunSummary, RunError>> + Send + 'a>>;

/// Takes one backup. Dropping the future abandons the run and must leave no partial package: the
/// folder executor's work directory removes itself when dropped.
pub trait Executor: Send + Sync {
    /// The start-up check: the target, the tools and the key provider, or why they cannot serve.
    ///
    /// # Errors
    ///
    /// A message an operator can act on.
    fn check(&self) -> Result<(), String>;

    /// Runs one backup for `plan`, reporting health conditions and alerts through `notes`.
    fn run<'a>(&'a self, plan: &'a RunPlan, notes: &'a Notices) -> ExecFuture<'a>;
}

/// Health conditions, alerts and media notices of the runs, kept (bounded) for the operator and
/// logged. They carry codes and plain text, never a path to a secret.
#[derive(Default)]
pub struct Notices(Mutex<Vec<String>>);

const NOTICES_KEPT: usize = 100;

impl Notices {
    /// Records one line (the oldest line goes when the list is full).
    pub fn push(&self, line: String) {
        let mut lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() >= NOTICES_KEPT {
            lines.remove(0);
        }
        lines.push(line);
    }

    /// The lines so far, oldest first.
    pub fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl HealthSink for Notices {
    fn raise(&self, condition: HealthCondition) {
        tracing::warn!(code = %condition.code, "backup health condition");
        self.push(format!(
            "health {} ({}): {}",
            condition.code,
            condition.severity.as_str(),
            condition.message
        ));
    }
}

impl AlertSink for Notices {
    fn alert(&self, alert: BackupAlert) {
        tracing::error!(code = %alert.code, "backup alert");
        self.push(format!("ALERT {}: {}", alert.code, alert.message));
    }
}

impl MediaNotifier for Notices {
    fn waiting(&self, volume: &str) {
        self.push(format!("waiting for the media of volume {volume}"));
    }
}

/// Everything the folder executor is built from.
pub struct ExecutorParts {
    /// The tenant (the site).
    pub tenant: String,
    /// The site's database name.
    pub database: String,
    /// The apps the site has installed (recorded in the manifest).
    pub apps: Vec<String>,
    /// The administrative connection, on the site's database.
    pub cfg: PgConfig,
    /// The site's own backend (its pool reads the recorded schema version).
    pub backend: PgBackend,
    /// The configured target's path, or why the target cannot be used.
    pub target: Result<PathBuf, String>,
    /// Directories a target may not be inside or hold (site data, app directories, `PGDATA`).
    pub forbidden: Vec<PathBuf>,
    /// The schedule the manifest records.
    pub schedule: spot_scheduler::Schedule,
    /// The retention applied after every success.
    pub retention: RetentionPolicy,
    /// The site's key provider, when it has one.
    pub keys: Option<Arc<dyn KeyProvider>>,
    /// Whether a site without a key provider may write in clear.
    pub allow_unencrypted: bool,
    /// The PostgreSQL client tools directory; `None` looks at `SPOT_PG_BIN`, then `PATH`.
    pub bin: Option<PathBuf>,
    /// The site clock.
    pub clock: Arc<dyn Clock>,
}

/// The executor `spot serve` uses.
pub struct FolderExecutor {
    parts: ExecutorParts,
    secrets: Vec<String>,
}

impl FolderExecutor {
    /// An executor over `parts`.
    pub fn new(parts: ExecutorParts) -> Self {
        let mut secrets = vec![parts.cfg.password.clone()];
        secrets.extend(connection(&parts.cfg, &parts.database));
        Self { parts, secrets }
    }

    fn failed(&self, error: &BackupError) -> RunError {
        let transient = matches!(
            error,
            BackupError::LedgerLocked { .. }
                | BackupError::TargetUnavailable { .. }
                | BackupError::Io { .. }
        );
        RunError {
            code: error.code(),
            message: self.scrub(&error.reason()),
            transient,
        }
    }

    fn refused(&self, code: &'static str, message: impl AsRef<str>, transient: bool) -> RunError {
        RunError {
            code,
            message: self.scrub(message.as_ref()),
            transient,
        }
    }

    fn scrub(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for secret in self.secrets.iter().filter(|s| !s.is_empty()) {
            out = out.replace(secret.as_str(), "***");
        }
        out
    }

    /// The site's recorded schema version: `spot_meta.catalog_version`, `0` when never recorded.
    /// A read that fails fails the run: it never falls back to a guess.
    async fn schema_version(&self) -> Result<u64, RunError> {
        let unreadable = || {
            self.refused(
                "backup.io",
                "the site's schema version cannot be read",
                true,
            )
        };
        let client = self
            .parts
            .backend
            .pool()
            .get()
            .await
            .map_err(|_| unreadable())?;
        let row = client
            .query_opt(
                "SELECT value FROM spot_meta WHERE key = 'catalog_version'",
                &[],
            )
            .await
            .map_err(|_| unreadable())?;
        match row {
            None => Ok(0),
            Some(row) => row.get::<_, String>(0).parse::<u64>().map_err(|_| {
                self.refused(
                    "backup.io",
                    "the recorded schema version is not a number",
                    false,
                )
            }),
        }
    }

    async fn run_inner(&self, plan: &RunPlan, notes: &Notices) -> Result<RunSummary, RunError> {
        let dir = match &self.parts.target {
            Ok(path) => resolve_target(path, &self.parts.forbidden).map_err(|(code, why)| {
                self.refused(code, why, code == "backup.target_unavailable")
            })?,
            Err(text) => return Err(self.refused("backup.target_unavailable", text, false)),
        };
        let key = self.parts.keys.as_ref().map(|keys| {
            let current = keys.current();
            KeyRef {
                id: current.id,
                version: current.version,
            }
        });
        if key.is_none() && !self.parts.allow_unencrypted {
            let text = "the site has no usable key provider ([services.secrets]) and allow_unencrypted is off; nothing was written";
            return Err(self.refused("backup.encryption_unavailable", text, false));
        }
        let bin = pg_bin(self.parts.bin.as_deref())
            .map_err(|text| self.refused("backup.snapshot", text, false))?;
        let work = WorkDir::create(&dir).map_err(|_| {
            self.refused(
                "backup.target_unavailable",
                "the target is not writable by this service",
                true,
            )
        })?;
        let _lock = FileLedger::lock(&dir).map_err(|e| self.failed(&e))?;
        let ledger = FileLedger::open(&dir).map_err(|e| self.failed(&e))?;
        let Some(conn) = connection(&self.parts.cfg, &self.parts.database) else {
            return Err(self.refused(
                "backup.io",
                "the database connection cannot be described",
                false,
            ));
        };
        let version = self.schema_version().await?;
        let provider: Arc<dyn spot_snapshot::SnapshotProvider> =
            Arc::new(PgDumpProvider::new(bin.clone(), self.parts.clock.clone()));
        let target = SnapshotTarget::Database {
            connection: conn,
            tenant: plan.tenant.clone(),
            database: self.parts.database.clone(),
        };
        let inner = SnapshotProducer::new(
            provider,
            target,
            work.0.clone(),
            version,
            self.parts.apps.clone(),
        );
        let producer: Arc<dyn BackupProducer> = match &self.parts.keys {
            Some(keys) => Arc::new(encrypting(Arc::new(inner), keys.clone())),
            None => Arc::new(inner),
        };
        let job = BackupJob {
            tenant: plan.tenant.clone(),
            schedule: self.parts.schedule.clone(),
            target: TargetRef::Folder { path: dir.clone() },
            retention: self.parts.retention,
            encrypt: key,
            verify: if plan.full {
                VerifyMode::Full
            } else {
                VerifyMode::Quick
            },
            chain_id: CHAIN.to_owned(),
            sequence: 1,
            parent_digest: None,
        };
        let folder = FolderTarget::new(&dir, Arc::new(SystemSpaceProbe));
        let scratch = PgScratch::new(
            self.parts.cfg.clone(),
            bin,
            self.parts.clock.clone(),
            work.0.clone(),
        );
        let runner = BackupRunner {
            target: &folder,
            producer: producer.as_ref(),
            ledger: &ledger,
            health: notes,
            alerts: notes,
            notifier: notes,
            scratch: Some(&scratch),
            invariants: Some(&ShippedInvariants),
        };
        let outcome = runner.run(&job, plan.now).await;
        let left = scratch.cleanup().await;
        for name in left {
            notes.push(format!(
                "backup.scratch_left: the scratch database `{name}` could not be dropped"
            ));
        }
        match outcome {
            Ok(report) => match report.outcome {
                BackupRunOutcome::Created {
                    backup,
                    verification,
                } => {
                    if let Ok(path) = spot_backup::target::package_path(&dir, &backup.id) {
                        make_private(&path);
                    }
                    Ok(RunSummary {
                        id: backup.id.clone(),
                        bytes: backup.package_bytes,
                        verified: verification.ok,
                        created_at: backup
                            .manifest
                            .created_at()
                            .map_or(plan.now.unix_timestamp(), |t| t.unix_timestamp()),
                        deleted: report.deleted,
                    })
                }
                BackupRunOutcome::WaitingForMedia { .. } => Err(self.refused(
                    "backup.target_unavailable",
                    "the target is waiting for media",
                    true,
                )),
            },
            Err(error) => Err(self.failed(&error)),
        }
    }
}

impl Executor for FolderExecutor {
    fn check(&self) -> Result<(), String> {
        let path = self.parts.target.as_ref().map_err(String::clone)?;
        resolve_target(path, &self.parts.forbidden)
            .map_err(|(code, why)| format!("{code}: {why}"))?;
        pg_bin(self.parts.bin.as_deref())?;
        if self.parts.keys.is_none() && !self.parts.allow_unencrypted {
            return Err("backup.encryption_unavailable: the site has no key provider ([services.secrets]); configure one or set allow_unencrypted".to_owned());
        }
        Ok(())
    }

    fn run<'a>(&'a self, plan: &'a RunPlan, notes: &'a Notices) -> ExecFuture<'a> {
        Box::pin(self.run_inner(plan, notes))
    }
}

#[cfg(unix)]
fn make_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn make_private(_path: &Path) {}

#[cfg(unix)]
fn private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}
#[cfg(not(unix))]
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

/// A private scratch directory inside the target, removed when dropped: a run that is abandoned
/// (the lease is lost, the site stops) leaves no partial package behind.
struct WorkDir(PathBuf);

impl WorkDir {
    fn create(dir: &Path) -> std::io::Result<WorkDir> {
        let mut last = None;
        for n in 0..8u32 {
            let path = dir.join(format!(".work-sched-{}-{n}", std::process::id()));
            match private_dir(&path) {
                Ok(()) => return Ok(WorkDir(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no work directory name was free")))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The real directory of a configured target, or the code and reason it is refused. The final
/// component must not be a symbolic link, and the real path must be neither inside nor above a
/// directory in `forbidden`.
pub fn resolve_target(
    path: &Path,
    forbidden: &[PathBuf],
) -> Result<PathBuf, (&'static str, String)> {
    let missing = || {
        (
            "backup.target_unavailable",
            "the target directory does not exist or cannot be read".to_owned(),
        )
    };
    let meta = std::fs::symlink_metadata(path).map_err(|_| missing())?;
    if meta.file_type().is_symlink() {
        return Err((
            "backup.target_unsafe",
            "the target is a symbolic link; name the real directory".to_owned(),
        ));
    }
    if !meta.is_dir() {
        return Err((
            "backup.target_unavailable",
            "the target is not a directory".to_owned(),
        ));
    }
    let real = std::fs::canonicalize(path).map_err(|_| missing())?;
    for bad in forbidden {
        let bad = std::fs::canonicalize(bad).unwrap_or_else(|_| bad.clone());
        if real.starts_with(&bad) || bad.starts_with(&real) {
            let why = "the target is inside, or holds, the site data, an app directory or the PostgreSQL data directory";
            return Err(("backup.target_unsafe", why.to_owned()));
        }
    }
    Ok(real)
}

/// A `postgres://` URL for `database` on the configured server. It holds the password: it goes
/// to the snapshot provider (which moves it into a child's environment) and nowhere else.
fn connection(cfg: &PgConfig, database: &str) -> Option<String> {
    let mut url = url::Url::parse("postgres://placeholder/").ok()?;
    url.set_host(Some(&cfg.host)).ok()?;
    url.set_port(Some(cfg.port)).ok()?;
    url.set_username(&cfg.user).ok()?;
    if !cfg.password.is_empty() {
        url.set_password(Some(&cfg.password)).ok()?;
    }
    url.path_segments_mut().ok()?.clear().push(database);
    if matches!(cfg.tls, Tls::Require) {
        url.query_pairs_mut().append_pair("sslmode", "require");
    }
    Some(url.into())
}

/// The directory of the PostgreSQL client tools: the configured one, else `SPOT_PG_BIN`, else the
/// first `pg_dump` on `PATH`; absolute and not world-writable.
fn pg_bin(configured: Option<&Path>) -> Result<PathBuf, String> {
    let env = || std::env::var_os("SPOT_PG_BIN").map(PathBuf::from);
    let path = || {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).find(|d| d.is_absolute() && d.join("pg_dump").is_file())
    };
    let Some(dir) = configured.map(Path::to_path_buf).or_else(env).or_else(path) else {
        return Err("backup.snapshot: the PostgreSQL client tools (pg_dump) were not found; put them on PATH or set SPOT_PG_BIN to their directory".to_owned());
    };
    if !dir.is_absolute() || !dir.join("pg_dump").is_file() {
        return Err("backup.snapshot: the PostgreSQL client tools directory must be absolute and hold pg_dump".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let open = std::fs::metadata(&dir)
            .map(|m| m.permissions().mode() & 0o002 != 0)
            .unwrap_or(true);
        if open {
            return Err(format!(
                "backup.target_unsafe: the PostgreSQL client tools directory {} is world-writable or unreadable; refusing to run its tools",
                dir.display()
            ));
        }
    }
    Ok(dir)
}

// Full verification: a scratch database, always dropped again

struct Quiet;
impl ProgressSink for Quiet {
    fn on(&self, _event: Progress) {}
}

/// The scratch site `Full` verification restores into. It records every scratch database it
/// creates, so [`PgScratch::cleanup`] drops them all whatever happened.
struct PgScratch {
    cfg: PgConfig,
    provider: PgDumpProvider,
    work: PathBuf,
    clock: Arc<dyn Clock>,
    made: Mutex<Vec<(String, ScratchMarker)>>,
    counter: AtomicU64,
}

/// What the scratch restore found; the invariant runner reads it back through `as_any`.
struct ScratchRestored {
    tenant: String,
    schema_version: u64,
    app_set: Vec<String>,
    pg_major: u32,
    checks_run: u64,
    violations: Vec<String>,
}

impl RestoredSite for ScratchRestored {
    fn tenant(&self) -> &str {
        &self.tenant
    }
    fn schema_version(&self) -> u64 {
        self.schema_version
    }
    fn app_set(&self) -> Vec<String> {
        self.app_set.clone()
    }
    fn pg_major(&self) -> u32 {
        self.pg_major
    }
    fn schema_checks(&self) -> spot_backup::Result<Vec<SchemaCheck>> {
        Ok(vec![SchemaCheck {
            name: "scratch.restored".to_owned(),
            passed: true,
            detail: String::new(),
        }])
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Reports the shipped snapshot checks the scratch restore ran. The L7 invariant registry is not
/// wired into the CLI yet (the same stated limit as `spot backup verify --full`).
struct ShippedInvariants;

impl InvariantRunner for ShippedInvariants {
    fn run<'a>(
        &'a self,
        site: &'a dyn RestoredSite,
    ) -> BoxFuture<'a, spot_backup::Result<InvariantReport>> {
        Box::pin(async move {
            let Some(restored) = site.as_any().downcast_ref::<ScratchRestored>() else {
                return Err(BackupError::Unsupported {
                    feature: "invariants over a site this service did not restore".to_owned(),
                });
            };
            Ok(if restored.violations.is_empty() {
                InvariantReport::passed("shipped_checks", restored.checks_run)
            } else {
                InvariantReport::failed(
                    "shipped_checks",
                    restored.checks_run,
                    restored.violations.clone(),
                )
            })
        })
    }
}

impl PgScratch {
    fn new(cfg: PgConfig, bin: PathBuf, clock: Arc<dyn Clock>, work: PathBuf) -> PgScratch {
        PgScratch {
            cfg,
            provider: PgDumpProvider::new(bin, clock.clone()),
            work,
            clock,
            made: Mutex::new(Vec::new()),
            counter: AtomicU64::new(0),
        }
    }

    async fn restore_package(
        &self,
        package: &PackageFile,
    ) -> spot_backup::Result<Box<dyn RestoredSite>> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let unpacked = self.work.join(format!("unpack-{n}"));
        let result = self.restore_unpacked(package, &unpacked, n).await;
        let _ = std::fs::remove_dir_all(&unpacked);
        result
    }

    async fn restore_unpacked(
        &self,
        package: &PackageFile,
        unpacked: &Path,
        n: u64,
    ) -> spot_backup::Result<Box<dyn RestoredSite>> {
        let (pkg, dest) = (package.clone(), unpacked.to_path_buf());
        let unpack =
            tokio::task::spawn_blocking(move || spot_backup::package::unpack(&pkg, &dest)).await;
        unpack.map_err(|_| BackupError::io("the unpack task did not finish"))??;
        let snapshot = Snapshot::open(unpacked).await?;
        let database = format!(
            "spot_scratch_sched_{}_{n}_{}",
            std::process::id(),
            self.clock.now_utc().unix_timestamp()
        );
        let Some(scratch_conn) = connection(&self.cfg, &database) else {
            return Err(BackupError::io(
                "the scratch connection cannot be described",
            ));
        };
        self.made
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((database.clone(), ScratchMarker::for_snapshot(&snapshot)));
        let target = RestoreTarget::Database {
            connection: scratch_conn,
            database: database.clone(),
        };
        self.provider
            .restore(
                &snapshot,
                &target,
                &RestoreOptions::default(),
                Arc::new(Quiet),
            )
            .await?;
        let mut scratch_cfg = self.cfg.clone().with_dbname(&database);
        scratch_cfg.pool_max_size = 2;
        let backend = PgBackend::connect(scratch_cfg)
            .await
            .map_err(|_| BackupError::io("the scratch database could not be opened"))?;
        let checks = shipped_checks(backend);
        let mut violations = Vec::new();
        for check in &checks {
            for found in check.check(&snapshot).await {
                violations.push(format!("{}: {}", found.check, found.detail));
            }
        }
        let manifest = package.manifest();
        Ok(Box::new(ScratchRestored {
            tenant: manifest.tenant.clone(),
            schema_version: manifest.schema_version,
            app_set: manifest.app_set.clone(),
            pg_major: manifest.pg_major,
            checks_run: u64::try_from(checks.len()).unwrap_or(u64::MAX),
            violations,
        }))
    }

    /// Drops every scratch database made; the names that could not be dropped.
    async fn cleanup(&self) -> Vec<String> {
        let made = std::mem::take(&mut *self.made.lock().unwrap_or_else(PoisonError::into_inner));
        let mut left = Vec::new();
        let Some(live) = connection(&self.cfg, &self.cfg.dbname) else {
            return made.into_iter().map(|(database, _)| database).collect();
        };
        for (database, marker) in made {
            let target = RestoreTarget::Database {
                connection: live.clone(),
                database: database.clone(),
            };
            if drop_scratch(&live, &target, &marker).await.is_err() {
                left.push(database);
            }
        }
        left
    }
}

impl ScratchSite for PgScratch {
    fn restore<'a>(
        &'a self,
        package: &'a PackageFile,
    ) -> BoxFuture<'a, spot_backup::Result<Box<dyn RestoredSite>>> {
        Box::pin(self.restore_package(package))
    }
}
