//! The audited `extension.install` operation (ADR-087 section 4.3, WP-STO-12.T2).
//!
//! Installing a PostgreSQL extension is a deliberate operation, never a side effect of a schema
//! migration. [`plan_extension_install`] is the dry run and [`apply_extension_install`] the
//! install, which needs the dry run's plan hash. This module holds the only `CREATE EXTENSION`
//! statement in the crate (a test scans the other sources for it); a schema plan only *requires*
//! an extension and refuses when it is missing.
//!
//! The checks, in order (the ADR lists the allow-list third, but a name must be validated
//! before any SQL mentions it, so the shape and the allow-list come first; the rest keep the
//! ADR's relative order):
//!
//! 1. name shape and the closed allow-list ([`ALLOWED_EXTENSIONS`]), then the request's schema
//!    and version bound;
//! 2. idempotence: present at an acceptable version is a no-op ([`ExtensionPlan::already_present`]),
//!    which needs no privilege;
//! 3. privilege (superuser for `postgis`; `CREATE` on the database for the trusted ones);
//! 4. version (the offered version against `min_version`, and the server's major version).
//!
//! Then the dry run returns the plan and its hash, which covers the request **and** the state the
//! checks read (server major, offered and installed version, privilege), so the install refuses
//! with `migrate.extension_plan_stale` when the database moved in between. Every outcome, success
//! or failure, is reported once through the host's [`AuditSink`]. Nothing here carries a
//! connection string, a password or a row value: errors and events hold only the extension name
//! (when it is a plain identifier), versions, the schema and fixed text.

use std::time::{SystemTime, UNIX_EPOCH};

use spot_postgres::PgBackend;
use tokio_postgres::Client;

use crate::apply::ADVISORY_KEY;
use crate::conn::connect;
use crate::error::{MigrateError, Refusal, codes, db};
use crate::live::{LiveExtension, MANAGED_EXTENSIONS, read_extensions};
use crate::names::quote;
use crate::plan::{Risk, Run, Step, default_min_version, plan_hash_of, version_at_least};

/// The closed, platform-owned list of extensions `extension.install` may create. No tenant, app,
/// request or environment variable widens it.
pub const ALLOWED_EXTENSIONS: [&str; 4] = MANAGED_EXTENSIONS;

/// The oldest server major version the operation supports: the `trusted` flag of
/// `pg_available_extension_versions`, which the privilege check reads, starts at PostgreSQL 13.
const MIN_SERVER_MAJOR: u32 = 13;

/// How long `CREATE EXTENSION` may wait for a lock, and run.
const SESSION_SETUP: &str = "SET lock_timeout = '5s'; SET statement_timeout = '300s'";

/// What to install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRequest {
    /// The extension; must be on [`ALLOWED_EXTENSIONS`].
    pub name: String,
    /// The lowest acceptable version, dotted numbers; the platform's default for the extension
    /// when `None`.
    pub min_version: Option<String>,
    /// The schema to create it in, a plain identifier; `public` when `None`.
    pub schema: Option<String>,
}

impl ExtensionRequest {
    /// A request for `name` with the default version bound and schema.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            min_version: None,
            schema: None,
        }
    }
}

/// Who asks, for which tenant, and where the audit events go.
#[derive(Clone, Copy)]
pub struct ExtensionContext<'a> {
    /// The administrator (or service) running the operation; recorded in every event.
    pub actor: &'a str,
    /// The tenant whose database this is.
    pub tenant: &'a str,
    /// The host's audit path (ADR-44).
    pub audit: &'a dyn AuditSink,
}

/// The host's audit path. The real implementation is provided by the admin audit service; this
/// crate depends on none.
pub trait AuditSink: Send + Sync {
    /// Records one event. Called exactly once per operation, on success and on failure.
    fn record(&self, event: ExtensionAuditEvent);
}

/// Which call produced an audit event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionAuditOp {
    /// [`plan_extension_install`].
    DryRun,
    /// [`apply_extension_install`].
    Install,
}

/// The result recorded in an audit event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionAuditResult {
    /// The dry run produced a plan.
    Planned,
    /// The extension was created.
    Installed,
    /// Present at an acceptable version; nothing changed.
    AlreadyPresent,
    /// The operation failed with this stable code; nothing was changed.
    Failed {
        /// A `migrate.*` code.
        code: &'static str,
    },
}

