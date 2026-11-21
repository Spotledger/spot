//! `spot bi plan|apply|list|password|revoke --site S` (WP-PRD-07.T2) over `spot-bi-views`. Guide:
//! `docs/running-spot/bi.md`.
//!
//! BI tools (Power BI, Excel) connect straight to the site's database and bypass the application's
//! permission engine, so what they can see is decided here and only here:
//!
//! - `plan`: the views, their columns and every column left out, with the reason. Read-only and
//!   offline. The exposure policy is the site's `bi.toml` (nothing is exposed without one); a
//!   column is shown only when the policy allows it **and** the metadata does not close it: a
//!   `secret` field, a protection-group member and a field tagged `secret`, `pii` or `health` are
//!   never shown, and an unclassified field is shown only when the entity's `include` lists it;
//! - `apply`: creates schema `bi` with the versioned views and the login role
//!   `spot_bi_<site>_ro`, a member of the read-only `spot_bi_reader`, with `SELECT` on `bi` only
//!   and no access to a base table, read-only by default and with timeouts; without `--apply` only
//!   the plan is shown. A second run changes nothing unless the catalogue or the policy changed;
//! - `list`: the views that exist and whether the login role does;
//! - `password`: sets a new password for the login role and writes it **once** to a file readable
//!   by its owner only (`--out`); it is printed to a terminal only when standard output is one;
//! - `revoke`: drops the login role (the views stay).
//!
//! Every change is recorded in the identity audit chain; an operation that cannot be audited does
//! not run. Exit codes: 0 done, 1 usage, 2 refused (nothing changed), 3 failed. No password, key or
//! connection string reaches the output.
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, Command};
use deadpool_postgres::Pool;
use spot_bi_views::{
    BiPlan, BiPolicy, CONNECTION_LIMIT, ExclusionReason, IDLE_TRANSACTION_TIMEOUT, Protection,
    READER_ROLE, STATEMENT_TIMEOUT, generate, verify,
};
use spot_cli::config::{Loaded, Site};
use spot_cli::services::secrets::OperatorAudit;
use spot_meta::{ENGINE_FIELDS, EffectiveMeta, FieldType};
use spot_permissions::GroupName;
use spot_storage_contract::schema::system;

use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "bi";

/// The group an unclassified field is shown under in the plan.
const UNCLASSIFIED: &str = "unclassified";

/// Arguments and help.
pub fn command() -> Command {
    let site = || {
        Arg::new("site")
            .long("site")
            .value_name("SITE")
            .help("The site to act on (required)")
    };
    Command::new(NAME)
        .about("Curated read-only reporting views and a read-only login role for BI tools")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("plan")
                .about("Show the views and what is left out")
                .arg(site()),
        )
        .subcommand(
            Command::new("apply")
                .about("Create or refresh the views and the BI login role")
                .arg(site())
                .arg(
                    Arg::new("apply")
                        .long("apply")
                        .action(ArgAction::SetTrue)
                        .help("Make the change; without it only the plan is shown"),
                ),
        )
        .subcommand(
            Command::new("list")
                .about("The views and the BI role that exist")
                .arg(site()),
        )
        .subcommand(
            Command::new("password")
                .about("Set a new password for the BI login role and write it once to a file")
                .arg(site())
                .arg(Arg::new("out").long("out").value_name("FILE").help(
                    "The file to write the password to (created with mode 0600, must not exist); \
                     required unless standard output is a terminal",
                )),
        )
        .subcommand(
            Command::new("revoke")
                .about("Drop the BI login role")
                .arg(site()),
        )
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot bi <plan|apply|list|password|revoke> --site SITE"
        )?;
        return Ok(Exit::Usage);
    };
    let Some(site) = sub.get_one::<String>("site") else {
        writeln!(ctx.err(), "--site is required")?;
        return Ok(Exit::Usage);
    };
    let prepared = match prepare(ctx, site) {
        Ok(prepared) => prepared,
        Err(exit) => return Ok(exit),
    };
    match verb {
        "plan" => {
            show(ctx, &prepared)?;
            Ok(Exit::Ok)
        }
        "apply" => apply(ctx, &prepared, sub.get_flag("apply")).await,
        "list" => list(ctx, &prepared).await,
        "password" => password(ctx, &prepared, sub.get_one::<String>("out")).await,
        "revoke" => revoke(ctx, &prepared).await,
        _ => {
            writeln!(ctx.err(), "unknown bi command `{verb}`")?;
            Ok(Exit::Usage)
        }
    }
}

