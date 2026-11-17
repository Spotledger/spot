//! `spot update check|download|apply|rollback|status` (WP-PRD-03.T5). Guide:
//! `docs/running-spot/update.md`.
//!
//! - `check`: fetch the signed manifest, verify it (signature, trust anchor, key window,
//!   revocation) and say what would happen; nothing is downloaded or changed;
//! - `download`: verify, fetch the archive (digest checked) and keep it in the install root;
//! - `apply`: without `--apply` the plan; with it, inside the maintenance window only, a backup of
//!   every site first (a failed backup aborts), then the engine: stage, check the new version's
//!   migration plan, switch, restart, wait for readiness, roll back on any failure;
//! - `rollback`: switch back to the previous version (the data is not restored);
//! - `status`: what is installed, the journal and the last check.
//!
//! `--repo` (check, download, apply) reads the update from removable media instead of a server:
//! a runtime bundle made by `spot airgap export` (the bundle directory, its `offline.toml`, or the
//! export directory holding `runtime/`), the local repository `spot airgap import` fills, or a
//! plain update folder. A bundle or repository is verified the way `spot airgap import` verifies it
//! (signature chain, window, revocation list and its held generation, every file's size and
//! SHA-256, nothing unlisted, no symbolic link followed) and a downgrade is refused with
//! `airgap.downgrade`, all **before** anything is read as an update and before anything on disk is
//! written; then the update goes on as the online one does (its own signed manifest is verified
//! against the update trust anchor too). The newest revocation generation seen is remembered only
//! after `download` or `apply --apply` succeeded.
//!
//! The trust anchor is the file installed with the package; no flag names another, no flag skips a
//! verification and no flag lowers the TLS requirement. Exit codes: 0 done, 1 usage, 2 refused
//! (nothing changed), 3 failed. Output carries versions and stable codes, never a URL.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{Arg, ArgAction, ArgMatches, Command};
use semver::Version;
use spot_airgap::manifest::{MEDIA_FILES_DIR, MEDIA_MANIFEST};
use spot_airgap::{
    AirgapError, BundleKind, BundleTrust, LocalRepo, RepoEntry, Storage, verify_file, walk_files,
};
use spot_bundle_signing::{PublicKey, RevocationList, TrustAnchor, TrustedRoot};
use spot_cli::Error;
use spot_cli::services::updater::{
    self as up, HttpsClient, Offer, Seams, Settings, Source, Status,
};
use spot_core::Clock;
use spot_updater::{
    Channel, DataHooks, Decision, Digest, FileSystem, Journal, Layout, Outcome, ServiceControl,
    StdFileSystem, UpdateManifest, UpdaterError, UpdaterErrorCode, verify_payload,
};

use super::airgap::{HELD_GENERATION_FILE, NoLinkStorage, verify_media};
use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "update";

/// Arguments and help.
pub fn command() -> Command {
    let channel = || {
        Arg::new("channel")
            .long("channel")
            .value_name("CHANNEL")
            .help("stable or beta (default: services.updater.channel)")
    };
    let repo = || {
        [
            Arg::new("repo")
                .long("repo")
                .value_name("DIR")
                .help("Offline media instead of the update server: an airgap runtime bundle, an export or local repository directory, or an update folder"),
            Arg::new("trust")
                .long("trust")
                .value_name("FILE")
                .requires("repo")
                .help("With --repo: trust roots, one base64 root public key per line (default: [trust] roots in common.toml)"),
            Arg::new("revocations")
                .long("revocations")
                .value_name("FILE")
                .requires("repo")
                .help("With --repo: the signed revocation list, JSON (default: [trust] revocations in common.toml)"),
        ]
    };
    let apply = |help: &'static str| {
        Arg::new("apply")
            .long("apply")
            .action(ArgAction::SetTrue)
            .help(help)
    };
    Command::new(NAME)
        .about("Check for, download and apply a signed update; roll back; show the status")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("check")
                .about("Verify the offered update and say what would happen")
                .arg(channel())
                .args(repo()),
        )
        .subcommand(
            Command::new("download")
                .about("Verify the offered update and keep its archive for `apply`")
                .arg(channel())
                .args(repo()),
        )
        .subcommand(
            Command::new("apply")
                .about("Back up, stage, switch, restart and check; roll back on failure")
                .arg(channel())
                .args(repo())
                .arg(apply("Do it; without it only the plan is shown")),
        )
        .subcommand(
            Command::new("rollback")
                .about("Switch back to the previous version (the data is not restored)")
                .arg(apply("Do it; without it only the plan is shown")),
        )
        .subcommand(
            Command::new("status").about("The installed version, the journal, the last check"),
        )
}

