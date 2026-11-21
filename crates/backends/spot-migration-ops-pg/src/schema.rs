//! The tables, created by the migrator and protected afterwards.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_migration_plan` | plan hash (key), canonical plan text, author, created-at, state, version |
//! | `spot_migration_plan_author` | the author of a digest, recorded once (approval store) |
//! | `spot_migration_approval` | append-only approve/reject decisions in order |
//! | `spot_migration_rehearsal` | the rehearsal report per plan hash and clock |
//!
//! Names are unqualified, so they land in the site's schema under the connection's
//! `search_path`, like every other extra table. No column holds a credential: the plan text
//! describes schema, never data values.

use spot_pg_bridge::Bridge;
use spot_pg_migrate::{ExtraTable, Refusal};

use spot_migration_ops::LifecycleError;

/// The plan table.
pub const PLAN_TABLE: &str = "spot_migration_plan";
/// The author table.
pub const AUTHOR_TABLE: &str = "spot_migration_plan_author";
/// The approval table.
pub const APPROVAL_TABLE: &str = "spot_migration_approval";
/// The rehearsal table.
pub const REHEARSAL_TABLE: &str = "spot_migration_rehearsal";

/// The application role: reads and appends, and may change only `state` and `version` of a plan.
pub const APPLICATION_ROLE: &str = "spot_migration_app";

/// The plan text cap in characters, mirrored by a check constraint.
pub const PLAN_CHECK_CHARS: usize = 1_048_576;

/// The plan table: one idempotent statement, no semicolon.
pub const PLAN_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_migration_plan (\
    plan_hash text NOT NULL, \
    plan_json text NOT NULL, \
    author text NOT NULL, \
    created_at bigint NOT NULL, \
    state text NOT NULL, \
    version bigint NOT NULL, \
    CONSTRAINT spot_migration_plan_pkey PRIMARY KEY (plan_hash), \
    CONSTRAINT spot_migration_plan_hash_check CHECK (plan_hash ~ '^[0-9a-f]{64}$'), \
    CONSTRAINT spot_migration_plan_size_check CHECK (char_length(plan_json) <= 1048576), \
    CONSTRAINT spot_migration_plan_version_check CHECK (version >= 1), \
    CONSTRAINT spot_migration_plan_state_check CHECK (state IN \
        ('proposed', 'approved', 'rehearsed', 'applying', 'applied', 'failed', 'rejected', 'superseded')))";

/// The author table.
pub const AUTHOR_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_migration_plan_author (\
    plan_hash text NOT NULL, \
    author text NOT NULL, \
    CONSTRAINT spot_migration_plan_author_pkey PRIMARY KEY (plan_hash), \
    CONSTRAINT spot_migration_plan_author_hash_check CHECK (plan_hash ~ '^[0-9a-f]{64}$'))";

/// The approval table (append-only).
pub const APPROVAL_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_migration_approval (\
    seq bigint GENERATED ALWAYS AS IDENTITY, \
    plan_hash text NOT NULL, \
    approver text NOT NULL, \
    role text NOT NULL, \
    decision text NOT NULL, \
    comment text NOT NULL, \
    at bigint NOT NULL, \
    signature text, \
    CONSTRAINT spot_migration_approval_pkey PRIMARY KEY (seq), \
    CONSTRAINT spot_migration_approval_hash_check CHECK (plan_hash ~ '^[0-9a-f]{64}$'), \
    CONSTRAINT spot_migration_approval_decision_check CHECK (decision IN ('approve', 'reject')))";

/// The rehearsal table.
pub const REHEARSAL_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_migration_rehearsal (\
    plan_hash text NOT NULL, \
    at bigint NOT NULL, \
    verdict text NOT NULL, \
    report_json text NOT NULL, \
    CONSTRAINT spot_migration_rehearsal_pkey PRIMARY KEY (plan_hash, at), \
    CONSTRAINT spot_migration_rehearsal_hash_check CHECK (plan_hash ~ '^[0-9a-f]{64}$'), \
    CONSTRAINT spot_migration_rehearsal_verdict_check CHECK (char_length(verdict) <= 32), \
    CONSTRAINT spot_migration_rehearsal_size_check CHECK (char_length(report_json) <= 4194304))";

/// Each table as `(marker table name, statement)`, in creation order.
pub const TABLE_DDL: [(&str, &str); 4] = [
    (PLAN_TABLE, PLAN_DDL),
    (AUTHOR_TABLE, AUTHOR_DDL),
    (APPROVAL_TABLE, APPROVAL_DDL),
    (REHEARSAL_TABLE, REHEARSAL_DDL),
];

/// The entries for the migrator's `PlanOptions::extra_tables`.
///
/// # Errors
/// The migrator's refusal if it rejects a statement; a test proves it does not.
pub fn extra_tables() -> Result<Vec<ExtraTable>, Refusal> {
    TABLE_DDL
        .iter()
        .map(|(table, statement)| ExtraTable::new(*table, vec![(*statement).to_owned()]))
        .collect()
}

