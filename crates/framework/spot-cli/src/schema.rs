//! Planning and applying a site's schema change with the online migration engine.
//!
//! The default is always a dry run. A plan is applied only when the caller says so, a plan with
//! any refused (destructive) step is never applied, and an applied plan must be the reviewed one
//! when a plan hash is supplied.
//!
//! # The platform's non-document tables (WP-PRD-01.T11)
//!
//! The document tables come from the metadata catalogue. The tables the engine's participants
//! write to are not documents — `spot_graph_edge` and the drift tables edge sync keeps, and (when
//! their owning crates expose the DDL) the audit chain and the mail sent log. They are created
//! through `spot-pg-migrate`'s extra-table hook ([`spot_pg_migrate::ExtraTable`]) in the plan of
//! `spot migrate` and of the start-up schema check, so they are planned, reported, locked and
//! applied like every other step instead of appearing through a `prepare()` side door.
//!
//! The DDL is the owning crate's, never this module's: [`extra_tables`] splits the `DDL` string
//! each crate publishes into one statement per entry, and `spot_pg_migrate` refuses an entry whose
//! statement does not create the object the entry names, so a drift between the two is a refusal
//! and not a silently missing table.
use crate::config::{Loaded, Site};
use crate::{Error, Result, error};
use spot_meta::EffectiveMeta;
use spot_pg_migrate::{ApplyOptions, ApplyReport, ExtraTable, MigrationPlan, PlanOptions, Risk};
use spot_postgres::PgBackend;
use spot_storage_contract::{Catalog, Collation};
use std::fmt::Write;

/// The storage catalogue of `meta`, compiled the way the serving engine compiles it.
pub fn catalog(meta: &EffectiveMeta) -> Result<Catalog> {
    let collation = Collation::new("und").map_err(error)?;
    meta.catalog(collation).map_err(error)
}

/// A pooled backend on the site's database. Nothing is created or changed by connecting.
pub async fn backend(loaded: &Loaded, site: &Site) -> Result<PgBackend> {
    let mut pg = loaded.admin.clone().with_dbname(&site.database);
    pg.pool_max_size = loaded.common.postgres.pool_max_size;
    PgBackend::connect(pg).await.map_err(error)
}

/// The non-document tables the platform's own crates own, one entry per statement of their
/// published DDL, in dependency order (a table before the index built on it).
///
/// # Errors
///
/// [`Error`] when an owning crate's DDL holds a statement that is not a table, index or unique
/// index creation — `spot_pg_migrate` would refuse the whole plan, and a site refusing to plan is
/// better than one whose tables silently never appear.
pub fn extra_tables() -> Result<Vec<ExtraTable>> {
    let mut tables = Vec::new();
    // Edge sync's DDL is one statement per object, all of an accepted kind: a statement this
    // hook cannot carry is a drift between the two crates and refuses the plan.
    for statement in statements(spot_edge_pg::schema::DDL) {
        let object = created_object(statement).ok_or_else(|| {
            Error(format!(
                "spot-edge-pg: the published DDL holds a statement that is not a \
                 create-table or create-index statement with an if-not-exists guard, so the \
                 schema plan cannot carry it: {}",
                first_line(statement)
            ))
        })?;
        tables.push(
            ExtraTable::new(object, vec![statement.to_owned()])
                .map_err(|e| Error(e.reason.clone()))?,
        );
    }
    // The mail sent log's DDL also sets a column default and adds a check constraint through
    // `ALTER TABLE` and a `DO` block, which the hook refuses by design (it carries only idempotent
    // object creation). Its table and index are planned here; the default and the constraint are
    // added by the crate's own `prepare`, which the host runs at start-up when the durable sent
    // log is enabled. See the task's open questions.
    for statement in statements(spot_mail_queue_pg::schema::DDL) {
        let Some(object) = created_object(statement) else {
            continue;
        };
        tables.push(
            ExtraTable::new(object, vec![statement.to_owned()])
                .map_err(|e| Error(e.reason.clone()))?,
        );
    }
    // The hash-chained audit store's two tables (the identity audit trail, WP-PRD-01.T16). The
    // store publishes its DDL as `(marker table, statement)` pairs; the role, the grants and the
    // guard triggers cannot be a plan step, so `protect_audit` installs them after the plan.
    for (table, statement) in spot_audit_store::TABLE_DDL {
        tables.push(
            ExtraTable::new(table, vec![statement.to_owned()])
                .map_err(|e| Error(e.reason.clone()))?,
        );
    }
    // The condition store's two tables (WP-RTB-01.T2): the host refuses to start the conditions
    // service with one missing and says to run `spot migrate`; it never creates one itself.
    for statement in statements(spot_conditions_pg::schema::DDL) {
        let object = created_object(statement).ok_or_else(|| {
            Error(format!(
                "spot-conditions-pg: the published DDL holds a statement that is not a \
                 create-table or create-index statement with an if-not-exists guard, so the \
                 schema plan cannot carry it: {}",
                first_line(statement)
            ))
        })?;
        tables.push(
            ExtraTable::new(object, vec![statement.to_owned()])
                .map_err(|e| Error(e.reason.clone()))?,
        );
    }
    // The node role store's table (WP-RTB-03.T1, stored by WP-RTB-03.T5): one row per site, the
    // role recorded on first start; the host refuses to start with it missing.
    for statement in statements(spot_node_role_pg::schema::DDL) {
        let object = created_object(statement).ok_or_else(|| {
            Error(format!(
                "spot-node-role-pg: the published DDL holds a statement that is not a \
                 create-table or create-index statement with an if-not-exists guard, so the \
                 schema plan cannot carry it: {}",
                first_line(statement)
            ))
        })?;
        tables.push(
            ExtraTable::new(object, vec![statement.to_owned()])
                .map_err(|e| Error(e.reason.clone()))?,
        );
    }
    Ok(tables)
}

