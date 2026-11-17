//! `spot backup`: run, list, verify and prune a site's backups (WP-PRD-04.T2). Guide:
//! `docs/running-spot/backup.md`.
//!
//! - `run`: a logical dump of the site's database, sealed with the site's key provider when it
//!   has one, written to a named folder target of `common.toml` (`[backup.targets.<name>]`),
//!   checked after the write (`--full` also restores it into a scratch database and drops that
//!   again); `--kind stopped` instead copies the bundled PostgreSQL's data directory while the
//!   instance is stopped (WP-PRD-04.T8; refused with `backup.unsupported` for an external server);
//! - `list`: the site's backups on the target and whether each one passed verification;
//! - `verify`: checks one backup (the newest by default) and records the verdict;
//! - `prune`: prints what retention would remove; removes it only with `--apply`.
//!
//! The target always comes from `common.toml`, never from a path on the command line. Every
//! command holds the target's ledger lock while it works, so two never overlap. Exit codes:
//! 0 done, 1 usage, 2 refused (nothing changed), 3 failed. A key, a password or a connection
//! string never reaches the output, an error or the audit record: every line passes `scrub`.
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_backup::{
    AlertSink, BackupAlert, BackupError, HealthCondition, HealthSink, InvariantReport,
    MediaNotifier, RetentionPolicy, VerifyMode,
};
use spot_cli::config::{BackupRetention, Loaded, Site};
use spot_cli::services::secrets::{self as svc, OperatorAudit};
use spot_pg_supervisor::layout::PgLayout;
use spot_postgres::{PgConfig, Tls};
use spot_secrets::KeyProvider;

use super::{CommandCtx, Exit};

#[path = "backup/create.rs"]
mod create;
#[path = "backup/list.rs"]
mod list;
#[path = "backup/prune.rs"]
mod prune;
#[path = "backup/scratch.rs"]
mod scratch;
/// Target selection for every kind of target (WP-PRD-04.T13).
#[path = "backup/target.rs"]
pub mod target;
#[path = "backup/verify.rs"]
mod verify;

/// The subcommand name.
pub const NAME: &str = "backup";

const TARGET_UNSAFE: &str = "backup.target_unsafe";
const TARGET_UNAVAILABLE: &str = "backup.target_unavailable";
const UNSUPPORTED: &str = "backup.unsupported";
const INSTANCE_RUNNING: &str = "backup.instance_running";
/// The `provider_kind` a stopped-instance snapshot records in its manifest.
const STOPPED_KIND: &str = "stopped_instance";

