//! `spot migrate-audit list|show|verify` (WP-MIG-11.T2): the migration audit as the operator at the
//! host. Guide: `docs/running-spot/migrate_audit.md`.
//!
//! Registered as `migrate-audit` because `migrate` is a derive command of `main.rs`.
//!
//! * `list --site S [--plan HASH] [--since DATE] [--json]`: the runs of the site's migration chain.
//! * `show RUN_ID --site S [--explain]`: one run with its chain entries (sequence numbers and
//!   hashes), or in plain language.
//! * `verify --site S`: re-walks the chain and names the first broken link.
//!
//! The chain is the site's own (`migration:<site>`): no `--chain` or foreign scope exists, so the
//! command cannot read another site's runs. Output carries ids, digests, counts and codes, never a
//! row value. Exit codes: 0 done (verify: intact), 1 usage, 2 refused or the chain has a break,
//! 3 failed.

use std::sync::Arc;
use std::time::Duration;

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_audit_chain::{KeyRing, Verifier};
use spot_audit_store::PgAuditStore;
use spot_core::{Clock, SiteId};
use spot_migration_apply::BoxFuture;
use spot_migration_approval::{
    ApprovalError, ApprovalPolicy, ApprovalService, ApprovalStore, AuditSink,
};
use spot_migration_audit::{ChainRunStore, RoleAccess, RunFilter, RunStore};
use spot_migration_ops::audit::{ChainStatus, ChainVerifier, explain, list, show, verify};
use spot_migration_ops::{Actor, Channel, LifecycleContext, LifecycleError, LifecyclePorts};
use spot_migration_ops_pg::{PgMigrationStore, RecordingAlert};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;

use super::migrate_plan::{DEFAULT_ROLE, as_arg, exit_of, principal_of, role_arg, site_arg};
use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "migrate-audit";

/// The audit chain scope of a site's migration runs.
#[must_use]
pub fn scope_of(site: &str) -> String {
    format!("migration:{site}")
}

/// Arguments and help.
pub fn command() -> Command {
    Command::new(NAME)
        .about("List and explain migration runs, and verify their tamper-evident chain")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("list")
                .about("List the site's migration runs")
                .arg(site_arg())
                .arg(
                    Arg::new("plan")
                        .long("plan")
                        .value_name("HASH")
                        .help("Only runs of this plan"),
                )
                .arg(
                    Arg::new("since")
                        .long("since")
                        .value_name("DATE")
                        .help("Only runs that started on or after this day (YYYY-MM-DD, UTC)"),
                )
                .arg(Arg::new("json").long("json").action(ArgAction::SetTrue))
                .arg(as_arg())
                .arg(role_arg()),
        )
        .subcommand(
            Command::new("show")
                .about("Show one run with its chain entries, or explain it")
                .arg(Arg::new("run").value_name("RUN_ID").required(true))
                .arg(site_arg())
                .arg(
                    Arg::new("explain")
                        .long("explain")
                        .action(ArgAction::SetTrue)
                        .help("Plain language instead of chain entries"),
                )
                .arg(as_arg())
                .arg(role_arg()),
        )
        .subcommand(
            Command::new("verify")
                .about("Re-walk the chain and report the first broken link")
                .arg(site_arg())
                .arg(as_arg())
                .arg(role_arg()),
        )
}

/// Parses `YYYY-MM-DD` into unix microseconds at 00:00 UTC.
#[must_use]
pub fn parse_since(text: &str) -> Option<i64> {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    let date = time::Date::parse(text, &format).ok()?;
    date.midnight()
        .assume_utc()
        .unix_timestamp()
        .checked_mul(1_000_000)
}

struct NoAuditSink;

impl AuditSink for NoAuditSink {
    fn record(&self, _record: spot_audit_chain::AuditRecord) -> Result<(), ApprovalError> {
        Ok(())
    }
}

struct OpsClock(Arc<dyn Clock>);

