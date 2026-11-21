//! `spot migrate-apply apply|maintenance` (WP-MIG-07.T3): the lifecycle's dangerous verb as the
//! operator at the host. Guide: `docs/running-spot/migrate_apply.md`.
//!
//! Registered as `migrate-apply` because `migrate` is a derive command of `main.rs`; routing
//! `spot migrate apply` here is the one-line change of the task's open question.
//!
//! * `apply HASH --site S [--resume] [--as USER]`: applies the approved (and, where required,
//!   rehearsed) plan through `spot_migration_ops::apply::apply`: plan-state lock, maintenance gate,
//!   effect hold, named pre-apply snapshot, checkpointed steps, verification, restore on failure.
//!   Progress goes to stderr. On failure it prints the single next action and exits 3.
//! * `maintenance enter|leave --site S [--reason TEXT]`: closes or opens the site's maintenance
//!   gate (the control schema row every node polls).
//!
//! The existing `spot migrate --apply` keeps using the STO-11 engine unless the site's
//! `sites/<site>/migrations.toml` says `require_plan = true` (see `spot_cli::ops`).
//!
//! Exit codes: 0 done, 1 usage, 2 refused by policy or permission, 3 failed.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_core::{Clock, SiteId};
use spot_migration_apply::{
    AdmissionGate, ApplyEngine, ApplyOptions, ApplyService, BoxFuture, HoldServiceBridge,
    ProgressEvent, ProgressSink, SnapshotPort,
};
use spot_migration_apply_pg::{
    PgAdmissionGate, PgApplyJournal, PgApplySchema, PgDriftVerifier, PgSnapshotPort, PgStepRunner,
};
use spot_migration_ops::apply::{ApplyHost, apply};
use spot_migration_ops::{LifecycleContext, LifecycleError, PlanHash};
use spot_migration_plan::ReleasePlan;
use spot_postgres::{PgBackend, PgConfig, Tls};
use spot_snapshot::{
    PgDumpProvider, RestoreOptions, RestoreTarget, SnapshotProvider, SnapshotTarget,
};
use spot_storage_contract::Catalog;

use super::migrate_plan::{
    Opened, as_arg, build_lifecycle, exit_of, on_plain_thread, principal_of, role_arg, site_arg,
};
use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "migrate-apply";

/// The gate's cache lifetime on this node.
const GATE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// Arguments and help.
pub fn command() -> Command {
    Command::new(NAME)
        .about("Apply an approved migration plan, or close and open the maintenance gate")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("apply")
                .about("Apply the approved plan: gate, hold, snapshot, steps, verify, restore on failure")
                .arg(Arg::new("hash").value_name("HASH").required(true))
                .arg(site_arg())
                .arg(
                    Arg::new("resume")
                        .long("resume")
                        .action(ArgAction::SetTrue)
                        .help("Continue an unfinished or failed apply from its last checkpoint"),
                )
                .arg(as_arg())
                .arg(role_arg()),
        )
        .subcommand(
            Command::new("maintenance")
                .about("Close or open the site's maintenance gate")
                .subcommand_required(true)
                .subcommand(
                    Command::new("enter")
                        .arg(site_arg())
                        .arg(
                            Arg::new("reason")
                                .long("reason")
                                .value_name("TEXT")
                                .default_value("operator maintenance"),
                        ),
                )
                .subcommand(Command::new("leave").arg(site_arg())),
        )
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

