//! The DDL of the sent-log table and its indexes.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_mail_sent_log` | one row per `(site, message id)`: the site and message id, the instant the database recorded it (Unix microseconds), the accepted recipients, the rejected recipients and their reply codes, and nothing else |
//!
//! Constraints:
//!
//! - the primary key `(site, message_id)` — the point read of `is_sent` and `recorded`, and what
//!   makes a second receipt for one message impossible: the insert's `ON CONFLICT` clause names
//!   exactly this key.
//! - `spot_mail_sent_log_rejections_paired`: every rejected recipient has exactly one reply code, so a
//!   stored receipt can be rebuilt into a `SendReceipt` without guessing a code. The store always
//!   writes the two lists together, so the constraint refuses a row no code path produces — for
//!   instance one a later version of the store wrote and this one cannot read.
//!
//! Nothing here refuses a legitimate receipt. A message with a thousand recipients is unusual but
//! real, and a receipt the store refuses would be a message whose send is never recorded — which
//! is exactly the window this table exists to close. Only the pairing above is checked.
//!
//! Indexes (ADR-045: every hot filter is covered):
//!
//! - the `(site, message_id)` primary key: the `is_sent` probe and the `recorded` lookup.
//! - `spot_mail_sent_log_recorded_idx (site, recorded_at)`: the retention delete, which filters one
//!   site by `recorded_at` and takes the oldest rows first.
//!
//! `recorded_at` is stamped by the database's own clock, `(EXTRACT(EPOCH FROM now()) *
//! 1000000)::bigint` — exact numeric arithmetic, no floating point, one clock for every node.
//! The store never passes the column, so no process clock can end up in it.
//!
//! The DDL is created by [`prepare`], which is idempotent and serialised across nodes by a
//! transaction-scoped advisory lock, so several nodes may run it at start-up at the same time.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;

use crate::error::{migration, migration_both_tables, migration_pool};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x4D41_494C_0006;

/// The idempotent DDL. Every statement can run again.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_mail_sent_log (
    site          text    NOT NULL,
    message_id    text    NOT NULL,
    recorded_at   bigint  NOT NULL,
    accepted      text[]  NOT NULL,
    rejected      text[]  NOT NULL,
    rejected_code integer[] NOT NULL,
    PRIMARY KEY (site, message_id)
);

ALTER TABLE spot_mail_sent_log ALTER COLUMN recorded_at
    SET DEFAULT (EXTRACT(EPOCH FROM now()) * 1000000)::bigint;

DO $do$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                   WHERE conname = 'spot_mail_sent_log_rejections_paired'
                     AND conrelid = 'spot_mail_sent_log'::regclass) THEN
        ALTER TABLE spot_mail_sent_log ADD CONSTRAINT spot_mail_sent_log_rejections_paired CHECK (
            cardinality(rejected) = cardinality(rejected_code)) NOT VALID;
    END IF;
END
$do$;

CREATE INDEX IF NOT EXISTS spot_mail_sent_log_recorded_idx
    ON spot_mail_sent_log (site, recorded_at);
"#;

/// The table's name, with the `spot_` prefix `spot_pg_migrate` requires.
const NEW_TABLE: &str = "spot_mail_sent_log";
/// The prefix an earlier version of this crate did not use.
const PREFIX: &str = "spot_";

/// Which of the two table names exist in the connection's current schema (the tenant database
/// the pool points at): `(old, new)`.
const WHICH_TABLES: &str = "SELECT \
    EXISTS (SELECT 1 FROM pg_class c WHERE c.relkind = 'r' AND c.relname = $1 \
            AND c.relnamespace = to_regnamespace(current_schema())::oid), \
    EXISTS (SELECT 1 FROM pg_class c WHERE c.relkind = 'r' AND c.relname = $2 \
            AND c.relnamespace = to_regnamespace(current_schema())::oid)";

/// Renames the table and what is named after it, keeping every row. Runs inside `prepare`'s
/// transaction, so a failure anywhere leaves the old table exactly as it was. Each rename is
/// conditional on the old name existing in the current schema, and the names are quoted with
/// `%I`.
const RENAME_OLD: &str = r#"
DO $do$
DECLARE
    s       text := quote_ident(current_schema());
    new_t   text := 'spot_mail_sent_log';
    old_t   text := substr('spot_mail_sent_log', 6);
    new_idx text := 'spot_mail_sent_log_recorded_idx';
    old_idx text := substr('spot_mail_sent_log_recorded_idx', 6);
    c       record;
BEGIN
    EXECUTE format('ALTER TABLE %s.%I RENAME TO %I', s, old_t, new_t);
    -- the primary key, the pairing check and (PostgreSQL 18) the named NOT NULL constraints
    FOR c IN SELECT conname FROM pg_constraint
             WHERE conrelid = format('%s.%I', s, new_t)::regclass
               AND conname LIKE replace(old_t, '_', '\_') || '\_%' LOOP
        EXECUTE format('ALTER TABLE %s.%I RENAME CONSTRAINT %I TO %I',
                       s, new_t, c.conname, 'spot_' || c.conname);
    END LOOP;
    IF to_regclass(format('%s.%I', s, old_idx)) IS NOT NULL THEN
        EXECUTE format('ALTER INDEX %s.%I RENAME TO %I', s, old_idx, new_idx);
    END IF;
END
$do$;
"#;

/// Creates the table, its constraint and its indexes if they are missing. Idempotent and safe to
/// run from several nodes at once (a transaction-scoped advisory lock serialises them).
///
/// # Errors
/// `mail_queue_pg.migration_failed` when the database cannot apply the DDL, or when both the old
/// and the new table exist (neither is touched: an operator decides which holds the truth).
///
/// A database that still holds the table under its unprefixed name is upgraded in the
/// same transaction: the table, its primary key, its constraint and its index are renamed and
/// every row is kept, so a restart across the upgrade still does not resend a message. A second
/// call finds only the new name and changes nothing.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(|e| migration_pool(&e))?;
    let tx = conn.transaction().await.map_err(|e| migration(&e))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(|e| migration(&e))?;
    // The unprefixed name an earlier version created.
    let old_table = NEW_TABLE.strip_prefix(PREFIX).unwrap_or(NEW_TABLE);
    let row = tx
        .query_one(WHICH_TABLES, &[&old_table, &NEW_TABLE])
        .await
        .map_err(|e| migration(&e))?;
    let (old_exists, new_exists): (bool, bool) = (row.get(0), row.get(1));
    if old_exists && new_exists {
        return Err(migration_both_tables(old_table, NEW_TABLE));
    }
    if old_exists {
        tx.batch_execute(RENAME_OLD)
            .await
            .map_err(|e| migration(&e))?;
    }
    tx.batch_execute(DDL).await.map_err(|e| migration(&e))?;
    tx.commit().await.map_err(|e| migration(&e))
}