/// The role, the grants and the guard triggers, run by [`crate::PgMigrationStore::protect`]
/// in one transaction under an advisory lock. Idempotent.
///
/// * the role may `SELECT` and `INSERT` everywhere, and `UPDATE` only `state` and `version` of a
///   plan; it never receives `DELETE` or `TRUNCATE`;
/// * the guard refuses, even to the owner, an `UPDATE` of any plan column but `state` and
///   `version`, any `DELETE` or `TRUNCATE` of the four tables, and any `UPDATE` of the
///   append-only tables.
pub(crate) const PROTECT: &str = "\
DO $$ BEGIN \
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'spot_migration_app') THEN \
    CREATE ROLE spot_migration_app NOLOGIN; \
  END IF; \
EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; \
END $$; \
REVOKE UPDATE, DELETE, TRUNCATE ON spot_migration_plan, spot_migration_plan_author, \
    spot_migration_approval, spot_migration_rehearsal FROM spot_migration_app; \
GRANT SELECT, INSERT ON spot_migration_plan, spot_migration_plan_author, \
    spot_migration_approval, spot_migration_rehearsal TO spot_migration_app; \
GRANT UPDATE (state, version) ON spot_migration_plan TO spot_migration_app; \
CREATE OR REPLACE FUNCTION spot_migration_plan_guard() RETURNS trigger AS $$ \
BEGIN \
  IF TG_OP = 'UPDATE' THEN \
    IF NEW.plan_hash IS DISTINCT FROM OLD.plan_hash OR NEW.plan_json IS DISTINCT FROM OLD.plan_json \
       OR NEW.author IS DISTINCT FROM OLD.author OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN \
      RAISE EXCEPTION 'a migration plan is immutable except for its state' USING ERRCODE = '42501'; \
    END IF; \
    RETURN NEW; \
  END IF; \
  RAISE EXCEPTION 'migration plans are never deleted here' USING ERRCODE = '42501'; \
END $$ LANGUAGE plpgsql; \
CREATE OR REPLACE FUNCTION spot_migration_append_guard() RETURNS trigger AS $$ \
BEGIN \
  RAISE EXCEPTION 'this migration table is append-only' USING ERRCODE = '42501'; \
END $$ LANGUAGE plpgsql; \
DROP TRIGGER IF EXISTS spot_migration_plan_no_rewrite ON spot_migration_plan; \
CREATE TRIGGER spot_migration_plan_no_rewrite BEFORE UPDATE OR DELETE ON spot_migration_plan \
  FOR EACH ROW EXECUTE FUNCTION spot_migration_plan_guard(); \
DROP TRIGGER IF EXISTS spot_migration_plan_no_truncate ON spot_migration_plan; \
CREATE TRIGGER spot_migration_plan_no_truncate BEFORE TRUNCATE ON spot_migration_plan \
  FOR EACH STATEMENT EXECUTE FUNCTION spot_migration_append_guard(); \
DROP TRIGGER IF EXISTS spot_migration_author_no_rewrite ON spot_migration_plan_author; \
CREATE TRIGGER spot_migration_author_no_rewrite BEFORE UPDATE OR DELETE OR TRUNCATE ON spot_migration_plan_author \
  FOR EACH STATEMENT EXECUTE FUNCTION spot_migration_append_guard(); \
DROP TRIGGER IF EXISTS spot_migration_approval_no_rewrite ON spot_migration_approval; \
CREATE TRIGGER spot_migration_approval_no_rewrite BEFORE UPDATE OR DELETE OR TRUNCATE ON spot_migration_approval \
  FOR EACH STATEMENT EXECUTE FUNCTION spot_migration_append_guard(); \
DROP TRIGGER IF EXISTS spot_migration_rehearsal_no_rewrite ON spot_migration_rehearsal; \
CREATE TRIGGER spot_migration_rehearsal_no_rewrite BEFORE UPDATE OR DELETE OR TRUNCATE ON spot_migration_rehearsal \
  FOR EACH STATEMENT EXECUTE FUNCTION spot_migration_append_guard();";

/// The columns [`crate::PgMigrationStore::ensure_present`] expects of each table.
pub(crate) const COLUMNS: [(&str, &[&str]); 4] = [
    (
        PLAN_TABLE,
        &[
            "plan_hash",
            "plan_json",
            "author",
            "created_at",
            "state",
            "version",
        ],
    ),
    (AUTHOR_TABLE, &["plan_hash", "author"]),
    (
        APPROVAL_TABLE,
        &[
            "seq",
            "plan_hash",
            "approver",
            "role",
            "decision",
            "comment",
            "at",
            "signature",
        ],
    ),
    (
        REHEARSAL_TABLE,
        &["plan_hash", "at", "verdict", "report_json"],
    ),
];

/// The catalogue query for one table's column names (reads no row of the table).
pub(crate) const COLUMNS_SQL: &str = "\
SELECT c.column_name::text \
FROM information_schema.columns c \
WHERE (c.table_schema, c.table_name) = ( \
    SELECT n.nspname, r.relname FROM pg_class r \
    JOIN pg_namespace n ON n.oid = r.relnamespace \
    WHERE r.oid = to_regclass($1::text))";

/// `migration.store_failed` for every bridge refusal or database error here: the text carries no
/// SQL, value or connection detail.
pub(crate) fn failed<E>(_error: spot_pg_bridge::BridgeError<E>) -> LifecycleError {
    LifecycleError::Store
}

/// Checks that every table exists with the expected columns; it creates nothing.
///
/// # Errors
/// `migration.store_failed` when a table or column is missing or the catalogue is unreadable.
pub fn ensure_present(bridge: &Bridge) -> Result<(), LifecycleError> {
    for (table, expected) in COLUMNS {
        let found: Vec<String> = bridge
            .run_default(move |conn| {
                Box::pin(async move {
                    let rows = conn.query(COLUMNS_SQL, &[&table]).await?;
                    Ok::<_, tokio_postgres::Error>(
                        rows.iter().map(|r| r.get::<_, String>(0)).collect(),
                    )
                })
            })
            .map_err(failed)?;
        if !expected.iter().all(|c| found.iter().any(|f| f == c)) {
            return Err(LifecycleError::Store);
        }
    }
    Ok(())
}
