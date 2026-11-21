//! `spot extension list|plan|install --site S`: hosts the audited `extension.install` operation
//! (ADR-087 section 4.3, WP-STO-12.T2) for one site.
//!
//! - `list` is read-only: every allowed extension, its installed version, the platform's minimum,
//!   what needs it (the same answer a refused migration gives) and whether the role may install it.
//! - `plan` calls `plan_extension_install`: the plan and its hash, nothing changed.
//! - `install --plan-hash H` calls `apply_extension_install` bound to that hash. There is no flag
//!   that skips the hash and no way to name an extension outside the allow-list: the command
//!   refuses such a name before it connects, with the library's own code.
//!
//! Every call writes one event to the site's audit trail (the hash chain when the audit store is
//! prepared, else the site's admin audit file). Output carries stable `migrate.*` codes and fixed
//! sentences only: never a connection string, a password or a database URL.
//! Exit 0 done, 1 usage, 2 refused (nothing changed), 3 failed.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Arg, ArgMatches, Command};
use spot_cli::audit::chain::ChainAudit;
use spot_cli::audit::{AuditRecord, FileAudit, IdentityAudit};
use spot_pg_migrate::{
    ALLOWED_EXTENSIONS, AuditSink, ExtensionAuditEvent, ExtensionAuditOp, ExtensionAuditResult,
    ExtensionContext, ExtensionRequest, MigrateError, PlanOptions, apply_extension_install, codes,
    default_min_version, plan_extension_install, required_extensions,
};
use spot_postgres::PgBackend;

use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "extension";

fn site_arg() -> Arg {
    Arg::new("site")
        .long("site")
        .value_name("SITE")
        .help("The site whose database is concerned (required)")
}

fn target_args(cmd: Command) -> Command {
    cmd.arg(site_arg())
        .arg(
            Arg::new("name")
                .long("name")
                .value_name("EXTENSION")
                .help("postgis, btree_gist, pg_trgm or unaccent"),
        )
        .arg(
            Arg::new("min-version")
                .long("min-version")
                .value_name("V")
                .help("The lowest acceptable version (default: the platform's)"),
        )
        .arg(
            Arg::new("schema")
                .long("schema")
                .value_name("SCHEMA")
                .help("The schema to create it in (default: public)"),
        )
        .arg(
            Arg::new("actor")
                .long("actor")
                .value_name("NAME")
                .help("Recorded in the audit event (default: cli:<operating-system user>)"),
        )
}

/// Arguments and help.
pub fn command() -> Command {
    Command::new(NAME)
        .about("List, plan and install the allowed PostgreSQL extensions of a site")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("list")
                .about("Show the allowed extensions, what is installed and what needs them")
                .arg(site_arg()),
        )
        .subcommand(target_args(Command::new("plan").about(
            "Dry run: what installing the extension would do, and its plan hash",
        )))
        .subcommand(
            target_args(
                Command::new("install")
                    .about("Install the extension, bound to the plan hash of `plan`"),
            )
            .arg(
                Arg::new("plan-hash")
                    .long("plan-hash")
                    .value_name("HASH")
                    .help("The hash printed by `spot extension plan` (required)"),
            ),
        )
}

/// The fixed sentence of a stable code; never the library's free text, which can name a statement.
fn sentence(code: &str) -> &'static str {
    match code {
        codes::EXTENSION_NOT_ALLOWED => "that extension is not on the platform's allow-list",
        codes::EXTENSION_PRIVILEGE_MISSING => {
            "the connecting role may not create it: run it once as a database superuser or ask the hosting provider to enable it"
        }
        codes::EXTENSION_UNAVAILABLE => "the database server does not offer this extension",
        codes::EXTENSION_VERSION => "the version the server offers is older than the minimum",
        codes::EXTENSION_PLAN_STALE => "the database changed since the plan: run plan again",
        _ => "the operation failed; see the server log",
    }
}

fn is_refusal(code: &str) -> bool {
    [
        codes::EXTENSION_NOT_ALLOWED,
        codes::EXTENSION_PRIVILEGE_MISSING,
        codes::EXTENSION_UNAVAILABLE,
        codes::EXTENSION_VERSION,
        codes::EXTENSION_PLAN_STALE,
    ]
    .contains(&code)
}

/// Turns each library audit event into one record of the site's trail. A failed write is
/// remembered, so the command can say the trail is incomplete and fail.
struct TrailSink {
    site: String,
    trail: Arc<dyn IdentityAudit>,
    failed: AtomicBool,
}

