//! The operator commands that plan, change and check sites. Each returns the text it prints.
use crate::appdir::{self, PLATFORM};
use crate::bundle_install::{self, Refusal};
use crate::config;
use crate::onetime::Issue;
use crate::{Error, Result, apps, schema, seed, sitefile, users};
use spot_storage_contract::StorageBackend;
use std::fmt::Write;
use std::path::Path;

/// What `install-app` should do beyond planning.
#[derive(Clone, Copy, Debug, Default)]
pub struct Install {
    /// Apply the plan and record the install. Without it the command only plans.
    pub apply: bool,
    /// Load the app's `seed.toml` after installing.
    pub seed: bool,
}

/// Installs `app` on a site: validate, plan, and with `apply` change the schema, record the
/// install in the site file, and optionally seed. Installing twice is a no-op.
///
/// `app` is a catalogue name or app directory (development sites only: the site's
/// `[trust] require_signed` must be `false`), or a packed bundle file, which is verified against
/// the site's trust roots and revocation list before anything is written. A refusal
/// changes nothing and is written to the site audit file with `outcome = "refused"`.
pub async fn install_app(
    root: &Path,
    app: &str,
    site: Option<&str>,
    options: Install,
) -> Result<String> {
    install_app_with(root, app, site, options, &Issue::system()).await
}

/// Like [`install_app`], with the instant the signature check uses given.
pub async fn install_app_with(
    root: &Path,
    app: &str,
    site: Option<&str>,
    options: Install,
    issue: &Issue,
) -> Result<String> {
    let loaded = config::load(root, false)?;
    let site = sitefile::pick(root, site)?;
    match install_checked(root, &loaded, &site, app, options, issue).await {
        Ok(out) => Ok(out),
        Err(Failure::Other(e)) => Err(e),
        Err(Failure::Refused(refusal)) => {
            record_refusal(root, &site.site, &refusal);
            Err(refusal.into_error())
        }
    }
}

/// Why an install did not happen.
enum Failure {
    /// The trust gate refused it; the refusal is audited.
    Refused(Refusal),
    /// Any other error.
    Other(Error),
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Self::Other(e)
    }
}

impl From<Refusal> for Failure {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}

/// Writes the refusal to the site audit file: the code, the bundle digest and the publisher the
/// signature file claims, never a key or a payload. A record that cannot be written is a
/// warning; the refusal stands.
fn record_refusal(root: &Path, site: &str, refusal: &Refusal) {
    let body = serde_json::json!({
        "action": "install-app",
        "outcome": "refused",
        "code": refusal.code,
        "digest": refusal.digest,
        "claimed_publisher": refusal.claimed_publisher,
        "by": "operator",
    });
    if users::audit(root, site, &body.to_string()).is_err() {
        eprintln!("warning: the refusal could not be recorded in the site audit file");
    }
}

async fn install_checked(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
    app: &str,
    options: Install,
    issue: &Issue,
) -> std::result::Result<String, Failure> {
    let (source, mode) = if let Some(path) = bundle_install::bundle_path(root, app) {
        let installed = bundle_install::install(root, loaded, &path, issue)?;
        (installed.app, installed.note)
    } else {
        if let Some(refusal) = bundle_install::directory_policy(&loaded.common.trust) {
            return Err(refusal.into());
        }
        (
            apps::resolve(root, loaded, app)?,
            "install mode: development directory install ([trust] require_signed = false)"
                .to_owned(),
        )
    };
    install_planned(root, loaded, site, source, options, mode)
        .await
        .map_err(Failure::Other)
}