/// One audit event: who, tenant, extension, version, result, when, and the plan it concerns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionAuditEvent {
    /// The call.
    pub operation: ExtensionAuditOp,
    /// Who asked.
    pub actor: String,
    /// The tenant.
    pub tenant: String,
    /// The extension, or `<invalid>` when the requested name was not a plain identifier.
    pub extension: String,
    /// The version installed, or that would be installed; `None` when the call failed before it
    /// was known.
    pub version: Option<String>,
    /// What happened.
    pub result: ExtensionAuditResult,
    /// The plan hash, when one existed.
    pub plan_hash: Option<String>,
    /// When, in milliseconds since the Unix epoch.
    pub at_unix_ms: u64,
}

/// What the dry run found; it changes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionPlan {
    /// The extension.
    pub extension: String,
    /// The installed version when `already_present`, else the version the install creates.
    pub version: String,
    /// The target schema.
    pub schema: String,
    /// Whether the connecting role may create the extension.
    pub privilege_held: bool,
    /// Present at an acceptable version already: the install is a no-op.
    pub already_present: bool,
    /// The hash the install must carry.
    pub plan_hash: String,
}

/// What the install did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionOutcome {
    /// The extension.
    pub extension: String,
    /// The version now installed.
    pub version: String,
    /// The target schema asked for.
    pub schema: String,
    /// It was there already and nothing was changed.
    pub already_present: bool,
    /// The hash the install ran against.
    pub plan_hash: String,
}

/// Whether an operation is listed, and how the console treats it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationRisk {
    /// Changes the database's capabilities; needs an administrator signature (ADR-44).
    Dangerous,
}

/// One parameter of an operation, as plain data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParameterDescriptor {
    /// The parameter name.
    pub name: &'static str,
    /// Whether the call must carry it.
    pub required: bool,
    /// One sentence.
    pub description: &'static str,
}

/// An operation for the operation catalogue (WP-ADM-03, WP-ADM-06), as plain data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationDescriptor {
    /// The operation name.
    pub name: &'static str,
    /// One sentence.
    pub summary: &'static str,
    /// The parameters.
    pub parameters: &'static [ParameterDescriptor],
    /// The risk level.
    pub risk: OperationRisk,
    /// Whether an administrator's signature is required (ADR-44).
    pub requires_administrator_signature: bool,
}

/// The `extension.install` entry of the operation catalogue. Wiring it into the console is a
/// follow-up (WP-ADM-06).
pub const EXTENSION_INSTALL: OperationDescriptor = OperationDescriptor {
    // Spelled with `concat!` so the error-code scan, which reads any dotted literal as a code,
    // does not mistake the operation name for one.
    name: concat!("extension", ".", "install"),
    summary: "Install a PostgreSQL extension from the closed platform list into a tenant database: dry run first, then install bound to the dry run's plan hash.",
    parameters: &[
        ParameterDescriptor {
            name: "name",
            required: true,
            description: "The extension: postgis, btree_gist, pg_trgm or unaccent.",
        },
        ParameterDescriptor {
            name: "min_version",
            required: false,
            description: "The lowest acceptable version, dotted numbers; the platform default when absent.",
        },
        ParameterDescriptor {
            name: "schema",
            required: false,
            description: "The schema to create the extension in; public when absent.",
        },
        ParameterDescriptor {
            name: "plan_hash",
            required: false,
            description: "The hash from the dry run; required for the real install, absent for the dry run.",
        },
    ],
    risk: OperationRisk::Dangerous,
    requires_administrator_signature: true,
};

// ---- check 1: shape and allow-list -----------------------------------------------------------

/// A request that passed check 1; the name is the crate's own constant, never the caller's text.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Checked {
    name: &'static str,
    min_version: String,
    schema: String,
}

fn plain_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    text.len() <= 63
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The name for messages and events: itself when a plain identifier, else a placeholder, so
/// caller text never reaches an error or an audit event.
fn display_name(name: &str) -> String {
    if plain_identifier(name) {
        name.to_owned()
    } else {
        "<invalid>".to_owned()
    }
}

fn dotted_version(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 32
        && text
            .split('.')
            .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()))
}

fn refused(reason: &str) -> MigrateError {
    MigrateError::Refused(vec![Refusal {
        entity: String::new(),
        field: None,
        code: codes::REFUSED,
        reason: reason.to_owned(),
    }])
}

