//! `spot secrets-kms`: the operator's side of the customer-managed-key provider
//! (`[services.secrets] provider = "kms"`, WP-SEC-03.T14). Guide:
//! `docs/running-spot/secrets-kms.md`.
//!
//! - `init`: makes the site's derivation seed file, wrapped under the KMS key (the one write a
//!   first start needs; the server never makes it);
//! - `status`: what the running server last reported: key name, latest and minimum-decryption
//!   versions, cache size and age, health, the last KMS error code, refusals by reason, and
//!   whether the seed file is there;
//! - `flush-cache`: asks the running server to drop its cached data keys, so the next unseal
//!   asks the KMS (the server acts at its next tick, within seconds);
//! - `rewrap-seed`: moves the seed's wrapping to the KMS key's latest version (a step of
//!   rotation, before an old version is retired in the KMS).
//!
//! It is registered as `secrets-kms` because a registered command cannot nest under the `secrets`
//! command without editing `commands/secrets.rs` (WP-SEC-03.T9 owns that file). The sweep that
//! moves stored values is the existing `spot secrets rotate-kek`.
//!
//! Every command takes `--site`, prints its plan first and changes something only with `--apply`
//! (`status` changes nothing). Exit codes: 0 done, 1 usage, 2 refused (nothing changed), 3
//! failed. Nothing here prints a token, a key, the KMS address or a stored value.
use super::{CommandCtx, Exit};
use spot_cli::Result;
use spot_cli::services::secrets::kms::{self, KmsSettings, KmsSite, StatusFile};
use spot_egress::{EgressConnector, EgressPolicy, Guard, HttpSender, Purpose, Sender};
use spot_secrets_kms::OsEntropy;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const NAME: &str = "secrets-kms";

/// The longest one KMS call of this command may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

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
        .about("Seed file, status and key cache of the customer-managed-key (KMS) provider")
        .subcommand_required(true)
        .subcommand(
            clap::Command::new("init")
                .about("Create the site's seed file under the KMS key (back it up: a lost seed loses every blind index)")
                .arg(site())
                .arg(apply()),
        )
        .subcommand(
            clap::Command::new("status")
                .about("Key versions, key cache, health and refusals as the running server last reported them")
                .arg(site()),
        )
        .subcommand(
            clap::Command::new("flush-cache")
                .about("Ask the running server to drop its cached data keys now")
                .arg(site())
                .arg(apply()),
        )
        .subcommand(
            clap::Command::new("rewrap-seed")
                .about("Move the seed file's wrapping to the KMS key's latest version")
                .arg(site())
                .arg(apply()),
        )
}

pub async fn run(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot secrets-kms <init|status|flush-cache|rewrap-seed> --site SITE"
        )?;
        return Ok(Exit::Usage);
    };
    let site = sub
        .get_one::<String>("site")
        .map(String::as_str)
        .unwrap_or_default()
        .to_owned();
    let apply = sub
        .try_get_one::<bool>("apply")
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false);
    let loaded = spot_cli::config::load(ctx.root(), false)?;
    if !loaded.sites.iter().any(|s| s.site == site) {
        writeln!(ctx.err(), "there is no site `{site}`")?;
        return Ok(Exit::Usage);
    }
    let (settings, files) = match settings_and_files(ctx.root(), &loaded, &site) {
        Ok(found) => found,
        Err(e) => {
            writeln!(ctx.err(), "{e}")?;
            return Ok(Exit::Refused);
        }
    };
    match verb {
        "init" => init(ctx, &settings, &files, apply).await,
        "status" => status(ctx, &files),
        "flush-cache" => flush_cache(ctx, &files, apply),
        "rewrap-seed" => rewrap_seed(ctx, &settings, &files, apply).await,
        _ => {
            writeln!(ctx.err(), "unknown secrets-kms command `{verb}`")?;
            Ok(Exit::Usage)
        }
    }
}

