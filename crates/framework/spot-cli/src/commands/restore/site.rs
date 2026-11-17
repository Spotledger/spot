//! The restore target as the library sees it (`PgSite`), its ports, and what preflight and apply share.

use spot_backup::{
    BackupError, BackupManifest, BoxFuture, InvariantReport, InvariantRunner, MigrationApplied,
    PackageFile, PreflightBlocker, PreflightReport, RestoreTargetSite, RestoredSite, SchemaCheck,
    ServiceGate, SpaceProbe, SystemSpaceProbe,
};
use spot_cli::services::secrets::OperatorAudit;
use spot_core::Clock;
use spot_postgres::PgConfig;
use spot_snapshot::{
    PgDumpProvider, Progress, ProgressSink, RestoreOptions as SnapRestoreOptions, RestoreTarget,
    ScratchMarker, Snapshot, SnapshotManifest, SnapshotProvider, drop_scratch, shipped_checks,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::guard::fault;
use super::server::{admin, on_db, pool_error};
use super::{
    CommandCtx, Exit, Prepared, Step, UNVERIFIED_VERSIONS, connection, pg_bin, scrub, stop,
};

pub(super) struct Quiet;
impl ProgressSink for Quiet {
    fn on(&self, _event: Progress) {}
}

/// The gate the library closes and opens. The real gate (the row, the host, the sessions) is
/// closed by `quiesce` before the library runs; `close` is the library's commit point and
/// refuses unless the site was proven quiescent. The real gate is opened after the swap.
pub(super) struct GateFlag {
    pub(super) proven: AtomicBool,
    pub(super) open: AtomicBool,
}

impl ServiceGate for GateFlag {
    fn close(&self) -> spot_backup::Result<()> {
        if !self.proven.load(Ordering::SeqCst) {
            return Err(BackupError::RestoreRefused {
                reason: "the site was not proven quiescent".to_owned(),
            });
        }
        self.open.store(false, Ordering::SeqCst);
        Ok(())
    }
    fn open(&self) -> spot_backup::Result<()> {
        self.open.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }
}

/// What the restore into the scratch database found.
#[derive(Default)]
pub(super) struct Applied {
    pub(super) scratch: Option<(String, ScratchMarker)>,
    pub(super) data_version: Option<u64>,
    pub(super) violations: Vec<String>,
    pub(super) checks_run: u64,
    pub(super) snapshot: Option<SnapshotManifest>,
    pub(super) migrated: Option<MigrationApplied>,
}

/// The target of one restore: the live database (or the name a new site will get) and the scratch
/// database the backup is restored into first. It is the library's [`RestoreTargetSite`].
pub(super) struct PgSite {
    pub(super) tenant: String,
    pub(super) cfg: PgConfig,
    pub(super) live: String,
    pub(super) exists: bool,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) work: PathBuf,
    pub(super) bin: PathBuf,
    pub(super) meta: Arc<spot_meta::EffectiveMeta>,
    pub(super) catalog_version: u64,
    pub(super) manifest_version: u64,
    pub(super) verified: bool,
    pub(super) allow_migrate: bool,
    pub(super) apps: Vec<String>,
    pub(super) pg_major: u32,
    pub(super) free: u64,
    pub(super) state: Mutex<Applied>,
}

