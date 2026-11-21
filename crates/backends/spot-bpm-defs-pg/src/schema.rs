//! The DDL of the definition tables.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_bpm_definition` | one row per published version, primary key `(tenant, name, version)` |
//! | `spot_bpm_definition_head` | the latest version number per `(tenant, name)`: the atomic counter `publish` increments |
//!
//! A stored version is immutable: `CHECK` constraints bound the text (256 KiB), the hash, the
//! name grammar and the labels in the database itself, and a trigger refuses any `UPDATE` that
//! changes anything but the one move `published -> retired`, and any `DELETE`. So a bug or a
//! hand-written statement elsewhere cannot rewrite a version a Run is pinned to.

use deadpool_postgres::Pool;
use spot_bpm_definitions::DefinitionError;

use crate::error;

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x0062_706d_6465_6601;

/// The idempotent DDL.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_bpm_definition_head (
    tenant text NOT NULL,
    name   text NOT NULL CHECK (name ~ '^[a-z][a-z0-9_]{0,63}$'),
    latest integer NOT NULL CHECK (latest >= 0),
    PRIMARY KEY (tenant, name)
);

CREATE TABLE IF NOT EXISTS spot_bpm_definition (
    tenant       text NOT NULL,
    name         text NOT NULL CHECK (name ~ '^[a-z][a-z0-9_]{0,63}$'),
    version      integer NOT NULL CHECK (version >= 1),
    text         text NOT NULL CHECK (octet_length(text) <= 262144),
    content_hash text NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    state        text NOT NULL CHECK (state IN ('published', 'retired')),
    published_by text NOT NULL CHECK (published_by <> '' AND octet_length(published_by) <= 1024),
    published_at timestamptz NOT NULL,
    note         text CHECK (note IS NULL OR octet_length(note) <= 1024),
    retired_at   timestamptz,
    retired_by   text CHECK (retired_by IS NULL OR
                             (retired_by <> '' AND octet_length(retired_by) <= 1024)),
    PRIMARY KEY (tenant, name, version),
    CHECK ((state = 'retired') = (retired_at IS NOT NULL AND retired_by IS NOT NULL))
);

CREATE OR REPLACE FUNCTION spot_bpm_definition_guard() RETURNS trigger
LANGUAGE plpgsql AS $guard$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'a stored process version cannot be deleted' USING ERRCODE = '23000';
    END IF;
    IF NEW.tenant IS DISTINCT FROM OLD.tenant
       OR NEW.name IS DISTINCT FROM OLD.name
       OR NEW.version IS DISTINCT FROM OLD.version
       OR NEW.text IS DISTINCT FROM OLD.text
       OR NEW.content_hash IS DISTINCT FROM OLD.content_hash
       OR NEW.published_by IS DISTINCT FROM OLD.published_by
       OR NEW.published_at IS DISTINCT FROM OLD.published_at
       OR NEW.note IS DISTINCT FROM OLD.note
       OR (OLD.state = 'retired'
           AND (NEW.state IS DISTINCT FROM OLD.state
                OR NEW.retired_at IS DISTINCT FROM OLD.retired_at
                OR NEW.retired_by IS DISTINCT FROM OLD.retired_by))
    THEN
        RAISE EXCEPTION 'a stored process version is immutable' USING ERRCODE = '23000';
    END IF;
    RETURN NEW;
END
$guard$;

DROP TRIGGER IF EXISTS spot_bpm_definition_guard_trg ON spot_bpm_definition;
CREATE TRIGGER spot_bpm_definition_guard_trg
    BEFORE UPDATE OR DELETE ON spot_bpm_definition
    FOR EACH ROW EXECUTE FUNCTION spot_bpm_definition_guard();
"#;

/// Creates the tables, the guard function and the trigger if they are missing. Idempotent and
/// safe to run from several nodes at once (a transaction-scoped advisory lock serialises them).
///
/// # Errors
/// [`DefinitionError::StoreFailed`] when the database cannot apply the DDL; the error carries a
/// SQLSTATE class, never a row value.
pub async fn prepare(pool: &Pool) -> Result<(), DefinitionError> {
    let mut conn = pool.get().await.map_err(|_| error::pool())?;
    let tx = conn.transaction().await.map_err(|e| error::pg(&e))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(|e| error::pg(&e))?;
    tx.batch_execute(DDL).await.map_err(|e| error::pg(&e))?;
    tx.commit().await.map_err(|e| error::pg(&e))
}
