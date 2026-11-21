//! The DDL of the installer registry tables.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_installer_site` | one row per site that ever set its lifecycle flag: `idle` or `installing` |
//! | `spot_installer_app` | the apps installed on a site: version, digest (hex), approved capabilities, applied patch ids, install order and dependencies |
//! | `spot_installer_journal` | the install journal of a site's current run, one row per step index |
//! | `spot_installer_owned_doctype` | the DocTypes an app owns on a site, read before an uninstall |
//!
//! Every table is keyed by `site` first: no statement in this crate runs without a `site`
//! predicate, so one site can never read or write another's rows. The journal and the owned
//! DocTypes are separate from the app row so that clearing a run's journal never touches the
//! registry.
//!
//! Earlier versions created the tables as `installer_site`, `installer_app`, `installer_journal`
//! and `installer_owned_doctype` (no `spot_` prefix, which `spot_pg_migrate` refuses). [`prepare`]
//! upgrades such a database in one transaction under the advisory lock: it renames the tables and
//! every constraint named after them (the primary keys, the checks and, on PostgreSQL 18, the
//! named NOT NULL constraints) and keeps every row. Nothing is dropped or recreated, so no row
//! and no ownership record is ever lost, and a refusal leaves the old tables exactly as they were.
//!
//! The DDL is created by [`prepare`], which is idempotent and serialised across nodes by a
//! transaction-scoped advisory lock.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;
use tokio_postgres::Transaction;

use crate::error::{migration, migration_pool, migration_refused};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x4953_5450_0001;

/// The idempotent DDL. Every statement is `IF NOT EXISTS`.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_installer_site (
    site      text PRIMARY KEY,
    lifecycle text NOT NULL CHECK (lifecycle IN ('idle', 'installing'))
);

CREATE TABLE IF NOT EXISTS spot_installer_app (
    site            text NOT NULL,
    app             text NOT NULL,
    version         text NOT NULL,
    digest          text NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
    approved        jsonb NOT NULL,
    applied_patches jsonb NOT NULL,
    ord             integer NOT NULL CHECK (ord >= 0),
    depends         jsonb NOT NULL,
    PRIMARY KEY (site, app)
);

CREATE TABLE IF NOT EXISTS spot_installer_journal (
    site       text NOT NULL,
    step_index integer NOT NULL CHECK (step_index >= 0),
    plan_hash  text NOT NULL,
    kind       text NOT NULL
        CHECK (kind IN ('snapshot', 'patch', 'fixture', 'remove_fixture',
                        'commit', 'load', 'unload', 'smoke')),
    name       text NOT NULL,
    state      text NOT NULL CHECK (state IN ('started', 'done', 'undone')),
    started_at bigint NOT NULL,
    undo       jsonb,
    PRIMARY KEY (site, step_index)
);

CREATE TABLE IF NOT EXISTS spot_installer_owned_doctype (
    site    text NOT NULL,
    app     text NOT NULL,
    doctype text NOT NULL,
    PRIMARY KEY (site, app, doctype)
);
"#;

/// One table of this crate: its old unprefixed name, its new name, and the columns (`name:type`,
/// sorted bytewise) the old table must have to be recognised as this crate's own.
struct Table {
    old: &'static str,
    new: &'static str,
    columns: &'static [&'static str],
}

/// The crate's tables. None references another, and the crate creates no standalone index,
/// sequence or trigger.
const TABLES: [Table; 4] = [
    Table {
        old: "installer_site",
        new: "spot_installer_site",
        columns: &["lifecycle:text", "site:text"],
    },
    Table {
        old: "installer_app",
        new: "spot_installer_app",
        columns: &[
            "app:text",
            "applied_patches:jsonb",
            "approved:jsonb",
            "depends:jsonb",
            "digest:text",
            "ord:integer",
            "site:text",
            "version:text",
        ],
    },
    Table {
        old: "installer_journal",
        new: "spot_installer_journal",
        columns: &[
            "kind:text",
            "name:text",
            "plan_hash:text",
            "site:text",
            "started_at:bigint",
            "state:text",
            "step_index:integer",
            "undo:jsonb",
        ],
    },
    Table {
        old: "installer_owned_doctype",
        new: "spot_installer_owned_doctype",
        columns: &["app:text", "doctype:text", "site:text"],
    },
];

/// A constant identifier as a quoted one (the names above are fixed ASCII; the escape is defensive).
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The `relkind` of relation `name` in `schema`, or `None` when there is no such relation.
async fn relation_kind(
    tx: &Transaction<'_>,
    schema: &str,
    name: &str,
) -> Result<Option<String>, tokio_postgres::Error> {
    let row = tx
        .query_opt(
            "SELECT c.relkind::text FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2",
            &[&schema, &name],
        )
        .await?;
    Ok(row.map(|r| r.get(0)))
}