/// The `[services.secrets.kms]` settings and the site's files, or the refusal that says what is
/// wrong with the configuration.
fn settings_and_files(
    root: &Path,
    loaded: &spot_cli::config::Loaded,
    site: &str,
) -> Result<(KmsSettings, KmsSite)> {
    let table = loaded.common.services.services_extra.get("secrets");
    let Some(table) = table.and_then(toml::Value::as_table) else {
        return Err(spot_cli::Error(
            "secrets_kms_host.settings_invalid: common.toml has no [services.secrets] table".into(),
        ));
    };
    if table.get("provider").and_then(toml::Value::as_str) != Some("kms") {
        return Err(spot_cli::Error(
            "secrets_kms_host.settings_invalid: services.secrets.provider is not `kms`".into(),
        ));
    }
    let Some(kms_table) = table.get("kms") else {
        return Err(spot_cli::Error(
            "secrets_kms_host.settings_invalid: [services.secrets.kms] is missing".into(),
        ));
    };
    let settings = KmsSettings::from_value(kms_table)?;
    let files = settings.for_site(root, site, loaded.sites.len())?;
    Ok((settings, files))
}

/// A guard that allows this site to reach the configured KMS and nothing else. The command is
/// the operator naming the KMS they configured; the running server uses the host's own policy.
fn guard_for(settings: &KmsSettings, site: &str, ctx: &CommandCtx) -> Result<Arc<Guard>> {
    let bad = |why: &str| {
        spot_cli::Error(format!(
            "secrets_kms_host.settings_invalid: services.secrets.kms.address {why}"
        ))
    };
    let url = url::Url::parse(&settings.address).map_err(|_| bad("is not a URL"))?;
    let host = url.host_str().ok_or_else(|| bad("has no host"))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let private = host
        .trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
        .map(|ip| format!("allow_private = [\"{ip}\"]\n"))
        .unwrap_or_default();
    let text = format!(
        "[tenants.\"{site}\"]\n[[tenants.\"{site}\".allow]]\nscheme = \"https\"\nhost = \"{host}\"\nports = [{port}]\npurpose = \"connector\"\n{private}"
    );
    let policy =
        EgressPolicy::from_toml(&text).map_err(|_| bad("cannot be turned into a policy"))?;
    Ok(Arc::new(Guard::with_defaults(
        policy,
        Arc::clone(ctx.clock()),
    )))
}

async fn init(
    ctx: &mut CommandCtx,
    settings: &KmsSettings,
    files: &KmsSite,
    apply: bool,
) -> Result<Exit> {
    writeln!(
        ctx.out(),
        "plan: create the seed file {} for site `{}` under KMS key `{}`",
        files.seed_file.display(),
        files.site,
        settings.key_name
    )?;
    if files.seed_file.exists() {
        writeln!(
            ctx.err(),
            "{}: the seed file already exists; init never overwrites it",
            kms::codes::SEED_EXISTS
        )?;
        return Ok(Exit::Refused);
    }
    if !apply {
        writeln!(ctx.out(), "nothing changed; add --apply to create it")?;
        return Ok(Exit::Ok);
    }
    let guard = guard_for(settings, &files.site, ctx)?;
    let sender: Arc<dyn Sender> = Arc::new(HttpSender::new(
        EgressConnector::new(Arc::clone(&guard), files.site.clone(), Purpose::Connector),
        CALL_TIMEOUT,
    ));
    let done = kms::init_seed(
        settings,
        files,
        guard,
        sender,
        Arc::clone(ctx.clock()),
        Arc::new(OsEntropy),
    )
    .await;
    if let Err(e) = done {
        writeln!(ctx.err(), "{e}")?;
        return Ok(Exit::Failed);
    }
    writeln!(
        ctx.out(),
        "created {}. Back it up now, apart from the data: a lost seed file loses every blind index (it is useless without the KMS key).",
        files.seed_file.display()
    )?;
    Ok(Exit::Ok)
}

