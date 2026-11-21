//! `spot migrate-rehearse run HASH | clean --site S`: rehearse an approved plan on a clone
//! (WP-MIG-06.T5). Guide: `docs/running-spot/migrate_rehearse.md`.
//!
//! * `run HASH --site S [--as USER] [--budget-minutes N] [--json]`: snapshots the site with
//!   `pg_dump`, restores it into a scratch database (named from the plan hash and the clock, never
//!   the live name), applies the plan to the scratch with the real apply engine, measures the
//!   fingerprint, runs the invariants, stores the report and, when it passes, marks the plan
//!   `rehearsed`. The scratch is destroyed on every exit path. Exit 0 pass, 2 refused, 3 failed
//!   (the ledger key of the stored report is printed).
//! * `clean --site S`: drops the scratch databases that killed rehearsals of this site left
//!   behind (only databases that carry the scratch marker; never the live database).
//!
//! The `pg_dump` directory comes from `SPOT_PG_BIN` (an absolute path) or the first `pg_dump` on
//! `PATH`. Snapshots are written under the system temporary directory (`SPOT_REHEARSAL_DIR`
//! overrides it) and removed with the scratch.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_core::{Clock, SiteId};
use spot_migration_apply::BoxFuture;
use spot_migration_ops::rehearse::{EngineRunner, RehearsalHost, RehearsalRunner, clean, rehearse};
use spot_migration_ops::{LifecycleContext, LifecycleError, PlanHash, RehearsalLedger};
use spot_migration_plan::ReleasePlan;
use spot_postgres::{PgBackend, PgConfig, Tls};
use spot_rehearsal::{
    CloneRequest, FingerprintSpec, PgMigrateApplier, Rehearsal, SnapshotScratch,
    StorageFingerprintSource,
};
use spot_snapshot::{
    Destination, PgDumpProvider, RestoreTarget, ScratchMarker, SnapshotTarget, drop_scratch,
    scratch::MARKER_PREFIX,
};
use spot_storage_contract::Catalog;

use super::migrate_plan::{
    Opened, as_arg, build_lifecycle, exit_of, principal_of, role_arg, site_arg,
};
use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "migrate-rehearse";

/// The default wall-clock budget, in minutes.
pub const DEFAULT_BUDGET_MINUTES: u64 = 30;

/// The prefix of every scratch database a rehearsal makes (followed by the site and the plan).
pub const SCRATCH_PREFIX: &str = "spot_rehearsal_";

/// Arguments and help.
pub fn command() -> Command {
    Command::new(NAME)
        .about(
            "Rehearse an approved migration plan on a clone of the site (never touches the site)",
        )
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("run")
                .about("Rehearse the plan with this hash; mark it rehearsed when it passes")
                .arg(Arg::new("hash").value_name("HASH").required(true))
                .arg(site_arg())
                .arg(as_arg())
                .arg(role_arg())
                .arg(
                    Arg::new("budget_minutes")
                        .long("budget-minutes")
                        .value_name("N")
                        .value_parser(clap::value_parser!(u64).range(1..=1440))
                        .help("Abort and clean up after this many minutes (default 30)"),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .action(ArgAction::SetTrue)
                        .help("Print the result as JSON"),
                ),
        )
        .subcommand(
            Command::new("clean")
                .about("Drop the scratch databases killed rehearsals left behind")
                .arg(site_arg())
                .arg(as_arg())
                .arg(role_arg()),
        )
}