/// Installs the audit store's role, grants and guard triggers once its tables exist. Idempotent;
/// a no-op for a database that does not have the tables yet (a plan that left them out). Called
/// after every apply that may have created them, as `spot migrate` and provisioning do, because
/// between the plan and this call the chain is unguarded.
///
/// # Errors
///
/// [`Error`] when the database refuses the guard.
pub async fn protect_audit(backend: &PgBackend) -> Result<()> {
    let pool = backend.pool();
    let state = spot_audit_store::verify(&pool).await.map_err(error)?;
    if state.chain_table && state.checkpoint_table {
        spot_audit_store::install_guard(&pool)
            .await
            .map_err(error)?;
    }
    Ok(())
}

/// Why a serving site must refuse: the audit store's tables exist but are not fully guarded.
/// `None` when the store is complete or its tables are still pending (the pending-table check
/// says that).
///
/// # Errors
///
/// [`Error`] when the database cannot be asked.
pub async fn audit_protection_gap(backend: &PgBackend) -> Result<Option<String>> {
    let state = spot_audit_store::verify(&backend.pool())
        .await
        .map_err(error)?;
    if !(state.chain_table && state.checkpoint_table) || state.is_complete() {
        return Ok(None);
    }
    Ok(Some(state.missing().join(", ")))
}

/// The statements of a published DDL string, trimmed and without the empty tail.
fn statements(ddl: &str) -> impl Iterator<Item = &str> {
    ddl.split(';').map(str::trim).filter(|s| !s.is_empty())
}