/// Arguments and help.
pub fn command() -> Command {
    let site = || {
        Arg::new("site")
            .long("site")
            .value_name("SITE")
            .help("The site to act on (required)")
    };
    let target = || {
        Arg::new("target")
            .long("target")
            .value_name("NAME")
            .help("A target of common.toml [backup.targets] (default: backup.default_target, or the only one)")
    };
    let full_flag = |help: &'static str| {
        Arg::new("full")
            .long("full")
            .action(ArgAction::SetTrue)
            .help(help)
    };
    Command::new(NAME)
        .about("Back up a site to a configured target; list, verify and prune its backups")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("run")
                .about("Take, seal, write and verify one backup")
                .arg(site())
                .arg(target())
                .arg(
                    Arg::new("kind")
                        .long("kind")
                        .value_name("KIND")
                        .help("logical (the default) or stopped: a file copy of the bundled PostgreSQL, which must be stopped (the site is unavailable meanwhile)"),
                )
                .arg(full_flag(
                    "Also restore the backup into a scratch database and check it",
                ))
                .arg(
                    Arg::new("allow_unencrypted")
                        .long("allow-unencrypted")
                        .action(ArgAction::SetTrue)
                        .help(
                            "Accept a site without a key provider on a target that is not a folder",
                        ),
                ),
        )
        .subcommand(
            Command::new("list")
                .about("The site's backups on the target")
                .arg(site())
                .arg(target()),
        )
        .subcommand(
            Command::new("verify")
                .about("Verify one backup (the newest by default) and record the result")
                .arg(site())
                .arg(target())
                .arg(
                    Arg::new("id")
                        .long("id")
                        .value_name("ID")
                        .help("The backup to verify"),
                )
                .arg(full_flag(
                    "Also restore it into a scratch database and check it",
                )),
        )
        .subcommand(
            Command::new("prune")
                .about("Show what retention would remove; remove it with --apply")
                .arg(site())
                .arg(target())
                .arg(
                    Arg::new("apply")
                        .long("apply")
                        .action(ArgAction::SetTrue)
                        .help("Remove the backups; without it only the plan is printed"),
                ),
        )
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot backup <run|list|verify|prune> --site SITE"
        )?;
        return Ok(Exit::Usage);
    };
    let done = match verb {
        "run" => create::run_backup(ctx, sub).await,
        "list" => list::list_backups(ctx, sub).await,
        "verify" => verify::verify_backup(ctx, sub).await,
        "prune" => prune::prune_backups(ctx, sub).await,
        _ => Err(stop(
            ctx,
            Exit::Usage,
            format!("unknown backup command `{verb}`"),
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

fn scrub(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(secret.as_str(), "***");
    }
    out
}

fn stop_err(ctx: &mut CommandCtx, exit: Exit, error: &BackupError, secrets: &[String]) -> Stop {
    stop(
        ctx,
        exit,
        scrub(&format!("{}: {}", error.code(), error.reason()), secrets),
    )
}

/// Refused when another run holds the lock or the target cannot be used; failed otherwise.
fn exit_of(error: &BackupError) -> Exit {
    match error {
        BackupError::LedgerLocked { .. }
        | BackupError::TargetUnavailable { .. }
        | BackupError::NotFound { .. }
        | BackupError::Unsupported { .. } => Exit::Refused,
        _ => Exit::Failed,
    }
}

/// What every verb needs: the configuration, the site and the resolved target directory.
struct Prepared {
    loaded: Loaded,
    site: Site,
    name: String,
    dir: PathBuf,
    /// How to eject a removable drive, printed after a successful run.
    eject_hint: Option<String>,
    cfg: PgConfig,
    retention: BackupRetention,
    verify: VerifyMode,
    secrets: Vec<String>,
}

async fn prepare(ctx: &mut CommandCtx, matches: &ArgMatches, writes: bool) -> Step<Prepared> {
    prepare_with(ctx, matches, writes, Vec::new()).await
}

/// [`prepare`] with directories a target must not be inside or hold (the bundled PostgreSQL's
/// `data_dir`, `bin_dir` and `log_dir` for a stopped-instance backup).
async fn prepare_with(
    ctx: &mut CommandCtx,
    matches: &ArgMatches,
    writes: bool,
    extra_forbidden: Vec<PathBuf>,
) -> Step<Prepared> {
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
    let wanted = matches.get_one::<String>("target").map(String::as_str);
    let (name, target) = match loaded.common.backup.target(wanted) {
        Ok((name, target)) => (name.to_owned(), target.clone()),
        Err(error) => return Err(stop(ctx, Exit::Refused, error.to_string())),
    };
    // Every kind resolves to a directory through one place (`backup/target.rs`): a folder is its
    // path, a removable drive its mount point plus `subdir` (an unplugged drive is
    // `backup.target_absent`, retry at the next slot), a cloud target is refused until the host
    // supplies a connector.
    let resolved = match target::resolve(&target, target::system_devices()) {
        Ok(resolved) => resolved,
        Err(refusal) => {
            let retry = if refusal.retry_next_slot {
                " (nothing was written; the next scheduled slot will try again)"
            } else {
                ""
            };
            let text = format!("{}: {}{retry}", refusal.code, refusal.reason);
            return Err(stop(ctx, Exit::Refused, text));
        }
    };
    let path = resolved.dir.clone();
    let eject_hint = resolved.eject_hint.clone();
    // Never inside the site data, an app directory or the PostgreSQL data directory.
    let mut forbidden = vec![ctx.root().join("sites")];
    forbidden.extend(extra_forbidden);
    forbidden.extend(loaded.common.apps.iter().map(|app| ctx.root().join(app)));
    if let Some(pgdata) = std::env::var_os("PGDATA").filter(|v| !v.is_empty()) {
        forbidden.push(PathBuf::from(pgdata));
    }
    if writes && let Some(dir) = pg_data_dir(ctx, &site.site).await {
        forbidden.push(dir);
    }
    let dir = match resolve_target(&path, &forbidden) {
        Ok(dir) => dir,
        Err((code, why)) => return Err(stop(ctx, Exit::Refused, format!("{code}: {why}"))),
    };
    let cfg = loaded.admin.clone().with_dbname(&site.database);
    let mut secrets = vec![cfg.password.clone()];
    secrets.extend(connection(&cfg, &site.database));
    let verify = if loaded.common.backup.verify == "full" {
        VerifyMode::Full
    } else {
        VerifyMode::Quick
    };
    let retention = loaded.common.backup.retention;
    Ok(Prepared {
        loaded,
        site,
        name,
        dir,
        eject_hint,
        cfg,
        retention,
        verify,
        secrets,
    })
}

/// The data directory the server reports, when the database role may read the setting.
async fn pg_data_dir(ctx: &mut CommandCtx, site: &str) -> Option<PathBuf> {
    let backend = ctx.backend(site).await.ok()?;
    let client = backend.pool().get().await.ok()?;
    let row = client.query_one("SHOW data_directory", &[]).await.ok()?;
    row.try_get::<_, String>(0).ok().map(PathBuf::from)
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

/// How the manifest's schema version was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionSource {
    /// Read from the site's `spot_meta` (`catalog_version`): a real recorded version.
    Recorded,
    /// The site records none (never migrated, or a stopped-instance backup that cannot ask the
    /// stopped server): the manifest says 0 and a restore must treat it as unverified.
    Unrecorded,
}

impl VersionSource {
    /// The word the summary line and the audit record use.
    pub fn label(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Unrecorded => "unrecorded",
        }
    }
}

/// The version a site's recorded catalogue text means: empty is unrecorded (0), a whole number is
/// recorded, anything else is an error (never a guess).
///
/// # Errors
///
/// The text is neither empty nor a whole number.
pub fn parse_recorded_version(text: &str) -> Result<(u64, VersionSource), String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok((0, VersionSource::Unrecorded));
    }
    text.parse::<u64>()
        .map(|version| (version, VersionSource::Recorded))
        .map_err(|_| "the recorded schema version is not a whole number".to_owned())
}

