//! `spot script add` (WP-DX-01.T3): add a TypeScript route handler and its manifest rows. Forwards to `spot-scaffold`
//! through `app::forward_scaffold`: the arguments are the scaffolder's own derive, the output goes to the
//! command context, and the exit codes follow the table in `app.rs`. Needs no site and no
//! database. Guide: `docs/running-spot/developer_commands.md`.

use clap::{ArgMatches, Command, FromArgMatches, Subcommand};
use spot_scaffold::cli::{Command as ScaffoldCommand, ScriptCommand};

use super::{CommandCtx, Exit};

pub const NAME: &str = "script";

pub fn command() -> Command {
    let command = Command::new(NAME)
        .about("Add TypeScript scripts to an app")
        .after_help("Exit codes: 0 done, 1 usage, 2 refused (nothing written), 3 failed.")
        .subcommand_required(true)
        .arg_required_else_help(true);
    ScriptCommand::augment_subcommands(command)
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let action =
        ScriptCommand::from_arg_matches(matches).map_err(|e| spot_cli::Error(e.to_string()))?;
    let wrapped = ScaffoldCommand::Script { command: action };
    super::app::forward_scaffold(&wrapped, ctx)
}