impl spot_migration_ops::Clock for OpsClock {
    fn now(&self) -> i64 {
        i64::try_from(self.0.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
    }
}

/// Verifies the chain of one scope. The store is leaked for the life of this one-shot process
/// (the run store borrows it).
struct PgChainVerifier {
    store: &'static PgAuditStore,
    scope: String,
}

impl ChainVerifier for PgChainVerifier {
    fn verify(&self) -> BoxFuture<'_, Result<ChainStatus, LifecycleError>> {
        Box::pin(async move {
            let runs = ChainRunStore::new(self.store, self.scope.clone());
            let keys = KeyRing::new();
            let report = runs
                .verify(&Verifier::new(&keys))
                .await
                .map_err(|_| LifecycleError::Store)?;
            let head = runs.head_seq().await.map_err(|_| LifecycleError::Store)?;
            Ok(ChainStatus {
                checked: report.checked,
                head,
                breaks: report
                    .failures
                    .iter()
                    .map(|f| (f.seq, f.kind.code().to_owned()))
                    .collect(),
            })
        })
    }
}

fn open(
    sub: &ArgMatches,
    backend: &PgBackend,
    site: &str,
    clock: Arc<dyn Clock>,
    store: &'static PgAuditStore,
) -> spot_cli::Result<LifecycleContext> {
    let site_id = SiteId::new(site)
        .map_err(|_| spot_cli::Error(format!("`{site}` is not a valid site name")))?;
    let bridge = Bridge::for_backend(
        BridgeConfig {
            name: "migrate-audit".to_owned(),
            workers: 2,
            max_in_flight: 2,
            queue_capacity: 8,
            default_deadline: Duration::from_secs(10),
            max_deadline: Duration::from_secs(60),
        },
        backend,
    )
    .map_err(|_| spot_cli::Error("the migration stores cannot reach the database".to_owned()))?;
    let migration_store = Arc::new(PgMigrationStore::new(
        Arc::new(bridge),
        Arc::new(RecordingAlert::new()),
    ));
    let approval_store: Arc<dyn ApprovalStore> = migration_store.clone();
    let approvals = ApprovalService::new(
        approval_store,
        spot_migration_ops::SharedAuditSink(Arc::new(NoAuditSink)),
        ApprovalPolicy::strictest(),
    )
    .map_err(|e| spot_cli::Error(e.to_string()))?;
    let runs: Arc<dyn RunStore> = Arc::new(ChainRunStore::new(store, scope_of(site)));
    let plans: Arc<dyn spot_migration_ops::PlanStore> = migration_store;
    let ports = LifecyclePorts {
        clock: Arc::new(OpsClock(clock)),
        approvals: Arc::new(approvals),
        runs: Some(runs),
        snapshots: None,
        gate: Arc::new(spot_migration_apply::InMemoryAdmissionGate::new()),
        hold: Arc::new(spot_migration_apply::RecordingHold::new()),
        faults: Arc::new(spot_migration_apply::NoFaults),
        plans,
        access: Arc::new(RoleAccess::new().granting(DEFAULT_ROLE)),
    };
    let actor =
        Actor::new(principal_of(sub), Channel::Cli).map_err(|e| spot_cli::Error(e.to_string()))?;
    Ok(LifecycleContext::new(site_id, actor, ports))
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot migrate-audit <list|show|verify> --site <SITE>"
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
    let store: &'static PgAuditStore = Box::leak(Box::new(PgAuditStore::new(backend.pool())));
    let lifecycle = open(sub, &backend, &site, Arc::clone(ctx.clock()), store)?;
    match verb {
        "list" => list_cmd(ctx, sub, &lifecycle).await,
        "show" => show_cmd(ctx, sub, &lifecycle).await,
        _ => {
            let verifier = PgChainVerifier {
                store,
                scope: scope_of(&site),
            };
            verify_cmd(ctx, &lifecycle, &verifier).await
        }
    }
}