/// Everything a verb needs.
struct Env {
    settings: Settings,
    parsed: up::Parsed,
    channel: Channel,
    root: PathBuf,
    layout: Layout,
    now: i64,
}

fn env(ctx: &mut CommandCtx, sub: &ArgMatches) -> spot_cli::Result<Env> {
    let settings: Settings = match ctx.common()?.services.services_extra.get("updater") {
        Some(table) => table
            .clone()
            .try_into()
            .map_err(|_| Error("services.updater: invalid settings".into()))?,
        None => Settings::default(),
    };
    let parsed = settings.validate()?;
    let channel = match sub.try_get_one::<String>("channel").ok().flatten() {
        Some(text) => {
            Channel::parse(text).map_err(|_| Error("--channel must be stable or beta".into()))?
        }
        None => parsed.channel,
    };
    let root = settings
        .install_root
        .clone()
        .unwrap_or_else(|| ctx.root().to_path_buf());
    Ok(Env {
        settings,
        parsed,
        channel,
        layout: Layout::new(root.clone()),
        root,
        now: ctx.clock().now_utc().unix_timestamp(),
    })
}

/// A stable code and message to the error writer; refused for a verification or policy outcome,
/// failed for an environment fault.
fn report(ctx: &mut CommandCtx, error: &UpdaterError) -> spot_cli::Result<Exit> {
    writeln!(ctx.err(), "{}: {error}", error.code().as_str())?;
    Ok(match error.code() {
        UpdaterErrorCode::Io
        | UpdaterErrorCode::DiskFull
        | UpdaterErrorCode::Service
        | UpdaterErrorCode::Health
        | UpdaterErrorCode::Hook => Exit::Failed,
        _ => Exit::Refused,
    })
}

/// Where this verb reads the update from when `--repo` is not given: (for `apply`) the downloaded
/// cache, else the configured server.
fn source(
    env: &Env,
    _sub: &ArgMatches,
    installed: &Version,
    cached: bool,
) -> spot_cli::Result<Source> {
    if cached
        && env
            .root
            .join(up::CACHE_DIR)
            .join(env.channel.as_str())
            .join("update.toml")
            .is_file()
    {
        return Ok(Source::Dir(env.root.join(up::CACHE_DIR)));
    }
    let base = env.parsed.url.clone().ok_or_else(|| {
        Error("no update source: set services.updater.url or pass --repo DIR".into())
    })?;
    let query = env.settings.telemetry.then(|| {
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        format!("v={installed}&p={os}-{arch}&c={}", env.channel.as_str())
    });
    Ok(Source::Https {
        base,
        client: HttpsClient::platform()?,
        query,
    })
}

// --- --repo: offline media, verified before it is read -------------------------------------------

/// Why `--repo` stopped the command: nothing was changed.
enum RepoStop {
    /// Wrong or missing arguments (exit 1).
    Usage(String),
    /// Refused by a check (exit 2).
    Refused(String),
    /// An environment fault (exit 3).
    Failed(String),
}

impl From<AirgapError> for RepoStop {
    fn from(error: AirgapError) -> Self {
        let text = format!("refused: {}: {error}", error.code());
        match error {
            AirgapError::Io { .. } => RepoStop::Failed(text),
            _ => RepoStop::Refused(text),
        }
    }
}

/// The newest revocation generation a verified `--repo` showed: remembered once the command
/// succeeded, so a refused or failed run changes nothing.
struct Held {
    file: PathBuf,
    generation: u64,
}

/// Where this verb reads the update from: `--repo` (verified as `spot airgap import` verifies),
/// else what [`source`] picks. `Err(exit)` when `--repo` stopped the command (the reason is
/// already written).
fn pick(
    ctx: &mut CommandCtx,
    env: &Env,
    sub: &ArgMatches,
    installed: &Version,
    cached: bool,
) -> spot_cli::Result<Result<(Source, Option<Held>), Exit>> {
    let Some(text) = sub.try_get_one::<String>("repo").ok().flatten() else {
        return Ok(Ok((source(env, sub, installed, cached)?, None)));
    };
    match offline(ctx, env, sub, text, installed) {
        Ok(found) => Ok(Ok(found)),
        Err(stop) => {
            let (exit, text) = match stop {
                RepoStop::Usage(text) => (Exit::Usage, text),
                RepoStop::Refused(text) => (Exit::Refused, text),
                RepoStop::Failed(text) => (Exit::Failed, text),
            };
            writeln!(ctx.err(), "{text}")?;
            Ok(Err(exit))
        }
    }
}

