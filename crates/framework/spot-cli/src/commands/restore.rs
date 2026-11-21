//! `spot restore`: preflight, apply and purge-quarantine (WP-PRD-04.T3). Guide:
//! `docs/running-spot/restore.md`.
//!
//! - `preflight`: reads one backup of a named folder target (`common.toml` `[backup.targets]`),
//!   checks it against the site and prints what a restore would replace, the schema version
//!   comparison and a single-use **confirm token** (256 random bits, 10 minutes, bound to the
//!   backup, the target and the invoking OS user, kept as a digest in `sites/<site>/state/`);
//! - `apply`: the library order, nothing added silently: preflight again, the maintenance gate
//!   (gate row closed, the host told to reload, in-flight work drained, new connections barred and
//!   the rest ended: only then is the site proven quiescent), restore into a **scratch**
//!   database, schema checks and invariants on the scratch, node-local state reset on the
//!   scratch, and only then one atomic swap that renames the live database to
//!   `<db>_replaced_<seconds>` (kept, never dropped) and the scratch to the live name;
//! - `purge-quarantine`: drops quarantined databases of the site older than `--older-than`
//!   (never younger than 24 h), and only with `--apply`.
//!
//! Exit codes: 0 done, 1 usage, 2 refused (nothing changed), 3 failed (the previous database is
//! back and the gate open, or the line says it is not). A key, a password, a connection string or
//! a token never reaches an error, an audit record or a log line: every line passes `scrub`.
use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_cli::config::{BackupTarget as ConfiguredTarget, Loaded, Site};
use spot_core::Clock;
use spot_postgres::{PgConfig, Tls};
use spot_snapshot::{RestoreTarget, drop_scratch};
use std::path::{Path, PathBuf};

use super::{CommandCtx, Exit};
use apply::apply_verb;
use preflight::preflight_verb;
use server::{admin, pool_error, quarantine_marker};
use site::audit;

#[path = "restore/apply.rs"]
mod apply;
#[path = "restore/guard.rs"]
mod guard;
#[path = "restore/preflight.rs"]
mod preflight;
#[path = "restore/server.rs"]
mod server;
#[path = "restore/site.rs"]
mod site;

/// The subcommand name.
pub const NAME: &str = "restore";

const TARGET_UNSAFE: &str = "backup.target_unsafe";
const TARGET_UNAVAILABLE: &str = "backup.target_unavailable";
/// How long a confirm token lives.
const TOKEN_TTL_SECS: i64 = 600;
/// The youngest quarantine `purge-quarantine` will drop, whatever `--older-than` says.
const MIN_PURGE_SECS: i64 = 86_400;
/// Schema versions a backup cannot vouch for: `0` is "never recorded" (WP-PRD-04.T10) and `1` is
/// the placeholder an older `spot backup` wrote.
const UNVERIFIED_VERSIONS: [u64; 2] = [0, 1];
/// How long the gate waits for in-flight work to finish.
const DRAIN_MICROS: i64 = 30_000_000;