/// The site's recorded schema version, read through the existing facts reader of the snapshot
/// crate (`spot_meta.catalog_version`, the value the schema preparation writes and only ever
/// moves forward). It is read on its own connection just before the dump starts, not inside the
/// dump's snapshot (the provider does not expose one): a migration that lands in between makes
/// the manifest one version behind the data, never ahead.
async fn read_site_version(prep: &Prepared) -> Result<(u64, VersionSource), String> {
    let backend = spot_cli::schema::backend(&prep.loaded, &prep.site)
        .await
        .map_err(|e| e.to_string())?;
    let conn = backend
        .pool()
        .get()
        .await
        .map_err(|_| "no database connection is available".to_owned())?;
    let facts = spot_snapshot::facts::read_database_facts(&conn)
        .await
        .map_err(|e| e.to_string())?;
    parse_recorded_version(&facts.catalogue_version)
}

/// Where the directory of the PostgreSQL client tools came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgBinSource {
    /// `[backup] pg_bin_dir`.
    Config,
    /// The `SPOT_PG_BIN` environment variable.
    Environment,
    /// The first absolute `PATH` entry that holds `pg_dump`.
    Path,
}

impl PgBinSource {
    /// The word the run's notice uses.
    pub fn label(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Environment => "SPOT_PG_BIN",
            Self::Path => "PATH",
        }
    }
}

fn has_pg_dump(dir: &Path) -> bool {
    dir.join("pg_dump").is_file() || dir.join("pg_dump.exe").is_file()
}

