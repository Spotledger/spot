//! The DDL of the DAG run tables, and its indexes.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_dag_run` | one row per run: the pinned DAG (key, version, content hash), the caller's business key, the run status, the start and end instants, the optimistic `version` and the taken edges as `jsonb` |
//! | `spot_dag_node_state` | one row per node of a run: its status, attempt count, output payload (`jsonb`), failure reason and start and end instants; a child of `spot_dag_run`, so it is removed with the run |
//!
//! Indexes (ADR-045: every hot filter is covered):
//!
//! - `spot_dag_run_key_uniq` unique on `key`: `find_by_key` and the "the business key is unused"
//!   rule of `create_run`.
//! - `spot_dag_run_ready_idx (id)` partial on `status = 'running'`: the outer scan of `list_ready`.
//! - `spot_dag_node_ready_idx (run_id, node)` partial on `status = 'ready'`: the ready nodes of a run.
//! - the `(run_id, node)` primary key: loading a run's nodes and updating one.
//!
//! Earlier versions created the tables as `dag_run` and `dag_node_state` (no `spot_` prefix, which
//! `spot_pg_migrate` refuses). [`prepare`] upgrades such a database in one transaction under the
//! advisory lock: it renames the tables, every constraint named after them (the primary keys, the
//! foreign key, the checks, `dag_run_key_uniq`, and on PostgreSQL 18 the named NOT NULL
//! constraints) and the two partial indexes, and keeps every row. Nothing is dropped or recreated,
//! so the unique constraint on the run key is in force at every instant.
//!
//! The DDL is created by [`prepare`], which is idempotent and serialised across nodes by a
//! transaction-scoped advisory lock. The tables are this crate's own: they never touch, and are
//! never touched by, the storage backend's document tables.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;
use tokio_postgres::Transaction;

use crate::error::{migration, migration_pool, migration_refused};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x4441_4750_0001;

/// The idempotent DDL. Every statement is `IF NOT EXISTS`.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_dag_run (
    id           text PRIMARY KEY,
    tenant       text NOT NULL,
    key          text NOT NULL,
    dag_key      text NOT NULL,
    dag_version  integer NOT NULL,
    content_hash text NOT NULL,
    status       text NOT NULL
        CHECK (status IN ('running', 'completed', 'failed', 'cancelled')),
    started_at   timestamptz NOT NULL,
    ended_at     timestamptz,
    version      bigint NOT NULL DEFAULT 0 CHECK (version >= 0),
    taken        jsonb NOT NULL DEFAULT '[]'::jsonb,
    CONSTRAINT spot_dag_run_key_uniq UNIQUE (key)
);

ALTER TABLE spot_dag_run ADD COLUMN IF NOT EXISTS tenant text;

CREATE TABLE IF NOT EXISTS spot_dag_node_state (
    run_id     text NOT NULL REFERENCES spot_dag_run (id) ON DELETE CASCADE,
    tenant     text NOT NULL,
    node       text NOT NULL,
    status     text NOT NULL
        CHECK (status IN ('pending', 'ready', 'running', 'done', 'skipped', 'failed')),
    attempts   integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    output     jsonb,
    failure    text,
    started_at timestamptz,
    ended_at   timestamptz,
    PRIMARY KEY (run_id, node)
);

ALTER TABLE spot_dag_node_state ADD COLUMN IF NOT EXISTS tenant text;

CREATE INDEX IF NOT EXISTS spot_dag_run_ready_idx
    ON spot_dag_run (id) WHERE status = 'running';

CREATE INDEX IF NOT EXISTS spot_dag_node_ready_idx
    ON spot_dag_node_state (run_id, node) WHERE status = 'ready';

CREATE UNIQUE INDEX IF NOT EXISTS spot_dag_run_tenant_id_uniq
    ON spot_dag_run (tenant, id);

CREATE UNIQUE INDEX IF NOT EXISTS spot_dag_node_state_tenant_run_node_uniq
    ON spot_dag_node_state (tenant, run_id, node);

CREATE UNIQUE INDEX IF NOT EXISTS spot_dag_run_tenant_key_uniq
    ON spot_dag_run (tenant, key);
"#;

/// One table of this crate: its old unprefixed name, its new name, the columns (`name:type`,
/// sorted) the old table must have to be recognised as this crate's own, and the standalone
/// indexes (old name, new name) it carries.
struct Table {
    old: &'static str,
    new: &'static str,
    columns: &'static [&'static str],
    indexes: &'static [(&'static str, &'static str)],
}

/// The crate's tables, parents first.
const TABLES: [Table; 2] = [
    Table {
        old: "dag_run",
        new: "spot_dag_run",
        columns: &[
            "content_hash:text",
            "dag_key:text",
            "dag_version:integer",
            "ended_at:timestamp with time zone",
            "id:text",
            "key:text",
            "started_at:timestamp with time zone",
            "status:text",
            "taken:jsonb",
            "version:bigint",
        ],
        indexes: &[("dag_run_ready_idx", "spot_dag_run_ready_idx")],
    },
    Table {
        old: "dag_node_state",
        new: "spot_dag_node_state",
        columns: &[
            "attempts:integer",
            "ended_at:timestamp with time zone",
            "failure:text",
            "node:text",
            "output:jsonb",
            "run_id:text",
            "started_at:timestamp with time zone",
            "status:text",
        ],
        indexes: &[("dag_node_ready_idx", "spot_dag_node_ready_idx")],
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

/// The names of the constraints of table `table` that start with `prefix_`, sorted.
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
             ORDER BY k.conname::text",
            &[&schema, &table, &pattern],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// Whether index `name` of `schema` is an index of table `table`.
async fn index_belongs_to(
    tx: &Transaction<'_>,
    schema: &str,
    name: &str,
    table: &str,
) -> Result<bool, tokio_postgres::Error> {
    let row = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_index i \
             JOIN pg_class ic ON ic.oid = i.indexrelid \
             JOIN pg_class tc ON tc.oid = i.indrelid \
             JOIN pg_namespace n ON n.oid = tc.relnamespace \
             WHERE n.nspname = $1 AND ic.relname = $2 AND tc.relname = $3)",
            &[&schema, &name, &table],
        )
        .await?;
    Ok(row.get(0))
}

