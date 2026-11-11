//! The DDL of the quarantine tables, their triggers and indexes.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_quarantine_item` | one row per item: id (identity, so ids increase with insertion), sender, stream sequence, apply key (record name and version), the root document's entity and id, reason code, detail, first-seen (Unix microseconds), attempts and state |
//! | `spot_quarantine_payload` | one row per item, written with it: the record image JSON text, the topic and the conversation. **Write-once** |
//!
//! Triggers:
//!
//! - `spot_quarantine_payload_write_once` refuses `UPDATE` and `DELETE` of a payload row,
//!   `spot_quarantine_payload_no_truncate` refuses `TRUNCATE` of the table.
//! - `spot_quarantine_item_keep` refuses `DELETE` of an item, and `TRUNCATE` of the table is refused
//!   by `spot_quarantine_item_no_truncate`. A queue entry is closed (released, discarded), never
//!   removed, so the audit trail always has the item it names.
//!
//! - `spot_quarantine_item_closed_terminal` refuses any `UPDATE` of an item that is already
//!   `released` or `discarded`, and any change to the identity columns (id, sender, sequence,
//!   apply key, entity, record id, first-seen) of an open one, so a raw client cannot rewrite
//!   what happened to a closed item.
//!
//! Constraints: size caps (`spot_quarantine_item_sizes`: source 128, reason code 128, detail 4096
//! bytes; `spot_quarantine_payload_size`: payload 256 KiB, the same numbers as the `spot-quarantine`
//! constants) and `spot_quarantine_item_closure` (an open item has no closure, a closed one has both
//! who and when). They are added `NOT VALID` so an existing table with older rows still upgrades;
//! every new or changed row is checked. [`prepare`] then runs `VALIDATE CONSTRAINT` on all three, so
//! after it returns they are `convalidated` and also vouch for the rows that were already there; a
//! table holding a row that breaks one makes `prepare` fail rather than start on it.
//!
//! The state column: the store itself changes `state` (a retry, a release, a discard), so the role
//! it runs as can. Run the **consumer** (the sync intake that only inserts) as a different role
//! that has no `UPDATE` on `spot_quarantine_item.state`; [`check_state_grant`] reports which kind
//! of role a pool connects as, and a host prints [`StateGrant::warning`] at start-up.
//!
//! Indexes (ADR-045: every hot filter is covered):
//!
//! - `spot_quarantine_item_open_key` unique on `(source, record_name, record_version)` for open
//!   items: `find`, and it makes a second open item with the same apply key from the same
//!   sender impossible.
//! - `spot_quarantine_item_state_idx (state, id)`, `spot_quarantine_item_reason_idx (reason_code, id)`
//!   and `spot_quarantine_item_source_idx (source, id)`: the three `ItemFilter` filters, each in
//!   insertion order.
//! - the `id` primary key: ordered listing and lookups by id.
//!
//! Earlier versions created the tables as `quarantine_item` and `quarantine_payload` (no `spot_`
//! prefix, which `spot_pg_migrate` refuses). [`prepare`] upgrades such a database in one
//! transaction under the advisory lock: it renames the tables and everything named after them
//! (primary keys, the foreign key, the checks, the named NOT NULL constraints of PostgreSQL 18,
//! the identity sequence, the triggers and the standalone indexes) and keeps every row. Nothing
//! is dropped, recreated or copied, so the write-once payloads and the closed items survive
//! byte for byte. A database holding both names of a pair, an old-named table that is not this
//! crate's, or a taken target name is refused before anything is renamed.
//!
//! The DDL is created by [`prepare`], which is idempotent and serialised across nodes by a
//! transaction-scoped advisory lock.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;
use tokio_postgres::{GenericClient, Transaction};

use crate::error::{grant_check, migration, migration_pool, migration_refused};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x5155_4152_0001;

/// The idempotent DDL. Every statement can run again.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_quarantine_item (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source         text NOT NULL,
    seq            bigint NOT NULL CHECK (seq >= 0),
    record_name    text NOT NULL,
    record_version bigint NOT NULL CHECK (record_version >= 1),
    entity         text NOT NULL,
    record_id      text NOT NULL,
    reason_code    text NOT NULL,
    detail         text NOT NULL,
    first_seen     bigint NOT NULL,
    attempts       integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    state          text NOT NULL
        CHECK (state IN ('held', 'retrying', 'released', 'discarded'))
);