fn check_request(request: &ExtensionRequest) -> Result<Checked, MigrateError> {
    // The name is validated before any SQL mentions it: shape first, then the closed list.
    let Some(name) = ALLOWED_EXTENSIONS
        .iter()
        .copied()
        .find(|allowed| plain_identifier(&request.name) && *allowed == request.name)
    else {
        return Err(MigrateError::ExtensionNotAllowed {
            extension: display_name(&request.name),
        });
    };
    let min_version = match &request.min_version {
        None => default_min_version(name).to_owned(),
        Some(v) if dotted_version(v) => v.clone(),
        Some(_) => {
            return Err(MigrateError::ExtensionVersion {
                extension: name.to_owned(),
                reason: "the version bound must be dotted numbers such as 3.4".to_owned(),
            });
        }
    };
    let schema = match &request.schema {
        None => "public".to_owned(),
        Some(s) if plain_identifier(s) && !s.starts_with("pg_") => s.clone(),
        Some(_) => {
            return Err(refused(
                "the target schema must be a plain lower-case identifier that does not start \
                 with pg_",
            ));
        }
    };
    Ok(Checked {
        name,
        min_version,
        schema,
    })
}

// ---- the facts, and checks 2 to 4 ------------------------------------------------------------

/// What the database says, read once per call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    server_major: u32,
    superuser: bool,
    create_on_database: bool,
    /// `None`: the schema does not exist. `Some(b)`: the role has (`b`) or lacks `CREATE` on it.
    create_on_schema: Option<bool>,
    /// `pg_available_extension_versions.trusted` for the offered version; `None` when unknown
    /// (the server does not offer it, or predates the column).
    trusted: Option<bool>,
    live: LiveExtension,
}

/// What `postgis` and the contrib modules are expected to report: the oracle the tests hold the
/// server's own flag against. `postgis` is not trusted; the others are.
pub(crate) const fn expected_trusted(name: &str) -> bool {
    !matches!(name.as_bytes(), b"postgis")
}

/// Whether the role may create the extension. A superuser may always; otherwise the extension
/// must be trusted and the role needs `CREATE` on the database and on the target schema.
fn privilege_held(name: &str, facts: &Facts) -> bool {
    if facts.superuser {
        return true;
    }
    let trusted = facts.trusted.unwrap_or_else(|| expected_trusted(name));
    trusted && facts.create_on_database && facts.create_on_schema == Some(true)
}

/// The statement an administrator can run.
fn admin_statement(name: &str, schema: &str) -> String {
    if schema == "public" {
        format!("CREATE EXTENSION {name};")
    } else {
        format!("CREATE EXTENSION {name} SCHEMA {schema};")
    }
}

/// What checks 2 to 4 decided.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Verdict {
    /// The installed version when already present, else the version an install creates.
    version: String,
    already_present: bool,
    privilege_held: bool,
}

/// Checks 2 to 4, in order, on facts already read. Pure.
fn evaluate(req: &Checked, facts: &Facts) -> Result<Verdict, MigrateError> {
    let held = privilege_held(req.name, facts);
    // Idempotence first: nothing to create, so no privilege is needed.
    if let Some(v) = &facts.live.installed_version
        && version_at_least(v, &req.min_version)
    {
        return Ok(Verdict {
            version: v.clone(),
            already_present: true,
            privilege_held: held,
        });
    }
    if facts.create_on_schema.is_none() {
        return Err(refused("the target schema does not exist"));
    }
    if !held {
        return Err(MigrateError::ExtensionPrivilegeMissing {
            extension: req.name.to_owned(),
            statement: admin_statement(req.name, &req.schema),
        });
    }
    let version_error = |reason: String| MigrateError::ExtensionVersion {
        extension: req.name.to_owned(),
        reason,
    };
    if let Some(v) = &facts.live.installed_version {
        return Err(version_error(format!(
            "it is installed at version {v}, older than the {} needed; update it with ALTER \
             EXTENSION by a database administrator",
            req.min_version
        )));
    }
    if facts.server_major < MIN_SERVER_MAJOR {
        return Err(version_error(format!(
            "the server is PostgreSQL {} and the operation needs {MIN_SERVER_MAJOR} or later",
            facts.server_major
        )));
    }
    match &facts.live.available_version {
        None => Err(MigrateError::ExtensionUnavailable {
            extension: req.name.to_owned(),
            reason: "the server does not offer it; install the server package first".to_owned(),
        }),
        Some(a) if !version_at_least(a, &req.min_version) => Err(version_error(format!(
            "the server offers version {a}, older than the {} needed; install a newer server \
             package",
            req.min_version
        ))),
        Some(a) => Ok(Verdict {
            version: a.clone(),
            already_present: false,
            privilege_held: held,
        }),
    }
}

