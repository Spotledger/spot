//! `spot secrets`: the operator's side of the secrets service (WP-SEC-03.T9). Guide:
//! `docs/running-spot/secrets.md`.
//!
//! - `init`: creates the site's key file (a random version 1, mode 0600);
//! - `status`: the key versions the file holds and how many stored values are under each;
//! - `rotate-kek`: adds a key version, makes the running server adopt it (a reload), then moves
//!   every sealed value onto it in batches while the site serves; the old version is kept;
//! - `retire-kek`: destroys the key material of versions no stored value uses any more.
//!
//! Every command takes `--site`, prints its plan first and changes something only with
//! `--apply`. Exit codes: 0 done, 1 usage, 2 refused (nothing changed), 3 failed. Nothing here
//! prints key material or a stored value: only versions, file names and counts.
use super::{CommandCtx, Exit};
use spot_cli::Result;
use spot_cli::services::secrets::{self as svc, OperatorAudit, RotationLock, codes};
use std::sync::Arc;

/// Printed when `retire-kek --apply` lacks `--confirm-all-servers-reloaded`.
const RETIRE_UNCONFIRMED: &str = "secrets_host.retire_unconfirmed";

pub const NAME: &str = "secrets";

pub fn command() -> clap::Command {
    let site = || {
        clap::Arg::new("site")
            .long("site")
            .value_name("SITE")
            .required(true)
            .help("The site to act on")
    };
    let apply = || {
        clap::Arg::new("apply")
            .long("apply")
            .action(clap::ArgAction::SetTrue)
            .help("Change things; without it only the plan is printed")
    };
    clap::Command::new(NAME)
        .about("Key file, key versions and rotation of the secrets service")
        .subcommand_required(true)
        .subcommand(
            clap::Command::new("init")
                .about("Create the site's key file (back it up: a lost key file loses every sealed value)")
                .arg(site())
                .arg(apply()),
        )
        .subcommand(
            clap::Command::new("status")
                .about("Key versions in use and the stored values still under an old one")
                .arg(site()),
        )
        .subcommand(
            clap::Command::new("rotate-kek")
                .about("Add a key version and move every sealed value onto it, without downtime")
                .arg(site())
                .arg(apply())
                .arg(
                    clap::Arg::new("resume")
                        .long("resume")
                        .action(clap::ArgAction::SetTrue)
                        .help("Finish an unfinished rotation: move the values still under an old version"),
                )
                .arg(
                    clap::Arg::new("settle")
                        .long("settle-seconds")
                        .value_name("SECONDS")
                        .value_parser(clap::value_parser!(u64).range(0..=600))
                        .default_value("3")
                        .help("Seconds to let running servers adopt the new version before values are moved (raise it for a cluster)"),
                )
                .arg(
                    clap::Arg::new("batch")
                        .long("batch")
                        .value_name("N")
                        .value_parser(clap::value_parser!(u32).range(1..=500))
                        .default_value("100")
                        .help("Records per transaction"),
                ),
        )
        .subcommand(
            clap::Command::new("retire-kek")
                .about("Destroy the key material of versions no stored value uses (irreversible)")
                .arg(site())
                .arg(apply())
                .arg(
                    clap::Arg::new("confirm_reloaded")
                        .long("confirm-all-servers-reloaded")
                        .action(clap::ArgAction::SetTrue)
                        .help("Required with --apply")
                        .long_help("I have checked that every server of this site has reloaded onto the current key version"),
                ),
        )
}

pub async fn run(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(ctx.err(), "usage: spot secrets <init|status|rotate-kek|retire-kek> --site SITE")?;
        return Ok(Exit::Usage);
    };
    let site = sub
        .get_one::<String>("site")
        .map(String::as_str)
        .unwrap_or_default()
        .to_owned();
    let apply = sub.try_get_one::<bool>("apply").ok().flatten().copied().unwrap_or(false);
    let loaded = spot_cli::config::load(ctx.root(), false)?;
    let Some(site_cfg) = loaded.sites.iter().find(|s| s.site == site) else {
        writeln!(ctx.err(), "there is no site `{site}`")?;
        return Ok(Exit::Usage);
    };
    let path = match svc::key_path(ctx.root(), &loaded, site_cfg) {
        Ok(Some(path)) => path,
        Ok(None) => {
            writeln!(
                ctx.err(),
                "{}: common.toml has no [services.secrets] table; add one with `kek_file` first",
                codes::KEK_UNSET
            )?;
            return Ok(Exit::Refused);
        }
        Err(e) => {
            writeln!(ctx.err(), "{e}")?;
            return Ok(Exit::Refused);
        }
    };
    match verb {
        "init" => init(ctx, &loaded, site_cfg, &path, apply).await,
        "status" => status(ctx, &loaded, site_cfg).await,
        "rotate-kek" => {
            let resume = sub.get_flag("resume");
            let batch = sub.get_one::<u32>("batch").copied().unwrap_or(100);
            let settle = sub.get_one::<u64>("settle").copied().unwrap_or(3);
            rotate(ctx, &loaded, site_cfg, Plan { apply, resume, batch, settle }).await
        }
        "retire-kek" => retire(ctx, &loaded, site_cfg, apply, sub.get_flag("confirm_reloaded")).await,
        _ => {
            writeln!(ctx.err(), "unknown secrets command `{verb}`")?;
            Ok(Exit::Usage)
        }
    }
}