/// The site part of a scratch name: lower-case identifier characters only, at most 15 so the
/// whole name stays within PostgreSQL's 63 bytes (`clean` then also sees a site that shares the
/// first 15 characters, which is harmless: it only drops databases that carry the scratch marker).
fn site_part(site: &str) -> String {
    site.chars()
        .take(15)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The prefix of every scratch database of `site`'s rehearsals.
#[must_use]
pub fn site_scratch_prefix(site: &str) -> String {
    format!("{SCRATCH_PREFIX}{}_", site_part(site))
}

/// The scratch database name of a rehearsal: the site's prefix, twelve characters of the plan
/// hash and the clock, all lower-case identifier characters, so it can never be a tenant database
/// name and is recognisable by [`SCRATCH_PREFIX`].
#[must_use]
pub fn scratch_name(site: &str, hash: &PlanHash, now: i64) -> String {
    let short = hash.as_str().get(..12).unwrap_or(hash.as_str());
    format!("{}{short}_{now}", site_scratch_prefix(site))
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

fn work_dir() -> PathBuf {
    std::env::var_os("SPOT_REHEARSAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Free kilobytes of the filesystem holding `dir`, from `df -Pk` (POSIX). Anywhere that cannot
/// say is a refusal: the guard fails closed.
fn free_bytes_of(dir: &std::path::Path) -> Result<u64, LifecycleError> {
    let output = std::process::Command::new("df")
        .arg("-Pk")
        .arg(dir)
        .output()
        .map_err(|_| LifecycleError::PortUnavailable { port: "disk_space" })?;
    let text = String::from_utf8_lossy(&output.stdout);
    let available = text
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().nth(3))
        .and_then(|kilobytes| kilobytes.parse::<u64>().ok())
        .ok_or(LifecycleError::PortUnavailable { port: "disk_space" })?;
    Ok(available.saturating_mul(1024))
}

/// The host side of a rehearsal for one site.
pub struct CliHost {
    site: String,
    backend: PgBackend,
    catalog: Catalog,
    clock: Arc<dyn Clock>,
    ledger: Arc<dyn RehearsalLedger>,
    bin: PathBuf,
    budget: Duration,
}

impl RehearsalHost for CliHost {
    fn runner(
        &self,
        _plan: &ReleasePlan,
        hash: &PlanHash,
        now: i64,
    ) -> Result<Arc<dyn RehearsalRunner>, LifecycleError> {
        let cfg = self.backend.config().clone();
        let scratch = scratch_name(&self.site, hash, now);
        let (Some(live), Some(scratch_conn)) =
            (connection(&cfg, &cfg.dbname), connection(&cfg, &scratch))
        else {
            return Err(LifecycleError::PortUnavailable { port: "connection" });
        };
        let target = RestoreTarget::Database {
            connection: scratch_conn,
            database: scratch.clone(),
        };
        let mut request = CloneRequest::new(
            SnapshotTarget::Database {
                connection: live.clone(),
                tenant: self.site.clone(),
                database: cfg.dbname.clone(),
            },
            Destination {
                dir: work_dir().join(format!("{scratch}-snapshot")),
            },
            target.clone(),
        );
        request.reaper = Some(Arc::new(SnapshotScratch::new(live, target)));
        let provider = Arc::new(PgDumpProvider::new(
            self.bin.clone(),
            Arc::clone(&self.clock),
        ));
        // No fingerprint source is registered yet (the ERP registers its queries), so the spec is
        // empty and the measurement is a recorded pass; the source is never asked anything.
        let fingerprint = Arc::new(StorageFingerprintSource::new(
            Arc::new(self.backend.clone()),
            Arc::new(self.catalog.clone()),
        ));
        let engine = Rehearsal::new(
            provider,
            Arc::new(PgMigrateApplier::new(self.catalog.clone())),
            fingerprint,
            FingerprintSpec::new(Vec::new()),
            request,
        );
        Ok(Arc::new(EngineRunner(engine)))
    }

    fn free_bytes(&self) -> BoxFuture<'_, Result<u64, LifecycleError>> {
        Box::pin(async move { free_bytes_of(&work_dir()) })
    }

    fn snapshot_bytes(&self) -> BoxFuture<'_, Result<u64, LifecycleError>> {
        Box::pin(async move {
            let client = self
                .backend
                .pool()
                .get()
                .await
                .map_err(|_| LifecycleError::Store)?;
            let row = client
                .query_one("SELECT pg_database_size(current_database())", &[])
                .await
                .map_err(|_| LifecycleError::Store)?;
            let size: i64 = row.get(0);
            u64::try_from(size).map_err(|_| LifecycleError::Store)
        })
    }

    fn budget(&self) -> Duration {
        self.budget
    }

    fn ledger(&self) -> Arc<dyn RehearsalLedger> {
        Arc::clone(&self.ledger)
    }

    fn clean_leftovers(&self) -> BoxFuture<'_, Result<Vec<String>, LifecycleError>> {
        Box::pin(async move {
            let cfg = self.backend.config().clone();
            let Some(live) = connection(&cfg, &cfg.dbname) else {
                return Err(LifecycleError::PortUnavailable { port: "connection" });
            };
            // Only databases of this site's rehearsals that carry the scratch marker.
            let like = format!("{}%", site_scratch_prefix(&self.site).replace('_', "\\_"));
            let client = self
                .backend
                .pool()
                .get()
                .await
                .map_err(|_| LifecycleError::Store)?;
            let rows = client
                .query(
                    "SELECT datname, shobj_description(oid, 'pg_database') FROM pg_database \
                     WHERE datname LIKE $1 AND datname <> current_database()",
                    &[&like],
                )
                .await
                .map_err(|_| LifecycleError::Store)?;
            let mut dropped = Vec::new();
            for row in rows {
                let name: String = row.get(0);
                let comment: Option<String> = row.get(1);
                let Some(id) = comment
                    .as_deref()
                    .and_then(|c| c.strip_prefix(MARKER_PREFIX))
                else {
                    continue;
                };
                let target = RestoreTarget::Database {
                    connection: live.clone(),
                    database: name.clone(),
                };
                if drop_scratch(&live, &target, &ScratchMarker::for_snapshot_id(id))
                    .await
                    .is_ok()
                {
                    dropped.push(name);
                }
            }
            Ok(dropped)
        })
    }
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot migrate-rehearse <run|clean> --site <SITE>"
        )?;
        return Ok(Exit::Usage);
    };
    let site = sub.get_one::<String>("site").cloned().unwrap_or_default();
    if ctx.site(&site).is_err() {
        writeln!(ctx.err(), "there is no such site")?;
        return Ok(Exit::Usage);
    }
    let Ok(backend) = ctx.backend(&site).await else {
        writeln!(ctx.err(), "the site has no usable PostgreSQL backend")?;
        return Ok(Exit::Failed);
    };
    if SiteId::new(&site).is_err() {
        writeln!(ctx.err(), "`{site}` is not a valid site name")?;
        return Ok(Exit::Usage);
    }
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
    let minutes = sub
        .try_get_one::<u64>("budget_minutes")
        .ok()
        .flatten()
        .copied()
        .unwrap_or(DEFAULT_BUDGET_MINUTES);
    let host: Arc<dyn RehearsalHost> = Arc::new(CliHost {
        site,
        backend: (*backend).clone(),
        catalog,
        clock,
        ledger: store.clone(),
        bin,
        budget: Duration::from_secs(minutes * 60),
    });
    let lifecycle: Arc<LifecycleContext> = Arc::new(lifecycle);
    match verb {
        "run" => {
            let hash = sub.get_one::<String>("hash").cloned().unwrap_or_default();
            let json = sub.get_flag("json");
            run_rehearse(ctx, &lifecycle, &host, &hash, json).await
        }
        _ => match clean(&lifecycle, host.as_ref()).await {
            Ok(dropped) => {
                for name in &dropped {
                    writeln!(ctx.out(), "dropped scratch database {name}")?;
                }
                writeln!(ctx.out(), "{} scratch database(s) removed", dropped.len())?;
                Ok(Exit::Ok)
            }
            Err(error) => {
                writeln!(ctx.err(), "{error}")?;
                Ok(exit_of(&error))
            }
        },
    }
}

