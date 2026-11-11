//! The table: `spot_environment_set` in the control schema, created idempotently.
//!
//! | Column | Holds |
//! | --- | --- |
//! | `name` | the set's name (primary key); lower-case letters, digits, `-`, `_`, at most 63 |
//! | `version` | the optimistic version: 1 on create, +1 on every replace (`bigint`, at least 1) |
//! | `doc` | the set as a `jsonb` document ([`crate::store`] writes only validated sets) |
//!
//! No extension is needed and no promotion chain is stored here. The statements name the table
//! with its schema, so nothing depends on, or changes, the session's `search_path`.

use spot_pg_bridge::Bridge;

use spot_environments::EnvironmentError;

use crate::error::{cause, setup};

/// The control schema the table lives in (shared with the tenant registry).
pub const CONTROL_SCHEMA: &str = "spot_control";

/// The table.
pub const TABLE: &str = "spot_environment_set";

/// The advisory lock that serialises concurrent schema preparation (transaction-scoped).
const SCHEMA_LOCK: i64 = 0x0045_4e56_5345_5401;

/// The idempotent DDL. Every name starts with `spot_`; the constraints are named.
pub const DDL: &str = "\
CREATE SCHEMA IF NOT EXISTS spot_control; \
CREATE TABLE IF NOT EXISTS spot_control.spot_environment_set (\
    name text NOT NULL, \
    version bigint NOT NULL, \
    doc jsonb NOT NULL, \
    CONSTRAINT spot_environment_set_pkey PRIMARY KEY (name), \
    CONSTRAINT spot_environment_set_version_check CHECK (version >= 1), \
    CONSTRAINT spot_environment_set_name_check CHECK (char_length(name) BETWEEN 1 AND 63))";

/// Creates the schema and table if they are missing, in one transaction under an advisory lock.
/// Safe to call from several processes at once and on every start.
///
/// # Errors
/// `environments_pg.setup_failed` carrying the bridge code or the SQLSTATE.
pub(crate) fn ensure(
    bridge: &Bridge,
    deadline: std::time::Duration,
) -> Result<(), EnvironmentError> {
    bridge
        .run(deadline, |mut conn| {
            Box::pin(async move {
                let tx = conn.transaction().await?;
                tx.execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK])
                    .await?;
                tx.batch_execute(DDL).await?;
                tx.commit().await
            })
        })
        .map_err(|e| {
            setup(format!(
                "the environment set table could not be prepared ({})",
                cause(&e)
            ))
        })
}