/// The numeric user id of this process, where the platform tells us without a system call
/// wrapper (Linux's `/proc/self`); `None` elsewhere, where the ownership rule is not enforced.
#[cfg(unix)]
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// Refuses a client-tools directory that cannot be trusted: it must exist and be a directory, hold
/// `pg_dump`, and (Unix) neither the directory (after following any link) nor the `pg_dump` file
/// may be world-writable, and both must belong to root or to the user running this process.
fn check_pg_bin_dir(dir: &Path, source: PgBinSource) -> Result<(), String> {
    let at = source.label();
    if !dir.is_absolute() {
        return Err(format!(
            "backup.snapshot: the PostgreSQL client tools directory ({at}) must be absolute"
        ));
    }
    if !dir.is_dir() {
        return Err(format!(
            "backup.snapshot: the PostgreSQL client tools directory {} ({at}) does not exist or is not a directory",
            dir.display()
        ));
    }
    if !has_pg_dump(dir) {
        return Err(format!(
            "backup.snapshot: the PostgreSQL client tools directory {} ({at}) holds no pg_dump",
            dir.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let unsafe_dir = |why: &str| {
            format!(
                "{TARGET_UNSAFE}: the PostgreSQL client tools directory {} ({at}) {why}; refusing to run its tools",
                dir.display()
            )
        };
        // Follow links: a link into a directory others can write is as unsafe as that directory.
        let real = std::fs::canonicalize(dir).map_err(|_| unsafe_dir("cannot be resolved"))?;
        let me = current_uid();
        for path in [real.clone(), real.join("pg_dump")] {
            // A missing `pg_dump` here means the `.exe` spelling; nothing to check on it on Unix.
            let Ok(meta) = std::fs::metadata(&path) else {
                if path.ends_with("pg_dump") {
                    continue;
                }
                return Err(unsafe_dir("is unreadable"));
            };
            if meta.permissions().mode() & 0o002 != 0 {
                return Err(unsafe_dir("or its pg_dump is world-writable"));
            }
            if me.is_some_and(|me| meta.uid() != 0 && meta.uid() != me) {
                return Err(unsafe_dir(
                    "or its pg_dump belongs to neither root nor this user",
                ));
            }
        }
    }
    Ok(())
}

/// The directory of the PostgreSQL client tools, decided by one rule:
///
/// 1. `[backup] pg_bin_dir` (`config`), then
/// 2. `SPOT_PG_BIN` (looked up through `env`), then
/// 3. the first `PATH` entry (`path_var`) that holds `pg_dump`. An empty entry and a relative
///    entry are skipped, never read as the current directory.
///
/// The first source that is set but unusable is an error: it never falls through to the next.
/// The environment and `PATH` are arguments so the function reads no global state.
///
/// # Errors
///
/// The chosen directory is unsafe or holds no `pg_dump`, or no source names one (the message
/// lists the `PATH` directories that were searched, never a secret).
pub fn resolve_pg_bin_dir(
    config: Option<&Path>,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    path_var: Option<&std::ffi::OsStr>,
) -> Result<(PathBuf, PgBinSource), String> {
    if let Some(dir) = config {
        check_pg_bin_dir(dir, PgBinSource::Config)?;
        return Ok((dir.to_path_buf(), PgBinSource::Config));
    }
    if let Some(value) = env("SPOT_PG_BIN").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(value);
        check_pg_bin_dir(&dir, PgBinSource::Environment)?;
        return Ok((dir, PgBinSource::Environment));
    }
    let mut searched = Vec::new();
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(path_var) {
            if dir.as_os_str().is_empty() || !dir.is_absolute() {
                continue;
            }
            if has_pg_dump(&dir) {
                check_pg_bin_dir(&dir, PgBinSource::Path)?;
                return Ok((dir, PgBinSource::Path));
            }
            searched.push(dir.display().to_string());
        }
    }
    Err(format!(
        "backup.snapshot: the PostgreSQL client tools (pg_dump) were not found; set [backup] pg_bin_dir or SPOT_PG_BIN, or put them on PATH (searched: {})",
        if searched.is_empty() {
            "no absolute PATH entry".to_owned()
        } else {
            searched.join(", ")
        }
    ))
}

/// The client tools' directory for this run, from the site's `[backup]` table, the process
/// environment and `PATH`; the winning source is named on stderr (`pg_dump: <dir> (config)`).
fn pg_bin(ctx: &mut CommandCtx, backup: &spot_cli::config::Backup) -> Result<PathBuf, Stop> {
    let path_var = std::env::var_os("PATH");
    let found = resolve_pg_bin_dir(
        backup.pg_bin_dir.as_deref(),
        &|name| std::env::var_os(name),
        path_var.as_deref(),
    );
    match found {
        Ok((dir, source)) => {
            let _ = writeln!(ctx.err(), "pg_dump: {} ({})", dir.display(), source.label());
            Ok(dir)
        }
        Err(text) => Err(stop(ctx, Exit::Refused, text)),
    }
}