/// Resolves `--repo` to a verified update source:
///
/// 1. a bundle directory (it holds `offline.toml`), that file itself, or an export directory whose
///    `runtime/` holds one: verified end to end like `spot airgap verify`, which must be a runtime
///    bundle; its `files/` is the update folder;
/// 2. a local repository (`spot airgap import`'s): its newest runtime bundle, every file checked
///    against the size and digest the index records and nothing unlisted present;
/// 3. otherwise a plain update folder, whose signed manifest the updater verifies as for a server.
///
/// A downgrade is refused in 1 and 2.
fn offline(
    ctx: &mut CommandCtx,
    env: &Env,
    sub: &ArgMatches,
    text: &str,
    installed: &Version,
) -> Result<(Source, Option<Held>), RepoStop> {
    let storage = NoLinkStorage::new();
    let mut path = PathBuf::from(text);
    if path.is_file() {
        if path.file_name().and_then(|name| name.to_str()) != Some(MEDIA_MANIFEST) {
            return Err(RepoStop::Usage(format!(
                "--repo takes a directory, or the {MEDIA_MANIFEST} of a bundle directory"
            )));
        }
        path = path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| RepoStop::Usage("--repo names no directory".to_owned()))?;
    }
    for candidate in [path.clone(), path.join("runtime")] {
        if storage.exists(&candidate.join(MEDIA_MANIFEST))? {
            return from_media(ctx, env, sub, &storage, &candidate, installed);
        }
    }
    let repo = LocalRepo::new(&path);
    let index = repo.read_index(&storage)?;
    let newest = index
        .entries
        .iter()
        .filter(|entry| entry.kind == BundleKind::Runtime)
        .max_by(|a, b| a.version.cmp(&b.version));
    if let Some(entry) = newest {
        let dir = repo.bundle_dir(entry.kind, &entry.name, &entry.version);
        verify_entry(&storage, &dir, entry)?;
        refuse_downgrade(&entry.version, installed)?;
        return Ok((Source::Dir(dir), None));
    }
    Ok((Source::Dir(path), None))
}

/// A runtime bundle on media: verified as `spot airgap verify` does, against the trust of
/// `[trust]` in `common.toml` or `--trust` and `--revocations`.
fn from_media(
    ctx: &mut CommandCtx,
    env: &Env,
    sub: &ArgMatches,
    storage: &NoLinkStorage,
    media: &Path,
    installed: &Version,
) -> Result<(Source, Option<Held>), RepoStop> {
    let (anchor, revocations) = airgap_trust(ctx, sub)?;
    let file = env.root.join(HELD_GENERATION_FILE);
    let trust = BundleTrust {
        anchor: &anchor,
        revocations: &revocations,
        now: env.now,
        held_revocation_generation: held_generation(storage, &file),
    };
    let verified = verify_media(storage, media, &trust)?;
    if verified.kind != BundleKind::Runtime {
        return Err(RepoStop::Refused(format!(
            "refused: the bundle holds {}, not the runtime",
            verified.kind.as_str()
        )));
    }
    refuse_downgrade(&verified.version, installed)?;
    let held = Held {
        file,
        generation: revocations.body.generation,
    };
    Ok((Source::Dir(media.join(MEDIA_FILES_DIR)), Some(held)))
}

/// A bundle older than what is installed is never offered.
fn refuse_downgrade(offered: &Version, installed: &Version) -> Result<(), RepoStop> {
    if offered < installed {
        return Err(AirgapError::Downgrade {
            installed: installed.to_string(),
            offered: offered.to_string(),
        }
        .into());
    }
    Ok(())
}

/// Every file an imported bundle's index lists matches its size and SHA-256, and no other file is
/// present.
fn verify_entry(storage: &dyn Storage, dir: &Path, entry: &RepoEntry) -> Result<(), AirgapError> {
    for present in walk_files(storage, dir)? {
        if !entry.files.iter().any(|file| file.path == present) {
            return Err(AirgapError::ExtraFile { path: present });
        }
    }
    for file in &entry.files {
        let path = dir.join(&file.path);
        if !storage.exists(&path)? {
            return Err(AirgapError::FileMissing {
                path: file.path.clone(),
            });
        }
        let bytes = storage
            .read(&path)
            .map_err(|_| AirgapError::MediaUnreadable {
                path: path.display().to_string(),
            })?;
        verify_file(file, &bytes)?;
    }
    Ok(())
}