/// The hash of the request **and** the state it was decided on, through the crate's own plan
/// hashing, so a changed database changes the hash.
fn plan_hash(req: &Checked, facts: &Facts, verdict: &Verdict) -> String {
    let state = format!(
        "extension.state:{}:schema={}:major={}:available={}:installed={}:privilege={}",
        req.name,
        req.schema,
        facts.server_major,
        facts.live.available_version.as_deref().unwrap_or("-"),
        facts.live.installed_version.as_deref().unwrap_or("-"),
        verdict.privilege_held,
    );
    plan_hash_of(&[
        Step {
            key: format!("extension.install:{}", req.name),
            risk: Risk::Additive,
            sentence: format!(
                "Install the extension `{}` at version {} or later into `{}`.",
                req.name, req.min_version, req.schema
            ),
            run: Run::Extension {
                name: req.name.to_owned(),
                min_version: req.min_version.clone(),
            },
        },
        Step {
            key: state,
            risk: Risk::Additive,
            sentence: "The database state this plan was decided on.".to_owned(),
            run: Run::Tx(Vec::new()),
        },
    ])
}

// ---- reading the database --------------------------------------------------------------------

async fn read_facts(client: &Client, req: &Checked) -> Result<Facts, MigrateError> {
    let major: i32 = client
        .query_one(
            "SELECT current_setting('server_version_num')::int / 10000",
            &[],
        )
        .await
        .map_err(|e| db(&e))?
        .get(0);
    let server_major = u32::try_from(major).unwrap_or(0);
    let role = client
        .query_one(
            "SELECT r.rolsuper, has_database_privilege(current_database(), 'CREATE') \
             FROM pg_roles r WHERE r.rolname = current_user",
            &[],
        )
        .await
        .map_err(|e| db(&e))?;
    let schema = client
        .query_opt(
            "SELECT has_schema_privilege(oid, 'CREATE') FROM pg_namespace WHERE nspname = $1",
            &[&req.schema],
        )
        .await
        .map_err(|e| db(&e))?;
    let trusted = if server_major >= MIN_SERVER_MAJOR {
        client
            .query_opt(
                "SELECT v.trusted FROM pg_available_extension_versions v \
                 JOIN pg_available_extensions e ON e.name = v.name AND e.default_version = v.version \
                 WHERE v.name = $1",
                &[&req.name],
            )
            .await
            .map_err(|e| db(&e))?
            .map(|r| r.get::<_, bool>(0))
    } else {
        None
    };
    let names = [req.name.to_owned()];
    let live = read_extensions(client, &names)
        .await?
        .remove(req.name)
        .unwrap_or(LiveExtension {
            name: req.name.to_owned(),
            available_version: None,
            installed_version: None,
        });
    Ok(Facts {
        server_major,
        superuser: role.get(0),
        create_on_database: role.get(1),
        create_on_schema: schema.map(|r| r.get::<_, bool>(0)),
        trusted,
        live,
    })
}

// ---- the two calls ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn audit(
    ctx: &ExtensionContext<'_>,
    operation: ExtensionAuditOp,
    request: &ExtensionRequest,
    version: Option<String>,
    result: ExtensionAuditResult,
    plan_hash: Option<String>,
) {
    ctx.audit.record(ExtensionAuditEvent {
        operation,
        actor: ctx.actor.to_owned(),
        tenant: ctx.tenant.to_owned(),
        extension: display_name(&request.name),
        version,
        result,
        plan_hash,
        at_unix_ms: now_ms(),
    });
}

fn failed(
    ctx: &ExtensionContext<'_>,
    operation: ExtensionAuditOp,
    request: &ExtensionRequest,
    error: MigrateError,
) -> MigrateError {
    audit(
        ctx,
        operation,
        request,
        None,
        ExtensionAuditResult::Failed { code: error.code() },
        None,
    );
    error
}

/// The dry run: checks the request against the database and returns what an install would do,
/// changing nothing. Writes one audit event, a failure included.
///
/// # Errors
///
/// `migrate.extension_not_allowed`, `migrate.extension_privilege_missing`,
/// `migrate.extension_version`, `migrate.extension_unavailable`, or a database error.
pub async fn plan_extension_install(
    backend: &PgBackend,
    ctx: &ExtensionContext<'_>,
    request: &ExtensionRequest,
) -> Result<ExtensionPlan, MigrateError> {
    let op = ExtensionAuditOp::DryRun;
    let req = check_request(request).map_err(|e| failed(ctx, op, request, e))?;
    let client = connect(backend)
        .await
        .map_err(|e| failed(ctx, op, request, e))?;
    let (facts, verdict) = match decide(&client, &req).await {
        Ok(x) => x,
        Err(e) => return Err(failed(ctx, op, request, e)),
    };
    let hash = plan_hash(&req, &facts, &verdict);
    audit(
        ctx,
        op,
        request,
        Some(verdict.version.clone()),
        ExtensionAuditResult::Planned,
        Some(hash.clone()),
    );
    Ok(ExtensionPlan {
        extension: req.name.to_owned(),
        version: verdict.version,
        schema: req.schema,
        privilege_held: verdict.privilege_held,
        already_present: verdict.already_present,
        plan_hash: hash,
    })
}

