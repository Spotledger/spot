//! The DDL of the BPM tables, and its indexes.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_bpm_case` | one row per Case: the DAG run's header (id, key, pin, status, version, times, taken edges) and its whole node map as `jsonb`, plus the two flags `list_runnable` needs |
//! | `spot_bpm_case_var` | the Case's variables as `jsonb`, in their own row so a variable change never rewrites the run row |
//! | `spot_bpm_node_outcome` | one row per node attempt: the engine attempt (state, output, branches, failure, times) and the node's wait (due/event/human) — the `(case, node, attempt)` idempotency key is the exactly-once guard |
//! | `spot_bpm_case.children` | the sub-process child links, as a `jsonb` map keyed by node |
//!
//! The tables are the store's own: they never touch, and are never touched by, the document
//! storage backend's tables (ADR-45: the store owns its schema and its SQL).
//!
//! # Indexes
//!
//! * `spot_bpm_case_key_idx` unique `(tenant, business_key)`: `find_by_key`, and the tenant-scoped
//!   business key a duplicate Case is refused by.
//! * `spot_bpm_case_ready_idx (case_id)` partial on running Cases with a ready node: `list_runnable`.
//! * `spot_bpm_node_outcome_idem_idx` unique `(case_id, node, attempt)`: the idempotency key. A
//!   second write of an attempt already recorded is the "already ran" guard.
//! * `spot_bpm_node_outcome_event_idx (tenant, event)` partial on waiting nodes: "Cases waiting on
//!   event X".
//! * `spot_bpm_node_outcome_due_idx (due)` partial on waiting nodes: "timers due".
//!
//! An earlier version created the tables without the `spot_` prefix (`bpm_case`, `bpm_case_var`,
//! `bpm_node_outcome`), which `spot_pg_migrate` refuses. [`prepare`] renames each old table, its
//! primary key and other constraints (including the PostgreSQL 18 NOT NULL constraints) and its
//! indexes in the one transaction that also creates the schema, keeping every row: nothing is
//! dropped or recreated, so a running Case, its variables and its recorded node outcomes (and the
//! exactly-once key that guards them) are in force at every instant.
//!
//! Every hot filter is covered; `tests/plans.rs` asserts from `EXPLAIN` that the two named
//! lookups use their index.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;
use tokio_postgres::Transaction;

use crate::error;

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x6270_6d70_6701;

/// The idempotent DDL. Every statement is `IF NOT EXISTS`.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_bpm_case (
    case_id      text PRIMARY KEY,
    tenant       text NOT NULL,
    business_key text NOT NULL,
    dag_key      text NOT NULL,
    dag_version  integer NOT NULL,
    content_hash text NOT NULL,
    status       text NOT NULL
        CHECK (status IN ('running', 'completed', 'failed', 'cancelled')),
    version      bigint NOT NULL CHECK (version >= 0),
    started_at   timestamptz NOT NULL,
    ended_at     timestamptz,
    initiator    text,
    nodes        jsonb NOT NULL,
    taken        jsonb NOT NULL DEFAULT '[]'::jsonb,
    children     jsonb NOT NULL DEFAULT '{}'::jsonb,
    has_ready    boolean NOT NULL DEFAULT false,
    has_children boolean NOT NULL DEFAULT false
);

CREATE UNIQUE INDEX IF NOT EXISTS spot_bpm_case_key_idx
    ON spot_bpm_case (tenant, business_key);

CREATE INDEX IF NOT EXISTS spot_bpm_case_ready_idx
    ON spot_bpm_case (case_id) WHERE status = 'running' AND has_ready;

CREATE TABLE IF NOT EXISTS spot_bpm_case_var (
    case_id   text PRIMARY KEY REFERENCES spot_bpm_case (case_id) ON DELETE CASCADE,
    tenant    text NOT NULL,
    variables jsonb NOT NULL
);