/// A regular file of sane size (the trust roots and the revocation list), never a link.
fn read_regular(path: &Path, what: &str) -> Result<Vec<u8>, RepoStop> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|_| RepoStop::Refused(format!("refused: the {what} file cannot be read")))?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return Err(RepoStop::Refused(format!(
            "refused: the {what} is not a regular file of sane size"
        )));
    }
    std::fs::read(path)
        .map_err(|_| RepoStop::Refused(format!("refused: the {what} file cannot be read")))
}

/// The roots and revocation list a bundle's envelope is checked against: `--trust` and
/// `--revocations`, else `[trust]` in `common.toml`. The same files as `spot airgap`; this host
/// trusts nothing without them.
fn airgap_trust(
    ctx: &mut CommandCtx,
    sub: &ArgMatches,
) -> Result<(TrustAnchor, RevocationList), RepoStop> {
    let explicit_roots = sub
        .try_get_one::<String>("trust")
        .ok()
        .flatten()
        .map(PathBuf::from);
    let explicit_revocations = sub
        .try_get_one::<String>("revocations")
        .ok()
        .flatten()
        .map(PathBuf::from);
    let root = ctx.root().to_path_buf();
    let common = ctx.common().map_err(|error| {
        RepoStop::Refused(format!(
            "refused: no trust configured ({error}); pass --trust and --revocations"
        ))
    })?;
    let roots = explicit_roots.or_else(|| common.trust.roots.as_ref().map(|p| root.join(p)));
    let revocations =
        explicit_revocations.or_else(|| common.trust.revocations.as_ref().map(|p| root.join(p)));
    let (Some(roots), Some(revocations)) = (roots, revocations) else {
        return Err(RepoStop::Refused(
            "refused: this host trusts no publisher: pass --trust and --revocations, or set [trust] in common.toml"
                .to_owned(),
        ));
    };
    let text = String::from_utf8(read_regular(&roots, "trust roots")?)
        .map_err(|_| RepoStop::Refused("refused: the trust roots file is not text".to_owned()))?;
    let mut list = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let key = spot_bundle_signing::canonical::unb64(line)
            .and_then(|bytes| PublicKey::from_bytes(&bytes).ok())
            .ok_or_else(|| {
                RepoStop::Refused("refused: a trust root is not a public key".to_owned())
            })?;
        list.push(TrustedRoot::new(key, 0, None));
    }
    if list.is_empty() {
        return Err(RepoStop::Refused(
            "refused: the trust roots file names no root".to_owned(),
        ));
    }
    let revocations = RevocationList::from_json(&read_regular(&revocations, "revocation list")?)
        .map_err(|_| RepoStop::Refused("refused: the revocation list is not valid".to_owned()))?;
    Ok((TrustAnchor::new(list), revocations))
}

/// The newest revocation generation remembered in `file`; 0 when none.
fn held_generation(storage: &dyn Storage, file: &Path) -> u64 {
    match storage.exists(file) {
        Ok(true) => storage
            .read(file)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or(0),
        _ => 0,
    }
}

/// Remembers the generation of a list that verified, after the command succeeded. Best effort: a
/// failed write only means the next run holds the older number.
fn remember(held: Option<&Held>) {
    let Some(held) = held else { return };
    let storage = NoLinkStorage::new();
    if held.generation <= held_generation(&storage, &held.file) {
        return;
    }
    if let Some(parent) = held.file.parent() {
        let _ = storage.create_dir_all(parent);
    }
    let _ = storage.write_atomic(&held.file, format!("{}\n", held.generation).as_bytes());
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot update <check|download|apply|rollback|status>"
        )?;
        return Ok(Exit::Usage);
    };
    let env = env(ctx, sub)?;
    match verb {
        "check" => check(ctx, &env, sub, false).await,
        "download" => check(ctx, &env, sub, true).await,
        "apply" => apply(ctx, &env, sub).await,
        "rollback" => rollback(ctx, &env, sub.get_flag("apply")).await,
        "status" => status(ctx, &env),
        _ => {
            writeln!(ctx.err(), "unknown update command `{verb}`")?;
            Ok(Exit::Usage)
        }
    }
}

