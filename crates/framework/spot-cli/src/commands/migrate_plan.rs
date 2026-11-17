//! `spot migrate-plan plan|show --site S`: the migration lifecycle's first verbs (WP-MIG-04.T7).
//!
//! The same operations the management API runs (`spot_migration_ops::plan`), as the operator at
//! the host. Registered as `migrate-plan` because `migrate` is a derive command of `main.rs`;
//! routing `spot migrate plan|show` here is a one-line change there (open question of the task).
//! The existing `spot migrate [--apply]` keeps using the STO-11 engine untouched.
//!
//! * `plan --site S [--as USER] [--json]`: computes the plan from the installed apps' metadata
//!   and the live schema, applies the expand/contract gate, stores it by hash and prints the exact
//!   next command.
//! * `show HASH --site S`: prints the stored plan, its state, risk and approvals.
//!
//! The identity is asserted by the operator at the host (`--as`, default `system`); the management
//! API authenticates it. Output carries names and counts, never row data. Exit codes: 0 done,
//! 1 usage, 2 refused by policy or permission, 3 failed.

use std::sync::Arc;
use std::time::Duration;

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_core::{Clock, SiteId};
use spot_migration_approval::{
    ApprovalError, ApprovalPolicy, ApprovalService, ApprovalStore, AuditSink,
};
use spot_migration_audit::RoleAccess;
use spot_migration_ops::plan::{PlanInputs, plan, show};
use spot_migration_ops::{
    Actor, Channel, LifecycleContext, LifecycleError, LifecyclePorts, SharedAuditSink,
};
use spot_migration_ops_pg::{PgMigrationStore, RecordingAlert};
use spot_migration_plan::{DocTypeDescription, SchemaPlan, SystemDescription};
use spot_permissions::Principal;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;

use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "migrate-plan";

/// The role a person acts under unless `--role` says otherwise.
pub const DEFAULT_ROLE: &str = "release_manager";

/// Arguments and help.
pub fn command() -> Command {
    Command::new(NAME)
        .about("Plan a site's migration and show a stored plan (changes nothing)")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("plan")
                .about("Compute, gate and store the plan; print its hash and the next command")
                .arg(site_arg())
                .arg(as_arg())
                .arg(role_arg())
                .arg(
                    Arg::new("json")
                        .long("json")
                        .action(ArgAction::SetTrue)
                        .help("Print the result as JSON"),
                ),
        )
        .subcommand(
            Command::new("show")
                .about("Show a stored plan, its state and its approvals")
                .arg(Arg::new("hash").value_name("HASH").required(true))
                .arg(site_arg())
                .arg(as_arg())
                .arg(role_arg()),
        )
}

pub(super) fn site_arg() -> Arg {
    Arg::new("site")
        .long("site")
        .value_name("SITE")
        .required(true)
        .help("The site (required)")
}

pub(super) fn as_arg() -> Arg {
    Arg::new("as")
        .long("as")
        .value_name("USER")
        .help("Act as this person (asserted at the host; default: system)")
}

pub(super) fn role_arg() -> Arg {
    Arg::new("role")
        .long("role")
        .value_name("ROLE")
        .action(ArgAction::Append)
        .help("A role the person holds (default: release_manager)")
}

/// The principal `--as` and `--role` name: the system when no user is given.
pub fn principal_of(matches: &ArgMatches) -> Principal {
    match matches.get_one::<String>("as") {
        None => Principal::system(),
        Some(user) => {
            let roles: Vec<&str> = matches
                .get_many::<String>("role")
                .map(|r| r.map(String::as_str).collect())
                .unwrap_or_default();
            if roles.is_empty() {
                Principal::with_roles(user, &[DEFAULT_ROLE])
            } else {
                Principal::with_roles(user, &roles)
            }
        }
    }
}

/// Records approval actions as tracing events (target `spot::audit::migration`): kind, actor and
/// the plan hash, never a comment's content beyond what the service already truncated. Appending
/// them to the site's hash-chained audit store is an open question of the task.
struct TracingApprovalAudit;

impl AuditSink for TracingApprovalAudit {
    fn record(&self, record: spot_audit_chain::AuditRecord) -> Result<(), ApprovalError> {
        tracing::info!(
            target: "spot::audit::migration",
            kind = %record.kind,
            actor = %record.actor,
            plan = record.subject.as_ref().map_or("", |s| s.id.as_str()),
            at = record.at,
            "migration lifecycle action"
        );
        Ok(())
    }
}