impl PgSite {
    pub(super) fn applied(&self) -> std::sync::MutexGuard<'_, Applied> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn scratch_name(&self) -> Option<String> {
        self.applied()
            .scratch
            .as_ref()
            .map(|(name, _)| name.clone())
    }

    pub(super) async fn apply_inner(&self, package: &PackageFile) -> spot_backup::Result<()> {
        let unpacked = self.work.join("unpacked");
        let (pkg, dest) = (package.clone(), unpacked.clone());
        let unpack =
            tokio::task::spawn_blocking(move || spot_backup::package::unpack(&pkg, &dest)).await;
        unpack.map_err(|_| BackupError::io("the unpack task did not finish"))??;
        let snapshot = Snapshot::open(&unpacked).await?;
        let secs = self.clock.now_utc().unix_timestamp();
        let database = format!("spot_scratch_rs_{secs}_{}", std::process::id());
        let Some(conn) = connection(&self.cfg, &database) else {
            return Err(BackupError::io(
                "the scratch connection cannot be described",
            ));
        };
        self.applied().scratch = Some((database.clone(), ScratchMarker::for_snapshot(&snapshot)));
        let target = RestoreTarget::Database {
            connection: conn,
            database: database.clone(),
        };
        let provider = PgDumpProvider::new(self.bin.clone(), self.clock.clone());
        provider
            .restore(
                &snapshot,
                &target,
                &SnapRestoreOptions::default(),
                Arc::new(Quiet),
            )
            .await?;
        fault("after_scratch");
        let scratch = on_db(&self.cfg, &database).await.map_err(BackupError::io)?;
        let checks = shipped_checks(scratch.clone());
        let mut violations = Vec::new();
        for check in &checks {
            for found in check.check(&snapshot).await {
                violations.push(format!("{}: {}", found.check, found.detail));
            }
        }
        let client = scratch
            .pool()
            .get()
            .await
            .map_err(|_| BackupError::io("the scratch database could not be read"))?;
        let row = client
            .query_opt(
                "SELECT value FROM spot_meta WHERE key = 'catalog_version'",
                &[],
            )
            .await
            .map_err(|_| {
                BackupError::io("the schema version of the restored data cannot be read")
            })?;
        drop(client);
        let data = row.and_then(|r| r.get::<_, String>(0).parse::<u64>().ok());
        let refuse = |text: String| BackupError::RestoreRefused { reason: text };
        let found = data.unwrap_or(0);
        let mut migrated = None;
        if self.verified && found != self.manifest_version {
            return Err(refuse(format!(
                "restore.schema_mismatch: the manifest says schema {} but the restored data is at {found}; the backup is inconsistent",
                self.manifest_version
            )));
        }
        if !self.verified && found > self.catalog_version {
            return Err(refuse(format!(
                "restore.too_new: the restored data is at schema {found}, newer than this build's {}",
                self.catalog_version
            )));
        }
        if !self.verified && found < self.catalog_version {
            if !self.allow_migrate {
                return Err(refuse(format!(
                    "restore.schema_older: the restored data is at schema {found}, older than this build's {}; pass --migrate-forward",
                    self.catalog_version
                )));
            }
            migrated = Some(self.migrate(&database, found, self.catalog_version).await?);
        }
        drop(scratch);
        let _ = std::fs::remove_dir_all(&unpacked);
        let mut state = self.applied();
        state.data_version = Some(found);
        state.violations = violations;
        state.checks_run = u64::try_from(checks.len()).unwrap_or(u64::MAX);
        state.snapshot = Some(snapshot.manifest().clone());
        state.migrated = migrated.or(state.migrated.take());
        Ok(())
    }

    /// Plans and applies the schema change to this build's catalogue on the scratch database.
    pub(super) async fn migrate(
        &self,
        database: &str,
        from: u64,
        to: u64,
    ) -> spot_backup::Result<MigrationApplied> {
        let wrong = |text: String| BackupError::RestoreRefused {
            reason: format!("restore.migrate_failed: {text}"),
        };
        let backend = on_db(&self.cfg, database).await.map_err(BackupError::io)?;
        let catalog = spot_cli::schema::catalog(&self.meta).map_err(|e| wrong(e.to_string()))?;
        let plan = spot_cli::schema::plan_with_platform_tables(&backend, &catalog)
            .await
            .map_err(|e| wrong(e.to_string()))?;
        spot_cli::schema::refusals(&plan).map_err(|e| wrong(e.to_string()))?;
        let steps: Vec<String> = plan.steps.iter().map(|s| s.key.clone()).collect();
        spot_cli::schema::apply(&backend, &plan, None)
            .await
            .map_err(|e| wrong(e.to_string()))?;
        Ok(MigrationApplied { from, to, steps })
    }

    /// Clears node-local state on the scratch: the manifest's keys, coordination rows and a new
    /// outbox generation.
    pub(super) async fn reset_inner(&self) -> spot_backup::Result<()> {
        let (database, manifest) = {
            let state = self.applied();
            (
                state.scratch.as_ref().map(|(n, _)| n.clone()),
                state.snapshot.clone(),
            )
        };
        let (Some(database), Some(manifest)) = (database, manifest) else {
            return Err(BackupError::io("nothing was restored to reset"));
        };
        let backend = on_db(&self.cfg, &database).await.map_err(BackupError::io)?;
        let client = backend
            .pool()
            .get()
            .await
            .map_err(|_| BackupError::io("the scratch database could not be opened"))?;
        spot_snapshot::reset_node_local(&client, &manifest).await?;
        let io = |_| BackupError::io("node-local state could not be reset");
        for table in ["spot_coord_lease", "spot_coord_node", "spot_coord_rate"] {
            let present = client
                .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
                .await
                .map_err(io)?;
            if present.get::<_, bool>(0) {
                client
                    .execute(&format!("DELETE FROM {table}"), &[])
                    .await
                    .map_err(io)?;
            }
        }
        let generation = client
            .query_opt("SELECT 1 FROM spot_meta WHERE key = 'feed_generation'", &[])
            .await
            .map_err(io)?;
        drop(client);
        if generation.is_some() {
            spot_snapshot::post_restore(&backend).await?;
        }
        Ok(())
    }

    /// Drops the scratch database when it still exists under its own name.
    pub(super) async fn cleanup(&self) -> Option<String> {
        let (name, marker) = self.applied().scratch.clone()?;
        let live = connection(&self.cfg, &self.cfg.dbname)?;
        let target = RestoreTarget::Database {
            connection: live.clone(),
            database: name.clone(),
        };
        drop_scratch(&live, &target, &marker)
            .await
            .err()
            .map(|_| name)
    }
}