fn describe(manifest: &UpdateManifest, decision: &Decision) -> String {
    let (version, channel) = (&manifest.version, manifest.channel.as_str());
    match decision {
        Decision::Apply => format!("update {version} ({channel}) is available"),
        Decision::StepVia(via) => format!("update {version} needs {via} installed first"),
        Decision::Skip(reason) => format!("nothing to do: {reason}"),
        Decision::Refuse(code) => format!("update {version} is refused: {}", code.as_str()),
    }
}

/// `check` and `download`: verify, decide, and (for `download`) keep the archive.
async fn check(
    ctx: &mut CommandCtx,
    env: &Env,
    sub: &ArgMatches,
    download: bool,
) -> spot_cli::Result<Exit> {
    let fs = StdFileSystem::new();
    let installed = match up::installed(&fs, &env.layout, &env.settings, env.channel) {
        Ok(installed) => installed,
        Err(error) => return report(ctx, &error),
    };
    let (source, held) = match pick(ctx, env, sub, &installed.version, false)? {
        Ok(found) => found,
        Err(exit) => return Ok(exit),
    };
    let policy = env.settings.policy(env.channel);
    let (offer, decision) =
        match up::check(&source, &fs, &env.root, &installed, &policy, env.now).await {
            Ok(found) => found,
            Err(error) => {
                record(&fs, env, "failed", None, error.to_string());
                return report(ctx, &error);
            }
        };
    writeln!(
        ctx.out(),
        "installed {}; {}",
        installed.version,
        describe(&offer.manifest, &decision)
    )?;
    let (state, available) = match &decision {
        Decision::Apply | Decision::StepVia(_) => ("available", Some(&offer.manifest.version)),
        Decision::Skip(_) => ("current", None),
        Decision::Refuse(_) => ("refused", None),
    };
    record(
        &fs,
        env,
        state,
        available,
        describe(&offer.manifest, &decision),
    );
    if let Decision::Refuse(_) = decision {
        return Ok(Exit::Refused);
    }
    if download && matches!(decision, Decision::Apply) {
        let payload = match fetch_payload(&source, env.channel, &offer).await {
            Ok(payload) => payload,
            Err(error) => return report(ctx, &error),
        };
        if let Err(error) = store_cache(&fs, &env.root, env.channel, &offer, &payload) {
            return report(ctx, &error);
        }
        writeln!(
            ctx.out(),
            "downloaded and verified {} bytes; `spot update apply --apply` installs it",
            payload.len()
        )?;
        remember(held.as_ref());
    }
    Ok(Exit::Ok)
}

fn record(fs: &StdFileSystem, env: &Env, state: &str, available: Option<&Version>, detail: String) {
    let status = Status {
        checked_at: env.now,
        channel: env.channel.as_str().to_owned(),
        state: state.to_owned(),
        available: available.map(ToString::to_string),
        detail,
    };
    let _ = up::write_status(fs, &env.root, &status);
}

/// The archive of a verified offer: capped at the signed size, digest checked.
async fn fetch_payload(
    source: &Source,
    channel: Channel,
    offer: &Offer,
) -> Result<Vec<u8>, UpdaterError> {
    let m = &offer.manifest;
    let bytes = source
        .read(channel, &m.payload.file, m.payload.size)
        .await?;
    verify_payload(m, bytes.len() as u64, Digest::of(&bytes))?;
    Ok(bytes)
}

/// Keeps a verified offer and its archive, so `apply` can run without the network.
fn store_cache(
    fs: &dyn FileSystem,
    root: &Path,
    channel: Channel,
    offer: &Offer,
    payload: &[u8],
) -> Result<(), UpdaterError> {
    let dir = root.join(up::CACHE_DIR).join(channel.as_str());
    fs.remove_dir_all(&dir)?;
    fs.create_dir_all(&dir)?;
    for (name, text) in ["update.toml", "update.sig", "revocations.toml"]
        .iter()
        .zip(&offer.texts)
    {
        fs.write_atomic(&dir.join(name), text.as_bytes())?;
    }
    fs.write_atomic(&dir.join(&offer.manifest.payload.file), payload)
}

// --- apply ---------------------------------------------------------------------------------------