fn status(ctx: &mut CommandCtx, files: &KmsSite) -> Result<Exit> {
    let now = ctx.clock().now_utc().unix_timestamp();
    let seed_present = files.seed_file.is_file();
    let reported = std::fs::read(kms::status_path(&files.seed_file))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<StatusFile>(&bytes).ok());
    let out = ctx.out();
    writeln!(out, "site: {}", files.site)?;
    writeln!(out, "provider: kms")?;
    writeln!(
        out,
        "seed file: {} ({})",
        files.seed_file.display(),
        if seed_present { "present" } else { "MISSING" }
    )?;
    let Some(s) = reported else {
        writeln!(
            out,
            "no status reported: the server writes it while it runs with provider kms (not started yet, stopped, or another seed file)"
        )?;
        return Ok(Exit::Ok);
    };
    writeln!(out, "key name: {}", s.key_name)?;
    writeln!(out, "latest version: {}", s.latest_version)?;
    writeln!(
        out,
        "minimum decryption version: {}",
        s.min_decryption_version
    )?;
    writeln!(
        out,
        "key enabled: {}",
        if s.key_enabled { "yes" } else { "NO" }
    )?;
    writeln!(
        out,
        "key cache: {} data keys, emptied {} seconds ago",
        s.cache_entries, s.cache_age_seconds
    )?;
    write!(out, "health: {}", s.health)?;
    if let Some(left) = s.seconds_left {
        write!(out, " ({left} seconds before refusals begin)")?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "last KMS error: {}",
        s.last_error.as_deref().unwrap_or("none")
    )?;
    for (reason, count) in &s.refused {
        writeln!(out, "refused ({reason}): {count}")?;
    }
    writeln!(
        out,
        "reported {} seconds ago",
        now.saturating_sub(s.updated_unix).max(0)
    )?;
    Ok(Exit::Ok)
}

fn flush_cache(ctx: &mut CommandCtx, files: &KmsSite, apply: bool) -> Result<Exit> {
    let marker = kms::flush_marker_path(&files.seed_file);
    writeln!(
        ctx.out(),
        "plan: ask the running server of site `{}` to drop its cached data keys (marker {})",
        files.site,
        marker.display()
    )?;
    if !apply {
        writeln!(ctx.out(), "nothing changed; add --apply to ask")?;
        return Ok(Exit::Ok);
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if options.open(&marker).is_err() {
        writeln!(
            ctx.err(),
            "{}: the flush marker {} cannot be written",
            kms::codes::FILE_UNUSABLE,
            marker.display()
        )?;
        return Ok(Exit::Failed);
    }
    writeln!(
        ctx.out(),
        "asked. The server drops its cache at its next tick (a few seconds) and removes the marker; the next unseal asks the KMS."
    )?;
    Ok(Exit::Ok)
}

async fn rewrap_seed(
    ctx: &mut CommandCtx,
    settings: &KmsSettings,
    files: &KmsSite,
    apply: bool,
) -> Result<Exit> {
    writeln!(
        ctx.out(),
        "plan: move the wrapping of the seed file {} to the latest version of KMS key `{}`",
        files.seed_file.display(),
        settings.key_name
    )?;
    if !apply {
        writeln!(ctx.out(), "nothing changed; add --apply to move it")?;
        return Ok(Exit::Ok);
    }
    let guard = guard_for(settings, &files.site, ctx)?;
    let built = kms::build_kms_provider(
        settings,
        files,
        guard,
        Arc::clone(ctx.clock()),
        Arc::new(OsEntropy),
    )
    .await;
    let host = match built {
        Ok(host) => host,
        Err(e) => {
            writeln!(ctx.err(), "{e}")?;
            return Ok(Exit::Refused);
        }
    };
    match host.rewrap_seed().await {
        Ok(version) => {
            writeln!(
                ctx.out(),
                "the seed file is now wrapped under key version {version}"
            )?;
            Ok(Exit::Ok)
        }
        Err(e) => {
            writeln!(ctx.err(), "{e}")?;
            Ok(Exit::Failed)
        }
    }
}