async fn init(
    ctx: &mut CommandCtx,
    loaded: &spot_cli::config::Loaded,
    site: &spot_cli::config::Site,
    path: &std::path::Path,
    apply: bool,
) -> Result<Exit> {
    let _ = loaded;
    writeln!(ctx.out(), "plan: create the key file {} for site `{}`", path.display(), site.site)?;
    if path.exists() {
        writeln!(ctx.err(), "{}: the key file already exists; init never overwrites a key file", codes::KEK_FILE_UNUSABLE)?;
        return Ok(Exit::Refused);
    }
    if !apply {
        writeln!(ctx.out(), "nothing changed; add --apply to create it")?;
        return Ok(Exit::Ok);
    }
    if let Err(e) = svc::init_key_file(path, &site.site).await {
        writeln!(ctx.err(), "{e}")?;
        return Ok(Exit::Failed);
    }
    writeln!(
        ctx.out(),
        "created {}. Back it up now, apart from the data: a lost key file loses every sealed value.",
        path.display()
    )?;
    Ok(Exit::Ok)
}

/// What the commands that read data need about the site.
struct Site {
    meta: Arc<spot_meta::EffectiveMeta>,
    catalog: Arc<spot_storage_contract::Catalog>,
}

fn site_data(
    ctx: &CommandCtx,
    loaded: &spot_cli::config::Loaded,
    site: &spot_cli::config::Site,
) -> Result<Site> {
    let meta = spot_cli::services::metadata::effective(ctx.root(), loaded, site)?;
    let catalog = Arc::new(spot_cli::schema::catalog(&meta)?);
    Ok(Site { meta, catalog })
}