/// Arguments and help.
pub fn command() -> Command {
    let site = || {
        Arg::new("site")
            .long("site")
            .value_name("SITE")
            .help("The site to restore (required)")
    };
    let from = || {
        Arg::new("from")
            .long("from")
            .value_name("BACKUP_ID")
            .help("The backup to restore (required)")
    };
    let target = || {
        Arg::new("target")
            .long("target")
            .value_name("NAME")
            .help("A target of common.toml [backup.targets] (default: backup.default_target, or the only one)")
    };
    let as_new = || {
        Arg::new("as_new_site")
            .long("as-new-site")
            .value_name("NAME")
            .help("Restore into a new site called NAME; the site named by --site is untouched")
    };
    Command::new(NAME)
        .about("Restore a backup into a site (preflight, apply) and clean up what a restore replaced")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("preflight")
                .about("Check a restore and print the confirm token `apply` needs")
                .arg(site())
                .arg(from())
                .arg(target())
                .arg(as_new()),
        )
        .subcommand(
            Command::new("apply")
                .about("Restore the backup behind the maintenance gate; the replaced database is kept")
                .arg(site())
                .arg(from())
                .arg(target())
                .arg(as_new())
                .arg(
                    Arg::new("confirm")
                        .long("confirm")
                        // The token is base64 and may begin with `-`.
                        .allow_hyphen_values(true)
                        .value_name("TOKEN")
                        .help("The token `preflight` printed (single use, 10 minutes)"),
                )
                .arg(
                    Arg::new("accept_unverified_schema")
                        .long("accept-unverified-schema")
                        .action(ArgAction::SetTrue)
                        .help("Restore a backup whose schema version was never recorded (audited)"),
                )
                .arg(
                    Arg::new("migrate_forward")
                        .long("migrate-forward")
                        .action(ArgAction::SetTrue)
                        .help("Migrate the restored copy forward when the backup is older than this build"),
                ),
        )
        .subcommand(
            Command::new("purge-quarantine")
                .about("Drop quarantined databases of a site that are older than --older-than")
                .arg(site())
                .arg(
                    Arg::new("older_than")
                        .long("older-than")
                        .value_name("AGE")
                        .help("Age such as 7d or 48h (at least 24h)"),
                )
                .arg(
                    Arg::new("apply")
                        .long("apply")
                        .action(ArgAction::SetTrue)
                        .help("Drop them; without it only the list is printed"),
                ),
        )
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot restore <preflight|apply|purge-quarantine> --site SITE"
        )?;
        return Ok(Exit::Usage);
    };
    let done = match verb {
        "preflight" => preflight_verb(ctx, sub).await,
        "apply" => apply_verb(ctx, sub).await,
        "purge-quarantine" => purge_verb(ctx, sub).await,
        _ => Err(stop(
            ctx,
            Exit::Usage,
            format!("unknown restore command `{verb}`"),
        )),
    };
    Ok(done.unwrap_or_else(|Stop(exit)| exit))
}

/// A command that ended early; its message is already printed.
struct Stop(Exit);
type Step<T> = std::result::Result<T, Stop>;

fn stop(ctx: &mut CommandCtx, exit: Exit, text: impl AsRef<str>) -> Stop {
    let _ = writeln!(ctx.err(), "{}", text.as_ref());
    Stop(exit)
}

fn say(ctx: &mut CommandCtx, text: impl AsRef<str>) {
    let _ = writeln!(ctx.err(), "restore: {}", text.as_ref());
}

fn scrub(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(secret.as_str(), "***");
    }
    out
}

/// What every verb needs: the configuration, the site and the resolved target directory.
struct Prepared {
    loaded: Loaded,
    site: Site,
    name: String,
    dir: PathBuf,
    cfg: PgConfig,
    secrets: Vec<String>,
}

fn prepare(ctx: &mut CommandCtx, matches: &ArgMatches, with_target: bool) -> Step<Prepared> {
    let Some(site_name) = matches.get_one::<String>("site") else {
        return Err(stop(ctx, Exit::Usage, "--site is required"));
    };
    let loaded = match spot_cli::config::load(ctx.root(), false) {
        Ok(loaded) => loaded,
        Err(error) => return Err(stop(ctx, Exit::Failed, error.to_string())),
    };
    let Some(site) = loaded.sites.iter().find(|s| &s.site == site_name).cloned() else {
        return Err(stop(
            ctx,
            Exit::Usage,
            format!("there is no site `{site_name}`"),
        ));
    };
    let cfg = loaded.admin.clone().with_dbname(&site.database);
    let mut secrets = vec![cfg.password.clone()];
    secrets.extend(connection(&cfg, &site.database));
    let (mut name, mut dir) = (String::new(), PathBuf::new());
    if with_target {
        let wanted = matches.get_one::<String>("target").map(String::as_str);
        let (found, target) = match loaded.common.backup.target(wanted) {
            Ok((found, target)) => (found.to_owned(), target.clone()),
            Err(error) => return Err(stop(ctx, Exit::Refused, error.to_string())),
        };
        let ConfiguredTarget::Folder { path } = target else {
            let text = "backup.unsupported: cloud targets are not available yet (WP-PRD-04.T5); use a folder target";
            return Err(stop(ctx, Exit::Refused, text));
        };
        let mut forbidden = vec![ctx.root().join("sites")];
        forbidden.extend(loaded.common.apps.iter().map(|app| ctx.root().join(app)));
        if let Some(pgdata) = std::env::var_os("PGDATA").filter(|v| !v.is_empty()) {
            forbidden.push(PathBuf::from(pgdata));
        }
        dir = match resolve_target(&path, &forbidden) {
            Ok(dir) => dir,
            Err((code, why)) => return Err(stop(ctx, Exit::Refused, format!("{code}: {why}"))),
        };
        name = found;
    }
    Ok(Prepared {
        loaded,
        site,
        name,
        dir,
        cfg,
        secrets,
    })
}

