//! `spot migrate-hold hold|release|status --site S`: the side-effect hold of a site (WP-MIG-08.T4).
//!
//! The same lifecycle operations the management API runs (`spot_migration_ops::hold`), as the
//! operator at the host. The command is registered as `migrate-hold` because `migrate` is a derive
//! command of `main.rs`; routing `spot migrate hold ...` here is a one-line change there (see the
//! task's open questions).
//!
//! * `hold --site S --reason TEXT`: the site stops performing external effects; they are recorded.
//! * `release --site S --replay`: perform the held effects once, in order. Replay needs the
//!   performers the serving process registers, so a process without them (this CLI) refuses and
//!   the hold stays (exit 3); run it through the management operation `site.migrate.release`.
//! * `release --site S --discard --approved-by USER [--reason TEXT]`: drop the held effects. The
//!   second approver must differ from the operator and hold the migration right; on the CLI the
//!   name is asserted by the operator at the host, the management API authenticates it.
//! * `status --site S`: whether the site is held, since when, by which migration.
//!
//! Exit codes: 0 done, 1 usage, 2 refused by policy or permission, 3 failed.

use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_core::SiteId;
use spot_effect_hold::AuditSink;
use spot_migration_ops::hold::{
    HoldView, MANUAL_HOLD_ID, ReleaseMode, hold as run_hold, release as run_release,
    status as run_status,
};
use spot_migration_ops::{Channel, LifecycleError};
use spot_permissions::{Principal, TENANT_ADMINISTRATOR};

use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "migrate-hold";

fn site_arg() -> Arg {
    Arg::new("site")
        .long("site")
        .value_name("SITE")
        .required(true)
        .help("The site (required)")
}

/// Arguments and help.
pub fn command() -> Command {
    Command::new(NAME)
        .about("Hold, release or inspect a site's external side effects (mail, webhooks, jobs)")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("hold")
                .about("Stop external effects of the site and record them")
                .arg(site_arg())
                .arg(
                    Arg::new("reason")
                        .long("reason")
                        .value_name("TEXT")
                        .required(true)
                        .help("Why (recorded in the audit)"),
                ),
        )
        .subcommand(
            Command::new("release")
                .about("Release a manual hold: replay or discard the held effects")
                .arg(site_arg())
                .arg(
                    Arg::new("replay")
                        .long("replay")
                        .action(ArgAction::SetTrue)
                        .conflicts_with("discard")
                        .help("Perform the held effects once, in order"),
                )
                .arg(
                    Arg::new("discard")
                        .long("discard")
                        .action(ArgAction::SetTrue)
                        .requires("approved_by")
                        .help("Drop the held effects (needs --approved-by)"),
                )
                .arg(
                    Arg::new("approved_by")
                        .long("approved-by")
                        .value_name("USER")
                        .help("The second approver of a discard"),
                )
                .arg(
                    Arg::new("reason")
                        .long("reason")
                        .value_name("TEXT")
                        .help("Why (a discard is audited with it)"),
                ),
        )
        .subcommand(
            Command::new("status")
                .about("Show whether the site is held")
                .arg(site_arg()),
        )
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    match matches.subcommand() {
        Some(("hold", sub)) => {
            let reason = text(sub, "reason");
            go(ctx, sub, Op::Hold(reason)).await
        }
        Some(("release", sub)) => {
            let mode = if sub.get_flag("discard") {
                let Some(user) = sub.get_one::<String>("approved_by") else {
                    writeln!(ctx.err(), "--discard needs --approved-by <USER>")?;
                    return Ok(Exit::Usage);
                };
                ReleaseMode::Discard {
                    approver: Principal::with_roles(user, &[TENANT_ADMINISTRATOR]),
                    reason: text(sub, "reason"),
                }
            } else if sub.get_flag("replay") {
                ReleaseMode::Replay
            } else {
                writeln!(ctx.err(), "release needs --replay or --discard")?;
                return Ok(Exit::Usage);
            };
            go(ctx, sub, Op::Release(mode)).await
        }
        Some(("status", sub)) => go(ctx, sub, Op::Status).await,
        _ => {
            writeln!(
                ctx.err(),
                "usage: spot migrate-hold <hold|release|status> --site <SITE>"
            )?;
            Ok(Exit::Usage)
        }
    }
}

fn text(matches: &ArgMatches, name: &str) -> String {
    matches.get_one::<String>(name).cloned().unwrap_or_default()
}

enum Op {
    Hold(String),
    Release(ReleaseMode),
    Status,
}

fn exit_of(error: &LifecycleError) -> Exit {
    if error.exit_code() == 2 {
        Exit::Refused
    } else {
        Exit::Failed
    }
}

async fn go(ctx: &mut CommandCtx, sub: &ArgMatches, op: Op) -> spot_cli::Result<Exit> {
    let site = text(sub, "site");
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
    let settings = spot_cli::services::effect_hold::Settings::default();
    let ledger = spot_cli::services::effect_hold::pg_ledger(&backend);
    let audit: Arc<dyn AuditSink> = Arc::new(spot_cli::services::effect_hold::TracingHoldAudit);
    let parts = spot_cli::services::effect_hold::build(
        site_id,
        ledger,
        Arc::clone(&clock),
        &settings,
        audit,
    );
    // The operator at the host acts as the platform; the ledger tables come from the server's
    // start (or `prepare`), never from this command.
    let lifecycle = spot_cli::services::effect_hold::lifecycle_context(
        &parts,
        clock,
        Principal::system(),
        Channel::Cli,
    )?;
    let result = match op {
        Op::Hold(reason) => run_hold(&lifecycle, MANUAL_HOLD_ID, &reason)
            .await
            .map(|out| format!("site {site} is held ({})", out.migration_id)),
        Op::Release(mode) => {
            let what = match &mode {
                ReleaseMode::Replay => "replayed",
                ReleaseMode::Discard { .. } => "discarded",
                ReleaseMode::Selective { .. } => "released selectively",
            };
            run_release(&lifecycle, MANUAL_HOLD_ID, mode)
                .await
                .map(|_| format!("site {site}: held effects {what}; the hold is released"))
        }
        Op::Status => run_status(&lifecycle, parts.inspector.as_ref())
            .await
            .map(|view| describe(&site, &view)),
    };
    match result {
        Ok(line) => {
            writeln!(ctx.out(), "{line}")?;
            Ok(Exit::Ok)
        }
        Err(error) => {
            writeln!(ctx.err(), "{error}")?;
            Ok(exit_of(&error))
        }
    }
}

fn describe(site: &str, view: &HoldView) -> String {
    if !view.held {
        return format!("site {site}: open (no hold)");
    }
    format!(
        "site {site}: held by {} since {} ({})",
        view.migration_id.as_deref().unwrap_or("unknown"),
        view.since
            .map_or_else(|| "unknown".to_owned(), |s| s.to_string()),
        view.reason.as_deref().unwrap_or("no reason recorded")
    )
}
