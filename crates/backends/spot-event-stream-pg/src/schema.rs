//! The DDL of the event-stream consumer tables.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_event_consumer` | one row per (site, consumer): the definition as `jsonb`, the partition count, the registration instant |
//! | `spot_event_partition` | one row per (site, consumer, partition): the checkpoint (`cp_generation`, `cp_seq`), the parked flag and when it was parked; a child of the consumer row, removed with it |
//!
//! Every key starts with `site`, so one database can hold several sites and a statement that
//! forgets the site matches nothing. Every statement of the store is a primary-key lookup or a
//! range on a primary-key prefix (ADR-045), so no further index is needed.
//!
//! [`prepare`] is idempotent and serialised across nodes by a transaction-scoped advisory lock.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;

use crate::error::{migration, migration_pool};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x4553_5047_0001;

/// The idempotent DDL. Every statement is `IF NOT EXISTS`.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_event_consumer (
    site          text NOT NULL,
    name          text NOT NULL,
    spec          jsonb NOT NULL,
    partitions    integer NOT NULL CHECK (partitions BETWEEN 1 AND 64),
    registered_at timestamptz NOT NULL,
    PRIMARY KEY (site, name)
);

CREATE TABLE IF NOT EXISTS spot_event_partition (
    site          text NOT NULL,
    consumer      text NOT NULL,
    part          integer NOT NULL CHECK (part >= 0),
    cp_generation bigint NOT NULL CHECK (cp_generation >= 0),
    cp_seq        bigint NOT NULL CHECK (cp_seq >= 0),
    parked        boolean NOT NULL DEFAULT false,
    parked_since  timestamptz,
    PRIMARY KEY (site, consumer, part),
    FOREIGN KEY (site, consumer)
        REFERENCES spot_event_consumer (site, name) ON DELETE CASCADE
);
"#;

/// Creates the tables if they are missing. Idempotent and safe to run from several nodes at
/// once.
///
/// # Errors
/// `event_stream_pg.migration_failed` when the database cannot apply the DDL.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(migration_pool)?;
    let tx = conn.transaction().await.map_err(migration)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(migration)?;
    tx.batch_execute(DDL).await.map_err(migration)?;
    tx.commit().await.map_err(migration)
}