fn snapshot_dir(site: &str) -> PathBuf {
    std::env::var_os("SPOT_SNAPSHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("spot-preapply-{site}"))
}

/// Progress lines on stderr: the step, never a value.
struct StderrProgress;

impl ProgressSink for StderrProgress {
    fn on(&self, event: ProgressEvent) {
        let _ = writeln!(std::io::stderr(), "apply: {event:?}");
    }
}

/// The host side of an apply for one site, over its PostgreSQL database.
pub struct CliApplyHost {
    site: SiteId,
    backend: PgBackend,
    catalog: Catalog,
    clock: Arc<dyn Clock>,
    bin: PathBuf,
    hold: Arc<HoldServiceBridge>,
    require_rehearsal: bool,
}

impl ApplyHost for CliApplyHost {
    fn engine<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        _hash: &'a PlanHash,
        now: i64,
    ) -> BoxFuture<'a, Result<Arc<dyn ApplyService>, LifecycleError>> {
        Box::pin(async move {
            let unavailable = |port: &'static str| LifecycleError::PortUnavailable { port };
            let pool = self.backend.pool();
            let schema = PgApplySchema::new(pool.clone());
            schema
                .prepare()
                .await
                .map_err(|_| unavailable("control_schema"))?;
            let cfg = self.backend.config().clone();
            let live = connection(&cfg, &cfg.dbname).ok_or_else(|| unavailable("connection"))?;
            let provider: Arc<dyn SnapshotProvider> = Arc::new(PgDumpProvider::new(
                self.bin.clone(),
                Arc::clone(&self.clock),
            ));
            let snapshot: Arc<dyn SnapshotPort> = Arc::new(
                PgSnapshotPort::new(
                    schema,
                    self.site.clone(),
                    provider,
                    SnapshotTarget::Database {
                        connection: live.clone(),
                        tenant: self.site.as_str().to_owned(),
                        database: cfg.dbname.clone(),
                    },
                    RestoreTarget::Database {
                        connection: live,
                        database: cfg.dbname.clone(),
                    },
                    snapshot_dir(self.site.as_str()),
                    Arc::clone(&self.clock),
                )
                // A restore after a failed step goes over the live name: the current database is
                // renamed aside and kept (never dropped), which needs the replace confirmation.
                .with_restore_options(RestoreOptions {
                    replace: true,
                    replace_token: cfg.dbname.clone(),
                    ..RestoreOptions::default()
                }),
            );
            let mut runner = PgStepRunner::new(
                self.backend.clone(),
                self.site.clone(),
                self.catalog.clone(),
                plan.steps(),
                Arc::clone(&self.clock),
            );
            runner
                .prepare()
                .await
                .map_err(|_| unavailable("step_runner"))?;
            let gate = Arc::new(PgAdmissionGate::new(
                pool.clone(),
                self.site.clone(),
                Arc::clone(&self.clock),
                GATE_TTL,
            ));
            gate.refresh(now)
                .await
                .map_err(|_| unavailable("maintenance_gate"))?;
            let engine = ApplyEngine::new(
                self.site.clone(),
                Arc::new(PgApplyJournal::new(pool, self.site.clone())),
                gate,
                self.hold.clone(),
                snapshot,
                Arc::new(runner),
                Arc::new(PgDriftVerifier::new(
                    self.backend.clone(),
                    self.catalog.clone(),
                )),
            )
            .with_progress(Arc::new(StderrProgress))
            .with_options(ApplyOptions::default());
            Ok(Arc::new(engine) as Arc<dyn ApplyService>)
        })
    }

    fn rehearsal_required(&self) -> bool {
        self.require_rehearsal
    }
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    match matches.subcommand() {
        Some(("apply", sub)) => apply_cmd(sub, ctx).await,
        Some(("maintenance", sub)) => match sub.subcommand() {
            Some(("enter", s)) => maintenance(s, ctx, true).await,
            Some(("leave", s)) => maintenance(s, ctx, false).await,
            _ => usage(ctx),
        },
        _ => usage(ctx),
    }
}

fn usage(ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    writeln!(
        ctx.err(),
        "usage: spot migrate-apply <apply HASH|maintenance enter|leave> --site <SITE>"
    )?;
    Ok(Exit::Usage)
}

