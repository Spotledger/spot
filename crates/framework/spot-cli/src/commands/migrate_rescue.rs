//! `spot migrate-rescue HASH --site S --mode down|restore [--confirm TOKEN --approved-by USER]`
//! (WP-MIG-10.T2): the supervised way back after a failed apply. Guide:
//! `docs/running-spot/migrate_rescue.md`.
//!
//! Registered as `migrate-rescue` because `migrate` is a derive command of `main.rs`.
//!
//! * Without `--confirm` it **previews**: the pre-apply snapshot, the counted loss window (rows per
//!   document type a restore replaces) and the confirm token, and changes nothing (exit 0).
//! * With `--confirm TOKEN --approved-by USER` it rescues. `--approved-by` is the second person
//!   (asserted at the host, as in `migrate-hold`; the management API authenticates it). The
//!   maintenance gate must already be closed (`spot migrate-apply maintenance enter`) and the effect
//!   hold active (`spot migrate-hold hold`): anything else is refused before a byte is touched.
//! * `restore` restores the snapshot over the live name; the current database is renamed aside
//!   (`<db>_replaced_<seconds>`) and kept, never dropped. The gate **stays closed**: this host has no
//!   invariant runner or journal service to prove the restored database, so the operator opens the
//!   gate (`spot migrate-apply maintenance leave`) after inspection. `down` needs declared
//!   down-patches, which this host does not hold: it refuses and says to use `restore`.
//!
//! Exit codes: 0 done (or previewed), 1 usage, 2 refused (nothing changed), 3 failed.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Arg, ArgMatches, Command};
use spot_core::{Clock, SiteId};
use spot_migration_apply::{BoxFuture, GateStatus, Progress, SnapshotId, SnapshotPort};
use spot_migration_apply_pg::{PgAdmissionGate, PgApplyJournal, PgApplySchema, PgSnapshotPort};
use spot_migration_ops::hold::HoldInspector;
use spot_migration_ops::rescue::{HostRun, Mode, RescueHost, SnapshotFacts, preview, rescue};
use spot_migration_ops::{LifecycleContext, LifecycleError, PlanHash};
use spot_migration_plan::ReleasePlan;
use spot_postgres::{PgBackend, PgConfig, Tls};
use spot_snapshot::{PgDumpProvider, RestoreOptions, RestoreTarget, SnapshotTarget};
use spot_storage_contract::Catalog;

use super::migrate_plan::{
    Opened, as_arg, build_lifecycle, exit_of, principal_of, role_arg, site_arg,
};
use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "migrate-rescue";

/// Arguments and help.
pub fn command() -> Command {
    Command::new(NAME)
        .about("Preview or run the supervised rescue of a failed or applied plan")
        .arg(Arg::new("hash").value_name("HASH").required(true))
        .arg(site_arg())
        .arg(
            Arg::new("mode")
                .long("mode")
                .value_name("MODE")
                .required(true)
                .value_parser(["down", "restore"])
                .help("down (declared down-patches) or restore (the pre-apply snapshot)"),
        )
        .arg(
            Arg::new("confirm")
                .long("confirm")
                .value_name("TOKEN")
                .help("The token the preview printed; it acknowledges the counted loss"),
        )
        .arg(
            Arg::new("approved_by")
                .long("approved-by")
                .value_name("USER")
                .help("The second person who confirms (must differ from --as)"),
        )
        .arg(as_arg())
        .arg(role_arg())
}

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

fn pg_bin() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SPOT_PG_BIN").map(PathBuf::from) {
        return dir.is_absolute().then_some(dir);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|dir| dir.join("pg_dump").is_file() && dir.is_absolute())
}

/// The host side of a rescue for one site, over its PostgreSQL database.
pub struct CliRescueHost {
    site: SiteId,
    backend: PgBackend,
    catalog: Catalog,
    clock: Arc<dyn Clock>,
    bin: PathBuf,
    inspector: Arc<dyn HoldInspector>,
}