/// The real directory of a configured target, or the code and reason it is refused. The final
/// component must not be a symbolic link, and the real path must be neither inside nor above a
/// directory in `forbidden`.
fn resolve_target(path: &Path, forbidden: &[PathBuf]) -> Result<PathBuf, (&'static str, String)> {
    let missing = || {
        (
            TARGET_UNAVAILABLE,
            "the target directory does not exist or cannot be read".to_owned(),
        )
    };
    let meta = std::fs::symlink_metadata(path).map_err(|_| missing())?;
    if meta.file_type().is_symlink() {
        return Err((
            TARGET_UNSAFE,
            "the target is a symbolic link; name the real directory".to_owned(),
        ));
    }
    if !meta.is_dir() {
        return Err((
            TARGET_UNAVAILABLE,
            "the target is not a directory".to_owned(),
        ));
    }
    let real = std::fs::canonicalize(path).map_err(|_| missing())?;
    for bad in forbidden {
        let bad = std::fs::canonicalize(bad).unwrap_or_else(|_| bad.clone());
        if real.starts_with(&bad) || bad.starts_with(&real) {
            let why = "the target is inside, or holds, the site data, an app directory or the PostgreSQL data directory";
            return Err((TARGET_UNSAFE, why.to_owned()));
        }
    }
    Ok(real)
}

/// A `postgres://` URL for `database` on the configured server. It holds the password: it goes
/// to the snapshot provider (which moves it into a child's environment) and nowhere else.
fn connection(cfg: &PgConfig, database: &str) -> Option<String> {
    let mut url = url::Url::parse("postgres://placeholder/").ok()?;
    url.set_host(Some(&cfg.host)).ok()?;
    url.set_port(Some(cfg.port)).ok()?;
    url.set_username(&cfg.user).ok()?;
    if !cfg.password.is_empty() {
        url.set_password(Some(&cfg.password)).ok()?;
    }
    url.path_segments_mut().ok()?.clear().push(database);
    if matches!(cfg.tls, Tls::Require) {
        url.query_pairs_mut().append_pair("sslmode", "require");
    }
    Some(url.into())
}

/// The directory of the PostgreSQL client tools: `SPOT_PG_BIN`, else the first `pg_dump` on
/// `PATH`; absolute and not world-writable. (`[backup] pg_bin_dir` is WP-PRD-04.T9.)
fn pg_bin() -> Result<PathBuf, String> {
    let env = || std::env::var_os("SPOT_PG_BIN").map(PathBuf::from);
    let path = || {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).find(|d| d.is_absolute() && d.join("pg_dump").is_file())
    };
    let Some(dir) = env().or_else(path) else {
        return Err("backup.snapshot: the PostgreSQL client tools (pg_restore) were not found; put them on PATH or set SPOT_PG_BIN to their directory".to_owned());
    };
    if !dir.is_absolute() || !dir.join("pg_restore").is_file() {
        return Err("backup.snapshot: the PostgreSQL client tools directory must be absolute and hold pg_restore".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let open = std::fs::metadata(&dir)
            .map(|m| m.permissions().mode() & 0o002 != 0)
            .unwrap_or(true);
        if open {
            return Err(format!(
                "{TARGET_UNSAFE}: the PostgreSQL client tools directory {} is world-writable or unreadable; refusing to run its tools",
                dir.display()
            ));
        }
    }
    Ok(dir)
}

/// The ids a backup may have: letters, digits, `.`, `_`, `-`, starting with a letter or digit.
fn valid_id(id: &str) -> bool {
    id.len() <= 200
        && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A database name that is safe to put between double quotes.
fn ident(name: &str) -> Result<String, String> {
    let safe = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if safe {
        Ok(format!("\"{name}\""))
    } else {
        Err(format!(
            "the database name {name:?} is not a safe identifier"
        ))
    }
}

fn micros(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1000).unwrap_or(i64::MAX)
}

