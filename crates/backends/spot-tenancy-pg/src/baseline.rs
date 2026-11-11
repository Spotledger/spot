//! The tenant baseline: planning and applying a [`Catalog`] to an empty (or partly migrated)
//! tenant schema, and the meta rows `spot-postgres` expects.
//!
//! `spot-pg-migrate` plans against the connection's `current_schema()`. The provisioner sets the
//! tenant schema in `search_path` before calling [`migrate`], so every unqualified name the plan
//! contains lands in the tenant's own schema; no statement ever names another tenant's schema.
//!
//! The execution is the same shape as `spot_pg_migrate::apply`, restricted to what a tenant
//! schema needs: each transaction step in one short transaction (so a crash rolls it back), each
//! outside step plainly, batched backfills, and a finalise that installs `spot_meta` and records
//! the catalogue version. A rerun reads the schema again and plans only what is left, so an
//! interrupted run resumes.

use std::collections::{BTreeMap, BTreeSet};

use spot_pg_migrate::{
    LiveColumn, LiveConstraint, LiveIndex, LiveSchema, LiveTable, Run, plan_with,
};
use spot_storage_contract::Catalog;
use tokio_postgres::Client;

/// Why a baseline migration failed. Fixed strings only: they become a port's `reason`.
pub(crate) type Reason = &'static str;

/// The progress table `spot-pg-migrate` uses for an interrupted required-column expansion. The
/// baseline is planned against a fresh schema, so it is normally empty; the table is created by
/// the planner's own statement and not read here.
const PROGRESS_TABLE: &str = "spot_migrate_progress";

/// Plans and applies `catalog` in the schema currently in `search_path`.
pub(crate) async fn migrate(client: &Client, catalog: &Catalog) -> Result<(), Reason> {
    let live = read_live(client).await?;
    let migration = plan_with(&live, catalog, &spot_pg_migrate::PlanOptions::default());
    if !migration.refused.is_empty() {
        return Err("the baseline migration refused a change");
    }
    for step in &migration.steps {
        match &step.run {
            Run::Tx(statements) => run_tx(client, statements).await?,
            Run::Outside(sql) => {
                client
                    .batch_execute(sql)
                    .await
                    .map_err(|_| "the baseline migration could not run a statement")?;
            }
            Run::Backfill(spec) => backfill(client, spec).await?,
            Run::SetNotNull(spec) => set_not_null(client, spec).await?,
            Run::Finalise => finalise(client, catalog).await?,
            // The schema planner never emits an extension requirement (the audited install
            // operation plans it), and the baseline never installs one; refuse rather than skip.
            Run::Extension { .. } => {
                return Err("the baseline migration cannot run an extension requirement");
            }
        }
    }
    // A plan can be empty while `spot_meta` is still missing (the first call on an empty schema
    // always has steps; this covers a schema whose only table is `spot_meta`).
    if migration.steps.is_empty() {
        finalise(client, catalog).await?;
    }
    Ok(())
}

async fn run_tx(client: &Client, statements: &[String]) -> Result<(), Reason> {
    client
        .batch_execute("BEGIN")
        .await
        .map_err(|_| "the baseline migration could not begin a transaction")?;
    for sql in statements {
        if client.batch_execute(sql).await.is_err() {
            let _ = client.batch_execute("ROLLBACK").await;
            return Err("the baseline migration could not run a statement");
        }
    }
    client
        .batch_execute("COMMIT")
        .await
        .map_err(|_| "the baseline migration could not commit")
}

async fn backfill(client: &Client, spec: &spot_pg_migrate::BackfillSpec) -> Result<(), Reason> {
    let table = quote(&spec.table)?;
    let column = quote(&spec.column)?;
    loop {
        let sql = format!(
            "UPDATE {table} SET {column} = ($1::text)::{} WHERE id IN \
             (SELECT id FROM {table} WHERE {column} IS NULL LIMIT 1000)",
            spec.ty
        );
        let done = client
            .execute(&sql, &[&spec.value])
            .await
            .map_err(|_| "the baseline migration could not backfill a column")?;
        if done == 0 {
            return Ok(());
        }
    }
}

async fn set_not_null(client: &Client, spec: &spot_pg_migrate::BackfillSpec) -> Result<(), Reason> {
    let table = quote(&spec.table)?;
    let column = quote(&spec.column)?;
    client
        .batch_execute("BEGIN")
        .await
        .map_err(|_| "the baseline migration could not begin a transaction")?;
    let run = async {
        client
            .execute(
                &format!(
                    "UPDATE {table} SET {column} = ($1::text)::{} WHERE {column} IS NULL",
                    spec.ty
                ),
                &[&spec.value],
            )
            .await?;
        client
            .batch_execute(&format!(
                "ALTER TABLE {table} ALTER COLUMN {column} SET NOT NULL"
            ))
            .await
    };
    match run.await {
        Ok(()) => client
            .batch_execute("COMMIT")
            .await
            .map_err(|_| "the baseline migration could not commit"),
        Err(_) => {
            let _ = client.batch_execute("ROLLBACK").await;
            Err("the baseline migration could not make a column required")
        }
    }
}