async fn install_planned(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
    source: appdir::AppSource,
    options: Install,
    mode: String,
) -> Result<String> {
    if source.name == PLATFORM {
        return Err(Error("the platform app is installed with the site".into()));
    }
    if site.apps.first().map(String::as_str) != Some(PLATFORM) {
        return Err(Error(format!(
            "site `{}` hosts every app in common.toml and has no install list; create a site \
             with `spot new-site --bare` to install apps one at a time",
            site.site
        )));
    }
    let installed = site.apps.contains(&source.name);
    let mut wanted = site.apps.clone();
    if !installed {
        wanted.push(source.name.clone());
    }
    let meta = apps::effective(root, loaded, site, &wanted)?;
    let backend = schema::backend(loaded, site).await?;
    let plan = schema::plan(&backend, &schema::catalog(&meta)?).await?;
    let mut out = format!("{mode}\n");
    if installed && plan.is_empty() && !options.seed {
        let _ = writeln!(
            out,
            "app `{}` is already installed on site `{}`; nothing to do",
            source.name, site.site
        );
        return Ok(out);
    }
    out.push_str(&schema::render(&plan));
    schema::refusals(&plan).map_err(|e| Error(format!("{out}{e}")))?;
    if !options.apply {
        let _ = writeln!(
            out,
            "dry run: nothing was changed; add --apply to install `{}`",
            source.name
        );
        return Ok(out);
    }
    if !plan.is_empty() {
        let report = schema::apply(&backend, &plan, None).await?;
        let _ = writeln!(out, "applied {} step(s)", report.executed.len());
    }
    if !installed {
        let name = source.name.clone();
        sitefile::update(root, &site.site, |s| {
            if !s.apps.contains(&name) {
                s.apps.push(name);
            }
            Ok(())
        })?;
        let _ = writeln!(
            out,
            "installed app `{}` {} on site `{}`",
            source.name, source.version, site.site
        );
    } else {
        let _ = writeln!(
            out,
            "app `{}` was already installed on site `{}`",
            source.name, site.site
        );
    }
    if options.seed {
        let text = std::fs::read_to_string(source.dir.join("seed.toml"))
            .map_err(|_| Error(format!("app `{}` has no seed.toml", source.name)))?;
        let administrator =
            users::administrator(&backend, std::sync::Arc::new(schema::catalog(&meta)?)).await?;
        let report = seed::run(loaded, site, meta, &source.name, &text, administrator).await?;
        let _ = writeln!(
            out,
            "seeded {} row(s); {} already present",
            report.created, report.present
        );
        if !report.skipped.is_empty() {
            let _ = writeln!(
                out,
                "warning: seed values for unknown field(s) were skipped: {}",
                report.skipped.join(", ")
            );
        }
    }
    Ok(out)
}

/// What `migrate` should do.
#[derive(Clone, Debug, Default)]
pub struct Migrate {
    /// Apply the plan. Without it the command only plans.
    pub apply: bool,
    /// The reviewed plan hash that an applied plan must match.
    pub plan_hash: Option<String>,
}

/// Plans (and with `apply` runs) the schema change from the live database to the installed apps'
/// current metadata.
pub async fn migrate(root: &Path, site: Option<&str>, options: Migrate) -> Result<String> {
    let loaded = config::load(root, false)?;
    let site = sitefile::pick(root, site)?;
    let meta = apps::serving(root, &loaded, &site)?;
    let backend = schema::backend(&loaded, &site).await?;
    // The plan carries the platform's non-document tables too (edge sync's and the mail sent
    // log's), so one command brings a database up to what the site's participants need.
    let plan = schema::plan_with_platform_tables(&backend, &schema::catalog(&meta)?).await?;
    let mut out = schema::render(&plan);
    schema::refusals(&plan).map_err(|e| Error(format!("{out}{e}")))?;
    if !options.apply {
        if !plan.is_empty() {
            out.push_str("dry run: nothing was changed; add --apply (and --plan-hash) to apply\n");
        }
        return Ok(out);
    }
    if plan.is_empty() {
        // Nothing to plan does not mean nothing to do: the ownership registry creates its own
        // tables (their triggers are outside the plan's hook), so a site that has just been
        // given a `[partition]` profile still needs this command to run.
        schema::protect_audit(&backend).await?;
        prepare_partition(&loaded, &site, &backend, &mut out).await?;
        return Ok(out);
    }
    let report = schema::apply(&backend, &plan, options.plan_hash.as_deref()).await?;
    let _ = writeln!(out, "applied {} step(s)", report.executed.len());
    // The audit chain's role, grants and guard triggers (the plan cannot hold them).
    schema::protect_audit(&backend).await?;
    prepare_partition(&loaded, &site, &backend, &mut out).await?;
    Ok(out)
}

/// Creates the ownership registry's tables for a partitioned site, and says so.
///
/// `spot-partition-pg` owns its DDL and runs it under its own advisory lock, idempotently, on a
/// plain thread (the store refuses to be built inside a runtime). It is never called from a
/// request or from a serve start-up: a serve verifies the tables and refuses with "run `spot
/// migrate --apply`" instead.
async fn prepare_partition(
    loaded: &config::Loaded,
    site: &config::Site,
    backend: &spot_postgres::PgBackend,
    out: &mut String,
) -> Result<()> {
    if site.partition.is_none() {
        return Ok(());
    }
    crate::services::partition::prepare(backend, loaded.common.postgres.pool_max_size).await?;
    let _ = writeln!(
        out,
        "partition: ownership tables present ({} tables)",
        crate::services::partition::TABLES.len()
    );
    Ok(())
}