#[cfg(unix)]
fn private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}
#[cfg(not(unix))]
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// The site's state directory (`sites/<site>/state`, mode 0700), created when missing.
fn state_dir(root: &Path, site: &str) -> std::io::Result<PathBuf> {
    let dir = root.join("sites").join(site).join("state");
    match private_dir(&dir) {
        Ok(()) => Ok(dir),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(dir),
        Err(error) => Err(error),
    }
}

// purge-quarantine

fn parse_age(text: &str) -> Option<i64> {
    if !text.is_ascii() {
        return None;
    }
    let (number, unit) = text.split_at(text.len().checked_sub(1)?);
    let n: i64 = number.parse().ok().filter(|n| *n > 0)?;
    n.checked_mul(match unit {
        "d" => 86_400,
        "h" => 3_600,
        _ => return None,
    })
}

async fn purge_verb(ctx: &mut CommandCtx, sub: &ArgMatches) -> Step<Exit> {
    let Some(age) = sub
        .get_one::<String>("older_than")
        .and_then(|t| parse_age(t))
    else {
        return Err(stop(
            ctx,
            Exit::Usage,
            "--older-than is required and must look like 7d or 48h",
        ));
    };
    if age < MIN_PURGE_SECS {
        return Err(stop(
            ctx,
            Exit::Refused,
            "restore.quarantine_young: a quarantined database younger than 24 hours is never removed; use --older-than 24h or more",
        ));
    }
    let prep = prepare(ctx, sub, false)?;
    let apply = sub.get_flag("apply");
    let now = ctx.clock().now_utc().unix_timestamp();
    let prefix = format!("spot-scratch:v1:quarantine:{}:", prep.site.site);
    let found = async {
        let backend = admin(&prep.cfg).await?;
        let client = backend.pool().get().await.map_err(pool_error)?;
        let pattern = format!(
            "{}\\_replaced\\_%",
            prep.site.database.chars().take(40).collect::<String>()
        );
        let rows = client.query("SELECT datname::text, shobj_description(oid, 'pg_database') FROM pg_database WHERE datname LIKE $1 ORDER BY datname", &[&pattern]).await.map_err(pool_error)?;
        Ok::<_, String>(
            rows.iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, Option<String>>(1)))
                .collect::<Vec<_>>(),
        )
    };
    let candidates = match found.await {
        Ok(rows) => rows,
        Err(text) => return Err(stop(ctx, Exit::Failed, format!("backup.io: {text}"))),
    };
    let (mut listed, mut dropped) = (0u64, 0u64);
    let live = connection(&prep.cfg, &prep.site.database);
    for (name, comment) in candidates {
        let Some(secs) = comment
            .as_deref()
            .and_then(|c| c.strip_prefix(&prefix))
            .and_then(|s| s.parse::<i64>().ok())
        else {
            continue;
        };
        if now.saturating_sub(secs) < age {
            let _ = writeln!(ctx.out(), "keep {name} (younger than the limit)");
            continue;
        }
        listed += 1;
        if !apply {
            let _ = writeln!(ctx.out(), "would drop {name}");
            continue;
        }
        let Some(live) = live.clone() else {
            return Err(stop(
                ctx,
                Exit::Failed,
                "the database connection cannot be described",
            ));
        };
        let target = RestoreTarget::Database {
            connection: live.clone(),
            database: name.clone(),
        };
        match drop_scratch(&live, &target, &quarantine_marker(&prep.site.site, secs)).await {
            Ok(_) => {
                dropped += 1;
                let _ = writeln!(ctx.out(), "dropped {name}");
                let _ = audit(ctx, &prep, "restore_quarantine_purged", 1, None, false).await;
            }
            Err(_) => {
                let _ = writeln!(ctx.err(), "backup.io: {name} could not be dropped");
                return Ok(Exit::Failed);
            }
        }
    }
    if !apply {
        let _ = writeln!(
            ctx.out(),
            "dry run: {listed} quarantined database(s) would be dropped; add --apply"
        );
    } else {
        let _ = writeln!(ctx.out(), "purge ok dropped={dropped}");
    }
    Ok(Exit::Ok)
}