/// The bundled PostgreSQL's layout for this host, or why the site does not have one.
///
/// `spot-cli` has no setting for a bundled server, so the layout follows the install convention
/// of `PgLayout::default()` (`pgsql/bin`, `pgsql/data`, `logs`) below the host root, and it is
/// used only when the server the site connects to is that instance: a loopback address on the
/// layout's port, a data directory marked by `PG_VERSION`, and the server binary present. A
/// data directory is never taken from the command line, and any doubt is a refusal. The reason
/// never quotes the connection URL (it holds the password).
fn bundled_layout(root: &Path, admin_url: &str) -> Result<PgLayout, String> {
    let refuse = |why: &str| {
        format!(
            "{UNSUPPORTED}: this site does not use the bundled PostgreSQL server ({why}); a stopped-kind backup is only available for it, so use --kind logical"
        )
    };
    let url =
        url::Url::parse(admin_url).map_err(|_| refuse("the server address cannot be read"))?;
    if !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) {
        return Err(refuse("the server is not on this machine"));
    }
    let base = root.join("pgsql");
    let layout = PgLayout {
        bin_dir: base.join("bin"),
        data_dir: base.join("data"),
        log_dir: root.join("logs"),
        ..PgLayout::default()
    };
    if url.port() != Some(layout.port) {
        return Err(refuse("the server port is not the bundled server's"));
    }
    let server = if cfg!(windows) {
        "postgres.exe"
    } else {
        "postgres"
    };
    if !layout.data_dir.join("PG_VERSION").is_file() || !layout.bin_dir.join(server).is_file() {
        return Err(refuse("no bundled server is installed under the host root"));
    }
    Ok(layout)
}

/// Whether the bundled instance is up: a postmaster pid file in its data directory, or something
/// answering on its port. A stopped-kind backup never copies the files of a live instance.
fn instance_running(layout: &PgLayout) -> bool {
    if layout.data_dir.join(concat!("postmaster", ".pid")).exists() {
        return true;
    }
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], layout.port));
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(1)).is_ok()
}

/// The layout for `--kind stopped`: the bundled server of this host, stopped.
fn stopped_layout(ctx: &mut CommandCtx) -> Step<PgLayout> {
    let loaded = match spot_cli::config::load(ctx.root(), false) {
        Ok(loaded) => loaded,
        Err(error) => return Err(stop(ctx, Exit::Failed, error.to_string())),
    };
    let layout = match bundled_layout(ctx.root(), &loaded.common.postgres.admin_url) {
        Ok(layout) => layout,
        Err(text) => return Err(stop(ctx, Exit::Refused, text)),
    };
    if instance_running(&layout) {
        let text = format!(
            "{INSTANCE_RUNNING}: the bundled PostgreSQL is running; a stopped-kind backup copies its data directory and never does that while it is up. Stop the service first (the site is unavailable until it is started again), or use --kind logical; nothing was written"
        );
        return Err(stop(ctx, Exit::Refused, text));
    }
    Ok(layout)
}

/// The ids a backup may have: letters, digits, `.`, `_`, `-`, starting with a letter or digit.
fn valid_id(id: &str) -> bool {
    id.len() <= 200
        && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
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

#[cfg(unix)]
fn make_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}
#[cfg(not(unix))]
fn make_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// A private scratch directory inside the target, removed when dropped. Creating it is also the
/// writability check: a target the user cannot write to is refused before anything is written.
struct WorkDir(PathBuf);