/// The apps of one site or of every site, with the catalogue of apps available to install.
pub fn list_apps(root: &Path, site: Option<&str>) -> Result<String> {
    let loaded = config::load(root, false)?;
    let catalogue = apps::catalogue(root, &loaded)?;
    let mut out = String::new();
    let sites: Vec<_> = loaded
        .sites
        .iter()
        .filter(|s| site.is_none_or(|n| s.site == n))
        .collect();
    if let Some(n) = site
        && sites.is_empty()
    {
        return Err(Error(format!("no site `{n}`")));
    }
    for s in sites {
        if s.apps.is_empty() {
            let _ = writeln!(
                out,
                "{}: hosts every app in common.toml (no install list)",
                s.site
            );
            continue;
        }
        let _ = writeln!(out, "{}:", s.site);
        for app in &s.apps {
            let version = catalogue.iter().find(|a| &a.name == app).map_or_else(
                || {
                    if app == PLATFORM {
                        "embedded".to_owned()
                    } else {
                        "missing from common.toml".to_owned()
                    }
                },
                |a| a.version.clone(),
            );
            let _ = writeln!(out, "  {app} {version}");
        }
    }
    let names: Vec<_> = catalogue
        .iter()
        .filter(|a| a.name != PLATFORM)
        .map(|a| format!("{} {}", a.name, a.version))
        .collect();
    let _ = writeln!(
        out,
        "available: {}",
        if names.is_empty() {
            "none".into()
        } else {
            names.join(", ")
        }
    );
    Ok(out)
}

/// Checks that serving would start and reports every problem at once.
pub async fn check(root: &Path) -> Result<String> {
    let (loaded, mut problems) = config::load_collect(root, false)?;
    match apps::catalogue(root, &loaded) {
        Ok(_) => {}
        Err(e) => problems.push(e.to_string()),
    }
    if loaded.sites.is_empty() {
        problems.push("no sites configured".into());
    }
    let mut checked = 0usize;
    for site in &loaded.sites {
        match apps::serving(root, &loaded, site) {
            Ok(_) => {}
            Err(e) => problems.push(format!("site `{}`: {e}", site.site)),
        }
        let backend = schema::backend(&loaded, site).await.ok();
        let reachable = match &backend {
            Some(backend) => {
                backend.health().await.status == spot_storage_contract::HealthStatus::Healthy
            }
            None => false,
        };
        if reachable {
            checked += 1;
            // Serving refuses when the participants' tables are missing, so `check` says so
            // first, with the command that creates them (WP-PRD-01.T11).
            let pending = match (backend.as_ref(), apps::serving(root, &loaded, site)) {
                (Some(backend), Ok(meta)) => match schema::catalog(&meta) {
                    Ok(catalog) => schema::pending_platform_tables(backend, &catalog).await?,
                    Err(e) => return Err(e),
                },
                _ => Vec::new(),
            };
            if !pending.is_empty() {
                problems.push(format!(
                    "site `{}`: the database is missing the platform tables {}; run \
                     `spot migrate --apply`",
                    site.site,
                    pending.join(", ")
                ));
            }
            // A node profile needs the ownership registry's own tables too, and serving
            // refuses without them.
            if site.partition.is_some()
                && let Some(backend) = backend.as_ref()
            {
                let missing = crate::services::partition::missing_tables(backend).await?;
                if !missing.is_empty() {
                    problems.push(format!(
                        "site `{}`: the database is missing the partition ownership tables {}; \
                         run `spot migrate --apply`",
                        site.site,
                        missing.join(", ")
                    ));
                }
            }
        } else {
            problems.push(format!(
                "site `{}`: cannot reach PostgreSQL database `{}`",
                site.site, site.database
            ));
        }
    }
    if loaded.sites.is_empty()
        && spot_postgres::database_exists(&loaded.admin, "postgres")
            .await
            .is_err()
    {
        problems.push("postgres.admin_url: cannot reach PostgreSQL".into());
    }
    problems.dedup();
    if problems.is_empty() {
        let mut out = format!("ok: {checked} site(s) ready to serve\n");
        for notice in loaded
            .sites
            .iter()
            .filter_map(config::Site::legacy_secret_notice)
        {
            out.push_str("notice: ");
            out.push_str(&notice);
            out.push('\n');
        }
        Ok(out)
    } else {
        Err(Error(problems.join("\n")))
    }
}