async fn apply(ctx: &mut CommandCtx, env: &Env, sub: &ArgMatches) -> spot_cli::Result<Exit> {
    let fs = StdFileSystem::new();
    let installed = match up::installed(&fs, &env.layout, &env.settings, env.channel) {
        Ok(installed) => installed,
        Err(error) => return report(ctx, &error),
    };
    let (source, held) = match pick(ctx, env, sub, &installed.version, true)? {
        Ok(found) => found,
        Err(exit) => return Ok(exit),
    };
    let policy = env.settings.policy(env.channel);
    let (offer, decision) =
        match up::check(&source, &fs, &env.root, &installed, &policy, env.now).await {
            Ok(found) => found,
            Err(error) => return report(ctx, &error),
        };
    writeln!(
        ctx.out(),
        "installed {}; {}",
        installed.version,
        describe(&offer.manifest, &decision)
    )?;
    match decision {
        Decision::Apply => {}
        Decision::Refuse(_) => return Ok(Exit::Refused),
        _ => return Ok(Exit::Ok),
    }
    let sites: Vec<String> = ctx.sites()?.iter().map(|s| s.site.clone()).collect();
    if !sub.get_flag("apply") {
        let window = env
            .parsed
            .window
            .map_or(String::new(), |_| " inside the maintenance window".into());
        writeln!(
            ctx.out(),
            "plan: back up {} site(s), stage, check the migration plan, switch, restart and wait for readiness; rolls back on failure. Nothing was changed; add --apply to run it{window}.",
            sites.len()
        )?;
        return Ok(Exit::Ok);
    }
    if env.parsed.window.is_some_and(|w| !w.contains(env.now)) {
        writeln!(
            ctx.err(),
            "refused: outside the maintenance window (services.updater.window)"
        )?;
        return Ok(Exit::Refused);
    }
    let payload = match fetch_payload(&source, env.channel, &offer).await {
        Ok(payload) => payload,
        Err(error) => return report(ctx, &error),
    };
    let trust = match up::load_trust(&fs, &env.root) {
        Ok(trust) => trust,
        Err(error) => return report(ctx, &error),
    };
    // The backup comes first, while the service still runs and before anything is staged; a
    // failed backup aborts the update with nothing changed.
    if !backup_sites(ctx, &sites).await? {
        writeln!(
            ctx.err(),
            "refused: the backup failed, so the update was not started"
        )?;
        return Ok(Exit::Failed);
    }
    let hooks = Hooks {
        backed_up: AtomicBool::new(true),
        host_root: ctx.root().to_path_buf(),
        staged: env.layout.staging_path(&offer.manifest.version),
        binary: offer
            .manifest
            .files
            .iter()
            .map(|f| f.path.clone())
            .find(|p| matches!(p.rsplit('/').next(), Some("spot" | "spot.exe"))),
        sites,
    };
    let service = HostService::new(ctx, env)?;
    let seams = Seams {
        fs: &fs,
        service: &service,
        hooks: &hooks,
    };
    let outcome = up::apply(
        &seams,
        &env.layout,
        (&trust, installed, &offer),
        (payload.as_slice(), policy, env.now),
    );
    let exit = finish(ctx, &outcome)?;
    if exit == Exit::Ok {
        remember(held.as_ref());
    }
    Ok(exit)
}

/// Runs `spot backup run` for every site; `true` when every one finished.
async fn backup_sites(ctx: &mut CommandCtx, sites: &[String]) -> spot_cli::Result<bool> {
    for site in sites {
        let matches = super::backup::command()
            .try_get_matches_from(["backup", "run", "--site", site])
            .map_err(|_| Error("the backup command did not parse".into()))?;
        if super::backup::run(&matches, ctx).await? != Exit::Ok {
            return Ok(false);
        }
    }
    Ok(true)
}

fn finish(ctx: &mut CommandCtx, outcome: &Outcome) -> spot_cli::Result<Exit> {
    let (text, exit) = match outcome {
        Outcome::Applied { version } => (
            format!("updated to {version}; the new version is serving"),
            Exit::Ok,
        ),
        Outcome::Kept { version } => (format!("{version} was kept"), Exit::Ok),
        Outcome::NothingToDo => ("nothing to do".to_owned(), Exit::Ok),
        Outcome::Skipped(reason) => (format!("skipped: {reason}"), Exit::Ok),
        Outcome::StepVia(version) => (format!("install {version} first"), Exit::Refused),
        Outcome::Refused(code) => (
            format!(
                "refused: {}; the installed version is untouched",
                code.as_str()
            ),
            Exit::Refused,
        ),
        Outcome::RolledBack {
            failed_step,
            reason,
        } => (
            format!(
                "failed at {}: {reason}; rolled back, the previous version serves",
                failed_step.as_str()
            ),
            Exit::Failed,
        ),
        Outcome::NeedsAttention { detail } => (format!("needs attention: {detail}"), Exit::Failed),
        Outcome::Interrupted { step } => (
            format!(
                "interrupted at {}; run `spot update apply --apply` to resume",
                step.as_str()
            ),
            Exit::Failed,
        ),
    };
    writeln!(
        if exit == Exit::Ok {
            ctx.out()
        } else {
            ctx.err()
        },
        "{text}"
    )?;
    Ok(exit)
}