/// The site clock as the lifecycle's time source (microseconds).
struct OpsClock(Arc<dyn Clock>);

impl spot_migration_ops::Clock for OpsClock {
    fn now(&self) -> i64 {
        i64::try_from(self.0.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
    }
}

/// No hold, gate or snapshot is used by plan, show, approve and reject.
fn inert_ports(
    clock: Arc<dyn Clock>,
    approvals: Arc<spot_migration_ops::Approvals>,
    plans: Arc<dyn spot_migration_ops::PlanStore>,
) -> LifecyclePorts {
    LifecyclePorts {
        clock: Arc::new(OpsClock(clock)),
        approvals,
        runs: None,
        snapshots: None,
        gate: Arc::new(spot_migration_apply::InMemoryAdmissionGate::new()),
        hold: Arc::new(spot_migration_apply::RecordingHold::new()),
        faults: Arc::new(spot_migration_apply::NoFaults),
        plans,
        access: Arc::new(RoleAccess::new().granting(DEFAULT_ROLE)),
    }
}

/// What a command needs to run one lifecycle operation for one site.
pub struct Opened {
    /// The context.
    pub lifecycle: LifecycleContext,
    /// The PostgreSQL store (checked with [`PgMigrationStore::ensure_present`] on a plain thread).
    pub store: Arc<PgMigrationStore>,
}

/// Builds the lifecycle context over the site's PostgreSQL stores. Nothing is read or created.
/// The approval policy is the strictest one (an absent policy means strictest).
///
/// # Errors
/// [`spot_cli::Error`] for a bridge that does not fit the pool, an invalid site name or an
/// anonymous actor.
pub fn build_lifecycle(
    backend: &PgBackend,
    site: &str,
    clock: Arc<dyn Clock>,
    actor: Principal,
) -> spot_cli::Result<Opened> {
    let site_id = SiteId::new(site)
        .map_err(|_| spot_cli::Error(format!("`{site}` is not a valid site name")))?;
    let bridge = Bridge::for_backend(
        BridgeConfig {
            name: "migrate-lifecycle".to_owned(),
            workers: 2,
            max_in_flight: 2,
            queue_capacity: 8,
            default_deadline: Duration::from_secs(10),
            max_deadline: Duration::from_secs(60),
        },
        backend,
    )
    .map_err(|e| {
        spot_cli::Error(format!(
            "the migration stores cannot reach the database ({})",
            e.code().unwrap_or("bridge")
        ))
    })?;
    let store = Arc::new(PgMigrationStore::new(
        Arc::new(bridge),
        Arc::new(RecordingAlert::new()),
    ));
    let approval_store: Arc<dyn ApprovalStore> = store.clone();
    let approvals = ApprovalService::new(
        approval_store,
        SharedAuditSink(Arc::new(TracingApprovalAudit)),
        ApprovalPolicy::strictest(),
    )
    .map_err(|e| spot_cli::Error(e.to_string()))?;
    let actor = Actor::new(actor, Channel::Cli).map_err(|e| spot_cli::Error(e.to_string()))?;
    let plans: Arc<dyn spot_migration_ops::PlanStore> = store.clone();
    let ports = inert_ports(clock, Arc::new(approvals), plans);
    Ok(Opened {
        lifecycle: LifecycleContext::new(site_id, actor, ports),
        store,
    })
}

/// Runs `work` on a plain thread: the blocking bridge refuses any thread inside a tokio runtime.
pub fn on_plain_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> spot_cli::Result<T> {
    std::thread::spawn(work)
        .join()
        .map_err(|_| spot_cli::Error("the lifecycle operation stopped unexpectedly".to_owned()))
}

/// The exit a lifecycle error maps to.
pub fn exit_of(error: &LifecycleError) -> Exit {
    if error.exit_code() == 2 {
        Exit::Refused
    } else {
        Exit::Failed
    }
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    match matches.subcommand() {
        Some(("plan", sub)) => plan_cmd(sub, ctx).await,
        Some(("show", sub)) => show_cmd(sub, ctx).await,
        _ => {
            writeln!(
                ctx.err(),
                "usage: spot migrate-plan <plan|show> --site <SITE>"
            )?;
            Ok(Exit::Usage)
        }
    }
}

async fn open(
    sub: &ArgMatches,
    ctx: &mut CommandCtx,
) -> spot_cli::Result<Result<(String, Arc<PgBackend>, Opened), Exit>> {
    let site = sub.get_one::<String>("site").cloned().unwrap_or_default();
    if ctx.site(&site).is_err() {
        writeln!(ctx.err(), "there is no such site")?;
        return Ok(Err(Exit::Usage));
    }
    let backend = match ctx.backend(&site).await {
        Ok(backend) => backend,
        Err(_) => {
            writeln!(ctx.err(), "the site has no usable PostgreSQL backend")?;
            return Ok(Err(Exit::Failed));
        }
    };
    let clock = Arc::clone(ctx.clock());
    let opened = build_lifecycle(&backend, &site, clock, principal_of(sub))?;
    Ok(Ok((site, backend, opened)))
}

/// The plan inputs of `site`: the installed apps' effective metadata as the target, the live
/// schema diff, and the built-in risk table. The live published metadata is not readable yet, so
/// the metadata side of the diff is empty and the plan is driven by the schema diff (an open
/// question of the task); the expand/contract gate is supplied by the release train.
async fn inputs_for(
    ctx: &mut CommandCtx,
    site: &str,
    backend: &PgBackend,
) -> spot_cli::Result<PlanInputs> {
    let loaded = spot_cli::config::load(ctx.root(), false)?;
    let site_config = ctx.site(site)?.clone();
    let meta = spot_cli::apps::serving(ctx.root(), &loaded, &site_config)?;
    let catalog = spot_cli::schema::catalog(&meta)?;
    let migration = spot_cli::schema::plan_with_platform_tables(backend, &catalog).await?;
    let to = SystemDescription {
        doctypes: meta
            .definitions()
            .iter()
            .map(|definition| DocTypeDescription {
                definition: definition.clone(),
                permissions: Vec::new(),
                views: std::collections::BTreeMap::new(),
            })
            .collect(),
        ..SystemDescription::new()
    };
    Ok(PlanInputs::new(
        to.clone(),
        to,
        SchemaPlan::from_migration(&migration),
    ))
}

async fn plan_cmd(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let (site, backend, opened) = match open(sub, ctx).await? {
        Ok(parts) => parts,
        Err(exit) => return Ok(exit),
    };
    let inputs = inputs_for(ctx, &site, &backend).await?;
    let json = sub.get_flag("json");
    let Opened { lifecycle, store } = opened;
    let outcome = on_plain_thread(move || {
        store.ensure_present().map_err(|e| (e, Vec::new()))?;
        plan(&lifecycle, &inputs).map_err(|r| (r.error, r.findings))
    })?;
    match outcome {
        Ok(report) => {
            if json {
                writeln!(
                    ctx.out(),
                    "{{\"plan_hash\":\"{}\",\"state\":\"{}\",\"risk\":\"{}\",\"created\":{},\"changes\":{}}}",
                    report.hash.as_str(),
                    report.state.as_str(),
                    report.risk,
                    report.created,
                    report.changes
                )?;
            } else {
                write!(ctx.out(), "{}", report.render(&site))?;
            }
            Ok(Exit::Ok)
        }
        Err((error, findings)) => {
            writeln!(ctx.err(), "{error}")?;
            for finding in findings {
                writeln!(
                    ctx.err(),
                    "  {}: {} ({})",
                    finding.code,
                    finding.target,
                    finding.detail
                )?;
            }
            Ok(exit_of(&error))
        }
    }
}

async fn show_cmd(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let (site, backend, opened) = match open(sub, ctx).await? {
        Ok(parts) => parts,
        Err(exit) => return Ok(exit),
    };
    let hash = sub.get_one::<String>("hash").cloned().unwrap_or_default();
    let current = inputs_for(ctx, &site, &backend).await.ok();
    let Opened { lifecycle, store } = opened;
    let outcome = on_plain_thread(move || {
        store.ensure_present()?;
        show(&lifecycle, &hash, current.as_ref())
    })?;
    match outcome {
        Ok(report) => {
            writeln!(
                ctx.out(),
                "plan {}\n  state: {}\n  risk: {}\n  author: {}\n  approvals: {} of {} ({})\n",
                report.hash.as_str(),
                report.state.as_str(),
                report.risk,
                report.author,
                report.approvals.approved_by.len(),
                report.approvals.required,
                report.approvals.approved_by.join(", ")
            )?;
            write!(ctx.out(), "{}", report.text)?;
            Ok(Exit::Ok)
        }
        Err(error) => {
            writeln!(ctx.err(), "{error}")?;
            Ok(exit_of(&error))
        }
    }
}