/// What every verb needs.
struct Prepared {
    loaded: Loaded,
    site: Site,
    role: String,
    plan: BiPlan,
    policy_file: bool,
}

fn say(ctx: &mut CommandCtx, text: impl AsRef<str>) -> Exit {
    let _ = writeln!(ctx.err(), "{}", text.as_ref());
    Exit::Refused
}

/// The login role of a site: `spot_bi_<site>_ro`. A role belongs to the cluster, so the site name
/// is in it; a name PostgreSQL cannot hold is refused rather than shortened.
fn role_of(site: &str) -> Result<String, String> {
    let plain = site
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let role = format!("spot_bi_{site}_ro");
    if plain && !site.is_empty() && role.len() <= 63 {
        Ok(role)
    } else {
        Err(format!(
            "the site name cannot make a role name (letters, digits, `_`, `-`; at most {} characters)",
            63 - "spot_bi__ro".len()
        ))
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn literal(text: &str) -> String {
    format!("E'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
}

/// Loads the site, its policy and the plan. Every problem is printed, once.
fn prepare(ctx: &mut CommandCtx, site_name: &str) -> Result<Prepared, Exit> {
    let loaded = spot_cli::config::load(ctx.root(), false).map_err(|e| say(ctx, e.to_string()))?;
    let Some(site) = loaded.sites.iter().find(|s| s.site == site_name).cloned() else {
        let _ = writeln!(ctx.err(), "there is no site `{site_name}`");
        return Err(Exit::Usage);
    };
    let role = role_of(site_name).map_err(|why| say(ctx, why))?;
    let meta =
        spot_cli::apps::serving(ctx.root(), &loaded, &site).map_err(|e| say(ctx, e.to_string()))?;
    let catalog = spot_cli::schema::catalog(&meta).map_err(|e| say(ctx, e.to_string()))?;
    let file = ctx.root().join("sites").join(site_name).join("bi.toml");
    let (text, policy_file) = match std::fs::read_to_string(&file) {
        Ok(text) => (text, true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
        Err(_) => return Err(say(ctx, "bi.toml could not be read")),
    };
    let policy = BiPolicy::from_toml(&text).map_err(|e| say(ctx, e.to_string()))?;
    let protection = protection_of(&meta, &policy).map_err(|e| say(ctx, e.to_string()))?;
    let plan = generate(&catalog, &policy, &protection).map_err(|e| say(ctx, e.to_string()))?;
    // One database per tenant: a view may read the base schema and nothing else.
    let foreign = foreign_schemas(&plan, policy.base_schema());
    if !foreign.is_empty() {
        return Err(say(
            ctx,
            "a planned view reads outside the site's base schema; nothing was applied",
        ));
    }
    Ok(Prepared {
        loaded,
        site,
        role,
        plan,
        policy_file,
    })
}

/// The fields the views must never show, from the metadata: `secret` fields and fields tagged
/// `secret`, `pii` or `health`, protection-group members, and, for an entity whose policy has no
/// `include` list, every unclassified field (an `include` is the policy allowing them by name).
fn protection_of(
    meta: &EffectiveMeta,
    policy: &BiPolicy,
) -> Result<Protection, spot_bi_views::BiError> {
    let mut protection = Protection::new();
    for def in meta.definitions() {
        let listed = spot_storage_contract::Name::new(&def.name)
            .ok()
            .and_then(|name| policy.entity(&name))
            .is_some_and(|entity| entity.include.is_some());
        for field in &def.fields {
            let tags = &field.classification;
            let group = |name: String| GroupName(name);
            let sensitive = tags.iter().find(|t| matches!(t.as_str(), "pii" | "health"));
            if field.ty == FieldType::Secret || tags.iter().any(|t| t == "secret") {
                protection = protection.secret_field(&def.name, &field.name)?;
            } else if let Some(g) = &field.protection_group {
                protection = protection.group_field(&def.name, &field.name, &group(g.clone()))?;
            } else if let Some(tag) = sensitive {
                let name = format!("classified {tag}");
                protection = protection.group_field(&def.name, &field.name, &group(name))?;
            } else if tags.is_empty() && !listed && !is_system(&field.name) {
                protection =
                    protection.group_field(&def.name, &field.name, &group(UNCLASSIFIED.into()))?;
            }
        }
    }
    Ok(protection)
}

fn is_system(name: &str) -> bool {
    system::ALL.contains(&name) || ENGINE_FIELDS.contains(&name)
}

/// Schemas other than `base` that a planned view reads (`FROM "s"."t"` and `JOIN "s"."t"`).
fn foreign_schemas(plan: &BiPlan, base: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for view in &plan.views {
        for word in ["FROM \"", "JOIN \""] {
            for part in view.body.split(word).skip(1) {
                if let Some((schema, rest)) = part.split_once('"')
                    && rest.starts_with('.')
                    && schema != base
                {
                    found.insert(schema.to_owned());
                }
            }
        }
    }
    found
}

fn why(reason: &ExclusionReason) -> String {
    match reason {
        ExclusionReason::NotIncluded => "not in the policy's include list".to_owned(),
        ExclusionReason::PolicyExcluded => "excluded by the policy".to_owned(),
        ExclusionReason::ProtectionGroup(g) if g == UNCLASSIFIED => {
            "unclassified: closed by default, shown only when the entity's include lists it"
                .to_owned()
        }
        ExclusionReason::ProtectionGroup(g) => format!("protected ({g})"),
        ExclusionReason::Secret => "secret".to_owned(),
        ExclusionReason::SystemInternal => "system internal".to_owned(),
        ExclusionReason::UnsupportedType(t) => format!("type {t} has no BI representation"),
    }
}

fn show(ctx: &mut CommandCtx, p: &Prepared) -> spot_cli::Result<()> {
    let site = &p.site.site;
    writeln!(
        ctx.out(),
        "site {site}: catalogue version {}, role {}",
        p.plan.catalog_version,
        p.role
    )?;
    if !p.policy_file {
        writeln!(ctx.out(), "no sites/{site}/bi.toml: nothing is exposed")?;
    }
    writeln!(ctx.out(), "views:")?;
    for view in &p.plan.views {
        writeln!(ctx.out(), "  bi.{} ({})", view.name, view.entity)?;
        for column in &view.columns {
            writeln!(ctx.out(), "    {} {}", column.name, column.pg_type)?;
        }
    }
    writeln!(ctx.out(), "excluded:")?;
    for item in &p.plan.excluded {
        writeln!(
            ctx.out(),
            "  {}.{}: {}",
            item.entity,
            item.column,
            why(&item.reason)
        )?;
    }
    Ok(())
}

// --- the database ----------------------------------------------------------------------------------

/// A statement that failed, as the SQLSTATE only: never the statement, never a value.
fn state(error: &tokio_postgres::Error) -> String {
    match error.code() {
        Some(code) => format!("the database refused the change (SQLSTATE {})", code.code()),
        None => "the database connection failed".to_owned(),
    }
}

async fn pool_of(ctx: &mut CommandCtx, site: &str) -> Result<Pool, Exit> {
    match ctx.backend(site).await {
        Ok(backend) => Ok(backend.pool()),
        Err(error) => {
            let _ = writeln!(ctx.err(), "{error}");
            Err(Exit::Failed)
        }
    }
}

async fn run_sql(pool: &Pool, statements: &[String]) -> Result<(), String> {
    let client = pool
        .get()
        .await
        .map_err(|_| "no connection from the pool".to_owned())?;
    for statement in statements {
        client
            .batch_execute(statement)
            .await
            .map_err(|e| state(&e))?;
    }
    Ok(())
}

async fn role_exists(pool: &Pool, role: &str) -> Result<bool, String> {
    let client = pool
        .get()
        .await
        .map_err(|_| "no connection from the pool".to_owned())?;
    let rows = client
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
        .await
        .map_err(|e| state(&e))?;
    Ok(!rows.is_empty())
}

/// The audit sink; an operation that must be audited does not run without it.
async fn audit_of(ctx: &mut CommandCtx, p: &Prepared) -> Result<OperatorAudit, Exit> {
    let clock = Arc::clone(ctx.clock());
    OperatorAudit::open(&p.loaded, &p.site, clock)
        .await
        .map_err(|e| say(ctx, e.to_string()))
}

/// The login role: created without a password (it cannot sign in until `spot bi password`), a
/// member of the read-only reader, read-only by default, with timeouts and a connection limit.
/// Settings of a group role are not inherited by its members, so they are set on the login role.
fn role_sql(role: &str) -> Vec<String> {
    let name = quote(role);
    vec![
        format!(
            "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = {}) THEN CREATE ROLE {name} LOGIN; END IF; END $$",
            literal(role)
        ),
        format!(
            "ALTER ROLE {name} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE INHERIT CONNECTION LIMIT {CONNECTION_LIMIT}"
        ),
        format!("GRANT {} TO {name}", quote(READER_ROLE)),
        format!("ALTER ROLE {name} SET default_transaction_read_only = on"),
        format!(
            "ALTER ROLE {name} SET statement_timeout = {}",
            literal(STATEMENT_TIMEOUT)
        ),
        format!(
            "ALTER ROLE {name} SET idle_in_transaction_session_timeout = {}",
            literal(IDLE_TRANSACTION_TIMEOUT)
        ),
    ]
}

async fn apply(ctx: &mut CommandCtx, p: &Prepared, go: bool) -> spot_cli::Result<Exit> {
    show(ctx, p)?;
    if !go {
        writeln!(
            ctx.out(),
            "dry run: nothing was changed; add --apply to create or refresh the views and the role"
        )?;
        return Ok(Exit::Ok);
    }
    let audit = match audit_of(ctx, p).await {
        Ok(audit) => audit,
        Err(exit) => return Ok(exit),
    };
    let pool = match pool_of(ctx, &p.site.site).await {
        Ok(pool) => pool,
        Err(exit) => return Ok(exit),
    };
    let report = match spot_bi_views::apply(&pool, &p.plan).await {
        Ok(report) => report,
        Err(error) => {
            audit.always("bi_apply_failed", 0, Some(error.code()));
            writeln!(ctx.err(), "{error}")?;
            return Ok(Exit::Refused);
        }
    };
    if let Err(text) = run_sql(&pool, &role_sql(&p.role)).await {
        audit.always("bi_apply_failed", 0, Some("bi.database"));
        writeln!(ctx.err(), "bi.database: {text}")?;
        return Ok(Exit::Failed);
    }
    audit.always("bi_apply", p.plan.views.len() as u64, None);
    writeln!(
        ctx.out(),
        "views: {} created, {} replaced, {} unchanged; role {} is ready (no password yet: `spot bi password`)",
        report.created.len(),
        report.replaced.len(),
        report.unchanged.len(),
        p.role
    )?;
    match verify(&pool).await {
        Ok(checks) if checks.passed() => {
            writeln!(ctx.out(), "verified: {} checks passed", checks.checks.len())?
        }
        Ok(checks) => {
            for failed in checks.failures() {
                writeln!(
                    ctx.err(),
                    "verify failed: {}: {}",
                    failed.name,
                    failed.detail
                )?;
            }
            return Ok(Exit::Failed);
        }
        Err(error) => {
            writeln!(ctx.err(), "{error}")?;
            return Ok(Exit::Failed);
        }
    }
    Ok(Exit::Ok)
}

async fn list(ctx: &mut CommandCtx, p: &Prepared) -> spot_cli::Result<Exit> {
    let pool = match pool_of(ctx, &p.site.site).await {
        Ok(pool) => pool,
        Err(exit) => return Ok(exit),
    };
    let client = pool
        .get()
        .await
        .map_err(|_| spot_cli::Error("no connection from the pool".into()))?;
    let views = client
        .query("SELECT table_name::text FROM information_schema.views WHERE table_schema = 'bi' ORDER BY 1", &[])
        .await
        .map_err(|e| spot_cli::Error(state(&e)))?;
    writeln!(ctx.out(), "views in bi: {}", views.len())?;
    for row in &views {
        writeln!(ctx.out(), "  {}", row.get::<_, String>(0))?;
    }
    let present = role_exists(&pool, &p.role).await.map_err(spot_cli::Error)?;
    writeln!(
        ctx.out(),
        "role {}: {}",
        p.role,
        if present { "present" } else { "absent" }
    )?;
    Ok(Exit::Ok)
}

async fn password(
    ctx: &mut CommandCtx,
    p: &Prepared,
    out: Option<&String>,
) -> spot_cli::Result<Exit> {
    let terminal = std::io::stdout().is_terminal();
    if out.is_none() && !terminal {
        writeln!(
            ctx.err(),
            "refused: standard output is not a terminal; give --out FILE (the password is written there, mode 0600)"
        )?;
        return Ok(Exit::Refused);
    }
    let audit = match audit_of(ctx, p).await {
        Ok(audit) => audit,
        Err(exit) => return Ok(exit),
    };
    let pool = match pool_of(ctx, &p.site.site).await {
        Ok(pool) => pool,
        Err(exit) => return Ok(exit),
    };
    match role_exists(&pool, &p.role).await {
        Ok(true) => {}
        Ok(false) => {
            writeln!(
                ctx.err(),
                "refused: role {} does not exist; run `spot bi apply --apply` first",
                p.role
            )?;
            return Ok(Exit::Refused);
        }
        Err(text) => {
            writeln!(ctx.err(), "bi.database: {text}")?;
            return Ok(Exit::Failed);
        }
    }
    // The file comes first: a password that cannot be delivered is never set.
    let secret = spot_auth::ApiKey::generate().0.text();
    if let Some(path) = out
        && let Err(error) = write_private(Path::new(path), &secret)
    {
        writeln!(ctx.err(), "refused: {error}")?;
        return Ok(Exit::Refused);
    }
    let alter = format!(
        "ALTER ROLE {} PASSWORD {}",
        quote(&p.role),
        literal(&secret)
    );
    if let Err(text) = run_sql(&pool, &[alter]).await {
        if let Some(path) = out {
            let _ = std::fs::remove_file(path);
        }
        audit.always("bi_password_failed", 0, Some("bi.database"));
        writeln!(ctx.err(), "bi.database: {text}")?;
        return Ok(Exit::Failed);
    }
    audit.always("bi_password", 1, None);
    match out {
        Some(path) => writeln!(
            ctx.out(),
            "new password for {} written to {path} (mode 0600); it is not shown again",
            p.role
        )?,
        None => writeln!(
            ctx.out(),
            "new password for {} (shown once): {secret}",
            p.role
        )?,
    }
    Ok(Exit::Ok)
}

/// Writes `text` to a new file only its owner can read; an existing file is never overwritten.
fn write_private(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| {
        format!(
            "the password file could not be created ({:?}); it must not exist",
            e.kind()
        )
    })?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|_| "the password file could not be written".to_owned())
}

async fn revoke(ctx: &mut CommandCtx, p: &Prepared) -> spot_cli::Result<Exit> {
    let audit = match audit_of(ctx, p).await {
        Ok(audit) => audit,
        Err(exit) => return Ok(exit),
    };
    let pool = match pool_of(ctx, &p.site.site).await {
        Ok(pool) => pool,
        Err(exit) => return Ok(exit),
    };
    match role_exists(&pool, &p.role).await {
        Ok(false) => {
            writeln!(
                ctx.out(),
                "role {} does not exist: nothing to revoke",
                p.role
            )?;
            return Ok(Exit::Ok);
        }
        Ok(true) => {}
        Err(text) => {
            writeln!(ctx.err(), "bi.database: {text}")?;
            return Ok(Exit::Failed);
        }
    }
    let name = quote(&p.role);
    let drop = [format!("DROP OWNED BY {name}"), format!("DROP ROLE {name}")];
    if let Err(text) = run_sql(&pool, &drop).await {
        audit.always("bi_revoke_failed", 0, Some("bi.database"));
        writeln!(ctx.err(), "bi.database: {text}")?;
        return Ok(Exit::Failed);
    }
    audit.always("bi_revoke", 1, None);
    writeln!(ctx.out(), "role {} dropped; the views stay", p.role)?;
    Ok(Exit::Ok)
}
