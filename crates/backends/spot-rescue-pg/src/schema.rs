//! The control schema `spot_rescue_control`, created idempotently under a transaction-scoped
//! advisory lock (the convention of `spot-pg-migrate` and `spot-migration-apply-pg`), so two nodes
//! may call [`PgRescueSchema::prepare`] at once.

use deadpool_postgres::{Object, Pool};

use crate::error::{Error, Result};

/// The schema every table of this crate lives in.
pub const CONTROL_SCHEMA: &str = "spot_rescue_control";

/// Every table this crate owns.
pub const TABLES: [&str; 1] = ["spot_rescue_journal"];

/// The advisory-lock pair that serialises `prepare` (`SPOT`/`RESC`).
const PREPARE_LOCK: (i32, i32) = (0x5350_4f54, 0x5245_5343);

const DDL: &str = "\
CREATE SCHEMA IF NOT EXISTS spot_rescue_control; \
CREATE TABLE IF NOT EXISTS spot_rescue_control.spot_rescue_journal (\
    site text NOT NULL, \
    seq bigint NOT NULL, \
    at bigint NOT NULL, \
    record jsonb NOT NULL, \
    PRIMARY KEY (site, seq));";

/// Creates the control schema and its table.
#[derive(Clone)]
pub struct PgRescueSchema {
    pool: Pool,
}

impl PgRescueSchema {
    /// A schema manager over `pool` (the pool belongs to the caller).
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Creates the schema and table if they are missing; idempotent and safe from several nodes.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the database refuses a statement.
    pub async fn prepare(&self) -> Result<()> {
        let mut client = client(&self.pool, "preparing the rescue control schema").await?;
        let tx = client
            .transaction()
            .await
            .map_err(|error| Error::database("preparing the rescue control schema", &error))?;
        tx.batch_execute(&format!(
            "SELECT pg_advisory_xact_lock({}, {})",
            PREPARE_LOCK.0, PREPARE_LOCK.1
        ))
        .await
        .map_err(|error| Error::database("locking the rescue control schema", &error))?;
        tx.batch_execute(DDL)
            .await
            .map_err(|error| Error::database("preparing the rescue control schema", &error))?;
        tx.commit()
            .await
            .map_err(|error| Error::database("committing the rescue control schema", &error))
    }
}

/// Takes one pooled connection, mapping a pool failure to this crate's error.
pub(crate) async fn client(pool: &Pool, operation: &str) -> Result<Object> {
    pool.get().await.map_err(|_| Error::ControlUnavailable {
        operation: operation.to_owned(),
        message: "the connection pool refused".to_owned(),
    })
}

/// The qualified name of a table of the control schema.
pub(crate) fn table(name: &str) -> String {
    format!("{CONTROL_SCHEMA}.{name}")
}
