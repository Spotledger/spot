//! The control schema: the journal, the gate, the snapshot index and the timings.
//!
//! The tables live in `spot_migration_control`, a schema of their own. `spot-pg-migrate`
//! reads and writes the current schema (`public`) only, so the control schema is outside
//! every tenant table a migration can diff, rewrite or drop. A restore of a tenant snapshot
//! therefore never touches the journal that records the restore.
//!
//! [`PgApplySchema::prepare`] creates the schema and its tables under a **transaction-scoped**
//! advisory lock, the convention `spot-pg-migrate` and `spot-coordinator-pg` use: two nodes
//! may call it at once, and a bare `CREATE TABLE IF NOT EXISTS` race cannot fail on a
//! duplicate object.

use deadpool_postgres::{Object, Pool};

use crate::error::{Error, Result};

/// The schema every table of this crate lives in.
pub const CONTROL_SCHEMA: &str = "spot_migration_control";

/// Every table this crate owns, in creation order.
pub const TABLES: [&str; 4] = ["journal", "gate", "snapshot", "timing"];

/// The advisory-lock pair that serialises `prepare` (`SPOT`/`MIGR`).
const PREPARE_LOCK: (i32, i32) = (0x5350_4f54, 0x4d49_4752);

/// Every statement the control schema needs, in one round trip.
///
/// The `at`, `since` and `taken_at` columns are unix **microseconds** in `bigint`, so a time
/// never rounds through a float and a caller's instant round-trips exactly.
const DDL: &str = "\
CREATE SCHEMA IF NOT EXISTS spot_migration_control; \
CREATE TABLE IF NOT EXISTS spot_migration_control.journal (\
    site text NOT NULL, \
    seq bigint NOT NULL, \
    at bigint NOT NULL, \
    record jsonb NOT NULL, \
    PRIMARY KEY (site, seq)); \
CREATE TABLE IF NOT EXISTS spot_migration_control.gate (\
    site text PRIMARY KEY, \
    state text NOT NULL CHECK (state IN ('open', 'closed')), \
    reason text NOT NULL DEFAULT '', \
    since bigint NOT NULL DEFAULT 0, \
    epoch bigint NOT NULL DEFAULT 0); \
CREATE TABLE IF NOT EXISTS spot_migration_control.snapshot (\
    site text NOT NULL, \
    snapshot_id text NOT NULL, \
    dir text NOT NULL, \
    label text NOT NULL, \
    taken_at bigint NOT NULL, \
    PRIMARY KEY (site, snapshot_id)); \
CREATE TABLE IF NOT EXISTS spot_migration_control.timing (\
    id bigserial PRIMARY KEY, \
    site text NOT NULL, \
    phase text NOT NULL, \
    duration_micros bigint NOT NULL, \
    snapshot text NOT NULL DEFAULT '', \
    at bigint NOT NULL); \
CREATE INDEX IF NOT EXISTS spot_migration_control_timing_site_phase \
    ON spot_migration_control.timing (site, phase);";

/// Creates the control schema and its tables; idempotent and safe to call from several nodes.
#[derive(Clone)]
pub struct PgApplySchema {
    pool: Pool,
}

impl PgApplySchema {
    /// A schema manager over `pool`; the pool belongs to the caller.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Creates the schema, its tables and its index if they are missing.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the database refuses a statement.
    pub async fn prepare(&self) -> Result<()> {
        let mut client = client(&self.pool, "preparing the control schema").await?;
        let tx = client
            .transaction()
            .await
            .map_err(|error| Error::database("preparing the control schema", &error))?;
        tx.batch_execute(&format!(
            "SELECT pg_advisory_xact_lock({}, {})",
            PREPARE_LOCK.0, PREPARE_LOCK.1
        ))
        .await
        .map_err(|error| Error::database("locking the control schema", &error))?;
        tx.batch_execute(DDL)
            .await
            .map_err(|error| Error::database("preparing the control schema", &error))?;
        tx.commit()
            .await
            .map_err(|error| Error::database("committing the control schema", &error))
    }

    /// Takes one pooled connection.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the pool cannot hand one out.
    pub async fn client(&self, operation: &str) -> Result<Object> {
        client(&self.pool, operation).await
    }

    /// The pool this manager was built over, for a collaborator that needs the same one.
    #[must_use]
    pub fn pool(&self) -> Pool {
        self.pool.clone()
    }
}

/// Takes one pooled connection, mapping a pool failure to this crate's error.
pub(crate) async fn client(pool: &Pool, operation: &str) -> Result<Object> {
    pool.get().await.map_err(|error| Error::ControlUnavailable {
        operation: operation.to_owned(),
        message: format!("the connection pool refused: {error}"),
    })
}

/// Opens one dedicated driver connection to the database `backend` points at.
///
/// `spot-postgres` keeps its pool private, and a session-level statement (the invalid-index
/// cleanup, which must not run inside a pooled transaction) needs a connection of its own.
/// The password rides the parsed driver config in-process only and never reaches a log.
pub(crate) async fn backend_client(
    backend: &spot_postgres::PgBackend,
    operation: &str,
) -> Result<tokio_postgres::Client> {
    let config = backend.config();
    let mut driver = tokio_postgres::Config::new();
    driver
        .host(&config.host)
        .port(config.port)
        .dbname(&config.dbname)
        .user(&config.user)
        .password(&config.password)
        .application_name("spot-migration-apply-pg")
        .connect_timeout(config.connect_timeout);
    let (client, connection) = driver
        .connect(tokio_postgres::NoTls)
        .await
        .map_err(|error| Error::ControlUnavailable {
            operation: operation.to_owned(),
            message: crate::error::sqlstate_message(&error),
        })?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

/// The quoted, schema-qualified name of one of [`TABLES`].
///
/// The argument is matched against the fixed list, never interpolated as a name: a caller
/// cannot reach SQL text through it.
pub(crate) fn table(name: &str) -> String {
    debug_assert!(TABLES.contains(&name), "unknown control table {name}");
    format!("\"{CONTROL_SCHEMA}\".\"{name}\"")
}