ALTER TABLE spot_quarantine_item ADD COLUMN IF NOT EXISTS closed_by text;
ALTER TABLE spot_quarantine_item ADD COLUMN IF NOT EXISTS closed_at bigint;

CREATE TABLE IF NOT EXISTS spot_quarantine_payload (
    item_id      bigint PRIMARY KEY REFERENCES spot_quarantine_item (id),
    payload      text NOT NULL,
    topic        text,
    conversation text
);

DO $do$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                   WHERE conname = 'spot_quarantine_item_sizes'
                     AND conrelid = 'spot_quarantine_item'::regclass) THEN
        ALTER TABLE spot_quarantine_item ADD CONSTRAINT spot_quarantine_item_sizes CHECK (
            octet_length(source) <= 128
            AND octet_length(reason_code) <= 128
            AND octet_length(detail) <= 4096) NOT VALID;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                   WHERE conname = 'spot_quarantine_item_closure'
                     AND conrelid = 'spot_quarantine_item'::regclass) THEN
        ALTER TABLE spot_quarantine_item ADD CONSTRAINT spot_quarantine_item_closure CHECK (
            (state IN ('held', 'retrying') AND closed_by IS NULL AND closed_at IS NULL)
            OR (state IN ('released', 'discarded')
                AND closed_by IS NOT NULL AND closed_at IS NOT NULL)) NOT VALID;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                   WHERE conname = 'spot_quarantine_payload_size'
                     AND conrelid = 'spot_quarantine_payload'::regclass) THEN
        ALTER TABLE spot_quarantine_payload ADD CONSTRAINT spot_quarantine_payload_size CHECK (
            octet_length(payload) <= 262144) NOT VALID;
    END IF;
END
$do$;

CREATE UNIQUE INDEX IF NOT EXISTS spot_quarantine_item_open_key
    ON spot_quarantine_item (source, record_name, record_version)
    WHERE state IN ('held', 'retrying');

CREATE INDEX IF NOT EXISTS spot_quarantine_item_state_idx ON spot_quarantine_item (state, id);
CREATE INDEX IF NOT EXISTS spot_quarantine_item_reason_idx ON spot_quarantine_item (reason_code, id);
CREATE INDEX IF NOT EXISTS spot_quarantine_item_source_idx ON spot_quarantine_item (source, id);

CREATE OR REPLACE FUNCTION quarantine_refuse_change() RETURNS trigger
LANGUAGE plpgsql AS $fn$
BEGIN
    RAISE EXCEPTION 'quarantine rows are write-once: % of % is refused', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'integrity_constraint_violation';
END;
$fn$;

CREATE OR REPLACE FUNCTION quarantine_item_guard() RETURNS trigger
LANGUAGE plpgsql AS $fn$
BEGIN
    IF OLD.state IN ('released', 'discarded') THEN
        RAISE EXCEPTION 'quarantine item % is closed and cannot change', OLD.id
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.source IS DISTINCT FROM OLD.source
       OR NEW.seq IS DISTINCT FROM OLD.seq
       OR NEW.record_name IS DISTINCT FROM OLD.record_name
       OR NEW.record_version IS DISTINCT FROM OLD.record_version
       OR NEW.entity IS DISTINCT FROM OLD.entity
       OR NEW.record_id IS DISTINCT FROM OLD.record_id
       OR NEW.first_seen IS DISTINCT FROM OLD.first_seen THEN
        RAISE EXCEPTION 'the identity of quarantine item % cannot change', OLD.id
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END;
$fn$;

DROP TRIGGER IF EXISTS spot_quarantine_item_closed_terminal ON spot_quarantine_item;
CREATE TRIGGER spot_quarantine_item_closed_terminal
    BEFORE UPDATE ON spot_quarantine_item
    FOR EACH ROW EXECUTE FUNCTION quarantine_item_guard();

DROP TRIGGER IF EXISTS spot_quarantine_payload_write_once ON spot_quarantine_payload;
CREATE TRIGGER spot_quarantine_payload_write_once
    BEFORE UPDATE OR DELETE ON spot_quarantine_payload
    FOR EACH ROW EXECUTE FUNCTION quarantine_refuse_change();