impl AuditSink for TrailSink {
    fn record(&self, event: ExtensionAuditEvent) {
        let op = match event.operation {
            ExtensionAuditOp::DryRun => "dry_run",
            ExtensionAuditOp::Install => "install",
        };
        let (result, detail) = match event.result {
            ExtensionAuditResult::Planned => ("planned", event.version.clone()),
            ExtensionAuditResult::Installed => ("installed", event.version.clone()),
            ExtensionAuditResult::AlreadyPresent => ("already_present", event.version.clone()),
            ExtensionAuditResult::Failed { code } => ("failed", Some(code.to_owned())),
        };
        // The record's fields are typed and bounded: the actor is the chain's actor, the
        // extension the "name" field, the plan hash the opaque id and the version or the failure
        // code the reason. The tenant is the record's site.
        let record = AuditRecord {
            at: i64::try_from(event.at_unix_ms / 1000).unwrap_or(i64::MAX),
            site: self.site.clone(),
            kind: format!("extension_{op}_{result}"),
            user: Some(event.actor),
            attempted: Some(event.extension),
            source: None,
            reason: detail,
            session: event.plan_hash,
            count: None,
            until: None,
            key_id: None,
            path: None,
            channel: Some("cli".to_owned()),
            os_user: None,
        };
        if self.trail.append(&record).is_err() {
            self.failed.store(true, Ordering::SeqCst);
        }
    }
}

async fn open_trail(root: &Path, site: &str) -> spot_cli::Result<Arc<dyn IdentityAudit>> {
    let loaded = spot_cli::config::load(root, false)?;
    let found = loaded
        .sites
        .iter()
        .find(|s| s.site == site)
        .ok_or_else(|| spot_cli::Error("there is no such site".into()))?;
    let mut pg = loaded.admin.clone().with_dbname(&found.database);
    pg.pool_max_size = loaded.common.postgres.pool_max_size.clamp(1, 4);
    match ChainAudit::open(pg, site, None).await? {
        Ok((chain, _)) => Ok(Arc::new(chain)),
        // The chain's tables are not there (`spot migrate --apply` creates them): the event goes
        // to the site's admin audit file rather than nowhere.
        Err(_) => Ok(Arc::new(FileAudit::open(
            &root.join("sites").join(site).join("identity-audit.log"),
        )?)),
    }
}

fn default_actor() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned());
    format!("cli:{user}")
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot extension list|plan|install --site <SITE>"
        )?;
        return Ok(Exit::Usage);
    };
    let Some(site) = sub.get_one::<String>("site").cloned() else {
        writeln!(ctx.err(), "--site is required")?;
        return Ok(Exit::Usage);
    };
    if verb == "list" {
        return list(ctx, &site).await;
    }
    let Some(name) = sub.get_one::<String>("name").cloned() else {
        writeln!(
            ctx.err(),
            "--name is required (one of: {})",
            ALLOWED_EXTENSIONS.join(", ")
        )?;
        return Ok(Exit::Usage);
    };
    // The allow-list is checked before anything connects, so a typo never reaches a database.
    if !ALLOWED_EXTENSIONS.contains(&name.as_str()) {
        writeln!(
            ctx.out(),
            "{}: {}",
            codes::EXTENSION_NOT_ALLOWED,
            sentence(codes::EXTENSION_NOT_ALLOWED)
        )?;
        writeln!(ctx.out(), "allowed: {}", ALLOWED_EXTENSIONS.join(", "))?;
        return Ok(Exit::Refused);
    }
    // Only `install` declares the flag; `plan` has no such argument to read.
    let hash = sub
        .try_get_one::<String>("plan-hash")
        .ok()
        .flatten()
        .cloned();
    if verb == "install" && hash.is_none() {
        writeln!(
            ctx.err(),
            "--plan-hash is required: run `spot extension plan --site {site} --name {name}` first and pass its plan_hash"
        )?;
        return Ok(Exit::Usage);
    }
    if let Err(usage) = known_site(ctx, &site) {
        writeln!(ctx.err(), "{usage}")?;
        return Ok(Exit::Usage);
    }
    let mut request = ExtensionRequest::new(name);
    request.min_version = sub.get_one::<String>("min-version").cloned();
    request.schema = sub.get_one::<String>("schema").cloned();
    let actor = sub
        .get_one::<String>("actor")
        .cloned()
        .unwrap_or_else(default_actor);
    let trail = open_trail(ctx.root(), &site).await;
    let Ok(trail) = trail else {
        writeln!(
            ctx.out(),
            "the site's audit trail cannot be opened: nothing was changed"
        )?;
        return Ok(Exit::Failed);
    };
    let Ok(backend) = ctx.backend(&site).await else {
        writeln!(ctx.out(), "the site has no usable PostgreSQL backend")?;
        return Ok(Exit::Failed);
    };
    let sink = TrailSink {
        site: site.clone(),
        trail,
        failed: AtomicBool::new(false),
    };
    let context = ExtensionContext {
        actor: &actor,
        tenant: &site,
        audit: &sink,
    };
    let exit = if verb == "plan" {
        plan(ctx, &backend, &context, &request).await?
    } else {
        install(
            ctx,
            &backend,
            &context,
            hash.as_deref().unwrap_or_default(),
            &request,
        )
        .await?
    };
    if sink.failed.load(Ordering::SeqCst) {
        writeln!(
            ctx.out(),
            "the audit event could not be written to the site's trail"
        )?;
        return Ok(Exit::Failed);
    }
    Ok(exit)
}