/// The name a statement creates, when it is one of the three kinds the extra-table hook carries.
fn created_object(statement: &str) -> Option<&str> {
    let trimmed = statement.trim_start();
    let start = [
        concat!("create ", "table if not exists "),
        concat!("create unique index", " if not exists "),
        concat!("create index", " if not exists "),
    ]
    .iter()
    .find_map(|prefix| {
        let head = trimmed.get(..prefix.len())?;
        head.eq_ignore_ascii_case(prefix).then_some(prefix.len())
    })?;
    let rest = &trimmed[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// The first line of a statement, for an error that names what was refused without echoing SQL
/// and without reproducing a second statement.
fn first_line(statement: &str) -> &str {
    statement.lines().next().unwrap_or(statement).trim()
}

/// The plan options of a site: no backfill values, and the platform's non-document tables.
///
/// # Errors
///
/// [`Error`] when [`extra_tables`] refuses an owning crate's DDL.
pub fn plan_options() -> Result<PlanOptions> {
    Ok(PlanOptions {
        extra_tables: extra_tables()?,
        ..PlanOptions::default()
    })
}

/// Plans the change from the live schema to `wanted`, without side effects.
pub async fn plan(backend: &PgBackend, wanted: &Catalog) -> Result<MigrationPlan> {
    spot_pg_migrate::dry_run(backend, wanted, &PlanOptions::default())
        .await
        .map_err(error)
}

/// Plans the change to `wanted` **and** the platform's non-document tables, as `spot migrate`
/// does.
///
/// # Errors
///
/// [`Error`] when the plan cannot be built, or when an owning crate's DDL is not accepted by the
/// extra-table hook.
pub async fn plan_with_platform_tables(
    backend: &PgBackend,
    wanted: &Catalog,
) -> Result<MigrationPlan> {
    let options = plan_options()?;
    spot_pg_migrate::dry_run(backend, wanted, &options)
        .await
        .map_err(error)
}

/// The extra-table steps of the site's schema that are still to be applied, oldest first.
///
/// A serve start-up calls this and refuses with "run `spot migrate`" rather than creating a table
/// itself: the migrator is the one path that plans, reports and locks a schema change.
///
/// # Errors
///
/// [`Error`] when the plan cannot be built.
pub async fn pending_platform_tables(backend: &PgBackend, wanted: &Catalog) -> Result<Vec<String>> {
    let plan = plan_with_platform_tables(backend, wanted).await?;
    Ok(plan
        .steps
        .iter()
        .filter(|step| step.key.starts_with("extra_table:"))
        .map(|step| step.key.trim_start_matches("extra_table:").to_owned())
        .collect())
}

fn risk(risk: Risk) -> &'static str {
    match risk {
        Risk::Additive => "additive",
        Risk::NeedsBackfill => "needs-backfill",
        Risk::NeedsValidation => "needs-validation",
    }
}

/// The plan as text: every step with its risk class, every refusal with its reason, and the
/// hash a later `--plan-hash` binds to.
pub fn render(plan: &MigrationPlan) -> String {
    let mut out = String::new();
    if plan.is_empty() {
        out.push_str("schema is up to date; nothing to do\n");
        return out;
    }
    let _ = writeln!(out, "plan hash: {}", plan.plan_hash);
    for step in &plan.steps {
        let _ = writeln!(out, "  [{}] {}", risk(step.risk), step.sentence);
    }
    for refusal in &plan.refused {
        let target = match (refusal.entity.as_str(), &refusal.field) {
            ("", _) => String::new(),
            (entity, None) => format!("{entity}: "),
            (entity, Some(field)) => format!("{entity}.{field}: "),
        };
        let _ = writeln!(out, "  [refused] {target}{}", refusal.reason);
    }
    out
}

/// Turns a plan with refusals into the error the command exits with.
pub fn refusals(plan: &MigrationPlan) -> Result<()> {
    if plan.refused.is_empty() {
        Ok(())
    } else {
        Err(Error(format!(
            "{} change(s) refused; nothing was applied. Destructive changes belong to the \
             expand and contract migration path",
            plan.refused.len()
        )))
    }
}

/// Applies `plan` online. `expected` is the reviewed plan hash, when the operator gave one.
pub async fn apply(
    backend: &PgBackend,
    plan: &MigrationPlan,
    expected: Option<&str>,
) -> Result<ApplyReport> {
    refusals(plan)?;
    if let Some(expected) = expected
        && !expected.eq_ignore_ascii_case(&plan.plan_hash)
    {
        return Err(Error(format!(
            "the plan changed since it was reviewed (reviewed {expected}, current {}); \
             nothing was applied",
            plan.plan_hash
        )));
    }
    spot_pg_migrate::apply(backend, plan, ApplyOptions::default())
        .await
        .map_err(error)
}