async fn run_rehearse(
    ctx: &mut CommandCtx,
    lifecycle: &Arc<LifecycleContext>,
    host: &Arc<dyn RehearsalHost>,
    hash: &str,
    json: bool,
) -> spot_cli::Result<Exit> {
    writeln!(
        ctx.out(),
        "rehearsing plan {hash} on a clone: snapshot, restore, apply, verify"
    )?;
    match rehearse(lifecycle, host, hash).await {
        Ok(report) => {
            if json {
                writeln!(
                    ctx.out(),
                    "{{\"plan_hash\":\"{}\",\"passed\":{},\"outcome\":\"{}\",\"ledger_at\":{},\"state\":\"{}\",\"scratch_cleaned\":{}}}",
                    report.hash,
                    report.passed,
                    report.outcome,
                    report.recorded_at,
                    report.state.as_str(),
                    report.cleaned
                )?;
            } else {
                write!(ctx.out(), "{}", report.text)?;
                writeln!(
                    ctx.out(),
                    "result: {} ({}); plan state {}; report stored at ledger key {}{}",
                    if report.passed { "pass" } else { "FAIL" },
                    report.outcome,
                    report.state.as_str(),
                    report.recorded_at,
                    if report.cleaned {
                        ""
                    } else {
                        "; the scratch could not be dropped: run `spot migrate-rehearse clean`"
                    }
                )?;
            }
            Ok(if report.passed {
                Exit::Ok
            } else {
                Exit::Failed
            })
        }
        Err(error) => {
            writeln!(ctx.err(), "{error}")?;
            Ok(exit_of(&error))
        }
    }
}