fn known_site(ctx: &mut CommandCtx, site: &str) -> Result<(), String> {
    let sites = ctx.sites().map_err(|e| e.to_string())?;
    if sites.iter().any(|s| s.site == site) {
        return Ok(());
    }
    let names: Vec<&str> = sites.iter().map(|s| s.site.as_str()).collect();
    Err(format!(
        "there is no site `{site}`; sites: {}",
        names.join(", ")
    ))
}

fn refusal_or_failure(ctx: &mut CommandCtx, error: &MigrateError) -> spot_cli::Result<Exit> {
    let code = error.code();
    writeln!(ctx.out(), "{code}: {}", sentence(code))?;
    Ok(if is_refusal(code) {
        Exit::Refused
    } else {
        Exit::Failed
    })
}

async fn plan(
    ctx: &mut CommandCtx,
    backend: &PgBackend,
    context: &ExtensionContext<'_>,
    request: &ExtensionRequest,
) -> spot_cli::Result<Exit> {
    match plan_extension_install(backend, context, request).await {
        Ok(p) => {
            writeln!(ctx.out(), "extension: {}", p.extension)?;
            writeln!(ctx.out(), "version: {}", p.version)?;
            writeln!(ctx.out(), "schema: {}", p.schema)?;
            writeln!(ctx.out(), "already_present: {}", p.already_present)?;
            writeln!(ctx.out(), "privilege_held: {}", p.privilege_held)?;
            writeln!(ctx.out(), "plan_hash: {}", p.plan_hash)?;
            Ok(Exit::Ok)
        }
        Err(e) => refusal_or_failure(ctx, &e),
    }
}

async fn install(
    ctx: &mut CommandCtx,
    backend: &PgBackend,
    context: &ExtensionContext<'_>,
    hash: &str,
    request: &ExtensionRequest,
) -> spot_cli::Result<Exit> {
    match apply_extension_install(backend, context, hash, request).await {
        Ok(o) => {
            writeln!(ctx.out(), "extension: {}", o.extension)?;
            writeln!(ctx.out(), "version: {}", o.version)?;
            writeln!(ctx.out(), "schema: {}", o.schema)?;
            writeln!(ctx.out(), "already_present: {}", o.already_present)?;
            writeln!(ctx.out(), "plan_hash: {}", o.plan_hash)?;
            Ok(Exit::Ok)
        }
        Err(e) => refusal_or_failure(ctx, &e),
    }
}

async fn list(ctx: &mut CommandCtx, site: &str) -> spot_cli::Result<Exit> {
    if let Err(usage) = known_site(ctx, site) {
        writeln!(ctx.err(), "{usage}")?;
        return Ok(Exit::Usage);
    }
    let Ok(backend) = ctx.backend(site).await else {
        writeln!(ctx.out(), "the site has no usable PostgreSQL backend")?;
        return Ok(Exit::Failed);
    };
    let names: Vec<String> = ALLOWED_EXTENSIONS.iter().map(|s| (*s).to_owned()).collect();
    let rows = match backend.pool().get().await {
        Ok(client) => client
            .query(
                "SELECT extname, extversion FROM pg_extension WHERE extname = ANY($1)",
                &[&names],
            )
            .await
            .ok(),
        Err(_) => None,
    };
    let Some(rows) = rows else {
        writeln!(ctx.out(), "the database could not be read")?;
        return Ok(Exit::Failed);
    };
    let installed: std::collections::BTreeMap<String, String> = rows
        .iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
        .collect();
    // What the site's own metadata needs, by the answer a refused migration gives.
    let needs = site_needs(ctx, site).await;
    for name in ALLOWED_EXTENSIONS {
        let version = installed.get(name).map_or("-", String::as_str);
        let needed: Vec<String> = needs
            .iter()
            .filter(|n| n.0 == name)
            .map(|n| n.1.clone())
            .collect();
        writeln!(
            ctx.out(),
            "{name}  installed: {version}  minimum: {}  can_install: unknown  needed by: {}",
            default_min_version(name),
            if needed.is_empty() {
                "-".to_owned()
            } else {
                needed.join(", ")
            }
        )?;
    }
    Ok(Exit::Ok)
}

/// `(extension, "entity (because)")` for every extension the site's catalogue needs; empty when
/// the catalogue cannot be built (the list is still useful without it).
async fn site_needs(ctx: &mut CommandCtx, site: &str) -> Vec<(String, String)> {
    let root = ctx.root().to_path_buf();
    let Ok(loaded) = spot_cli::config::load(&root, false) else {
        return Vec::new();
    };
    let Some(found) = loaded.sites.iter().find(|s| s.site == site).cloned() else {
        return Vec::new();
    };
    let Ok(permission) = spot_cli::permissions::site(&root, &loaded, &found).await else {
        return Vec::new();
    };
    required_extensions(&permission.catalog, &PlanOptions::default())
        .into_iter()
        .map(|n| (n.name, format!("{} ({})", n.entity, n.because)))
        .collect()
}