DROP TRIGGER IF EXISTS spot_quarantine_payload_no_truncate ON spot_quarantine_payload;
CREATE TRIGGER spot_quarantine_payload_no_truncate
    BEFORE TRUNCATE ON spot_quarantine_payload
    FOR EACH STATEMENT EXECUTE FUNCTION quarantine_refuse_change();

DROP TRIGGER IF EXISTS spot_quarantine_item_keep ON spot_quarantine_item;
CREATE TRIGGER spot_quarantine_item_keep
    BEFORE DELETE ON spot_quarantine_item
    FOR EACH ROW EXECUTE FUNCTION quarantine_refuse_change();

DROP TRIGGER IF EXISTS spot_quarantine_item_no_truncate ON spot_quarantine_item;
CREATE TRIGGER spot_quarantine_item_no_truncate
    BEFORE TRUNCATE ON spot_quarantine_item
    FOR EACH STATEMENT EXECUTE FUNCTION quarantine_refuse_change();
"#;

/// Validates the constraints the DDL adds `NOT VALID`. Each statement is a no-op on a
/// constraint that is already validated, so the whole batch is idempotent. A table that holds a
/// row breaking a constraint fails here, and `prepare` fails with it: the store does not start on
/// data it was told cannot exist.
const VALIDATE: &str = r#"
ALTER TABLE spot_quarantine_item VALIDATE CONSTRAINT spot_quarantine_item_sizes;
ALTER TABLE spot_quarantine_item VALIDATE CONSTRAINT spot_quarantine_item_closure;
ALTER TABLE spot_quarantine_payload VALIDATE CONSTRAINT spot_quarantine_payload_size;
"#;

/// One table of this crate: its old unprefixed name, its new name, the columns (`name:type`,
/// sorted) the old table must have, the columns it may additionally have (added later by the
/// DDL), and the standalone indexes (old name, new name) it carries.
struct Table {
    old: &'static str,
    new: &'static str,
    required: &'static [&'static str],
    optional: &'static [&'static str],
    indexes: &'static [(&'static str, &'static str)],
}