impl RestoredSite for PgSite {
    fn tenant(&self) -> &str {
        &self.tenant
    }
    fn schema_version(&self) -> u64 {
        self.applied().data_version.unwrap_or(self.manifest_version)
    }
    fn app_set(&self) -> Vec<String> {
        self.apps.clone()
    }
    fn pg_major(&self) -> u32 {
        self.pg_major
    }
    fn schema_checks(&self) -> spot_backup::Result<Vec<SchemaCheck>> {
        let state = self.applied();
        let restored = state.scratch.is_some() && state.data_version.is_some();
        Ok(vec![SchemaCheck {
            name: "scratch.restored".to_owned(),
            passed: restored,
            detail: if restored {
                String::new()
            } else {
                "the backup was not restored into the scratch database".to_owned()
            },
        }])
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl RestoreTargetSite for PgSite {
    fn runtime_schema_version(&self) -> u64 {
        if self.verified {
            self.catalog_version
        } else {
            self.manifest_version
        }
    }
    fn free_space(&self) -> u64 {
        self.free
    }
    fn is_empty(&self) -> spot_backup::Result<bool> {
        Ok(!self.exists)
    }
    fn apply<'a>(
        &'a self,
        package: &'a PackageFile,
        replace: bool,
    ) -> BoxFuture<'a, spot_backup::Result<()>> {
        Box::pin(async move {
            if self.exists && !replace {
                return Err(BackupError::RestoreRefused {
                    reason: "the target already holds data".to_owned(),
                });
            }
            self.apply_inner(package).await
        })
    }
    fn migrate_forward<'a>(
        &'a self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'a, spot_backup::Result<MigrationApplied>> {
        Box::pin(async move {
            let Some(database) = self.scratch_name() else {
                return Err(BackupError::io("nothing was restored to migrate"));
            };
            if !self.allow_migrate {
                return Err(BackupError::RestoreRefused {
                    reason: "restore.schema_older: pass --migrate-forward".to_owned(),
                });
            }
            let done = self.migrate(&database, from, to).await?;
            self.applied().migrated = Some(done.clone());
            Ok(done)
        })
    }
    fn reset_node_local<'a>(&'a self) -> BoxFuture<'a, spot_backup::Result<()>> {
        Box::pin(self.reset_inner())
    }
}