/// Installs `spot_meta` and records the catalogue's collation and version, exactly the bookkeeping
/// `spot-postgres::prepare` writes. Idempotent and monotonic in the version.
async fn finalise(client: &Client, catalog: &Catalog) -> Result<(), Reason> {
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS spot_meta \
             (key text COLLATE \"C\" PRIMARY KEY, value text NOT NULL)",
        )
        .await
        .map_err(|_| "the baseline migration could not create the meta table")?;
    client
        .execute(
            "INSERT INTO spot_meta (key, value) VALUES ('collation', $1) \
             ON CONFLICT (key) DO NOTHING",
            &[&catalog.collation.as_str()],
        )
        .await
        .map_err(|_| "the baseline migration could not record the collation")?;
    client
        .execute(
            "INSERT INTO spot_meta (key, value) VALUES ('catalog_version', $1) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value \
             WHERE spot_meta.value::bigint < EXCLUDED.value::bigint",
            &[&catalog.version.to_string()],
        )
        .await
        .map_err(|_| "the baseline migration could not record the catalogue version")?;
    Ok(())
}

/// Reads the schema currently in `search_path` as the plain data `spot-pg-migrate` plans from.
async fn read_live(client: &Client) -> Result<LiveSchema, Reason> {
    let mut live = LiveSchema {
        tables: BTreeMap::new(),
        indexes: BTreeMap::new(),
        constraints: BTreeMap::new(),
        collation: None,
        expanding: BTreeSet::new(),
    };

    let names: Vec<String> = client
        .query(
            "SELECT c.relname::text FROM pg_class c \
             WHERE c.relnamespace = to_regnamespace(current_schema())::oid AND c.relkind = 'r'",
            &[],
        )
        .await
        .map_err(|_| "the baseline migration could not read the schema")?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for name in &names {
        live.tables.insert(
            name.clone(),
            LiveTable {
                columns: BTreeMap::new(),
                has_rows: false,
            },
        );
    }

    let columns = client
        .query(
            "SELECT c.relname::text, a.attname::text, format_type(a.atttypid, a.atttypmod), \
                    a.attnotnull, \
                    (SELECT co.collname::text FROM pg_collation co \
                      WHERE co.oid = a.attcollation AND a.attcollation <> t.typcollation) \
             FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_type t ON t.oid = a.atttypid \
             WHERE c.relnamespace = to_regnamespace(current_schema())::oid \
               AND c.relkind = 'r' AND a.attnum > 0 AND NOT a.attisdropped",
            &[],
        )
        .await
        .map_err(|_| "the baseline migration could not read the schema")?;
    for row in &columns {
        let table: String = row.get(0);
        let column = LiveColumn {
            name: row.get(1),
            ty: row.get(2),
            not_null: row.get(3),
            collation: row.get(4),
        };
        if let Some(t) = live.tables.get_mut(&table) {
            t.columns.insert(column.name.clone(), column);
        }
    }
    for name in &names {
        if name.starts_with("spot_") {
            continue;
        }
        let has_rows = client
            .query_one(
                &format!("SELECT EXISTS (SELECT 1 FROM {})", quote(name)?),
                &[],
            )
            .await
            .map_err(|_| "the baseline migration could not read the schema")?
            .get(0);
        if let Some(t) = live.tables.get_mut(name) {
            t.has_rows = has_rows;
        }
    }

    let indexes = client
        .query(
            "SELECT c.relname::text, t.relname::text, i.indisunique, i.indisvalid, \
                    COALESCE((SELECT array_agg(a.attname::text ORDER BY k.ord) \
                              FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                              JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum), \
                             ARRAY[]::text[]) \
             FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             JOIN pg_class t ON t.oid = i.indrelid \
             WHERE t.relnamespace = to_regnamespace(current_schema())::oid \
               AND NOT i.indisprimary",
            &[],
        )
        .await
        .map_err(|_| "the baseline migration could not read the schema")?;
    for row in &indexes {
        let index = LiveIndex {
            name: row.get(0),
            table: row.get(1),
            unique: row.get(2),
            valid: row.get(3),
            columns: row.get(4),
        };
        live.indexes.insert(index.name.clone(), index);
    }

    let constraints = client
        .query(
            "SELECT con.conname::text, c.relname::text, con.contype::text, con.convalidated, \
                    pg_get_constraintdef(con.oid) \
             FROM pg_constraint con JOIN pg_class c ON c.oid = con.conrelid \
             WHERE c.relnamespace = to_regnamespace(current_schema())::oid \
               AND con.contype IN ('f', 'c')",
            &[],
        )
        .await
        .map_err(|_| "the baseline migration could not read the schema")?;
    for row in &constraints {
        let kind: String = row.get(2);
        let constraint = LiveConstraint {
            name: row.get(0),
            table: row.get(1),
            kind: kind.chars().next().unwrap_or('c'),
            validated: row.get(3),
            definition: row.get(4),
        };
        live.constraints.insert(
            (constraint.table.clone(), constraint.name.clone()),
            constraint,
        );
    }

    if live.tables.contains_key("spot_meta") {
        let row = client
            .query_opt("SELECT value FROM spot_meta WHERE key = 'collation'", &[])
            .await
            .map_err(|_| "the baseline migration could not read the schema")?;
        live.collation = row.map(|row| row.get(0));
    }
    if live.tables.contains_key(PROGRESS_TABLE) {
        let rows = client
            .query(
                &format!(
                    "SELECT key FROM {} WHERE state = 'expanding'",
                    quote(PROGRESS_TABLE)?
                ),
                &[],
            )
            .await
            .map_err(|_| "the baseline migration could not read the schema")?;
        live.expanding = rows.iter().map(|row| row.get(0)).collect();
    }
    Ok(live)
}

/// Quotes a validated SQL identifier. Every caller passes a [`spot_storage_contract::Name`]
/// component or a fixed table name; anything else is refused before it can reach SQL.
fn quote(identifier: &str) -> Result<String, Reason> {
    if identifier.is_empty()
        || identifier.len() > 63
        || !identifier
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err("the baseline migration met an invalid identifier");
    }
    Ok(format!("\"{identifier}\""))
}
