//! `spot edges verify --site S [--page-size N]` and
//! `spot edges repair --site S --report KEY [--apply] [--batch N]` (WP-DAG-04.T6). Guide:
//! `docs/running-spot/edge_integrity.md`.
//!
//! `verify` runs the edge invariant scan now (not through the job queue), writes a drift report
//! and prints `checked <n> documents, <m> drift entries, report <key>` and, for at most the first
//! 50 entries, one line each: `<kind> <doctype> <record id> <edge type>`. Identifiers only, never
//! a field value. `repair` without `--apply` prints what the named report would change and
//! writes nothing; with `--apply` it applies the report in audited batches.
//!
//! Exit codes follow the host-wide convention: 0 done (and, for `verify`, clean), 1 usage, 2
//! refused or drift found (nothing was changed), 3 could not run.

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_cli::services::edge_integrity::{self as integrity, LoadError};

use super::{CommandCtx, Exit};

pub const NAME: &str = "edges";

fn site_arg() -> Arg {
    Arg::new("site")
        .long("site")
        .value_name("SITE")
        .help("The site whose edges are checked (required)")
}

pub fn command() -> Command {
    Command::new(NAME)
        .about("Check that a site's edges equal its link fields, and repair them")
        .subcommand(
            Command::new("verify")
                .about("Scan now and write a drift report (changes no edge)")
                .arg(site_arg())
                .arg(
                    Arg::new("page-size")
                        .long("page-size")
                        .value_name("N")
                        .help("Documents per page (default 500)"),
                ),
        )
        .subcommand(
            Command::new("repair")
                .about("Show, or with --apply make, the changes a drift report names")
                .arg(site_arg())
                .arg(
                    Arg::new("report")
                        .long("report")
                        .value_name("KEY")
                        .help("The drift report to repair from (required)"),
                )
                .arg(
                    Arg::new("apply")
                        .long("apply")
                        .action(ArgAction::SetTrue)
                        .help("Apply the changes; without it nothing is written"),
                )
                .arg(
                    Arg::new("batch")
                        .long("batch")
                        .value_name("N")
                        .help("Changes per audited batch (default 200)"),
                ),
        )
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    match matches.subcommand() {
        Some(("verify", sub)) => verify(sub, ctx).await,
        Some(("repair", sub)) => repair(sub, ctx).await,
        _ => {
            writeln!(
                ctx.err(),
                "usage: spot edges verify --site <SITE> | spot edges repair --site <SITE> --report <KEY> [--apply]"
            )?;
            Ok(Exit::Usage)
        }
    }
}

fn number(sub: &ArgMatches, name: &str) -> Result<Option<usize>, String> {
    match sub.get_one::<String>(name) {
        None => Ok(None),
        Some(text) => match text.parse::<usize>() {
            Ok(n) if n >= 1 => Ok(Some(n)),
            _ => Err(format!("--{name} must be a whole number of at least 1")),
        },
    }
}

/// The metadata the site serves, its backend and the time, or the reason the command cannot run.
async fn site_parts(
    ctx: &mut CommandCtx,
    site: &str,
) -> Result<
    (
        std::sync::Arc<spot_postgres::PgBackend>,
        std::sync::Arc<spot_meta::EffectiveMeta>,
    ),
    String,
> {
    let root = ctx.root().to_path_buf();
    let meta = {
        let loaded = ctx.load().map_err(|e| e.to_string())?;
        let found = loaded
            .sites
            .iter()
            .find(|s| s.site == site)
            .ok_or_else(|| format!("there is no site `{site}`"))?;
        spot_cli::apps::serving(&root, loaded, found).map_err(|e| e.to_string())?
    };
    let backend = ctx.backend(site).await.map_err(|e| e.to_string())?;
    Ok((backend, meta))
}

fn could_not_run(ctx: &mut CommandCtx, why: &str) -> spot_cli::Result<Exit> {
    writeln!(ctx.err(), "could not run: {why}")?;
    Ok(Exit::Failed)
}

fn storage_hint(code: &str) -> &'static str {
    match code {
        "edge_pg.unavailable" | "edge_pg.write_failed" => {
            " (is the database reachable, and has `spot migrate` created the edge tables?)"
        }
        _ => "",
    }
}