/// Reports what the shipped snapshot checks found on the scratch restore. The L7 invariant
/// registry is not wired into the CLI yet (the same stated limit as `spot backup verify --full`).
pub(super) struct ShippedInvariants;

impl InvariantRunner for ShippedInvariants {
    fn run<'a>(
        &'a self,
        site: &'a dyn RestoredSite,
    ) -> BoxFuture<'a, spot_backup::Result<InvariantReport>> {
        Box::pin(async move {
            let Some(site) = site.as_any().downcast_ref::<PgSite>() else {
                return Err(BackupError::Unsupported {
                    feature: "invariants over a site this command did not restore".to_owned(),
                });
            };
            let state = site.applied();
            Ok(if state.violations.is_empty() {
                InvariantReport::passed("shipped_checks", state.checks_run)
            } else {
                InvariantReport::failed(
                    "shipped_checks",
                    state.checks_run,
                    state.violations.clone(),
                )
            })
        })
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Flags {
    pub(super) accept_unverified: bool,
    pub(super) migrate_forward: bool,
}

/// Facts about the target, read once.
pub(super) async fn build_site(
    ctx: &mut CommandCtx,
    prep: &Prepared,
    manifest: &BackupManifest,
    new_site: Option<&str>,
    flags: Flags,
    work: PathBuf,
) -> Step<PgSite> {
    let live = new_site.map_or_else(|| prep.site.database.clone(), |n| n.replace('-', "_"));
    let facts = async {
        let backend = admin(&prep.cfg).await?;
        let client = backend.pool().get().await.map_err(pool_error)?;
        let version: String = client
            .query_one("SHOW server_version_num", &[])
            .await
            .map_err(pool_error)?
            .get(0);
        let exists = !client
            .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&live])
            .await
            .map_err(pool_error)?
            .is_empty();
        Ok::<_, String>((version.parse::<u32>().unwrap_or(0) / 10_000, exists))
    };
    let (pg_major, exists) = match facts.await {
        Ok(facts) => facts,
        Err(text) => return Err(stop(ctx, Exit::Failed, format!("backup.io: {text}"))),
    };
    let meta = match spot_cli::apps::serving(ctx.root(), &prep.loaded, &prep.site) {
        Ok(meta) => meta,
        Err(error) => return Err(stop(ctx, Exit::Failed, error.to_string())),
    };
    let catalog_version = match spot_cli::schema::catalog(&meta) {
        Ok(catalog) => catalog.version,
        Err(error) => return Err(stop(ctx, Exit::Failed, error.to_string())),
    };
    let verified = !UNVERIFIED_VERSIONS.contains(&manifest.schema_version);
    Ok(PgSite {
        tenant: prep.site.site.clone(),
        cfg: prep.cfg.clone(),
        live,
        exists,
        clock: ctx.clock().clone(),
        work,
        bin: pg_bin().unwrap_or_default(),
        meta,
        catalog_version,
        manifest_version: manifest.schema_version,
        verified,
        allow_migrate: flags.migrate_forward,
        apps: prep.site.apps.clone(),
        pg_major,
        free: SystemSpaceProbe.free_bytes(ctx.root()).unwrap_or(u64::MAX),
        state: Mutex::new(Applied::default()),
    })
}

