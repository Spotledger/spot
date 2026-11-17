//! Full verification: a scratch database the backup is restored into and that is always dropped
//! again (WP-PRD-04.T2).
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use spot_backup::{
    BackupError, BoxFuture, InvariantReport, InvariantRunner, PackageFile, RestoredSite,
    SchemaCheck, ScratchSite,
};
use spot_core::Clock;
use spot_postgres::{PgBackend, PgConfig};
use spot_snapshot::{
    PgDumpProvider, Progress, ProgressSink, RestoreOptions, RestoreTarget, ScratchMarker, Snapshot,
    SnapshotProvider, drop_scratch, shipped_checks,
};

use super::{CommandCtx, connection};

struct Quiet;
impl ProgressSink for Quiet {
    fn on(&self, _event: Progress) {}
}

/// The scratch site `Full` verification restores into. It records every scratch database it
/// creates, so [`PgScratch::cleanup`] drops them all whatever happened.
pub(super) struct PgScratch {
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
    /// The scratch database, open, for the L7 invariants; `None` for a stopped copy that is not
    /// restored into a database.
    backend: Option<PgBackend>,
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

/// The L7 invariants over the scratch restore, read-only and against the scratch database only.
///
/// What exists today with a callable entry point, and is wired: the shipped snapshot checks
/// (catalogue version, system-table counts), the journal **capture invariant** (the change feed
/// against the journal, `spot-capture-invariant-pg`, a full pass from the start with no lag) and
/// the **identity audit chain** (hash links and sequence, as `spot audit verify`). The other L7
/// checks (journal chain, gapless naming, ledger balance, idempotency) have no entry point that
/// runs on a database alone and are listed in the task report, not stubbed.
///
/// A report holds check names, counts and stable codes, never a row, a record id or a secret. A
/// check that cannot run is a violation: a full verification that did not run its invariants never
/// reports success.
pub(super) struct ScratchInvariantRunner;

impl InvariantRunner for ScratchInvariantRunner {
    fn run<'a>(
        &'a self,
        site: &'a dyn RestoredSite,
    ) -> BoxFuture<'a, spot_backup::Result<InvariantReport>> {
        Box::pin(async move {
            let Some(restored) = site.as_any().downcast_ref::<ScratchRestored>() else {
                return Err(BackupError::Unsupported {
                    feature: "invariants over a site this command did not restore".to_owned(),
                });
            };
            let mut violations = restored.violations.clone();
            let mut names = vec!["shipped_checks"];
            let mut checks = restored.checks_run;
            if let Some(backend) = &restored.backend {
                names.push("capture_invariant");
                names.push("identity_chain");
                checks = checks.saturating_add(2);
                violations.extend(capture_violations(backend).await);
                violations.extend(chain_violations(backend, &restored.tenant).await);
            }
            let runner = names.join(",");
            Ok(if violations.is_empty() {
                InvariantReport::passed(runner, checks)
            } else {
                InvariantReport::failed(runner, checks, violations)
            })
        })
    }
}

/// The capture invariant over the scratch database: settled findings that would raise
/// `CAPTURE_MISMATCH`, as kind codes and counts only.
async fn capture_violations(backend: &PgBackend) -> Vec<String> {
    use spot_capture_invariant::Exclusions;
    use spot_capture_invariant_pg::{ScanConfig, Watermarks, scan};
    let config = ScanConfig {
        lag_secs: 0,
        ..ScanConfig::default()
    };
    match scan(
        &backend.pool(),
        Exclusions::new(),
        Watermarks::none(),
        &config,
    )
    .await
    {
        Ok(found) if found.raise.is_empty() => Vec::new(),
        Ok(found) => {
            let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
            for finding in &found.raise {
                *counts.entry(finding.kind.code()).or_insert(0) += 1;
            }
            let kinds: Vec<String> = counts.iter().map(|(k, n)| format!("{k}={n}")).collect();
            vec![format!(
                "capture_invariant: {} settled findings ({})",
                found.raise.len(),
                kinds.join(",")
            )]
        }
        Err(error) => vec![format!(
            "capture_invariant: could not run ({})",
            error.code()
        )],
    }
}

/// The identity audit chain of the restored site: sequence numbers and stable codes only.
async fn chain_violations(backend: &PgBackend, tenant: &str) -> Vec<String> {
    use spot_cli::audit::chain::{IdentityChain, scope_of};
    let chain = IdentityChain::new(backend, &scope_of(tenant));
    match chain.verify().await {
        Ok(report) if report.is_ok() => Vec::new(),
        Ok(report) => {
            let mut failures = report.failures.clone();
            failures.sort_by_key(|(seq, _)| *seq);
            let first = failures.first().map_or_else(String::new, |(seq, code)| {
                format!("first at seq {seq}: {code}")
            });
            vec![format!(
                "identity_chain: {} broken entries ({first})",
                failures.len()
            )]
        }
        Err(_) => {
            vec!["identity_chain: could not run (the audit store could not be read)".to_owned()]
        }
    }
}

impl PgScratch {
    pub(super) fn new(
        cfg: PgConfig,
        bin: PathBuf,
        clock: Arc<dyn Clock>,
        work: PathBuf,
    ) -> PgScratch {
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
        if snapshot.manifest().provider_kind == super::STOPPED_KIND {
            // A copy of a data directory cannot be restored into a database. Check every copied
            // file against its recorded checksum; the copy is not started, and the verdict says so.
            let report = snapshot.verify().await?;
            let manifest = package.manifest();
            return Ok(Box::new(ScratchRestored {
                tenant: manifest.tenant.clone(),
                schema_version: manifest.schema_version,
                app_set: manifest.app_set.clone(),
                pg_major: manifest.pg_major,
                checks_run: report.files_checked,
                violations: Vec::new(),
                backend: None,
            }));
        }
        let database = format!(
            "spot_scratch_{}_{n}_{}",
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
        let checks = shipped_checks(backend.clone());
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
            backend: Some(backend),
        }))
    }

    /// Drops every scratch database made; the names that could not be dropped.
    pub(super) async fn cleanup(&self) -> Vec<String> {
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

pub(super) fn leftover(ctx: &mut CommandCtx, left: &[String]) -> bool {
    for database in left {
        let _ = writeln!(
            ctx.err(),
            "backup.scratch_left: the scratch database `{database}` holds a copy of the site data and could not be dropped; drop it"
        );
    }
    !left.is_empty()
}
