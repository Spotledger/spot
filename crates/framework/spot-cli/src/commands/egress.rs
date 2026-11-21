//! `spot egress`: the operator's side of the hosted egress policy (WP-SEC-09.T7). Guide:
//! `docs/running-spot/egress.md`.
//!
//! - `check <url> --purpose P [--tenant T]`: would the guard let this destination through? It
//!   resolves the host, classifies every answer and applies the site's policy file, and it never
//!   opens a connection. Exit 0 allowed, 2 denied (the registry's "refused"), 3 failed (for
//!   example an invalid policy file), 1 usage.
//! - `denials [--since 1h] [--purpose P] [--tenant T] [--limit N] [--json]`: the denial log,
//!   newest first, read from the file's tail (bounded).
//! - `reload`: validates the policy file and, when it is good, asks the running server to re-read
//!   it. A file that fails is reported with every problem and nothing is asked (exit 2).
//!
//! Every subcommand takes `--site` (optional when the host has one site). Nothing here prints a
//! query string, userinfo, a header or a resolved address of a denied destination.
use super::{CommandCtx, Exit};
use spot_cli::Result;
use spot_cli::services::egress::{
    self as svc, CheckOutcome, CheckRequest, DenialLine, DenialQuery, Paths, Settings, codes,
};
use spot_egress::{EgressPolicy, SystemResolver};
use std::sync::Arc;

pub const NAME: &str = "egress";

pub fn command() -> clap::Command {
    let site = || {
        clap::Arg::new("site")
            .long("site")
            .value_name("SITE")
            .help("The site to act on (optional when the host has exactly one)")
    };
    clap::Command::new(NAME)
        .about(
            "Check a destination against the egress policy, read the denial log, reload the policy",
        )
        .subcommand_required(true)
        .subcommand(
            clap::Command::new("check")
                .about("Would the egress policy let this destination through? Opens no connection")
                .arg(
                    clap::Arg::new("url")
                        .value_name("URL")
                        .required(true)
                        .help("An http or https URL; only its scheme, host and port are used"),
                )
                .arg(
                    clap::Arg::new("purpose")
                        .long("purpose")
                        .value_name("PURPOSE")
                        .required(true)
                        .help("webhook, script, connector, mail, llm, template, pdf or app"),
                )
                .arg(
                    clap::Arg::new("tenant")
                        .long("tenant")
                        .value_name("TENANT")
                        .help("The tenant to check as (default: the site's name)"),
                )
                .arg(site()),
        )
        .subcommand(
            clap::Command::new("denials")
                .about("The recorded denials, newest first")
                .arg(
                    clap::Arg::new("since")
                        .long("since")
                        .value_name("DURATION")
                        .help("Only denials this recent, such as 30m, 1h or 7d"),
                )
                .arg(
                    clap::Arg::new("purpose")
                        .long("purpose")
                        .value_name("PURPOSE"),
                )
                .arg(clap::Arg::new("tenant").long("tenant").value_name("TENANT"))
                .arg(
                    clap::Arg::new("limit")
                        .long("limit")
                        .value_name("N")
                        .value_parser(clap::value_parser!(u32).range(1..=1000))
                        .default_value("50"),
                )
                .arg(
                    clap::Arg::new("json")
                        .long("json")
                        .action(clap::ArgAction::SetTrue)
                        .help("One JSON object per line"),
                )
                .arg(site()),
        )
        .subcommand(
            clap::Command::new("reload")
                .about("Validate the policy file and ask the running server to re-read it")
                .arg(site()),
        )
}

/// The kind of an I/O error as a word; never its message, which can quote a path.
fn kind(e: &std::io::Error) -> String {
    format!("{:?}", e.kind()).to_lowercase()
}

/// What every subcommand needs about the site.
struct Target {
    site: String,
    paths: Paths,
}

