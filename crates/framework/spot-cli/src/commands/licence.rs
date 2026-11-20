//! `spot licence status|install|renew --site S`: the operator's view of a site's licence
//! (WP-CTL-04.T3).
//!
//! - `status [--json]`: edition, kind, validity window, renew-after, grace end, today's standing,
//!   entitlements, the last renewal attempt and its result code. Read-only.
//! - `install <FILE>`: verifies the file against the site's trusted keys and places it atomically;
//!   a lower or equal serial of the same licence is refused (no downgrade), as is a licence that
//!   fails verification or names another tenant.
//! - `renew`: one fetch from the control plane now (mutual TLS, the site's egress allow-list).
//!
//! Exit codes follow the registry convention (`Exit`): 0 done, 1 usage, 2 refused, 3 failed. A
//! licence that is `Restricted` or `Expired` makes `status` exit 2 (refused) so a script can alert;
//! a refused install exits 2 and changes nothing; an operational failure exits 3. Output carries
//! stable codes and sentences, never a licence's signature or a key.

use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_cli::services::licence::{
    LicenceGate, LicenceHost, Settings, Standing, renew, status_name,
};
use spot_license::{Licence, Status};

use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "licence";

/// Arguments and help.
pub fn command() -> Command {
    let site = || {
        Arg::new("site")
            .long("site")
            .value_name("SITE")
            .help("The site whose licence is used (required)")
    };
    Command::new(NAME)
        .about("Inspect, install or renew a site's licence")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("status")
                .about("Show the site's licence and what it allows today (read-only)")
                .arg(site())
                .arg(
                    Arg::new("json")
                        .long("json")
                        .action(ArgAction::SetTrue)
                        .help("Print one JSON object"),
                ),
        )
        .subcommand(
            Command::new("install")
                .about("Verify a licence file and install it (never a lower serial)")
                .arg(site())
                .arg(Arg::new("file").value_name("FILE").required(true)),
        )
        .subcommand(
            Command::new("renew")
                .about("Fetch the current licence from the control plane now")
                .arg(site()),
        )
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some((name, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot licence status|install|renew --site <SITE>"
        )?;
        return Ok(Exit::Usage);
    };
    let Some(site) = sub.get_one::<String>("site").cloned() else {
        writeln!(ctx.err(), "--site is required")?;
        return Ok(Exit::Usage);
    };
    let (settings, host) = match open(ctx, &site) {
        Ok(opened) => opened,
        Err(message) => {
            writeln!(ctx.err(), "{message}")?;
            return Ok(Exit::Failed);
        }
    };
    match name {
        "status" => status(ctx, &host, sub.get_flag("json")),
        "install" => {
            let file = sub.get_one::<String>("file").cloned().unwrap_or_default();
            install(ctx, &host, &file)
        }
        "renew" => renew_now(ctx, &host, &settings, &site).await,
        _ => {
            writeln!(
                ctx.err(),
                "usage: spot licence status|install|renew --site <SITE>"
            )?;
            Ok(Exit::Usage)
        }
    }
}

/// The site's `[services.licence]` settings and the host they describe. The command never
/// enforces `required` (an operator must be able to install the first licence): it loads with
/// `required = false`.
fn open(ctx: &mut CommandCtx, site: &str) -> Result<(Settings, LicenceHost), String> {
    ctx.site(site).map_err(|e| e.to_string())?;
    let table = ctx
        .common()
        .map_err(|e| e.to_string())?
        .services
        .services_extra
        .get("licence")
        .cloned();
    let mut settings: Settings = match table {
        Some(value) => value
            .try_into()
            .map_err(|_| "services.licence in common.toml is not valid".to_owned())?,
        None => Settings::default(),
    };
    settings.required = false;
    let root = ctx.root().to_path_buf();
    let clock = Arc::clone(ctx.clock());
    let host = settings
        .load_host(Some(&root), site, clock)
        .map_err(|e| e.to_string())?;
    Ok((settings, host))
}

fn date(epoch: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(epoch)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| epoch.to_string())
}

fn limit_text(licence: &Licence) -> String {
    match licence.entitlements.users() {
        spot_license::Limit::UpTo(n) => n.to_string(),
        spot_license::Limit::Unlimited => "unlimited".to_owned(),
    }
}

fn grace_end(licence: &Licence) -> Option<i64> {
    licence
        .expires_at
        .map(|e| e.saturating_add(i64::try_from(licence.grace.as_secs()).unwrap_or(i64::MAX)))
}