impl CliRescueHost {
    fn snapshot_port(&self) -> Result<PgSnapshotPort, LifecycleError> {
        let cfg = self.backend.config().clone();
        let live = connection(&cfg, &cfg.dbname)
            .ok_or(LifecycleError::PortUnavailable { port: "connection" })?;
        let provider = Arc::new(PgDumpProvider::new(
            self.bin.clone(),
            Arc::clone(&self.clock),
        ));
        let dir = std::env::var_os("SPOT_SNAPSHOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("spot-preapply-{}", self.site.as_str()));
        // Restoring over the live name moves the current database aside; it is never dropped.
        let options = RestoreOptions {
            replace: true,
            replace_token: cfg.dbname.clone(),
            ..RestoreOptions::default()
        };
        Ok(PgSnapshotPort::new(
            PgApplySchema::new(self.backend.pool()),
            self.site.clone(),
            provider,
            SnapshotTarget::Database {
                connection: live.clone(),
                tenant: self.site.as_str().to_owned(),
                database: cfg.dbname.clone(),
            },
            RestoreTarget::Database {
                connection: live,
                database: cfg.dbname,
            },
            dir,
            Arc::clone(&self.clock),
        )
        .with_restore_options(options))
    }

    async fn counts(&self) -> Result<BTreeMap<String, u64>, LifecycleError> {
        let client = self
            .backend
            .pool()
            .get()
            .await
            .map_err(|_| LifecycleError::Store)?;
        let mut counts = BTreeMap::new();
        for entity in self.catalog.entities() {
            let name = entity.name.to_string();
            if !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            {
                continue;
            }
            let row = client
                .query_one(&format!("SELECT count(*) FROM \"{name}\""), &[])
                .await
                .map_err(|_| LifecycleError::Store)?;
            let n: i64 = row.get(0);
            if let Some(n) = u64::try_from(n).ok().filter(|n| *n > 0) {
                counts.insert(name, n);
            }
        }
        Ok(counts)
    }
}

impl RescueHost for CliRescueHost {
    fn snapshot<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        _hash: &'a PlanHash,
    ) -> BoxFuture<'a, Result<Option<SnapshotFacts>, LifecycleError>> {
        Box::pin(async move {
            let journal = PgApplyJournal::new(self.backend.pool(), self.site.clone());
            let entries = spot_migration_apply::ApplyJournal::entries(&journal)
                .await
                .map_err(|_| LifecycleError::Store)?;
            let at = entries.first().map_or(0, |e| e.at);
            let progress = Progress::fold(entries);
            if progress.plan_digest.as_deref() != Some(plan.plan_digest.as_str()) {
                return Ok(None);
            }
            let Some(id) = progress.snapshot.and_then(|s| SnapshotId::new(s).ok()) else {
                return Ok(None);
            };
            let port = self.snapshot_port()?;
            let present = port.exists(&id).await.map_err(|_| LifecycleError::Store)?;
            Ok(Some(SnapshotFacts {
                id: id.to_string(),
                at,
                verified: present,
            }))
        })
    }

    fn loss_window(
        &self,
        _snapshot_at: i64,
    ) -> BoxFuture<'_, Result<BTreeMap<String, u64>, LifecycleError>> {
        // There is no creation time on a stored document, so the window is the upper bound: every
        // row now in the database, which a restore replaces. The journal service narrows it.
        Box::pin(self.counts())
    }

    fn quiesced(&self) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        Box::pin(async move {
            let gate = PgAdmissionGate::new(
                self.backend.pool(),
                self.site.clone(),
                Arc::clone(&self.clock),
                std::time::Duration::from_secs(1),
            );
            let now = i64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000)
                .unwrap_or(i64::MAX);
            let status = gate.refresh(now).await.map_err(|_| LifecycleError::Store)?;
            let held = self.inspector.state().await?.held;
            Ok(matches!(status, GateStatus::Closed { .. }) && held)
        })
    }

    fn run<'a>(
        &'a self,
        mode: Mode,
        _plan: &'a ReleasePlan,
        snapshot: &'a SnapshotFacts,
        now: i64,
    ) -> BoxFuture<'a, Result<HostRun, LifecycleError>> {
        Box::pin(async move {
            if mode == Mode::Down {
                // No declared down-patches are available to this host: restore is the way back.
                return Err(LifecycleError::RescueRefused);
            }
            let unreplayed = self.counts().await?;
            let id =
                SnapshotId::new(snapshot.id.clone()).map_err(|_| LifecycleError::RescueRefused)?;
            let restored = self
                .snapshot_port()?
                .restore(&id, now)
                .await
                .map_err(|_| LifecycleError::ApplyFailed)?;
            Ok(HostRun {
                text: format!(
                    "restored snapshot {} into {}; the previous database was kept aside; nothing \
                     was re-entered (no journal service), so the rows counted above are in the \
                     kept database; the gate stays closed until you inspect and leave maintenance",
                    restored.snapshot, restored.location
                ),
                unreplayed,
            })
        })
    }
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let site = matches
        .get_one::<String>("site")
        .cloned()
        .unwrap_or_default();
    let hash = matches
        .get_one::<String>("hash")
        .cloned()
        .unwrap_or_default();
    let mode = matches
        .get_one::<String>("mode")
        .and_then(|m| Mode::parse(m))
        .unwrap_or(Mode::Restore);
    let Ok(site_id) = SiteId::new(&site) else {
        writeln!(ctx.err(), "`{site}` is not a valid site name")?;
        return Ok(Exit::Usage);
    };
    if ctx.site(&site).is_err() {
        writeln!(ctx.err(), "there is no such site")?;
        return Ok(Exit::Usage);
    }
    let Ok(backend) = ctx.backend(&site).await else {
        writeln!(ctx.err(), "the site has no usable PostgreSQL backend")?;
        return Ok(Exit::Failed);
    };
    let Some(bin) = pg_bin() else {
        writeln!(
            ctx.err(),
            "pg_dump was not found: set SPOT_PG_BIN to its directory (absolute)"
        )?;
        return Ok(Exit::Refused);
    };
    let loaded = spot_cli::config::load(ctx.root(), false)?;
    let site_config = ctx.site(&site)?.clone();
    let meta = spot_cli::apps::serving(ctx.root(), &loaded, &site_config)?;
    let catalog = spot_cli::schema::catalog(&meta)?;
    let clock = Arc::clone(ctx.clock());
    let Opened { lifecycle, store } =
        build_lifecycle(&backend, &site, Arc::clone(&clock), principal_of(matches))?;
    let parts = spot_cli::services::effect_hold::build(
        site_id.clone(),
        spot_cli::services::effect_hold::pg_ledger(&backend),
        Arc::clone(&clock),
        &spot_cli::services::effect_hold::Settings::default(),
        Arc::new(spot_cli::services::effect_hold::TracingHoldAudit),
    );
    let host: Arc<dyn RescueHost> = Arc::new(CliRescueHost {
        site: site_id,
        backend: (*backend).clone(),
        catalog,
        clock,
        bin,
        inspector: Arc::clone(&parts.inspector),
    });
    if let Err(error) = super::migrate_plan::on_plain_thread(move || store.ensure_present())? {
        writeln!(ctx.err(), "{error}")?;
        return Ok(exit_of(&error));
    }
    let lifecycle: Arc<LifecycleContext> = Arc::new(lifecycle);
    let Some(confirm) = matches.get_one::<String>("confirm") else {
        return preview_cmd(ctx, &lifecycle, &host, &hash, mode, &site).await;
    };
    let second = matches
        .get_one::<String>("approved_by")
        .cloned()
        .unwrap_or_default();
    match rescue(&lifecycle, &host, &hash, mode, confirm, &second).await {
        Ok(report) => {
            writeln!(
                ctx.out(),
                "plan {} rescued: {}",
                report.hash,
                report.run.text
            )?;
            for (doctype, count) in &report.run.unreplayed {
                writeln!(ctx.out(), "  not re-entered: {doctype}: {count} row(s)")?;
            }
            Ok(Exit::Ok)
        }
        Err(error) => {
            writeln!(ctx.err(), "{error}")?;
            Ok(exit_of(&error))
        }
    }
}

