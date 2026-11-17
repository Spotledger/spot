//! `spot sync policy show`: what the host's sync policy configuration says, read only (WP-RTB-02.T10). Guide: `docs/running-spot/sync_policy.md`.
//!
//! The command reads the `[services.sync_policy]` table of `common.toml`: the default preset, the declared
//! policies (class, direction, scope owner, excluded **field names**) and the scope fields. It
//! prints names only: no field value, no key, no token, no payload. It changes nothing and opens no
//! connection. The verified generation of a running site, and why a generation was refused, are on
//! the readiness line (`sync_policy: generation 14`).
//!
//! Exit codes follow the host-wide convention: 0 done, 1 usage, 3 failed (the configuration does not
//! load or a declaration does not parse).

use clap::{ArgMatches, Command};

use super::{CommandCtx, Exit};

pub const NAME: &str = "sync";

pub fn command() -> Command {
    Command::new(NAME)
        .about("Inspect the sync configuration")
        .subcommand_required(true)
        .subcommand(
            Command::new("policy")
                .about("Sync policy")
                .subcommand_required(true)
                .subcommand(
                    Command::new("show")
                        .about("Show the sync policy configuration of this host (read only)"),
                ),
        )
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    match matches.subcommand() {
        Some(("policy", policy)) => match policy.subcommand() {
            Some(("show", show)) => show_policy(show, ctx),
            _ => Ok(Exit::Usage),
        },
        _ => Ok(Exit::Usage),
    }
}

fn show_policy(_matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let table = ctx
        .common()?
        .services
        .services_extra
        .get("sync_policy")
        .cloned();
    let lines = match spot_cli::services::sync_policy::describe(table.as_ref()) {
        Ok(lines) => lines,
        Err(error) => {
            writeln!(ctx.err(), "{error}")?;
            return Ok(Exit::Failed);
        }
    };
    for line in lines {
        writeln!(ctx.out(), "{line}")?;
    }
    Ok(Exit::Ok)
}