fn status(ctx: &mut CommandCtx, host: &LicenceHost, json: bool) -> spot_cli::Result<Exit> {
    let standing = LicenceGate::standing(host);
    let _ = host.flush();
    let licence = host.licence();
    let attempt = host.last_attempt();
    let invalid = host.invalid();
    let alert = matches!(
        standing,
        Standing::Licensed(Status::Restricted | Status::Expired)
    );
    if json {
        let value = serde_json::json!({
            "standing": status_name(standing),
            "licence_id": licence.as_ref().map(|l| l.licence_id.clone()),
            "serial": licence.as_ref().map(|l| l.serial),
            "edition": licence.as_ref().map(|l| l.edition.as_str().to_owned()),
            "kind": licence.as_ref().map(|l| format!("{:?}", l.kind).to_lowercase()),
            "not_before": licence.as_ref().map(|l| l.not_before),
            "expires_at": licence.as_ref().and_then(|l| l.expires_at),
            "renew_after": licence.as_ref().and_then(|l| l.renew_after),
            "grace_ends": licence.as_ref().and_then(grace_end),
            "users": licence.as_ref().map(limit_text),
            "apps": licence.as_ref().map(|l| l.entitlements.apps().to_vec()),
            "features": licence.as_ref().map(|l| l.entitlements.features().to_vec()),
            "not_accepted": invalid.as_ref().map(|e| e.code.clone()),
            "last_attempt": attempt.as_ref().map(|a| serde_json::json!({"at": a.at, "result": a.code})),
        });
        writeln!(ctx.out(), "{value}")?;
    } else {
        writeln!(ctx.out(), "standing: {}", status_name(standing))?;
        if let Some(error) = &invalid {
            writeln!(
                ctx.out(),
                "the licence file was not accepted: {}",
                error.code
            )?;
        }
        if let Some(l) = &licence {
            writeln!(ctx.out(), "licence: {} (serial {})", l.licence_id, l.serial)?;
            writeln!(ctx.out(), "edition: {}", l.edition.as_str())?;
            writeln!(
                ctx.out(),
                "kind: {}",
                format!("{:?}", l.kind).to_lowercase()
            )?;
            writeln!(ctx.out(), "valid from: {}", date(l.not_before))?;
            if let Some(e) = l.expires_at {
                writeln!(ctx.out(), "expires: {}", date(e))?;
            }
            if let Some(r) = l.renew_after {
                writeln!(ctx.out(), "renew after: {}", date(r))?;
            }
            if let Some(g) = grace_end(l) {
                writeln!(ctx.out(), "grace ends: {}", date(g))?;
            }
            writeln!(ctx.out(), "users: {}", limit_text(l))?;
            writeln!(ctx.out(), "apps: {}", l.entitlements.apps().join(", "))?;
            writeln!(
                ctx.out(),
                "features: {}",
                l.entitlements.features().join(", ")
            )?;
        }
        match &attempt {
            Some(a) => writeln!(
                ctx.out(),
                "last renewal attempt: {} ({})",
                date(a.at),
                a.code
            )?,
            None => writeln!(ctx.out(), "last renewal attempt: none")?,
        }
    }
    Ok(if alert { Exit::Refused } else { Exit::Ok })
}

fn install(ctx: &mut CommandCtx, host: &LicenceHost, file: &str) -> spot_cli::Result<Exit> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) if text.len() <= 64 * 1024 => text,
        _ => {
            writeln!(ctx.err(), "the licence file cannot be read")?;
            return Ok(Exit::Failed);
        }
    };
    match host.install_text(&text, false, &[]) {
        Ok(done) => {
            writeln!(
                ctx.out(),
                "installed licence {} serial {}: {}",
                done.licence_id,
                done.serial,
                status_name(done.standing)
            )?;
            Ok(Exit::Ok)
        }
        Err(error) => {
            writeln!(ctx.err(), "not installed: {error}")?;
            Ok(Exit::Refused)
        }
    }
}

async fn renew_now(
    ctx: &mut CommandCtx,
    host: &LicenceHost,
    settings: &Settings,
    site: &str,
) -> spot_cli::Result<Exit> {
    if settings.control_plane.is_empty() {
        writeln!(
            ctx.err(),
            "services.licence.control_plane is not set: this site renews offline (spot licence install)"
        )?;
        return Ok(Exit::Failed);
    }
    let clock = Arc::clone(ctx.clock());
    let fetcher = match spot_cli::services::licence::tls::fetcher(settings, site, clock) {
        Ok(fetcher) => fetcher,
        Err(error) => {
            writeln!(ctx.err(), "cannot renew: {error}")?;
            return Ok(Exit::Failed);
        }
    };
    let report = renew::renew_once(host, fetcher.as_ref(), true).await;
    match report.installed {
        Some(done) => {
            writeln!(
                ctx.out(),
                "renewed: licence {} serial {}: {}",
                done.licence_id,
                done.serial,
                status_name(done.standing)
            )?;
            Ok(Exit::Ok)
        }
        None => {
            writeln!(ctx.err(), "not renewed: {}", report.code)?;
            Ok(Exit::Failed)
        }
    }
}