/// The `name:type` list (sorted) of the columns of `table` in `schema`.
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
             ORDER BY column_name::text",
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

/// One planned rename of a table and what is named after it.
struct Upgrade {
    table: &'static Table,
    constraints: Vec<(String, String)>,
    indexes: Vec<(&'static str, &'static str)>,
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
            .map_err(migration)?
        else {
            continue;
        };
        if relation_kind(tx, schema, table.new)
            .await
            .map_err(migration)?
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
                .map_err(migration)?
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
            .map_err(migration)?
        {
            if let Some(new_name) = renamed(&name, table.old, table.new) {
                targets.push(new_name.clone());
                constraints.push((name, new_name));
            }
        }
        let mut indexes = Vec::new();
        for &(old_index, new_index) in table.indexes {
            if relation_kind(tx, schema, old_index)
                .await
                .map_err(migration)?
                .is_none()
            {
                continue;
            }
            if relation_kind(tx, schema, new_index)
                .await
                .map_err(migration)?
                .is_some()
            {
                return Err(migration_refused(format!(
                    "both `{old_index}` and `{new_index}` exist; nothing was renamed or dropped, resolve it by hand"
                )));
            }
            if !index_belongs_to(tx, schema, old_index, table.old)
                .await
                .map_err(migration)?
            {
                return Err(migration_refused(format!(
                    "the index `{old_index}` is not an index of `{}`, so nothing was renamed",
                    table.old
                )));
            }
            targets.push(new_index.to_owned());
            indexes.push((old_index, new_index));
        }
        plan.push(Upgrade {
            table,
            constraints,
            indexes,
        });
    }
    for target in targets {
        // A constraint-backed index takes the constraint's name, so a taken relation name would
        // fail half way; refuse before the first rename.
        if relation_kind(tx, schema, &target)
            .await
            .map_err(migration)?
            .is_some()
        {
            return Err(migration_refused(format!(
                "the name `{target}` is already taken, so nothing was renamed"
            )));
        }
    }
    Ok(plan)
}

/// Renames the old tables, their constraints and their indexes, keeping every row. Runs inside
/// `prepare`'s transaction under its lock, so a failure leaves the old tables exactly as they were.
async fn rename_old_tables(tx: &Transaction<'_>) -> AppResult<()> {
    let schema: String = tx
        .query_one("SELECT current_schema()::text", &[])
        .await
        .map_err(migration)?
        .try_get(0)
        .map_err(migration)?;
    let plan = plan_upgrade(tx, &schema).await?;
    let q = quote(&schema);
    for upgrade in plan {
        let table = upgrade.table;
        let statement =
            |sql: String| async move { tx.batch_execute(&sql).await.map_err(migration) };
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
        for (old_index, new_index) in &upgrade.indexes {
            statement(format!(
                "ALTER INDEX {q}.{} RENAME TO {}",
                quote(old_index),
                quote(new_index)
            ))
            .await?;
        }
    }
    Ok(())
}

/// Creates the tables and indexes if they are missing. Idempotent and safe to run from several
/// nodes at once (a transaction-scoped advisory lock serialises them).
///
/// A database that still holds the tables as `dag_run` and `dag_node_state` is upgraded in the
/// same transaction: the tables, their constraints and their indexes are renamed and every row is
/// kept. A second call finds only the new names and changes nothing.
///
/// # Errors
/// `dag_pg.migration_failed` when the database cannot apply the DDL, when both the old and the
/// new name of a table exist, or when an old-named table is not this crate's (nothing is touched).
/// The messages carry names and a SQLSTATE, never a row value.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(migration_pool)?;
    let tx = conn.transaction().await.map_err(migration)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(migration)?;
    rename_old_tables(&tx).await?;
    tx.batch_execute(DDL).await.map_err(migration)?;

    // Determine the tenant scope of the connection; fail if it cannot be determined.
    let tenant: String = tx
        .query_one("SELECT current_database()::text", &[])
        .await
        .map_err(migration)?
        .try_get(0)
        .map_err(migration)?;
    if tenant.trim().is_empty() {
        return Err(migration_refused(
            "the tenant scope of the connection could not be determined".to_owned(),
        ));
    }

    // Backfill any rows that predate the tenant column from the connection's scope, then make
    // the column NOT NULL. Done only while the column is still nullable, so a second `prepare`
    // takes no table lock and rewrites nothing.
    for table in ["spot_dag_run", "spot_dag_node_state"] {
        let nullable: bool = tx
            .query_one(
                "SELECT is_nullable = 'YES' FROM information_schema.columns \
                 WHERE table_schema = current_schema() AND table_name = $1 \
                   AND column_name = 'tenant'",
                &[&table],
            )
            .await
            .map_err(migration)?
            .try_get(0)
            .map_err(migration)?;
        if !nullable {
            continue;
        }
        let sql = format!(
            "UPDATE {} SET tenant = $1 WHERE tenant IS NULL",
            quote(table)
        );
        tx.execute(&sql, &[&tenant]).await.map_err(migration)?;
        let sql = format!(
            "ALTER TABLE {} ALTER COLUMN tenant SET NOT NULL",
            quote(table)
        );
        tx.batch_execute(&sql).await.map_err(migration)?;
    }

    tx.commit().await.map_err(migration)
}