/// The names of the constraints of table `table` that start with `<table>_`, bytewise sorted.
async fn constraints_named_after(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    let pattern = format!("{}\\_%", table.replace('_', "\\_"));
    Ok(tx
        .query(
            "SELECT k.conname::text FROM pg_constraint k \
             JOIN pg_class c ON c.oid = k.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND k.conname LIKE $3 \
             ORDER BY k.conname::text COLLATE \"C\"",
            &[&schema, &table, &pattern],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// The `name:type` list (bytewise sorted) of the columns of `table` in `schema`.
async fn columns_of(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    Ok(tx
        .query(
            "SELECT column_name::text || ':' || data_type::text \
             FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 \
             ORDER BY column_name::text COLLATE \"C\"",
            &[&schema, &table],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// `new` for `old` when `name` starts with `old`.
fn renamed(name: &str, old: &str, new: &str) -> Option<String> {
    name.strip_prefix(old).map(|rest| format!("{new}{rest}"))
}

/// One planned rename of a table and the constraints named after it.
struct Upgrade {
    table: &'static Table,
    constraints: Vec<(String, String)>,
}

/// Checks every old/new pair and returns what to rename, touching nothing. Refuses when both
/// names of a pair exist, when an old-named table is not this crate's, or when a new name is
/// already taken.
async fn plan_upgrade(tx: &Transaction<'_>, schema: &str) -> AppResult<Vec<Upgrade>> {
    let mut plan = Vec::new();
    let mut targets: Vec<String> = Vec::new();
    for table in &TABLES {
        let Some(kind) = relation_kind(tx, schema, table.old)
            .await
            .map_err(|e| migration(&e))?
        else {
            continue;
        };
        if relation_kind(tx, schema, table.new)
            .await
            .map_err(|e| migration(&e))?
            .is_some()
        {
            return Err(migration_refused(format!(
                "both `{}` and `{}` exist; nothing was renamed or dropped, resolve it by hand",
                table.old, table.new
            )));
        }
        let ours = kind == "r"
            && columns_of(tx, schema, table.old)
                .await
                .map_err(|e| migration(&e))?
                .iter()
                .map(String::as_str)
                .eq(table.columns.iter().copied());
        if !ours {
            return Err(migration_refused(format!(
                "the table `{}` is not the table of this crate, so it was not renamed to `{}`",
                table.old, table.new
            )));
        }
        targets.push(table.new.to_owned());
        let mut constraints = Vec::new();
        for name in constraints_named_after(tx, schema, table.old)
            .await
            .map_err(|e| migration(&e))?
        {
            if let Some(new_name) = renamed(&name, table.old, table.new) {
                targets.push(new_name.clone());
                constraints.push((name, new_name));
            }
        }
        plan.push(Upgrade { table, constraints });
    }
    for target in targets {
        // A constraint-backed index takes the constraint's name, so a taken relation name would
        // fail half way; refuse before the first rename.
        if relation_kind(tx, schema, &target)
            .await
            .map_err(|e| migration(&e))?
            .is_some()
        {
            return Err(migration_refused(format!(
                "the name `{target}` is already taken, so nothing was renamed"
            )));
        }
    }
    Ok(plan)
}

/// Renames the old tables and their constraints, keeping every row. Runs inside `prepare`'s
/// transaction under its lock, so a failure leaves the old tables exactly as they were.
async fn rename_old_tables(tx: &Transaction<'_>) -> AppResult<()> {
    let schema: String = tx
        .query_one("SELECT current_schema()::text", &[])
        .await
        .map_err(|e| migration(&e))?
        .try_get(0)
        .map_err(|e| migration(&e))?;
    let plan = plan_upgrade(tx, &schema).await?;
    let q = quote(&schema);
    for upgrade in plan {
        let table = upgrade.table;
        let statement =
            |sql: String| async move { tx.batch_execute(&sql).await.map_err(|e| migration(&e)) };
        // Lock the table first: the rename waits for in-flight statements rather than racing them.
        statement(format!(
            "LOCK TABLE {q}.{} IN ACCESS EXCLUSIVE MODE",
            quote(table.old)
        ))
        .await?;
        statement(format!(
            "ALTER TABLE {q}.{} RENAME TO {}",
            quote(table.old),
            quote(table.new)
        ))
        .await?;
        for (old_name, new_name) in &upgrade.constraints {
            statement(format!(
                "ALTER TABLE {q}.{} RENAME CONSTRAINT {} TO {}",
                quote(table.new),
                quote(old_name),
                quote(new_name)
            ))
            .await?;
        }
    }
    Ok(())
}

/// Creates the tables if they are missing. Idempotent and safe to run from several nodes at once
/// (a transaction-scoped advisory lock serialises them).
///
/// A database that still holds the tables under their unprefixed names is upgraded in the same
/// transaction: the tables and their constraints are renamed and every row is kept. A second call
/// finds only the new names and changes nothing.
///
/// Run `prepare` on every node before it serves. The rename takes an exclusive lock on each table; a
/// statement of another node that was already waiting on it resolved the old name before the
/// commit and fails once with an undefined-table error (`installer_pg.*`), after which the
/// new name is used. Nothing is lost: the failed statement wrote no row.
///
/// # Errors
/// `installer_pg.migration_failed` when the database cannot apply the DDL, when both the old and
/// the new name of a table exist, or when an old-named table is not this crate's (nothing is
/// touched). The messages carry names and a SQLSTATE, never a row value.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(|error| migration_pool(&error))?;
    let tx = conn
        .transaction()
        .await
        .map_err(|error| migration(&error))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(|error| migration(&error))?;
    rename_old_tables(&tx).await?;
    tx.batch_execute(DDL)
        .await
        .map_err(|error| migration(&error))?;
    tx.commit().await.map_err(|error| migration(&error))
}
