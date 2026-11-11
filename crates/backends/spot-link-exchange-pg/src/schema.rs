//! The tables of one tenant's schema, created idempotently.
//!
//! Every statement against these tables runs with the tenant's schema as the **only** entry of
//! `search_path` (set transaction-locally from a name derived from the tenant id, never from a
//! tenant-chosen string), so the SQL carries no schema name and a statement can only see the
//! tenant it was opened for.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_link_outbox` | the sender's entries: the message, its state, attempts, and the slot filler |
//! | `spot_link_outbox_event` | events for producers (applied, rejected, expired), numbered |
//! | `spot_link_inbox_conv` | each conversation's next expected sequence number; the row lock that serialises a conversation |
//! | `spot_link_inbox` | messages taken into sequence, with their progress |
//! | `spot_link_parked` | messages held behind a gap |
//! | `spot_link_seen` | every message id the inbox knows, and what became of it (the dedupe) |
//! | `spot_link_notice` | the delivery notice feed |
//! | `spot_link_notice_ack` | each consumer's acknowledged position |
//! | `spot_link_cursor` | the counters that number notices and producer events |
//!
//! # Upgrading the old names
//!
//! Earlier versions created the tables as `link_cursor`, `link_outbox`, `link_outbox_event`,
//! `link_inbox_conv`, `link_inbox`, `link_parked`, `link_seen`, `link_notice` and
//! `link_notice_ack` (no `spot_` prefix, which `spot_pg_migrate` refuses). [`prepare_tenant`]
//! upgrades such a schema in **one transaction** under the advisory lock: it checks every
//! old/new pair first, then renames the tables, every constraint named after them (primary keys,
//! unique constraints, checks and, on PostgreSQL 18, the named NOT NULL constraints) and the five
//! standalone indexes. Nothing is dropped, truncated or recreated, so every row (the replay memory
//! `spot_link_seen` and the unsent `spot_link_outbox` above all) is kept byte for byte.
//!
//! # Sizing and indexes
//!
//! Rows are small: a message is at most 256 KiB of payload (usually under 2 KiB), carried inside
//! the entry document, with the columns a query needs beside it. Expect about 1 KiB per
//! delivered message plus its payload, in `spot_link_outbox` on the sender and in `spot_link_inbox` and
//! `spot_link_seen` on the receiver, until the retention sweep empties the payload (the metadata row
//! stays for audit). A tenant exchanging a million messages a month holds about 1.5 GiB of rows
//! before the sweep and about 0.5 GiB after.
//!
//! * `spot_link_outbox (link, conversation, seq)` is unique: it is the conversation order and the
//!   guard that a sequence number is used once.
//! * `spot_link_outbox_live (link, conversation, seq) WHERE live`: the next-batch query reads only
//!   open messages (and unconfirmed fillers), so it stays small however much history remains.
//! * `spot_link_outbox_expiry (state_at) WHERE NOT live AND purged_at IS NULL`: the payload purge.
//! * `spot_link_inbox (link, conversation, seq)` is unique: order, and the guard against storing a
//!   slot twice. `spot_link_inbox_pending WHERE status = 'pending_apply'`: the apply path's queue.
//! * `spot_link_parked (link, conversation, seq)` is unique; `spot_link_parked_age (parked_at)`: the
//!   sweep of parked messages that expired.
//! * `spot_link_seen (id)` is the primary key: dedupe by message id.
//! * `spot_link_notice (cursor)` is the primary key: readers scan forward from their bookmark.
//!
//! # The gapless notice cursor
//!
//! A notice number comes from `UPDATE spot_link_cursor SET value = value + 1 WHERE name = 'notice'
//! RETURNING value`. The row lock is held to the end of the appending transaction, so numbers
//! are handed out one transaction at a time, in commit order, and a rolled-back transaction
//! returns its number (no gaps). A reader that has seen cursor `n` has seen every notice below
//! it, so a bookmark can never skip a late commit (the hazard ADR-053 describes for ids taken
//! from a sequence). The price is that two transactions appending notices to the same tenant
//! serialise for the length of the commit tail, which is why the append is the last statement of
//! the transaction.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;
use tokio_postgres::Transaction;

use crate::error::{migration, migration_pool, migration_refused};