async fn preview_cmd(
    ctx: &mut CommandCtx,
    lifecycle: &Arc<LifecycleContext>,
    host: &Arc<dyn RescueHost>,
    hash: &str,
    mode: Mode,
    site: &str,
) -> spot_cli::Result<Exit> {
    match preview(lifecycle, host, hash, mode).await {
        Ok(shown) => {
            writeln!(
                ctx.out(),
                "rescue of plan {} ({}) on site {site}: snapshot {} taken at {}",
                shown.hash,
                mode.as_str(),
                shown.snapshot.id,
                shown.snapshot.at
            )?;
            writeln!(
                ctx.out(),
                "a restore replaces everything now in the database:"
            )?;
            for (doctype, count) in &shown.loss {
                writeln!(ctx.out(), "  {doctype}: {count} row(s)")?;
            }
            writeln!(
                ctx.out(),
                "nothing was changed. To go ahead: close the gate and hold effects, then run\n  \
                 spot migrate-rescue {} --site {site} --mode {} --confirm {} --approved-by <SECOND PERSON>",
                shown.hash,
                mode.as_str(),
                shown.token
            )?;
            Ok(Exit::Ok)
        }
        Err(error) => {
            writeln!(ctx.err(), "{error}")?;
            Ok(exit_of(&error))
        }
    }
}