impl WorkDir {
    fn create(dir: &Path) -> std::io::Result<WorkDir> {
        let mut last = None;
        for n in 0..8u32 {
            let path = dir.join(format!(".work-{}-{n}", std::process::id()));
            match private_dir(&path) {
                Ok(()) => return Ok(WorkDir(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no work directory name was free")))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Health conditions, alerts and media notices of one command, printed on standard error at the
/// end: a verification failure is an alert the operator sees, not a silent log line.
#[derive(Default)]
struct Notices(Mutex<Vec<String>>);

impl Notices {
    fn push(&self, line: String) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}
impl HealthSink for Notices {
    fn raise(&self, condition: HealthCondition) {
        self.push(format!(
            "health {} ({}): {}",
            condition.code,
            condition.severity.as_str(),
            condition.message
        ));
    }
}
impl AlertSink for Notices {
    fn alert(&self, alert: BackupAlert) {
        self.push(format!("ALERT {}: {}", alert.code, alert.message));
    }
}
impl MediaNotifier for Notices {
    fn waiting(&self, volume: &str) {
        self.push(format!("waiting for the media of volume {volume}"));
    }
}

fn flush_notices(ctx: &mut CommandCtx, notes: &Notices, secrets: &[String]) {
    for line in notes.take() {
        let _ = writeln!(ctx.err(), "{}", scrub(&line, secrets));
    }
}

/// The audit chain, or `None` when it cannot be opened.
async fn audit_open(clock: Arc<dyn spot_core::Clock>, prep: &Prepared) -> Option<OperatorAudit> {
    OperatorAudit::open(&prep.loaded, &prep.site, clock)
        .await
        .ok()
}

/// The site's key provider, or `None` when the host configures no secrets service.
async fn key_provider(ctx: &mut CommandCtx, prep: &Prepared) -> Step<Option<Arc<dyn KeyProvider>>> {
    let opened = svc::open(ctx.root(), &prep.loaded, &prep.site).await;
    match opened {
        Ok(Some(host)) => {
            let provider: Arc<dyn KeyProvider> = host.keyring().clone();
            Ok(Some(provider))
        }
        Ok(None) => Ok(None),
        Err(error) => Err(stop(ctx, Exit::Refused, error.to_string())),
    }
}

fn retention_policy(retention: &BackupRetention) -> RetentionPolicy {
    // All four numbers of `[backup.retention]` go to the library policy; the protection window
    // is the library's own.
    RetentionPolicy {
        keep_daily: retention.keep_daily,
        keep_weekly: retention.keep_weekly,
        keep_monthly: retention.keep_monthly,
        keep_last: retention.keep_last,
        protect_newer_than_days: RetentionPolicy::default().protect_newer_than_days,
    }
}

/// A schedule the runner needs for its job shape; a manual run never fires it.
fn manual_schedule() -> Result<spot_scheduler::Schedule, String> {
    let cron = spot_manifest::Cron::from_str("0 0 0 * * *")
        .map_err(|_| "the manual schedule is invalid")?;
    let step = spot_app_api::StepName::new("spot_backup:backup")
        .map_err(|_| "the manual step is invalid")?;
    let zone = spot_scheduler::TimeZone::parse("UTC").map_err(|_| "the UTC zone is missing")?;
    spot_scheduler::ScheduleBuilder::new("manual")
        .cron(cron)
        .step(step)
        .zone(zone)
        .build()
        .map_err(|_| "the manual schedule did not build".to_owned())
}

/// What a passed check proves, stated exactly: never a bare "verified". A full verification names
/// the L7 invariants that ran; the older wording appears only when none could run (a stopped
/// copy of a data directory is not restored into a database, so it has none to run).
fn verdict(mode: VerifyMode, invariants: Option<&InvariantReport>) -> String {
    match mode {
        VerifyMode::Full => match invariants {
            Some(report) if report.runner.contains("capture_invariant") => {
                format!(
                    "verified: snapshot checks + L7 invariants ({})",
                    report.runner
                )
            }
            _ => "verified: snapshot checks only (L7 invariants not run)".to_owned(),
        },
        VerifyMode::Quick => {
            "verified: digest, size and header only (nothing was restored)".to_owned()
        }
    }
}

fn verdict_code(mode: VerifyMode, invariants: Option<&InvariantReport>) -> &'static str {
    match mode {
        VerifyMode::Full if invariants.is_some_and(|r| r.runner.contains("capture_invariant")) => {
            "verified_snapshot_and_l7"
        }
        VerifyMode::Full => "verified_snapshot_checks_only",
        VerifyMode::Quick => "verified_digest_only",
    }
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod version_tests {
    use super::{VersionSource, parse_recorded_version};

    #[test]
    fn a_recorded_text_is_read_without_guessing() {
        assert_eq!(
            parse_recorded_version("7"),
            Ok((7, VersionSource::Recorded))
        );
        assert_eq!(
            parse_recorded_version(" 12 "),
            Ok((12, VersionSource::Recorded))
        );
        assert_eq!(
            parse_recorded_version(""),
            Ok((0, VersionSource::Unrecorded))
        );
        assert!(parse_recorded_version("v3").is_err());
        assert!(parse_recorded_version("-1").is_err());
        assert!(parse_recorded_version("99999999999999999999999").is_err());
    }
}