/// The tables of one tenant's schema, in creation order.
pub const TENANT_TABLES: [&str; 9] = [
    "spot_link_cursor",
    "spot_link_outbox",
    "spot_link_outbox_event",
    "spot_link_inbox_conv",
    "spot_link_inbox",
    "spot_link_parked",
    "spot_link_seen",
    "spot_link_notice",
    "spot_link_notice_ack",
];

const TENANT_LOCK: i64 = 0x4c58_4348_4701;

/// The idempotent DDL for one tenant schema. Names are unqualified: it runs under the tenant's
/// `search_path`.
pub const TENANT_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_link_cursor (\
    name text PRIMARY KEY CHECK (name IN ('notice', 'event')), \
    value bigint NOT NULL CHECK (value >= 0)); \
INSERT INTO spot_link_cursor (name, value) VALUES ('notice', 0), ('event', 0) \
    ON CONFLICT (name) DO NOTHING; \
CREATE TABLE IF NOT EXISTS spot_link_outbox (\
    id uuid PRIMARY KEY, \
    link uuid NOT NULL, \
    conversation text NOT NULL, \
    seq bigint NOT NULL CHECK (seq >= 1), \
    state text NOT NULL CHECK (state IN ('queued', 'sent', 'acknowledged', 'applied', 'rejected', 'expired')), \
    state_at bigint NOT NULL CHECK (state_at >= 0), \
    live boolean NOT NULL, \
    filler_id uuid UNIQUE, \
    version bigint NOT NULL CHECK (version >= 1), \
    purged_at bigint, \
    entry jsonb NOT NULL, \
    UNIQUE (link, conversation, seq)); \
CREATE INDEX IF NOT EXISTS spot_link_outbox_live ON spot_link_outbox (link, conversation, seq) WHERE live; \
CREATE INDEX IF NOT EXISTS spot_link_outbox_expiry ON spot_link_outbox (state_at) \
    WHERE NOT live AND purged_at IS NULL; \
CREATE TABLE IF NOT EXISTS spot_link_outbox_event (\
    pos bigint PRIMARY KEY CHECK (pos >= 1), \
    event jsonb NOT NULL); \
CREATE TABLE IF NOT EXISTS spot_link_inbox_conv (\
    link uuid NOT NULL, \
    conversation text NOT NULL, \
    next_seq bigint NOT NULL CHECK (next_seq >= 1), \
    PRIMARY KEY (link, conversation)); \
CREATE TABLE IF NOT EXISTS spot_link_inbox (\
    link uuid NOT NULL, \
    id uuid NOT NULL, \
    conversation text NOT NULL, \
    seq bigint NOT NULL CHECK (seq >= 1), \
    status text NOT NULL CHECK (status IN ('pending_apply', 'applied', 'failed')), \
    received_at bigint NOT NULL CHECK (received_at >= 0), \
    purged_at bigint, \
    entry jsonb NOT NULL, \
    PRIMARY KEY (link, id), \
    UNIQUE (link, conversation, seq)); \
CREATE INDEX IF NOT EXISTS spot_link_inbox_pending ON spot_link_inbox (link, conversation, seq) \
    WHERE status = 'pending_apply'; \
CREATE INDEX IF NOT EXISTS spot_link_inbox_age ON spot_link_inbox (received_at) \
    WHERE status = 'applied' AND purged_at IS NULL; \
CREATE TABLE IF NOT EXISTS spot_link_parked (\
    link uuid NOT NULL, \
    id uuid NOT NULL, \
    conversation text NOT NULL, \
    seq bigint NOT NULL CHECK (seq >= 1), \
    parked_at bigint NOT NULL CHECK (parked_at >= 0), \
    entry jsonb NOT NULL, \
    PRIMARY KEY (link, id), \
    UNIQUE (link, conversation, seq)); \
CREATE INDEX IF NOT EXISTS spot_link_parked_age ON spot_link_parked (parked_at); \
CREATE TABLE IF NOT EXISTS spot_link_seen (\
    link uuid NOT NULL, \
    id uuid NOT NULL, \
    kind text NOT NULL CHECK (kind IN ('stored', 'parked', 'rejected')), \
    detail text NOT NULL, \
    PRIMARY KEY (link, id)); \
CREATE TABLE IF NOT EXISTS spot_link_notice (\
    cursor bigint PRIMARY KEY CHECK (cursor >= 1), \
    link uuid NOT NULL, \
    topic text NOT NULL, \
    message_id uuid NOT NULL, \
    conversation text NOT NULL, \
    source text NOT NULL, \
    received_at bigint NOT NULL CHECK (received_at >= 0)); \