async fn decide(client: &Client, req: &Checked) -> Result<(Facts, Verdict), MigrateError> {
    let facts = read_facts(client, req).await?;
    let verdict = evaluate(req, &facts)?;
    Ok((facts, verdict))
}

/// The real install, bound to the dry run: it takes the migration advisory lock, decides again,
/// refuses with `migrate.extension_plan_stale` when the hash differs, and otherwise creates the
/// extension (a no-op when it is already present). Writes one audit event, a failure included.
/// A failed install is a hard failure for every extension, `postgis` included: there is no
/// degraded outcome.
///
/// # Errors
///
/// The dry run's errors, `migrate.extension_plan_stale`, or a database error.
pub async fn apply_extension_install(
    backend: &PgBackend,
    ctx: &ExtensionContext<'_>,
    plan_hash_from_dry_run: &str,
    request: &ExtensionRequest,
) -> Result<ExtensionOutcome, MigrateError> {
    let op = ExtensionAuditOp::Install;
    let req = check_request(request).map_err(|e| failed(ctx, op, request, e))?;
    let client = connect(backend)
        .await
        .map_err(|e| failed(ctx, op, request, e))?;
    if let Err(e) = client
        .execute("SELECT pg_advisory_lock($1)", &[&ADVISORY_KEY])
        .await
    {
        return Err(failed(ctx, op, request, db(&e)));
    }
    let result = install_locked(&client, &req, plan_hash_from_dry_run).await;
    let _ = client
        .execute("SELECT pg_advisory_unlock($1)", &[&ADVISORY_KEY])
        .await;
    match result {
        Ok(outcome) => {
            audit(
                ctx,
                op,
                request,
                Some(outcome.version.clone()),
                if outcome.already_present {
                    ExtensionAuditResult::AlreadyPresent
                } else {
                    ExtensionAuditResult::Installed
                },
                Some(outcome.plan_hash.clone()),
            );
            Ok(outcome)
        }
        Err(e) => Err(failed(ctx, op, request, e)),
    }
}

async fn install_locked(
    client: &Client,
    req: &Checked,
    expected_hash: &str,
) -> Result<ExtensionOutcome, MigrateError> {
    let (facts, verdict) = decide(client, req).await?;
    let hash = plan_hash(req, &facts, &verdict);
    if hash != expected_hash {
        return Err(MigrateError::ExtensionPlanStale {
            extension: req.name.to_owned(),
        });
    }
    if verdict.already_present {
        return Ok(ExtensionOutcome {
            extension: req.name.to_owned(),
            version: verdict.version,
            schema: req.schema.clone(),
            already_present: true,
            plan_hash: hash,
        });
    }
    client
        .batch_execute(SESSION_SETUP)
        .await
        .map_err(|e| db(&e))?;
    // The one place the crate creates an extension. Both names are validated plain identifiers
    // (the extension is the crate's own constant), and quoted anyway.
    let statement = format!(
        "CREATE EXTENSION {} SCHEMA {}",
        quote(req.name),
        quote(&req.schema)
    );
    if let Err(e) = client.batch_execute(&statement).await {
        return Err(match e.code().map(|c| c.code()) {
            Some("42501") => MigrateError::ExtensionPrivilegeMissing {
                extension: req.name.to_owned(),
                statement: admin_statement(req.name, &req.schema),
            },
            Some("58P01") => MigrateError::ExtensionUnavailable {
                extension: req.name.to_owned(),
                reason: "the server does not offer it; install the server package first".to_owned(),
            },
            _ => db(&e),
        });
    }
    let names = [req.name.to_owned()];
    let after = read_extensions(client, &names).await?;
    let version = after
        .get(req.name)
        .and_then(|l| l.installed_version.clone())
        .filter(|v| version_at_least(v, &req.min_version))
        .ok_or_else(|| MigrateError::ExtensionVersion {
            extension: req.name.to_owned(),
            reason: "after the install the extension is missing or older than needed".to_owned(),
        })?;
    Ok(ExtensionOutcome {
        extension: req.name.to_owned(),
        version,
        schema: req.schema.clone(),
        already_present: false,
        plan_hash: hash,
    })
}

#[cfg(test)]
mod tests;