/// The data hooks: the backup has already run (a hook that finds it did not refuses), the
/// migration step checks the new version's plan, and nothing here changes data, so a rollback has
/// nothing to restore.
struct Hooks {
    backed_up: AtomicBool,
    host_root: PathBuf,
    staged: PathBuf,
    binary: Option<String>,
    sites: Vec<String>,
}

fn hook(detail: &str) -> UpdaterError {
    UpdaterError::Hook {
        detail: detail.to_owned(),
    }
}

impl DataHooks for Hooks {
    fn backup(&self) -> Result<(), UpdaterError> {
        if self.backed_up.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(hook("no backup was taken before the schema change"))
        }
    }

    /// Runs the staged version's `spot migrate --site S` (plan only) for every site; a plan the
    /// new version refuses, or one it cannot print, stops the update.
    fn migrate(&self, _from_epoch: u64, _to_epoch: u64) -> Result<(), UpdaterError> {
        let binary = self
            .binary
            .as_deref()
            .ok_or_else(|| hook("the update lists no `spot` binary to check"))?;
        for site in &self.sites {
            let status = std::process::Command::new(self.staged.join(binary))
                .arg("--root")
                .arg(&self.host_root)
                .args(["migrate", "--site", site])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map_err(|_| hook("the new version's migration plan could not be run"))?;
            if !status.success() {
                return Err(hook(
                    "the new version's migration plan was refused (destructive change without an approved plan)",
                ));
            }
        }
        Ok(())
    }

    fn restore(&self) -> Result<(), UpdaterError> {
        Ok(())
    }
}

// --- the host service ----------------------------------------------------------------------------

/// Stops and starts the host through the platform's service manager, and polls readiness.
struct HostService {
    unit: String,
    ready: std::net::SocketAddr,
    clock: Arc<dyn Clock>,
}

impl HostService {
    fn new(ctx: &mut CommandCtx, env: &Env) -> spot_cli::Result<Self> {
        let bind = ctx
            .common()?
            .server
            .get("bind")
            .and_then(|b| b.as_array().and_then(|a| a.first()).or(Some(b)))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let text = env
            .settings
            .ready_addr
            .clone()
            .or(bind)
            .unwrap_or_else(|| "127.0.0.1:8080".into());
        let mut ready: std::net::SocketAddr = text
            .parse()
            .map_err(|_| Error("services.updater.ready_addr is not host:port".into()))?;
        if ready.ip().is_unspecified() {
            ready.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
        }
        Ok(Self {
            unit: env.settings.unit.clone(),
            ready,
            clock: ctx.clock().clone(),
        })
    }

    fn control(&self, verb: &str) -> Result<(), UpdaterError> {
        let fault = || UpdaterError::Service {
            detail: format!("{verb} of the service failed"),
        };
        #[cfg(windows)]
        {
            let _ = &self.unit;
            match verb {
                "stop" => spot_winsvc::service::install::stop(),
                _ => spot_winsvc::service::install::start(),
            }
            .map_err(|_| fault())
        }
        #[cfg(not(windows))]
        {
            let status = std::process::Command::new("systemctl")
                .args([verb, &self.unit])
                .status()
                .map_err(|_| fault())?;
            if status.success() {
                Ok(())
            } else {
                Err(fault())
            }
        }
    }

    /// One plain `GET /api/health/ready` over loopback: healthy on `200`.
    fn probe(&self) -> bool {
        use std::io::{Read, Write};
        let Ok(mut stream) =
            std::net::TcpStream::connect_timeout(&self.ready, Duration::from_secs(2))
        else {
            return false;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let request = "GET /api/health/ready HTTP/1.0\r\nHost: localhost\r\n\r\n";
        let mut head = [0_u8; 12];
        stream.write_all(request.as_bytes()).is_ok()
            && stream.read_exact(&mut head).is_ok()
            && head[8..12] == *b" 200"
    }
}

impl ServiceControl for HostService {
    fn stop(&self) -> Result<(), UpdaterError> {
        self.control("stop")
    }

    fn start(&self, _version: &Version) -> Result<(), UpdaterError> {
        // The unit's start command follows the `current` pointer the engine has just written.
        self.control("start")
    }

