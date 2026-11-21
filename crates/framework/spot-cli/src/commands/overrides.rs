//! `spot overrides list --site S [--app A] [--json]` (WP-EXT-03.T2): who changed what on a site.
//! Guide: `docs/running-spot/seams_admin.md`.
//!
//! Builds the registry of the site's installed apps with the host's own builder
//! (`services::controllers::build`, which uses `platform_registry_builder`), starts no server and
//! reads no database: only the configuration and the app directories. Prints the library's
//! `OverrideReport::to_text`, or the same data as the admin method serves with `--json`.
//!
//! Exit codes: 0 done; 1 usage; 3 when the registry cannot be built (every conflict is printed,
//! one per line, with its stable code) or the site cannot be read.

use clap::{Arg, ArgAction, ArgMatches, Command};

use super::{CommandCtx, Exit};

pub const NAME: &str = "overrides";

pub fn command() -> Command {
    Command::new(NAME)
        .about("List the overrides and seams in force on a site")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("list")
                .about("Print who overrode what, why, and in which order")
                .arg(
                    Arg::new("site")
                        .long("site")
                        .value_name("SITE")
                        .required(true)
                        .help("The site"),
                )
                .arg(
                    Arg::new("app")
                        .long("app")
                        .value_name("APP")
                        .help("Only rows that involve this app"),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .action(ArgAction::SetTrue)
                        .help("Print the data the admin method serves"),
                ),
        )
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some(("list", sub)) = matches.subcommand() else {
        writeln!(ctx.err(), "name an overrides command: list")?;
        return Ok(Exit::Usage);
    };
    let site_name = sub.get_one::<String>("site").cloned().unwrap_or_default();
    let app = sub.get_one::<String>("app").cloned();
    let json = sub.get_flag("json");
    let root = ctx.root().to_path_buf();
    let built = (|| -> spot_cli::Result<_> {
        let loaded = spot_cli::config::load(&root, false)?;
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == site_name)
            .ok_or_else(|| spot_cli::Error(format!("there is no site `{site_name}`")))?;
        let meta = spot_cli::apps::serving(&root, &loaded, site)?;
        let perms = spot_cli::services::controllers::app_permissions(&root, &loaded, site)?;
        spot_cli::services::controllers::build(&root, &loaded, site, &meta, &perms)
    })();
    let installed = match built {
        Ok(installed) => installed,
        Err(error) => {
            // The build reports every conflict at once; print them one per line, code first.
            let text = error.to_string();
            for line in text.lines() {
                writeln!(ctx.err(), "{line}")?;
            }
            return Ok(Exit::Failed);
        }
    };
    let mut report = installed.seams.override_report();
    if let Some(app) = &app {
        report.overrides.retain(|r| &r.app == app);
        report
            .seams
            .retain(|s| &s.provider_app == app || s.replaced_by.as_ref() == Some(app));
    }
    if json {
        writeln!(
            ctx.out(),
            "{}",
            spot_cli::services::seams_admin::listing(&report, &[])
        )?;
    } else {
        write!(ctx.out(), "{}", report.to_text())?;
    }
    Ok(Exit::Ok)
}