/// The crate's tables, parents first.
const TABLES: [Table; 2] = [
    Table {
        old: "quarantine_item",
        new: "spot_quarantine_item",
        required: &[
            "attempts:integer",
            "detail:text",
            "entity:text",
            "first_seen:bigint",
            "id:bigint",
            "reason_code:text",
            "record_id:text",
            "record_name:text",
            "record_version:bigint",
            "seq:bigint",
            "source:text",
            "state:text",
        ],
        optional: &["closed_at:bigint", "closed_by:text"],
        indexes: &[
            ("quarantine_item_open_key", "spot_quarantine_item_open_key"),
            (
                "quarantine_item_state_idx",
                "spot_quarantine_item_state_idx",
            ),
            (
                "quarantine_item_reason_idx",
                "spot_quarantine_item_reason_idx",
            ),
            (
                "quarantine_item_source_idx",
                "spot_quarantine_item_source_idx",
            ),
        ],
    },
    Table {
        old: "quarantine_payload",
        new: "spot_quarantine_payload",
        required: &[
            "conversation:text",
            "item_id:bigint",
            "payload:text",
            "topic:text",
        ],
        optional: &[],
        indexes: &[],
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

/// A `LIKE` pattern for names that start with `table` and an underscore.
fn prefix_pattern(table: &str) -> String {
    format!("{}\\_%", table.replace('_', "\\_"))
}

/// The names of the constraints of table `table` that start with `table_`, sorted.
async fn constraints_named_after(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    Ok(tx
        .query(
            "SELECT k.conname::text FROM pg_constraint k \
             JOIN pg_class c ON c.oid = k.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND k.conname LIKE $3 \
             ORDER BY k.conname::text",
            &[&schema, &table, &prefix_pattern(table)],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// The names of the user triggers of table `table` that start with `table_`, sorted.
async fn triggers_named_after(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    Ok(tx
        .query(
            "SELECT g.tgname::text FROM pg_trigger g \
             JOIN pg_class c ON c.oid = g.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE NOT g.tgisinternal AND n.nspname = $1 AND c.relname = $2 \
               AND g.tgname LIKE $3 \
             ORDER BY g.tgname::text",
            &[&schema, &table, &prefix_pattern(table)],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// The sequences owned by table `table` (identity or serial) that start with `table_`, sorted.
async fn sequences_named_after(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    Ok(tx
        .query(
            "SELECT s.relname::text FROM pg_class s \
             JOIN pg_depend d ON d.classid = 'pg_class'::regclass AND d.objid = s.oid \
              AND d.refclassid = 'pg_class'::regclass AND d.deptype IN ('a', 'i') \
             JOIN pg_class t ON t.oid = d.refobjid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE s.relkind = 'S' AND n.nspname = $1 AND t.relname = $2 \
               AND s.relname LIKE $3 \
             ORDER BY s.relname::text",
            &[&schema, &table, &prefix_pattern(table)],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// Whether `table` of `schema` already has a constraint or a user trigger called `name`.
async fn table_has_name(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
    name: &str,
) -> Result<bool, tokio_postgres::Error> {
    let row = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint k \
                 JOIN pg_class c ON c.oid = k.conrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 AND k.conname = $3) \
             OR EXISTS (SELECT 1 FROM pg_trigger g \
                 JOIN pg_class c ON c.oid = g.tgrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE NOT g.tgisinternal AND n.nspname = $1 AND c.relname = $2 \
                   AND g.tgname = $3)",
            &[&schema, &table, &name],
        )
        .await?;
    Ok(row.get(0))
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

/// Whether `found` holds every required column and nothing beyond the required and optional ones.
fn is_ours(found: &[String], table: &Table) -> bool {
    table.required.iter().all(|c| found.iter().any(|f| f == c))
        && found
            .iter()
            .all(|f| table.required.contains(&f.as_str()) || table.optional.contains(&f.as_str()))
}

/// One planned rename of a table and what is named after it: `(old, new)` pairs.
struct Upgrade {
    table: &'static Table,
    constraints: Vec<(String, String)>,
    sequences: Vec<(String, String)>,
    triggers: Vec<(String, String)>,
    indexes: Vec<(&'static str, &'static str)>,
}

/// Checks every old/new pair and returns what to rename, touching nothing. Refuses when both
/// names of a pair exist, when an old-named table is not this crate's, or when a new name is
/// already taken.
async fn plan_upgrade(tx: &Transaction<'_>, schema: &str) -> AppResult<Vec<Upgrade>> {
    let mut plan = Vec::new();
    // Relation-namespace targets (tables, indexes, sequences and, conservatively, constraints:
    // a constraint-backed index takes its constraint's name).
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
            && is_ours(
                &columns_of(tx, schema, table.old)
                    .await
                    .map_err(|e| migration(&e))?,
                table,
            );
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
        let mut sequences = Vec::new();
        for name in sequences_named_after(tx, schema, table.old)
            .await
            .map_err(|e| migration(&e))?
        {
            if let Some(new_name) = renamed(&name, table.old, table.new) {
                targets.push(new_name.clone());
                sequences.push((name, new_name));
            }
        }
        let mut triggers = Vec::new();
        for name in triggers_named_after(tx, schema, table.old)
            .await
            .map_err(|e| migration(&e))?
        {
            if let Some(new_name) = renamed(&name, table.old, table.new) {
                triggers.push((name, new_name));
            }
        }
        for (_, new_name) in constraints.iter().chain(&triggers) {
            if table_has_name(tx, schema, table.old, new_name)
                .await
                .map_err(|e| migration(&e))?
            {
                return Err(migration_refused(format!(
                    "the name `{new_name}` is already taken on `{}`, so nothing was renamed",
                    table.old
                )));
            }
        }
        let mut indexes = Vec::new();
        for &(old_index, new_index) in table.indexes {
            if relation_kind(tx, schema, old_index)
                .await
                .map_err(|e| migration(&e))?
                .is_none()
            {
                continue;
            }
            if relation_kind(tx, schema, new_index)
                .await
                .map_err(|e| migration(&e))?
                .is_some()
            {
                return Err(migration_refused(format!(
                    "both `{old_index}` and `{new_index}` exist; nothing was renamed or dropped, resolve it by hand"
                )));
            }
            if !index_belongs_to(tx, schema, old_index, table.old)
                .await
                .map_err(|e| migration(&e))?
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
            sequences,
            triggers,
            indexes,
        });
    }
    for target in targets {
        // A taken relation name would fail half way; refuse before the first rename.
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

/// Renames the old tables and everything named after them, keeping every row. Runs inside
/// `prepare`'s transaction under its lock, so a failure leaves the old tables exactly as they were.
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
        for (old_name, new_name) in &upgrade.sequences {
            statement(format!(
                "ALTER SEQUENCE {q}.{} RENAME TO {}",
                quote(old_name),
                quote(new_name)
            ))
            .await?;
        }
        for (old_name, new_name) in &upgrade.triggers {
            statement(format!(
                "ALTER TRIGGER {} ON {q}.{} RENAME TO {}",
                quote(old_name),
                quote(table.new),
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

/// Creates the tables, triggers and indexes if they are missing. Idempotent and safe to run from
/// several nodes at once (a transaction-scoped advisory lock serialises them).
///
/// A database that still holds the tables as `quarantine_item` and `quarantine_payload` is
/// upgraded in the same transaction: the tables and everything named after them are renamed and
/// every row is kept. A second call finds only the new names and changes nothing.
///
/// # Errors
/// `quarantine_pg.migration_failed` when the database cannot apply the DDL, when both the old and
/// the new name of a table exist, or when an old-named table is not this crate's (nothing is
/// touched). The messages carry names and a SQLSTATE, never a row value.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(|e| migration_pool(&e))?;
    let tx = conn.transaction().await.map_err(|e| migration(&e))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(|e| migration(&e))?;
    rename_old_tables(&tx).await?;
    tx.batch_execute(DDL).await.map_err(|e| migration(&e))?;
    tx.batch_execute(VALIDATE)
        .await
        .map_err(|e| migration(&e))?;
    tx.commit().await.map_err(|e| migration(&e))
}

/// What the role a connection runs as may do to `spot_quarantine_item.state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateGrant {
    /// The database role the connection runs as (`current_user`): a name, never a credential.
    pub role: String,
    /// Whether the role can `UPDATE` the `state` column (a table-level grant, a column-level
    /// grant, ownership or superuser all count).
    pub can_update_state: bool,
}

impl StateGrant {
    /// Whether the role cannot change `state`: the arrangement the README recommends for a role
    /// that only feeds the queue.
    #[must_use]
    pub fn is_all_clear(&self) -> bool {
        !self.can_update_state
    }

    /// A one-line warning for the host to print when the role can change `state`, or `None` when
    /// it cannot. The line names the role and the column, never a value.
    #[must_use]
    pub fn warning(&self) -> Option<String> {
        self.can_update_state.then(|| {
            format!(
                "quarantine: database role `{}` can UPDATE spot_quarantine_item.state, so anything \
                 running as it can release or discard held items outside the audited path; run the \
                 intake as a role without that grant (see the spot-quarantine-pg README)",
                self.role
            )
        })
    }
}

/// Reports whether the role `client` is connected as can `UPDATE` the state column of
/// `spot_quarantine_item`. Read-only; the table must exist ([`prepare`] first).
///
/// # Errors
/// `quarantine_pg.migration_failed` when the question cannot be asked (for example the table does
/// not exist); the message carries a SQLSTATE, never a row value.
pub async fn check_state_grant_on<C: GenericClient + Sync>(client: &C) -> AppResult<StateGrant> {
    let row = client
        .query_one(
            "SELECT current_user::text, \
                    has_column_privilege(current_user, 'spot_quarantine_item', 'state', 'UPDATE')",
            &[],
        )
        .await
        .map_err(|e| grant_check(&e))?;
    Ok(StateGrant {
        role: row.try_get(0).map_err(|e| grant_check(&e))?,
        can_update_state: row.try_get(1).map_err(|e| grant_check(&e))?,
    })
}

/// [`check_state_grant_on`] for a connection taken from `pool`.
///
/// # Errors
/// As [`check_state_grant_on`], or when no connection can be checked out.
pub async fn check_state_grant(pool: &Pool) -> AppResult<StateGrant> {
    let conn = pool.get().await.map_err(|e| migration_pool(&e))?;
    check_state_grant_on(&**conn).await
}