    fn wait_ready(&self, deadline: i64) -> Result<(), UpdaterError> {
        while self.clock.now_utc().unix_timestamp() < deadline {
            if self.probe() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(UpdaterError::Health {
            detail: "the new version did not become ready in time".to_owned(),
        })
    }

    fn is_healthy(&self) -> Result<bool, UpdaterError> {
        Ok(self.probe())
    }
}

// --- rollback and status ---------------------------------------------------------------------------

async fn rollback(ctx: &mut CommandCtx, env: &Env, apply: bool) -> spot_cli::Result<Exit> {
    let fs = StdFileSystem::new();
    let layout = &env.layout;
    let (current, previous) = match (
        up::read_pointer(&fs, &layout.current_path()),
        up::read_pointer(&fs, &layout.previous_path()),
    ) {
        (Ok(Some(current)), Ok(Some(previous))) => (current, previous),
        _ => {
            writeln!(
                ctx.err(),
                "refused: there is no previous version to go back to"
            )?;
            return Ok(Exit::Refused);
        }
    };
    match Journal::load(&fs, &layout.journal_path()) {
        Ok(Some(journal))
            if journal.stage != spot_updater::Stage::Committed
                && journal.stage != spot_updater::Stage::RolledBack =>
        {
            writeln!(
                ctx.err(),
                "refused: an update is unfinished; run `spot update apply --apply` to resume it"
            )?;
            return Ok(Exit::Refused);
        }
        Err(error) => return report(ctx, &error),
        _ => {}
    }
    if !fs.exists(&layout.version_dir(&previous)).unwrap_or(false) {
        writeln!(
            ctx.err(),
            "refused: version {previous} is no longer on disk"
        )?;
        return Ok(Exit::Refused);
    }
    writeln!(
        ctx.out(),
        "plan: switch from {current} back to {previous}. The data is NOT restored: if {current} changed the schema, restore a backup instead."
    )?;
    if !apply {
        writeln!(ctx.out(), "Nothing was changed; add --apply to run it.")?;
        return Ok(Exit::Ok);
    }
    let service = HostService::new(ctx, env)?;
    let switch = |to: &Version, back: &Version| -> Result<(), UpdaterError> {
        service.stop()?;
        fs.durable_write(&layout.current_path(), to.to_string().as_bytes())?;
        fs.durable_write(&layout.previous_path(), back.to_string().as_bytes())?;
        service.start(to)
    };
    let deadline = env.now.saturating_add(env.settings.health_timeout_secs);
    let outcome = switch(&previous, &current).and_then(|()| service.wait_ready(deadline));
    if let Err(error) = outcome {
        // Put the pointers back and start what was running before.
        let _ = switch(&current, &previous);
        writeln!(
            ctx.err(),
            "{}: {error}; the update was kept",
            error.code().as_str()
        )?;
        return Ok(Exit::Failed);
    }
    writeln!(ctx.out(), "rolled back to {previous}")?;
    Ok(Exit::Ok)
}

fn status(ctx: &mut CommandCtx, env: &Env) -> spot_cli::Result<Exit> {
    let fs = StdFileSystem::new();
    let pointer = |path: PathBuf| match up::read_pointer(&fs, &path) {
        Ok(Some(v)) => v.to_string(),
        Ok(None) => "none".to_owned(),
        Err(_) => "unreadable".to_owned(),
    };
    writeln!(
        ctx.out(),
        "installed: {}",
        pointer(env.layout.current_path())
    )?;
    writeln!(
        ctx.out(),
        "previous: {}",
        pointer(env.layout.previous_path())
    )?;
    let journal = match Journal::load(&fs, &env.layout.journal_path()) {
        Ok(Some(j)) => format!(
            "{} (target {}, step {})",
            j.stage.as_str(),
            j.target,
            j.step.as_str()
        ),
        Ok(None) => "none".to_owned(),
        Err(error) => error.code().as_str().to_owned(),
    };
    writeln!(ctx.out(), "journal: {journal}")?;
    writeln!(
        ctx.out(),
        "policy: channel {}, mode {}",
        env.channel.as_str(),
        env.settings.mode
    )?;
    match up::read_status(&fs, &env.root) {
        Some(s) => writeln!(
            ctx.out(),
            "last check: {} at {} on {}: {}",
            s.state,
            s.checked_at,
            s.channel,
            s.detail
        )?,
        None => writeln!(ctx.out(), "last check: never")?,
    }
    Ok(Exit::Ok)
}