CREATE TABLE IF NOT EXISTS spot_bpm_node_outcome (
    case_id    text NOT NULL,
    tenant     text NOT NULL,
    node       text NOT NULL,
    attempt    integer NOT NULL CHECK (attempt >= 1),
    state      text NOT NULL
        CHECK (state IN ('started', 'waiting', 'retrying', 'done', 'failed')),
    output     jsonb,
    branches   jsonb NOT NULL DEFAULT '[]'::jsonb,
    failure    text,
    started_at timestamptz NOT NULL,
    not_before timestamptz,
    is_retry   boolean NOT NULL DEFAULT false,
    due        timestamptz,
    event      text,
    human      text,
    PRIMARY KEY (case_id, node, attempt)
);

CREATE INDEX IF NOT EXISTS spot_bpm_node_outcome_event_idx
    ON spot_bpm_node_outcome (tenant, event) WHERE event IS NOT NULL;

CREATE INDEX IF NOT EXISTS spot_bpm_node_outcome_due_idx
    ON spot_bpm_node_outcome (due) WHERE due IS NOT NULL;
"#;

/// One table the crate used to create without the prefix.
struct Renamed {
    /// The unprefixed name an earlier version created.
    old: &'static str,
    /// The prefixed name.
    new: &'static str,
    /// The columns (`name:type`) the old table must have to be recognised as this crate's own.
    columns: &'static [&'static str],
}

/// The three tables, parent first.
const RENAMED: [Renamed; 3] = [
    Renamed {
        old: "bpm_case",
        new: "spot_bpm_case",
        columns: &[
            "case_id:text",
            "tenant:text",
            "business_key:text",
            "dag_key:text",
            "dag_version:integer",
            "content_hash:text",
            "status:text",
            "version:bigint",
            "started_at:timestamp with time zone",
            "ended_at:timestamp with time zone",
            "initiator:text",
            "nodes:jsonb",
            "taken:jsonb",
            "children:jsonb",
            "has_ready:boolean",
            "has_children:boolean",
        ],
    },
    Renamed {
        old: "bpm_case_var",
        new: "spot_bpm_case_var",
        columns: &["case_id:text", "tenant:text", "variables:jsonb"],
    },
    Renamed {
        old: "bpm_node_outcome",
        new: "spot_bpm_node_outcome",
        columns: &[
            "case_id:text",
            "tenant:text",
            "node:text",
            "attempt:integer",
            "state:text",
            "output:jsonb",
            "branches:jsonb",
            "failure:text",
            "started_at:timestamp with time zone",
            "not_before:timestamp with time zone",
            "is_retry:boolean",
            "due:timestamp with time zone",
            "event:text",
            "human:text",
        ],
    },
];

/// A constant identifier as a quoted one (the names above are fixed ASCII; the escape is defensive).
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Whether `name` is a relation of the current schema (the schema `CREATE TABLE` writes to).
async fn exists(tx: &Transaction<'_>, name: &str) -> AppResult<bool> {
    let row = tx
        .query_one(
            "SELECT to_regclass(quote_ident(current_schema()) || '.' || quote_ident($1)) IS NOT NULL",
            &[&name],
        )
        .await
        .map_err(error::migration)?;
    Ok(row.get(0))
}

