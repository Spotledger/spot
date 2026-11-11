//! The tables: `spot_fix_journal` and `spot_fix_journal_head` in the control schema, created
//! idempotently, plus the role, grants and triggers that make the journal append-only.
//!
//! | Column | Holds |
//! | --- | --- |
//! | `seq` | the sequence number (`bigint`, primary key), assigned under the head row's lock |
//! | `fix` | the fix id |
//! | `at` | unix seconds, the argument of `append` (the crate never reads a clock) |
//! | `record` | the `JournalRecord` in its serde form (`jsonb`) |
//!
//! `spot_fix_journal_head` holds one row: the last sequence number handed out. Append-only is
//! enforced in three layers: the application role is granted `SELECT, INSERT` only, a trigger
//! raises on any `UPDATE`, `DELETE` or `TRUNCATE` (even for the owner), and the head row is the
//! only thing that is ever updated. No extension is needed. Statements name tables with their
//! schema, so nothing depends on, or changes, the session's `search_path`.

use std::time::Duration;

use spot_pg_bridge::Bridge;

use crate::error::{SetupError, cause};

/// The control schema the tables live in (shared with the tenant registry).
pub const CONTROL_SCHEMA: &str = "spot_control";
/// The journal table.
pub const JOURNAL_TABLE: &str = "spot_fix_journal";
/// The head table.
pub const HEAD_TABLE: &str = "spot_fix_journal_head";
/// The role a host connects the journal as. It may read and append, never rewrite.
pub const APPLICATION_ROLE: &str = "spot_fix_journal_app";

/// Serialises concurrent schema preparation (transaction-scoped advisory lock).
const SCHEMA_LOCK: i64 = 0x0046_4958_4a52_4e01;

/// The idempotent DDL. Every name starts with `spot_`; the constraints are named.
pub const DDL: &str = "\
CREATE SCHEMA IF NOT EXISTS spot_control; \
CREATE TABLE IF NOT EXISTS spot_control.spot_fix_journal (\
    seq bigint NOT NULL, \
    fix text NOT NULL, \
    at bigint NOT NULL, \
    record jsonb NOT NULL, \
    CONSTRAINT spot_fix_journal_pkey PRIMARY KEY (seq), \
    CONSTRAINT spot_fix_journal_seq_check CHECK (seq >= 1), \
    CONSTRAINT spot_fix_journal_fix_check CHECK (char_length(fix) BETWEEN 1 AND 128)); \
CREATE INDEX IF NOT EXISTS spot_fix_journal_fix_idx ON spot_control.spot_fix_journal (fix, seq); \
CREATE TABLE IF NOT EXISTS spot_control.spot_fix_journal_head (\
    id smallint NOT NULL, \
    last_seq bigint NOT NULL, \
    CONSTRAINT spot_fix_journal_head_pkey PRIMARY KEY (id), \
    CONSTRAINT spot_fix_journal_head_id_check CHECK (id = 1), \
    CONSTRAINT spot_fix_journal_head_seq_check CHECK (last_seq >= 0)); \
INSERT INTO spot_control.spot_fix_journal_head (id, last_seq) VALUES (1, 0) \
    ON CONFLICT (id) DO NOTHING;";

/// The role, its grants, the guard function and the triggers. The role is cluster-wide, so its
/// creation tolerates losing a race with another database's `CREATE ROLE`.
pub const GUARD: &str = "\
DO $$ BEGIN \
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'spot_fix_journal_app') THEN \
    CREATE ROLE spot_fix_journal_app NOLOGIN; \
  END IF; \
EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; \
END $$; \
GRANT USAGE ON SCHEMA spot_control TO spot_fix_journal_app; \
REVOKE UPDATE, DELETE, TRUNCATE ON spot_control.spot_fix_journal FROM spot_fix_journal_app; \
GRANT SELECT, INSERT ON spot_control.spot_fix_journal TO spot_fix_journal_app; \
REVOKE DELETE, TRUNCATE, INSERT ON spot_control.spot_fix_journal_head FROM spot_fix_journal_app; \
GRANT SELECT, UPDATE ON spot_control.spot_fix_journal_head TO spot_fix_journal_app; \
CREATE OR REPLACE FUNCTION spot_control.spot_fix_journal_guard() RETURNS trigger AS $$ \
BEGIN \
  RAISE EXCEPTION 'spot_fix_journal is append-only' USING ERRCODE = '42501'; \
END $$ LANGUAGE plpgsql; \
DROP TRIGGER IF EXISTS spot_fix_journal_no_rewrite ON spot_control.spot_fix_journal; \
CREATE TRIGGER spot_fix_journal_no_rewrite BEFORE UPDATE OR DELETE \
  ON spot_control.spot_fix_journal \
  FOR EACH ROW EXECUTE FUNCTION spot_control.spot_fix_journal_guard(); \
DROP TRIGGER IF EXISTS spot_fix_journal_no_truncate ON spot_control.spot_fix_journal; \
CREATE TRIGGER spot_fix_journal_no_truncate BEFORE TRUNCATE \
  ON spot_control.spot_fix_journal \
  FOR EACH STATEMENT EXECUTE FUNCTION spot_control.spot_fix_journal_guard();";

/// Creates the schema, tables, role and triggers if they are missing, in one transaction under an
/// advisory lock. Safe to call from several processes at once and on every start.
///
/// # Errors
/// `fix_pipeline_pg.setup_failed` carrying the bridge code or the SQLSTATE.
pub(crate) fn ensure(bridge: &Bridge, deadline: Duration) -> Result<(), SetupError> {
    bridge
        .run(deadline, |mut conn| {
            Box::pin(async move {
                let tx = conn.transaction().await?;
                tx.execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK])
                    .await?;
                tx.batch_execute(DDL).await?;
                tx.batch_execute(GUARD).await?;
                tx.commit().await
            })
        })
        .map_err(|e| {
            SetupError::new(format!(
                "the fix journal tables could not be prepared ({})",
                cause(&e)
            ))
        })
}