/// The library's preflight plus this command's schema rules. Strict (apply) turns an unverified or
/// older schema without its flag into a blocker; otherwise it is only a note.
pub(super) fn assess(
    manifest: &BackupManifest,
    site: &PgSite,
    strict: bool,
    flags: Flags,
) -> (PreflightReport, Vec<String>) {
    let options = spot_backup::RestoreOptions {
        allow_newer_schema: false,
        replace: site.exists,
    };
    let mut report = spot_backup::preflight(manifest, site, &options);
    for blocker in &mut report.blockers {
        if blocker.code == "backup.newer_schema" {
            "restore.too_new".clone_into(&mut blocker.code);
        }
    }
    let mut notes = Vec::new();
    let mut block = |code: &str, message: String| {
        report.blockers.push(PreflightBlocker {
            code: code.to_owned(),
            message,
        })
    };
    if !site.verified {
        let text = format!(
            "schema_version: unverified (the backup recorded {}, which no migration version vouches for)",
            manifest.schema_version
        );
        if strict && !flags.accept_unverified {
            block(
                "restore.schema_unverified",
                format!("{text}; pass --accept-unverified-schema to restore it anyway"),
            );
        } else {
            notes.push(format!("{text}; apply needs --accept-unverified-schema"));
        }
    } else if manifest.schema_version < site.catalog_version {
        let text = format!(
            "schema_version: {} is older than this build's {}; the restored copy must be migrated forward",
            manifest.schema_version, site.catalog_version
        );
        if strict && !flags.migrate_forward {
            block(
                "restore.schema_older",
                format!("{text}; pass --migrate-forward"),
            );
        } else {
            notes.push(format!("{text}; apply needs --migrate-forward"));
        }
    } else if manifest.schema_version == site.catalog_version {
        notes.push(format!(
            "schema_version: {} matches this build",
            manifest.schema_version
        ));
    }
    report.ok = report.blockers.is_empty();
    (report, notes)
}

/// The stored package `id` of the target, opened and checked to belong to the site.
pub(super) async fn open_package(
    ctx: &mut CommandCtx,
    prep: &Prepared,
    id: &str,
) -> Step<PackageFile> {
    let path = match spot_backup::target::package_path(&prep.dir, id) {
        Ok(path) => path,
        Err(error) => {
            return Err(stop(
                ctx,
                Exit::Usage,
                format!("{}: {}", error.code(), error.reason()),
            ));
        }
    };
    // A symbolic link is never followed: only a regular file in the target is a backup.
    if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        return Err(stop(
            ctx,
            Exit::Refused,
            format!("backup.not_found: no backup {id} exists on the target"),
        ));
    }
    let package = match spot_backup::package::open(&path) {
        Ok(package) => package,
        Err(error) => {
            let _ = audit(ctx, prep, "restore_refused", 0, Some(error.code()), false).await;
            return Err(stop(
                ctx,
                Exit::Refused,
                scrub(
                    &format!("{}: {}", error.code(), error.reason()),
                    &prep.secrets,
                ),
            ));
        }
    };
    if package.manifest().tenant != prep.site.site {
        let text = format!(
            "backup.restore_refused: backup {id} belongs to site `{}`, not to `{}`; a backup is never restored into another site",
            package.manifest().tenant,
            prep.site.site
        );
        let _ = audit(
            ctx,
            prep,
            "restore_refused",
            0,
            Some("backup.restore_refused"),
            false,
        )
        .await;
        return Err(stop(ctx, Exit::Refused, text));
    }
    Ok(package)
}

/// Records `kind` in the site's audit chain, opening and closing the chain's connections around
/// the write (a restore ends every session of the live database).
pub(super) async fn audit(
    ctx: &mut CommandCtx,
    prep: &Prepared,
    kind: &str,
    count: u64,
    reason: Option<&str>,
    required: bool,
) -> Result<(), String> {
    let chain = OperatorAudit::open(&prep.loaded, &prep.site, ctx.clock().clone())
        .await
        .map_err(|_| "backup.audit_unavailable: the audit chain is not available".to_owned())?;
    if required {
        chain
            .required(kind, count, reason)
            .map_err(|e| e.to_string())
    } else {
        chain.always(kind, count, reason);
        Ok(())
    }
}

pub(super) fn target_text(new_site: Option<&str>, site: &str) -> String {
    new_site.map_or_else(|| format!("site:{site}"), |n| format!("new:{n}"))
}

pub(super) fn print_blockers(ctx: &mut CommandCtx, report: &PreflightReport) {
    for blocker in &report.blockers {
        let _ = writeln!(ctx.err(), "{}: {}", blocker.code, blocker.message);
    }
}