fn fail(ctx: &mut CommandCtx, error: &LifecycleError) -> spot_cli::Result<Exit> {
    writeln!(ctx.err(), "{error}")?;
    Ok(exit_of(error))
}

async fn list_cmd(
    ctx: &mut CommandCtx,
    sub: &ArgMatches,
    lifecycle: &LifecycleContext,
) -> spot_cli::Result<Exit> {
    let mut filter = RunFilter::all();
    if let Some(text) = sub.get_one::<String>("since") {
        let Some(since) = parse_since(text) else {
            writeln!(ctx.err(), "--since is a day: YYYY-MM-DD")?;
            return Ok(Exit::Usage);
        };
        filter.started_after = Some(since);
    }
    let plan = sub.get_one::<String>("plan").cloned();
    let runs = match list(lifecycle, filter).await {
        Ok(runs) => runs,
        Err(error) => return fail(ctx, &error),
    };
    let json = sub.get_flag("json");
    for run in runs
        .iter()
        .filter(|r| plan.as_deref().is_none_or(|p| r.plan_digest == p))
    {
        if json {
            writeln!(
                ctx.out(),
                "{{\"run\":\"{}\",\"plan\":\"{}\",\"outcome\":\"{}\",\"actor\":\"{}\",\"started\":{},\"ended\":{}}}",
                run.id,
                run.plan_digest,
                run.outcome.as_str(),
                run.actor,
                run.started,
                run.ended
                    .map_or_else(|| "null".to_owned(), |e| e.to_string())
            )?;
        } else {
            writeln!(
                ctx.out(),
                "{}\t{}\t{}\t{}\tplan {}",
                run.id,
                run.outcome.as_str(),
                run.actor,
                run.duration_micros()
                    .map_or_else(|| "running".to_owned(), |d| format!("{d}us")),
                run.plan_digest
            )?;
        }
    }
    Ok(Exit::Ok)
}

async fn show_cmd(
    ctx: &mut CommandCtx,
    sub: &ArgMatches,
    lifecycle: &LifecycleContext,
) -> spot_cli::Result<Exit> {
    let run = sub.get_one::<String>("run").cloned().unwrap_or_default();
    if sub.get_flag("explain") {
        return match explain(lifecycle, &run).await {
            Ok(text) => {
                writeln!(ctx.out(), "{text}")?;
                Ok(Exit::Ok)
            }
            Err(error) => fail(ctx, &error),
        };
    }
    match show(lifecycle, &run).await {
        Ok(evidence) => {
            writeln!(
                ctx.out(),
                "run {} {} by {}, plan {}",
                evidence.run.id,
                evidence.run.outcome.as_str(),
                evidence.run.actor,
                evidence.run.plan_digest
            )?;
            for entry in &evidence.entries {
                writeln!(ctx.out(), "  seq {} {}", entry.seq, entry.record.kind)?;
            }
            Ok(Exit::Ok)
        }
        Err(error) => fail(ctx, &error),
    }
}

async fn verify_cmd(
    ctx: &mut CommandCtx,
    lifecycle: &LifecycleContext,
    verifier: &PgChainVerifier,
) -> spot_cli::Result<Exit> {
    match verify(lifecycle, verifier).await {
        Ok(status) if status.is_ok() => {
            writeln!(
                ctx.out(),
                "migration chain: {} entries checked, head {}: ok",
                status.checked,
                status.head
            )?;
            Ok(Exit::Ok)
        }
        Ok(status) => {
            writeln!(ctx.out(), "migration chain: BROKEN")?;
            for (seq, code) in &status.breaks {
                writeln!(ctx.out(), "seq {seq}: {code}")?;
            }
            if let Some(first) = status.first_break() {
                writeln!(ctx.out(), "first broken sequence {first}")?;
            }
            Ok(Exit::Refused)
        }
        Err(error) => fail(ctx, &error),
    }
}
