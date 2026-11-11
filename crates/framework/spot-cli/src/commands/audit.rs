//! `spot audit verify --site S [--from SEQ]`: checks the site's hash-chained identity trail.
//!
//! Read-only. The chain scope comes from the site name through `scope_of`, never from the
//! command line, so the command cannot be pointed at another site's chain or at the document
//! trail. Output carries sequence numbers and stable failure codes only, never a record's
//! content. Exit 0 verified, 2 the chain has a break, 3 the verification could not run, 1 usage.


use clap::{Arg, ArgMatches, Command};
use spot_cli::audit::chain::{IdentityChain, scope_of};

use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "audit";

/// Arguments and help.
pub fn command() -> Command {
    Command::new(NAME)
        .about("Inspect a site's audit trails")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("verify")
                .about("Verify the hash chain of a site's identity audit trail (read-only)")
                .arg(
                    Arg::new("site")
                        .long("site")
                        .value_name("SITE")
                        .help("The site whose identity trail is verified (required)"),
                )
                .arg(
                    Arg::new("from")
                        .long("from")
                        .allow_hyphen_values(true)
                        .value_name("SEQ")
                        .help("Verify from this sequence number (1 or more), anchored on the entry before it"),
                ),
        )
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some(("verify", sub)) = matches.subcommand() else {
        writeln!(ctx.err(), "usage: spot audit verify --site <SITE> [--from <SEQ>]")?;
        return Ok(Exit::Usage);
    };
    let Some(site) = sub.get_one::<String>("site") else {
        writeln!(ctx.err(), "--site is required")?;
        return Ok(Exit::Usage);
    };
    let from = match sub.get_one::<String>("from") {
        None => None,
        Some(text) => match text.parse::<u64>() {
            Ok(n) if n >= 1 => Some(n),
            _ => {
                writeln!(ctx.err(), "--from must be a whole sequence number of 1 or more")?;
                return Ok(Exit::Usage);
            }
        },
    };
    verify(ctx, site, from).await
}

async fn could_not_run(ctx: &mut CommandCtx, why: &str) -> spot_cli::Result<Exit> {
    writeln!(ctx.out(), "identity chain: not verified, {why}")?;
    Ok(Exit::Failed)
}

async fn verify(ctx: &mut CommandCtx, site: &str, from: Option<u64>) -> spot_cli::Result<Exit> {
    if ctx.site(site).is_err() {
        return could_not_run(ctx, "there is no such site").await;
    }
    let Ok(backend) = ctx.backend(site).await else {
        return could_not_run(ctx, "the site has no usable PostgreSQL backend").await;
    };
    let chain = IdentityChain::new(&backend, &scope_of(site));
    let result = match from {
        Some(seq) => chain.verify_from(seq).await,
        None => chain.verify().await,
    };
    let Ok(report) = result else {
        return could_not_run(
            ctx,
            "the audit store could not be read (is it prepared by new-site?)",
        )
        .await;
    };
    if from.is_some_and(|seq| seq > report.head) {
        // A range past the head checked nothing; that is not a pass.
        return could_not_run(ctx, "--from is past the chain head, so no entry was checked").await;
    }
    if report.is_ok() {
        writeln!(
            ctx.out(),
            "identity chain: {} entries checked, head {}: ok",
            report.checked,
            report.head
        )?;
        return Ok(Exit::Ok);
    }
    let mut failures = report.failures.clone();
    failures.sort_by_key(|(seq, _)| *seq);
    writeln!(ctx.out(), "identity chain: BROKEN")?;
    for (seq, code) in &failures {
        writeln!(ctx.out(), "seq {seq}: {code}")?;
    }
    if let Some((first, _)) = failures.first() {
        writeln!(
            ctx.out(),
            "{} entries checked, head {}, first broken sequence {first}",
            report.checked,
            report.head
        )?;
    }
    Ok(Exit::Refused)
}