CREATE TABLE IF NOT EXISTS spot_link_notice_ack (\
    consumer text PRIMARY KEY CHECK (length(consumer) BETWEEN 1 AND 128), \
    cursor bigint NOT NULL CHECK (cursor >= 0));";

/// One table of this crate: its old unprefixed name, its new name, the columns (`name:type`) the
/// old table must have to be recognised as this crate's own, the standalone indexes (old name, new
/// name) and the identity sequences (old name, new name) it carries.
struct Table {
    old: &'static str,
    new: &'static str,
    columns: &'static [&'static str],
    indexes: &'static [(&'static str, &'static str)],
    sequences: &'static [(&'static str, &'static str)],
}

/// The crate's tables, parents first.
const TABLES: [Table; 9] = [
    Table {
        old: "link_cursor",
        new: "spot_link_cursor",
        columns: &["name:text", "value:bigint"],
        indexes: &[],
        sequences: &[],
    },
    Table {
        old: "link_outbox",
        new: "spot_link_outbox",
        columns: &[
            "id:uuid",
            "link:uuid",
            "conversation:text",
            "seq:bigint",
            "state:text",
            "state_at:bigint",
            "live:boolean",
            "filler_id:uuid",
            "version:bigint",
            "purged_at:bigint",
            "entry:jsonb",
        ],
        indexes: &[
            ("link_outbox_live", "spot_link_outbox_live"),
            ("link_outbox_expiry", "spot_link_outbox_expiry"),
        ],
        sequences: &[],
    },
    Table {
        old: "link_outbox_event",
        new: "spot_link_outbox_event",
        columns: &["pos:bigint", "event:jsonb"],
        indexes: &[],
        sequences: &[],
    },
    Table {
        old: "link_inbox_conv",
        new: "spot_link_inbox_conv",
        columns: &["link:uuid", "conversation:text", "next_seq:bigint"],
        indexes: &[],
        sequences: &[],
    },
    Table {
        old: "link_inbox",
        new: "spot_link_inbox",
        columns: &[
            "id:uuid",
            "link:uuid",
            "conversation:text",
            "seq:bigint",
            "status:text",
            "received_at:bigint",
            "purged_at:bigint",
            "entry:jsonb",
        ],
        indexes: &[
            ("link_inbox_pending", "spot_link_inbox_pending"),
            ("link_inbox_age", "spot_link_inbox_age"),
        ],
        sequences: &[],
    },
    Table {
        old: "link_parked",
        new: "spot_link_parked",
        columns: &[
            "id:uuid",
            "link:uuid",
            "conversation:text",
            "seq:bigint",
            "parked_at:bigint",
            "entry:jsonb",
        ],
        indexes: &[("link_parked_age", "spot_link_parked_age")],
        sequences: &[],
    },
    Table {
        old: "link_seen",
        new: "spot_link_seen",
        columns: &["id:uuid", "link:uuid", "kind:text", "detail:text"],
        indexes: &[],
        sequences: &[],
    },
    Table {
        old: "link_notice",
        new: "spot_link_notice",
        columns: &[
            "cursor:bigint",
            "topic:text",
            "message_id:uuid",
            "conversation:text",
            "source:text",
            "received_at:bigint",
        ],
        indexes: &[],
        sequences: &[],
    },
    Table {
        old: "link_notice_ack",
        new: "spot_link_notice_ack",
        columns: &["consumer:text", "cursor:bigint"],
        indexes: &[],
        sequences: &[],
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

/// The names of the constraints of table `table` that start with `<table>_`, sorted.
async fn constraints_named_after(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    let pattern = format!("{}\\_%", table.replace('_', "\\_"));
    let mut names: Vec<String> = tx
        .query(
            "SELECT k.conname::text FROM pg_constraint k \
             JOIN pg_class c ON c.oid = k.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND k.conname LIKE $3",
            &[&schema, &table, &pattern],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    names.sort();
    Ok(names)
}

/// Whether table `table` already has a constraint called `name`.
async fn has_constraint(
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
             WHERE n.nspname = $1 AND c.relname = $2 AND k.conname = $3)",
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

/// The names of the sequences owned by (or the identity of) a column of table `table`, sorted.
async fn owned_sequences(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    let mut names: Vec<String> = tx
        .query(
            "SELECT s.relname::text FROM pg_depend d \
             JOIN pg_class s ON s.oid = d.objid AND s.relkind = 'S' \
             JOIN pg_class t ON t.oid = d.refobjid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass \
               AND d.refobjsubid > 0 AND d.deptype IN ('a', 'i') \
               AND n.nspname = $1 AND t.relname = $2",
            &[&schema, &table],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    names.sort();
    Ok(names)
}

/// The sorted `name:type` list of the columns of `table` in `schema`.
async fn columns_of(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    let mut columns: Vec<String> = tx
        .query(
            "SELECT column_name::text || ':' || data_type::text \
             FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    columns.sort();
    Ok(columns)
}

/// `new` for `old` when `name` starts with `old`.
fn renamed(name: &str, old: &str, new: &str) -> Option<String> {
    name.strip_prefix(old).map(|rest| format!("{new}{rest}"))
}

/// One planned rename of a table and what is named after it.
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
        let mut wanted: Vec<String> = table.columns.iter().map(|c| (*c).to_owned()).collect();
        wanted.sort();
        let ours =
            kind == "r" && columns_of(tx, schema, table.old).await.map_err(migration)? == wanted;
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
                if has_constraint(tx, schema, table.old, &new_name)
                    .await
                    .map_err(migration)?
                {
                    return Err(migration_refused(format!(
                        "both `{name}` and `{new_name}` exist on `{}`; nothing was renamed",
                        table.old
                    )));
                }
                // A constraint-backed index takes the constraint's name, so a taken relation
                // name would fail half way: it is checked with the other targets below.
                targets.push(new_name.clone());
                constraints.push((name, new_name));
            }
        }
        for &(old_index, new_index) in table.indexes {
            if relation_kind(tx, schema, old_index)
                .await
                .map_err(migration)?
                .is_none()
            {
                // Dropped by hand: the DDL below creates it under the new name.
                continue;
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
        }
        let owned = owned_sequences(tx, schema, table.old)
            .await
            .map_err(migration)?;
        let expected: Vec<&str> = table.sequences.iter().map(|&(old, _)| old).collect();
        if !owned
            .iter()
            .map(String::as_str)
            .eq(expected.iter().copied())
        {
            return Err(migration_refused(format!(
                "the sequences of `{}` are not the ones of this crate, so nothing was renamed",
                table.old
            )));
        }
        for &(_, new_sequence) in table.sequences {
            targets.push(new_sequence.to_owned());
        }
        plan.push(Upgrade { table, constraints });
    }
    for target in targets {
        // Tables, indexes and sequences share one name space in a schema.
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

/// Renames the old tables, their constraints, indexes and sequences, keeping every row. Runs
/// inside `prepare_tenant`'s transaction under its lock, so a failure leaves the old tables
/// exactly as they were.
async fn rename_old_tables(tx: &Transaction<'_>) -> AppResult<()> {
    let schema: Option<String> = tx
        .query_one("SELECT current_schema()::text", &[])
        .await
        .map_err(migration)?
        .try_get(0)
        .map_err(migration)?;
    let Some(schema) = schema else {
        return Err(migration_refused(
            "the tenant schema does not exist, so no table was created".to_owned(),
        ));
    };
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
        for &(old_index, new_index) in table.indexes {
            statement(format!(
                "ALTER INDEX {q}.{} RENAME TO {}",
                quote(old_index),
                quote(new_index)
            ))
            .await?;
        }
        for &(old_sequence, new_sequence) in table.sequences {
            statement(format!(
                "ALTER SEQUENCE {q}.{} RENAME TO {}",
                quote(old_sequence),
                quote(new_sequence)
            ))
            .await?;
        }
    }
    Ok(())
}

/// The tables whose dedupe key is `(link, id)`: a message id is only meaningful on the link that
/// carried it, so two links may use the same id. A schema created before this
/// key has a primary key on `id` alone; every row already names its link, so the upgrade is a
/// constraint swap and loses nothing.
const LINK_KEYED_TABLES: [&str; 3] = ["spot_link_inbox", "spot_link_parked", "spot_link_seen"];

/// The columns of the primary key of `table`, in key order, with the constraint's name; `None`
/// when the table has no primary key.
async fn primary_key(
    tx: &Transaction<'_>,
    table: &str,
) -> Result<Option<(String, String)>, tokio_postgres::Error> {
    let row = tx
        .query_opt(
            "SELECT k.conname::text, \
                    (SELECT string_agg(a.attname::text, ',' ORDER BY key.ord) \
                       FROM unnest(k.conkey) WITH ORDINALITY AS key(attnum, ord) \
                       JOIN pg_attribute a \
                         ON a.attrelid = k.conrelid AND a.attnum = key.attnum) \
             FROM pg_constraint k \
             JOIN pg_class c ON c.oid = k.conrelid \
             WHERE c.relname = $1 AND k.contype = 'p' \
               AND c.relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = current_schema())",
            &[&table],
        )
        .await?;
    Ok(row.map(|row| (row.get(0), row.get(1))))
}

/// Moves a schema created before the link-keyed dedupe onto `PRIMARY KEY (link, id)`, in the
/// caller's transaction. Runs once per table: a table that already has the key is left alone, so
/// `prepare` stays safe to repeat and needs no rebuild on every start.
async fn upgrade_link_keys(tx: &Transaction<'_>) -> AppResult<()> {
    for table in LINK_KEYED_TABLES {
        let Some((name, columns)) = primary_key(tx, table).await.map_err(migration)? else {
            return Err(migration_refused(format!(
                "the table `{table}` has no primary key, so nothing was upgraded"
            )));
        };
        if columns == "link,id" {
            continue;
        }
        tx.batch_execute(&format!(
            "ALTER TABLE {} DROP CONSTRAINT {}, ADD PRIMARY KEY (link, id)",
            quote(table),
            quote(&name)
        ))
        .await
        .map_err(migration)?;
    }
    upgrade_notice_link(tx).await
}

/// Gives a notice table created before the feed named its link the `link` column (nullable, then
/// filled, then `NOT NULL`), in the caller's transaction. Every old notice takes the link of its
/// inbox row (or, failing that, its dedupe row): the message id was unique on its own before the
/// key moved to `(link, id)`, so there is exactly one. A notice naming no known message is
/// refused loudly instead of being given a guessed link. Repeating it changes nothing.
async fn upgrade_notice_link(tx: &Transaction<'_>) -> AppResult<()> {
    tx.batch_execute(
        "ALTER TABLE spot_link_notice ADD COLUMN IF NOT EXISTS link uuid; \
         UPDATE spot_link_notice n SET link = i.link \
            FROM spot_link_inbox i WHERE n.link IS NULL AND i.id = n.message_id; \
         UPDATE spot_link_notice n SET link = s.link \
            FROM spot_link_seen s WHERE n.link IS NULL AND s.id = n.message_id;",
    )
    .await
    .map_err(migration)?;
    let orphans: i64 = tx
        .query_one(
            "SELECT count(*) FROM spot_link_notice WHERE link IS NULL",
            &[],
        )
        .await
        .map_err(migration)?
        .get(0);
    if orphans > 0 {
        return Err(migration_refused(format!(
            "{orphans} notice(s) name a message no inbox row knows, so nothing was upgraded"
        )));
    }
    tx.batch_execute("ALTER TABLE spot_link_notice ALTER COLUMN link SET NOT NULL")
        .await
        .map_err(migration)
}

/// Creates the exchange tables in the tenant's schema if they are missing, upgrading a schema that
/// still holds the old unprefixed names in the same transaction (every row is kept). The schema
/// itself is the tenant's, created at provisioning; a missing schema is a failure, never created
/// here.
///
/// # Errors
/// `link_exchange_pg.migration_failed` when the database cannot apply the DDL, when both the old
/// and the new name of a table exist, or when an old-named table is not this crate's (nothing is
/// touched). The messages carry names and a SQLSTATE, never a row value.
pub(crate) async fn prepare_tenant(pool: &Pool, schema: &str) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(migration_pool)?;
    let tx = conn.transaction().await.map_err(migration)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&TENANT_LOCK])
        .await
        .map_err(migration)?;
    tx.query_one("SELECT set_config('search_path', $1, true)", &[&schema])
        .await
        .map_err(migration)?;
    rename_old_tables(&tx).await?;
    tx.batch_execute(TENANT_DDL).await.map_err(migration)?;
    upgrade_link_keys(&tx).await?;
    tx.commit().await.map_err(migration)
}