async fn apply_cmd(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let site = sub.get_one::<String>("site").cloned().unwrap_or_default();
    let hash = sub.get_one::<String>("hash").cloned().unwrap_or_default();
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
        build_lifecycle(&backend, &site, Arc::clone(&clock), principal_of(sub))?;
    let hold_parts = spot_cli::services::effect_hold::build(
        site_id.clone(),
        spot_cli::services::effect_hold::pg_ledger(&backend),
        Arc::clone(&clock),
        &spot_cli::services::effect_hold::Settings::default(),
        Arc::new(spot_cli::services::effect_hold::TracingHoldAudit),
    );
    let host: Arc<dyn ApplyHost> = Arc::new(CliApplyHost {
        site: site_id,
        backend: (*backend).clone(),
        catalog,
        clock,
        bin,
        hold: Arc::new(HoldServiceBridge::new(Arc::clone(&hold_parts.service))),
        require_rehearsal: spot_cli::ops::requires_rehearsal(ctx.root(), &site),
    });
    let present = on_plain_thread(move || store.ensure_present())?;
    if let Err(error) = present {
        writeln!(ctx.err(), "{error}")?;
        return Ok(exit_of(&error));
    }
    let lifecycle: Arc<LifecycleContext> = Arc::new(lifecycle);
    let resume = sub.get_flag("resume");
    writeln!(
        ctx.out(),
        "applying plan {hash}{}: gate, hold, snapshot, steps, verify",
        if resume { " (resume)" } else { "" }
    )?;
    match apply(&lifecycle, &host, &hash, resume).await {
        Ok(report) => {
            writeln!(ctx.out(), "plan {}: {}", report.hash, report.text)?;
            // After the schema step: the platform's missing system rows (never mid-checkpoint).
            if spot_cli::ops::migrate_seed::has_platform(&site_config) {
                match spot_cli::ops::migrate_seed::seed_missing(&backend, &meta).await {
                    Ok(added) => {
                        write!(
                            ctx.out(),
                            "{}",
                            spot_cli::ops::migrate_seed::applied_lines(&added)
                        )?;
                    }
                    Err(spot_cli::Error(cause)) => {
                        writeln!(
                            ctx.err(),
                            "{}",
                            spot_cli::ops::migrate_seed::failure(&cause)
                        )?;
                        return Ok(Exit::Failed);
                    }
                }
            }
            Ok(Exit::Ok)
        }
        Err(error) => {
            writeln!(ctx.err(), "{error}")?;
            if matches!(error, LifecycleError::ApplyFailed) {
                writeln!(
                    ctx.err(),
                    "next: `spot migrate-apply apply {hash} --site {site} --resume` to continue, or \
                     `spot migrate rescue {hash}` to go back"
                )?;
            }
            Ok(exit_of(&error))
        }
    }
}

async fn maintenance(
    sub: &ArgMatches,
    ctx: &mut CommandCtx,
    enter: bool,
) -> spot_cli::Result<Exit> {
    let site = sub.get_one::<String>("site").cloned().unwrap_or_default();
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
    let clock = Arc::clone(ctx.clock());
    let now = i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX);
    let schema = PgApplySchema::new(backend.pool());
    if schema.prepare().await.is_err() {
        writeln!(ctx.err(), "the control schema could not be prepared")?;
        return Ok(Exit::Failed);
    }
    let gate = PgAdmissionGate::new(backend.pool(), site_id, clock, GATE_TTL);
    let result = if enter {
        let reason = sub.get_one::<String>("reason").cloned().unwrap_or_default();
        match gate.close(&reason, now).await {
            // Fail closed: the gate must be proven closed before anything starts.
            Ok(()) => gate.drain(30_000_000, now).await.map(|_| ()),
            Err(error) => Err(error),
        }
    } else {
        gate.open(now).await
    };
    match result {
        Ok(()) => {
            writeln!(
                ctx.out(),
                "site {site}: maintenance gate {}",
                if enter { "closed" } else { "open" }
            )?;
            Ok(Exit::Ok)
        }
        Err(_) => {
            writeln!(
                ctx.err(),
                "the maintenance gate could not be changed; treat it as closed"
            )?;
            Ok(Exit::Failed)
        }
    }
}