/// The names (sorted) a query returns in its first column.
async fn names(
    tx: &Transaction<'_>,
    sql: &str,
    args: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> AppResult<Vec<String>> {
    Ok(tx
        .query(sql, args)
        .await
        .map_err(error::migration)?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// Builds every `ALTER ... RENAME` the upgrade needs, checking each old/new pair first. Nothing is
/// executed here, so a refusal leaves the database exactly as it was.
async fn plan_renames(tx: &Transaction<'_>) -> AppResult<Vec<String>> {
    let mut statements = Vec::new();
    for table in &RENAMED {
        if !exists(tx, table.old).await? {
            continue;
        }
        if exists(tx, table.new).await? {
            return Err(error::migration_refused(format!(
                "both `{}` and `{}` exist; nothing was renamed or dropped, resolve it by hand",
                table.old, table.new
            )));
        }
        let mut expected: Vec<String> = table.columns.iter().map(|c| (*c).to_owned()).collect();
        expected.sort();
        let found = names(
            tx,
            "SELECT column_name::text || ':' || data_type::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 \
             ORDER BY column_name::text || ':' || data_type::text",
            &[&table.old],
        )
        .await?;
        if found != expected {
            return Err(error::migration_refused(format!(
                "the table `{}` does not have the columns of this crate's table, so it was not renamed to `{}`",
                table.old, table.new
            )));
        }
        let old_q = quote(table.old);
        statements.push(format!(
            "ALTER TABLE {old_q} RENAME TO {}",
            quote(table.new)
        ));
        let pattern = format!("{}\\_%", table.old.replace('_', "\\_"));
        let regclass = "to_regclass(quote_ident(current_schema()) || '.' || quote_ident($1))";
        // Constraints (the primary key, checks, the foreign key and the PostgreSQL 18 NOT NULL
        // constraints) carry the table name as a prefix; the index of a constraint follows it.
        let constraints = names(
            tx,
            &format!(
                "SELECT conname::text FROM pg_constraint \
                 WHERE conrelid = {regclass} AND conname LIKE $2 ORDER BY conname"
            ),
            &[&table.old, &pattern],
        )
        .await?;
        for name in constraints {
            let Some(rest) = name.strip_prefix(table.old) else {
                continue;
            };
            statements.push(format!(
                "ALTER TABLE {} RENAME CONSTRAINT {} TO {}",
                quote(table.new),
                quote(&name),
                quote(&format!("{}{rest}", table.new))
            ));
        }
        // The stand-alone indexes (not the ones a constraint owns).
        let indexes = names(
            tx,
            &format!(
                "SELECT c.relname::text FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
                 WHERE i.indrelid = {regclass} AND c.relname LIKE $2 \
                 AND NOT EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = i.indexrelid) \
                 ORDER BY c.relname"
            ),
            &[&table.old, &pattern],
        )
        .await?;
        for name in indexes {
            let Some(rest) = name.strip_prefix(table.old) else {
                continue;
            };
            let renamed = format!("{}{rest}", table.new);
            if exists(tx, &renamed).await? {
                return Err(error::migration_refused(format!(
                    "both the index `{name}` and `{renamed}` exist; nothing was renamed or dropped, resolve it by hand"
                )));
            }
            statements.push(format!(
                "ALTER INDEX {} RENAME TO {}",
                quote(&name),
                quote(&renamed)
            ));
        }
    }
    // A constraint-backed index whose new name a foreign relation already holds would fail its
    // rename; that rolls the whole transaction back, so the database is still untouched.
    Ok(statements)
}

/// Creates the tables and indexes if they are missing. Idempotent and safe to run from several
/// nodes at once (a transaction-scoped advisory lock serialises them).
///
/// A database that still holds the tables as `bpm_case`, `bpm_case_var` and `bpm_node_outcome` is
/// upgraded in the same transaction: the tables, their constraints and their indexes are renamed
/// and every row is kept. A second call finds only the new names and changes nothing.
///
/// # Errors
/// `bpm_pg.migration_failed` when the database cannot apply the DDL, when both the old and the new
/// name of a table (or an index) exist, or when an old-named table does not have this crate's
/// columns (nothing is touched). The messages carry names and a SQLSTATE, never a row value.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(error::migration_pool)?;
    let tx = conn.transaction().await.map_err(error::migration)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(error::migration)?;
    for statement in plan_renames(&tx).await? {
        tx.batch_execute(&statement)
            .await
            .map_err(error::migration)?;
    }
    tx.batch_execute(DDL).await.map_err(error::migration)?;
    tx.commit().await.map_err(error::migration)
}