async fn status(
    ctx: &mut CommandCtx,
    loaded: &spot_cli::config::Loaded,
    site: &spot_cli::config::Site,
) -> Result<Exit> {
    let Some(host) = svc::open(ctx.root(), loaded, site).await? else {
        return Ok(Exit::Refused);
    };
    let data = site_data(ctx, loaded, site)?;
    let backend = ctx.backend(&site.site).await?;
    let found = svc::scan(&backend, &data.catalog, &data.meta, None).await?;
    let versions = host.versions().await;
    let list = |v: &[u32]| {
        if v.is_empty() {
            "none".to_owned()
        } else {
            v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
        }
    };
    let out = ctx.out();
    writeln!(out, "site: {}", site.site)?;
    writeln!(out, "provider: local")?;
    writeln!(out, "key file: {}", host.path().display())?;
    writeln!(out, "key id: {}", versions.id)?;
    writeln!(out, "current version: {}", versions.current)?;
    writeln!(out, "versions with key material: {}", list(&versions.held))?;
    writeln!(out, "retired versions: {}", list(&versions.retired))?;
    for field in &found {
        let counts = field
            .by_key
            .iter()
            .map(|((_, version), n)| format!("version {version}: {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            out,
            "{}.{}: {}{}",
            field.doctype,
            field.field,
            if counts.is_empty() { "no sealed values" } else { &counts },
            if field.malformed > 0 {
                format!(" ({} not in the sealed format)", field.malformed)
            } else {
                String::new()
            }
        )?;
    }
    writeln!(
        out,
        "stored values under an old version: {}",
        svc::rows_on_old_versions(&found, versions.current)
    )?;
    Ok(Exit::Ok)
}

/// What `rotate-kek` was asked to do.
struct Plan {
    apply: bool,
    resume: bool,
    batch: u32,
    settle: u64,
}

async fn rotate(
    ctx: &mut CommandCtx,
    loaded: &spot_cli::config::Loaded,
    site: &spot_cli::config::Site,
    plan: Plan,
) -> Result<Exit> {
    let Plan {
        apply,
        resume,
        batch,
        settle,
    } = plan;
    // An applying run is audited, so the audit chain is opened first and a run that cannot be
    // audited changes nothing.
    let audit = if apply {
        match OperatorAudit::open(loaded, site, ctx.clock().clone()).await {
            Ok(audit) => Some(audit),
            Err(e) => {
                writeln!(ctx.err(), "{e}")?;
                return Ok(Exit::Refused);
            }
        }
    } else {
        None
    };
    let refused = |ctx: &mut CommandCtx, audit: &Option<OperatorAudit>, text: String, reason: &str| {
        if let Some(audit) = audit {
            audit.always("secrets_kek_rotation_refused", 0, Some(reason));
        }
        writeln!(ctx.err(), "{text}")
    };
    let Some(host) = svc::open(ctx.root(), loaded, site).await? else {
        return Ok(Exit::Refused);
    };
    let data = site_data(ctx, loaded, site)?;
    let backend = ctx.backend(&site.site).await?;
    let before = svc::scan(&backend, &data.catalog, &data.meta, None).await?;
    let current = host.current_version();
    let old = svc::rows_on_old_versions(&before, current);
    if !resume && old > 0 {
        refused(
            ctx,
            &audit,
            format!(
                "{}: a rotation is unfinished ({old} stored value(s) are under an older key version); run `spot secrets rotate-kek --site {} --resume --apply` to finish it",
                codes::ROTATION_RUNNING,
                site.site
            ),
            "unfinished",
        )?;
        return Ok(Exit::Refused);
    }
    if resume {
        writeln!(ctx.out(), "plan: move {old} stored value(s) onto key version {current}")?;
    } else {
        writeln!(
            ctx.out(),
            "plan: add key version {}, reload the running server so it seals with it, then move every sealed value onto it ({batch} records per transaction); version {current} is kept",
            current.saturating_add(1)
        )?;
    }
    let Some(audit) = audit else {
        writeln!(ctx.out(), "nothing changed; add --apply to run it")?;
        return Ok(Exit::Ok);
    };
    let _lock = match RotationLock::take(host.path()) {
        Ok(lock) => lock,
        Err(e) => {
            audit.always("secrets_kek_rotation_refused", 0, Some("locked"));
            writeln!(ctx.err(), "{e}")?;
            return Ok(Exit::Refused);
        }
    };
    let target = if resume { current } else { current.saturating_add(1) };
    if let Err(e) = audit.required("secrets_kek_rotation_started", u64::from(target), None) {
        writeln!(ctx.err(), "{e}")?;
        return Ok(Exit::Refused);
    }
    if !resume {
        let added = match svc::add_version(&host).await {
            Ok(version) => version,
            Err(e) => {
                audit.always("secrets_kek_rotation_failed", u64::from(target), Some("add_version"));
                writeln!(ctx.err(), "{e}")?;
                return Ok(Exit::Failed);
            }
        };
        writeln!(ctx.out(), "added key version {added}")?;
    }
    // The running server adopts the key file by rebuilding the site; until it has, it still
    // seals under the previous version, which is still held, so no write fails.
    let _ = spot_cli::request_reload(ctx.root(), &site.site);
    if !resume && settle > 0 {
        writeln!(ctx.out(), "waiting {settle}s for running servers to adopt it")?;
        tokio::time::sleep(std::time::Duration::from_secs(settle)).await;
    }
    let engine = svc::open_engine(ctx.root(), loaded, site, &host, ctx.clock().clone()).await?;
    let mut failed = 0u64;
    let mut moved = 0u64;
    for pass in 1..=8u32 {
        let mut lines = Vec::new();
        let totals = svc::sweep_once(
            &engine,
            &data.meta,
            &site.site,
            ctx.clock().as_ref(),
            batch,
            &mut |doctype, totals| {
                lines.push(format!(
                    "  pass {pass}: {doctype}: {} examined, {} rewrapped, {} reindexed",
                    totals.examined, totals.rewrapped, totals.reindexed
                ));
            },
        )
        .await;
        for line in lines {
            writeln!(ctx.out(), "{line}")?;
        }
        let totals = match totals {
            Ok(totals) => totals,
            Err(e) => {
                audit.always("secrets_kek_rotation_failed", moved, Some("rewrap_refused"));
                writeln!(ctx.err(), "{e}")?;
                return Ok(Exit::Failed);
            }
        };
        failed += totals.failed;
        moved += totals.rewrapped;
        if totals.rewrapped + totals.reindexed == 0 {
            break;
        }
    }
    let after = svc::scan(&backend, &data.catalog, &data.meta, None).await?;
    let left = svc::rows_on_old_versions(&after, host.current_version());
    if failed > 0 || left > 0 {
        audit.always("secrets_kek_rotation_failed", left, Some("incomplete"));
        writeln!(
            ctx.err(),
            "{}: {failed} value(s) could not be moved and {left} stored value(s) are still under an older key version; the old version is kept. Run `spot secrets status`, then `spot secrets rotate-kek --site {} --resume --apply`",
            codes::REWRAP_FAILED,
            site.site
        )?;
        return Ok(Exit::Failed);
    }
    if let Err(e) = audit.required("secrets_kek_rotation_finished", moved, None) {
        writeln!(ctx.err(), "{e}")?;
        return Ok(Exit::Failed);
    }
    writeln!(
        ctx.out(),
        "done: {moved} sealed value(s) moved; no stored value is under an older key version. Old versions are kept; `spot secrets retire-kek` destroys them once every server has reloaded."
    )?;
    Ok(Exit::Ok)
}

async fn retire(
    ctx: &mut CommandCtx,
    loaded: &spot_cli::config::Loaded,
    site: &spot_cli::config::Site,
    apply: bool,
    confirmed: bool,
) -> Result<Exit> {
    if apply && !confirmed {
        // Refused before the lock, the key file or the database is touched. The count-and-reason
        // record needs no lock, so it is written when the audit chain is reachable; the refusal
        // stands either way.
        if let Ok(audit) = OperatorAudit::open(loaded, site, ctx.clock().clone()).await {
            audit.always("secrets_kek_retire_refused", 0, Some("unconfirmed"));
        }
        writeln!(
            ctx.err(),
            "{RETIRE_UNCONFIRMED}: retiring destroys key material for good; check that every server of the site has reloaded onto the current key version, then add --confirm-all-servers-reloaded"
        )?;
        return Ok(Exit::Refused);
    }
    let audit = if apply {
        match OperatorAudit::open(loaded, site, ctx.clock().clone()).await {
            Ok(audit) => Some(audit),
            Err(e) => {
                writeln!(ctx.err(), "{e}")?;
                return Ok(Exit::Refused);
            }
        }
    } else {
        None
    };
    let Some(host) = svc::open(ctx.root(), loaded, site).await? else {
        return Ok(Exit::Refused);
    };
    let data = site_data(ctx, loaded, site)?;
    let backend = ctx.backend(&site.site).await?;
    let versions = host.versions().await;
    let found = svc::scan(&backend, &data.catalog, &data.meta, None).await?;
    let old = svc::rows_on_old_versions(&found, versions.current);
    let victims: Vec<u32> = versions
        .held
        .iter()
        .copied()
        .filter(|v| *v != versions.current)
        .collect();
    writeln!(
        ctx.out(),
        "plan: destroy the key material of version(s) {:?}; {old} stored value(s) are under a version other than {}",
        victims, versions.current
    )?;
    if victims.is_empty() {
        writeln!(ctx.out(), "nothing to retire")?;
        return Ok(Exit::Ok);
    }
    if old > 0 {
        if let Some(audit) = &audit {
            audit.always("secrets_kek_retire_refused", old, Some("in_use"));
        }
        writeln!(
            ctx.err(),
            "{}: {old} stored value(s) are still under an older key version; finish the rotation first",
            codes::RETIRE_REFUSED
        )?;
        return Ok(Exit::Refused);
    }
    let Some(audit) = audit else {
        writeln!(
            ctx.out(),
            "nothing changed; add --apply. Retiring is irreversible, and every server of the site must have reloaded onto the current version first; --apply will also require --confirm-all-servers-reloaded"
        )?;
        return Ok(Exit::Ok);
    };
    let _lock = match RotationLock::take(host.path()) {
        Ok(lock) => lock,
        Err(e) => {
            audit.always("secrets_kek_retire_refused", 0, Some("locked"));
            writeln!(ctx.err(), "{e}")?;
            return Ok(Exit::Refused);
        }
    };
    if let Err(e) = audit.required("secrets_kek_retire_started", victims.len() as u64, None) {
        writeln!(ctx.err(), "{e}")?;
        return Ok(Exit::Refused);
    }
    for version in &victims {
        if let Err(e) = svc::retire_version(&host, *version, 0).await {
            audit.always("secrets_kek_retire_failed", u64::from(*version), Some("provider"));
            writeln!(ctx.err(), "{e}")?;
            return Ok(Exit::Failed);
        }
        writeln!(ctx.out(), "retired key version {version}")?;
    }
    let _ = spot_cli::request_reload(ctx.root(), &site.site);
    audit.always("secrets_kek_retired", victims.len() as u64, None);
    Ok(Exit::Ok)
}