/// The site and its files, or the reason there is none (already printed; the exit to return).
fn target(
    ctx: &mut CommandCtx,
    sub: &clap::ArgMatches,
) -> Result<std::result::Result<Target, Exit>> {
    let loaded = spot_cli::config::load(ctx.root(), false)?;
    let site = match sub.get_one::<String>("site") {
        Some(site) => site.clone(),
        None => match loaded.sites.as_slice() {
            [only] => only.site.clone(),
            _ => {
                writeln!(
                    ctx.err(),
                    "name the site with --site: the host has several or none"
                )?;
                return Ok(Err(Exit::Usage));
            }
        },
    };
    if !loaded.sites.iter().any(|s| s.site == site) {
        writeln!(ctx.err(), "there is no site `{}`", site.escape_debug())?;
        return Ok(Err(Exit::Usage));
    }
    let settings = Settings::from_table(loaded.common.services.services_extra.get(svc::NAME))?;
    let paths = Paths::resolve(&settings, Some(ctx.root()), &site, loaded.sites.len())?;
    Ok(Ok(Target { site, paths }))
}

pub async fn run(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(ctx.err(), "usage: spot egress <check|denials|reload>")?;
        return Ok(Exit::Usage);
    };
    let target = match target(ctx, sub)? {
        Ok(target) => target,
        Err(exit) => return Ok(exit),
    };
    match verb {
        "check" => check(ctx, sub, &target).await,
        "denials" => denials(ctx, sub, &target),
        "reload" => reload(ctx, &target),
        _ => {
            writeln!(
                ctx.err(),
                "unknown egress command `{}`",
                verb.escape_debug()
            )?;
            Ok(Exit::Usage)
        }
    }
}

/// The policy in force: the file, or the built-in default.
fn policy_of(paths: &Paths) -> std::result::Result<EgressPolicy, svc::PolicyProblem> {
    match &paths.policy_file {
        Some(path) => svc::load_policy_file(path).map(|loaded| loaded.policy),
        None => Ok(EgressPolicy::default()),
    }
}

async fn check(ctx: &mut CommandCtx, sub: &clap::ArgMatches, target: &Target) -> Result<Exit> {
    let raw = sub
        .get_one::<String>("url")
        .map(String::as_str)
        .unwrap_or_default();
    let url = match url::Url::parse(raw) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() => url,
        _ => {
            writeln!(
                ctx.err(),
                "{}: the destination must be an http or https URL with a host",
                codes::URL_INVALID
            )?;
            return Ok(Exit::Usage);
        }
    };
    let purpose_name = sub
        .get_one::<String>("purpose")
        .map(String::as_str)
        .unwrap_or_default();
    let Some(purpose) = svc::parse_purpose(purpose_name) else {
        writeln!(
            ctx.err(),
            "{}: the purpose must be one of {}",
            codes::PURPOSE_UNKNOWN,
            svc::PURPOSES.join(", ")
        )?;
        return Ok(Exit::Usage);
    };
    let tenant = sub
        .get_one::<String>("tenant")
        .cloned()
        .unwrap_or_else(|| target.site.clone());
    let policy = match policy_of(&target.paths) {
        Ok(policy) => policy,
        Err(problem) => {
            writeln!(ctx.err(), "{}", problem.render())?;
            return Ok(Exit::Failed);
        }
    };
    let request = CheckRequest {
        url,
        purpose,
        tenant,
    };
    let outcome = svc::check(
        policy,
        Arc::new(SystemResolver),
        ctx.clock().clone(),
        &request,
    )
    .await;
    let subject = format!(
        "{}://{}:{} (tenant {}, purpose {})",
        request.url.scheme(),
        request.url.host_str().unwrap_or_default().escape_debug(),
        request.url.port_or_known_default().unwrap_or(0),
        request.tenant.escape_debug(),
        purpose_name,
    );
    match outcome {
        CheckOutcome::Allowed { addrs, .. } => {
            writeln!(ctx.out(), "allowed: {subject}")?;
            let list = addrs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(ctx.out(), "a caller would be pinned to: {list}")?;
            Ok(Exit::Ok)
        }
        CheckOutcome::Denied { code, rule, class } => {
            writeln!(ctx.out(), "denied: {subject}")?;
            writeln!(ctx.out(), "rule: {rule}")?;
            if let Some(class) = class {
                writeln!(ctx.out(), "address class: {class}")?;
            }
            writeln!(ctx.out(), "code: {code}")?;
            Ok(Exit::Refused)
        }
    }
}