async fn verify(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some(site) = sub.get_one::<String>("site") else {
        writeln!(ctx.err(), "--site is required")?;
        return Ok(Exit::Usage);
    };
    let page = match number(sub, "page-size") {
        Ok(page) => page,
        Err(why) => {
            writeln!(ctx.err(), "{why}")?;
            return Ok(Exit::Usage);
        }
    };
    let (backend, meta) = match site_parts(ctx, site).await {
        Ok(parts) => parts,
        Err(why) => return could_not_run(ctx, &why),
    };
    let now = ctx.clock().now_utc();
    let key = integrity::report_key("manual", now, &now.unix_timestamp().to_string());
    let page = u32::try_from(page.unwrap_or(spot_edge_pg::DEFAULT_PAGE_SIZE as usize))
        .unwrap_or(spot_edge_pg::DEFAULT_PAGE_SIZE);
    let report = match integrity::scan(&backend, &meta, now, &key, page).await {
        Ok(report) => report,
        Err(error) => {
            let why = format!("{}{}", error.code(), storage_hint(error.code()));
            return could_not_run(ctx, &why);
        }
    };
    writeln!(
        ctx.out(),
        "checked {} documents, {} drift entries, report {}",
        report.checked_documents,
        report.entries.len(),
        report.report_key
    )?;
    let (shown, more) = integrity::lines(&report);
    for line in &shown {
        writeln!(ctx.out(), "{line}")?;
    }
    if more > 0 {
        writeln!(ctx.out(), "... and {more} more (the report holds them all)")?;
    }
    Ok(if report.is_clean() {
        Exit::Ok
    } else {
        Exit::Refused
    })
}

async fn repair(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let (Some(site), Some(key)) = (
        sub.get_one::<String>("site"),
        sub.get_one::<String>("report"),
    ) else {
        writeln!(ctx.err(), "--site and --report are required")?;
        return Ok(Exit::Usage);
    };
    let batch = match number(sub, "batch") {
        Ok(batch) => batch.unwrap_or(spot_edge_pg::DEFAULT_BATCH_SIZE),
        Err(why) => {
            writeln!(ctx.err(), "{why}")?;
            return Ok(Exit::Usage);
        }
    };
    let apply = sub.get_flag("apply");
    let (backend, meta) = match site_parts(ctx, site).await {
        Ok(parts) => parts,
        Err(why) => return could_not_run(ctx, &why),
    };
    let now = ctx.clock().now_utc();
    let report = match integrity::load_report(&backend, &meta, key, now).await {
        Ok(report) => report,
        Err(LoadError::NotFound) => {
            writeln!(
                ctx.err(),
                "refused: there is no report `{key}`; run `spot edges verify --site {site}` again"
            )?;
            return Ok(Exit::Refused);
        }
        Err(LoadError::Stale { generated_at }) => {
            writeln!(
                ctx.err(),
                "refused: the report `{key}` was made at {generated_at}, more than 7 days ago and the data has moved on; run `spot edges verify --site {site}` again"
            )?;
            return Ok(Exit::Refused);
        }
        Err(LoadError::Failed(error)) => {
            let why = format!("{}{}", error.code(), storage_hint(error.code()));
            return could_not_run(ctx, &why);
        }
    };
    let (added, removed, updated) = match integrity::dry_run(&report) {
        Ok(counts) => counts,
        Err(error) => return could_not_run(ctx, error.code()),
    };
    writeln!(
        ctx.out(),
        "report {key}: would add {added}, remove {removed}, update {updated} edges"
    )?;
    let (shown, more) = integrity::lines(&report);
    for line in &shown {
        writeln!(ctx.out(), "{line}")?;
    }
    if more > 0 {
        writeln!(ctx.out(), "... and {more} more")?;
    }
    if !apply {
        writeln!(
            ctx.out(),
            "nothing was changed; add --apply to make these changes"
        )?;
        return Ok(Exit::Ok);
    }
    match integrity::repair(&backend, &meta, key, batch, now).await {
        Ok(done) => {
            for b in &done.batches {
                writeln!(
                    ctx.out(),
                    "batch {}: added {}, removed {}, updated {} (audit {key}/{})",
                    b.batch_no,
                    b.added,
                    b.removed,
                    b.updated,
                    b.batch_no
                )?;
            }
            writeln!(
                ctx.out(),
                "repaired: added {}, removed {}, updated {}; run `spot edges verify --site {site}` to confirm",
                done.added,
                done.removed,
                done.updated
            )?;
            Ok(Exit::Ok)
        }
        Err(error) => {
            writeln!(
                ctx.err(),
                "the repair stopped part-way ({}): batches that already committed stay, each with its audit row in spot_edge_repair_audit; run `spot edges verify --site {site}` to see what is left",
                error.code()
            )?;
            Ok(Exit::Failed)
        }
    }
}
