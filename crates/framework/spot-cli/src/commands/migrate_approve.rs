//! `spot migrate-approve approve|reject HASH --site S --as USER --comment TEXT`: the decision on a
//! stored plan (WP-MIG-04.T7).
//!
//! Separation of duties: the approver is not the plan's author (`migration.self_approval`), the
//! policy decides how many approvers a risk class needs (the strictest policy until the site sets
//! one), and every decision is audited before it is stored. `--as` and `--role` are asserted by the
//! operator at the host; the management API authenticates the approver (a signed command), which is
//! the control that makes separation of duties real. The system cannot approve.
//!
//! Exit codes: 0 done, 1 usage, 2 refused by policy or permission, 3 failed.

use std::sync::Arc;

use clap::{Arg, ArgMatches, Command};
use spot_migration_ops::approve::{approve, reject};

use super::migrate_plan::{
    Opened, as_arg, build_lifecycle, exit_of, on_plain_thread, principal_of, role_arg, site_arg,
};
use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "migrate-approve";

fn decision(name: &'static str, about: &'static str) -> Command {
    Command::new(name)
        .about(about)
        .arg(Arg::new("hash").value_name("HASH").required(true))
        .arg(site_arg())
        .arg(as_arg().required(true))
        .arg(role_arg())
        .arg(
            Arg::new("comment")
                .long("comment")
                .value_name("TEXT")
                .help("Why (recorded in the audit)"),
        )
}

/// Arguments and help.
pub fn command() -> Command {
    Command::new(NAME)
        .about("Approve or reject a stored migration plan (separation of duties applies)")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(decision("approve", "Approve the plan with this hash"))
        .subcommand(decision("reject", "Reject the plan with this hash"))
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let (approving, sub) = match matches.subcommand() {
        Some(("approve", sub)) => (true, sub),
        Some(("reject", sub)) => (false, sub),
        _ => {
            writeln!(
                ctx.err(),
                "usage: spot migrate-approve <approve|reject> HASH --site <SITE> --as <USER>"
            )?;
            return Ok(Exit::Usage);
        }
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
    let hash = sub.get_one::<String>("hash").cloned().unwrap_or_default();
    let comment = sub
        .get_one::<String>("comment")
        .cloned()
        .unwrap_or_default();
    let Opened { lifecycle, store } =
        build_lifecycle(&backend, &site, Arc::clone(ctx.clock()), principal_of(sub))?;
    let outcome = on_plain_thread(move || {
        store.ensure_present()?;
        if approving {
            approve(&lifecycle, &hash, &comment)
        } else {
            reject(&lifecycle, &hash, &comment)
        }
    })?;
    match outcome {
        Ok(report) => {
            writeln!(
                ctx.out(),
                "plan {}: {}; approvals {} of {} ({})",
                report.hash,
                report.state.as_str(),
                report.approvals.approved_by.len(),
                report.approvals.required,
                report.approvals.approved_by.join(", ")
            )?;
            Ok(Exit::Ok)
        }
        Err(error) => {
            writeln!(ctx.err(), "{error}")?;
            Ok(exit_of(&error))
        }
    }
}