fn denials(ctx: &mut CommandCtx, sub: &clap::ArgMatches, target: &Target) -> Result<Exit> {
    let now = ctx.clock().now_utc().unix_timestamp();
    let since = match sub.get_one::<String>("since") {
        Some(text) => match svc::parse_since(text) {
            Ok(seconds) => Some(now.saturating_sub(seconds)),
            Err(why) => {
                writeln!(ctx.err(), "{}: {why}", codes::QUERY_INVALID)?;
                return Ok(Exit::Usage);
            }
        },
        None => None,
    };
    let purpose = sub.get_one::<String>("purpose").cloned();
    if let Some(name) = &purpose
        && svc::parse_purpose(name).is_none()
    {
        writeln!(
            ctx.err(),
            "{}: the purpose must be one of {}",
            codes::PURPOSE_UNKNOWN,
            svc::PURPOSES.join(", ")
        )?;
        return Ok(Exit::Usage);
    }
    let limit = sub.get_one::<u32>("limit").copied().unwrap_or(50) as usize;
    let query = DenialQuery {
        since,
        purpose,
        tenant: sub.get_one::<String>("tenant").cloned(),
        limit,
    };
    let lines = match svc::read_denials(&target.paths.denial_log, &query) {
        Ok(lines) => lines,
        Err(e) => {
            writeln!(ctx.err(), "the denial log cannot be read ({})", kind(&e))?;
            return Ok(Exit::Failed);
        }
    };
    if sub.get_flag("json") {
        for line in &lines {
            match serde_json::to_string(line) {
                Ok(text) => writeln!(ctx.out(), "{text}")?,
                Err(_) => {
                    writeln!(ctx.err(), "a denial line could not be encoded")?;
                    return Ok(Exit::Failed);
                }
            }
        }
        return Ok(Exit::Ok);
    }
    if lines.is_empty() {
        writeln!(ctx.out(), "no denials recorded")?;
        return Ok(Exit::Ok);
    }
    for line in &lines {
        writeln!(ctx.out(), "{}", render(line))?;
    }
    Ok(Exit::Ok)
}

/// One text line. Every field a tenant controls is escaped, so a hostile host name cannot write
/// control sequences to the operator's terminal.
fn render(line: &DenialLine) -> String {
    let when = time::OffsetDateTime::from_unix_timestamp(line.at)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| line.at.to_string());
    let actor = line
        .actor
        .as_deref()
        .map(|a| format!(" actor={}", a.escape_debug()))
        .unwrap_or_default();
    format!(
        "{when} tenant={} purpose={} {}://{}:{} rule={} class={}{actor}",
        line.tenant.escape_debug(),
        line.purpose.escape_debug(),
        line.scheme.escape_debug(),
        line.host.escape_debug(),
        line.port,
        line.rule.escape_debug(),
        line.class.escape_debug(),
    )
}

fn reload(ctx: &mut CommandCtx, target: &Target) -> Result<Exit> {
    let Some(path) = &target.paths.policy_file else {
        writeln!(
            ctx.out(),
            "services.egress.policy_file is not set: the built-in default policy has nothing to reload"
        )?;
        return Ok(Exit::Ok);
    };
    let loaded = match svc::load_policy_file(path) {
        Ok(loaded) => loaded,
        Err(problem) => {
            writeln!(ctx.err(), "{}", problem.render())?;
            writeln!(
                ctx.err(),
                "nothing was asked of the server; its policy is unchanged"
            )?;
            return Ok(Exit::Refused);
        }
    };
    if let Err(e) = svc::request_reload(&target.paths.reload_marker) {
        writeln!(
            ctx.err(),
            "the reload request could not be written ({})",
            kind(&e)
        )?;
        return Ok(Exit::Failed);
    }
    writeln!(
        ctx.out(),
        "the policy is valid: {} tenant(s), digest {}",
        loaded.tenants.len(),
        loaded.digest.get(..12).unwrap_or(&loaded.digest)
    )?;
    writeln!(
        ctx.out(),
        "asked the running server to re-read it (SIGHUP does the same); a server that cannot apply it keeps the old policy and reports itself degraded"
    )?;
    Ok(Exit::Ok)
}
