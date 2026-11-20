//! The DDL of the two tables, created by [`prepare`].
//!
//! `spot_mail_intake_cursor` has the primary key `(site, account)`: the point read of `load` and
//! the conflict target of the fenced write. `spot_mail_intake_seen` has the primary key
//! `(site, account, digest)`: the duplicate probe and the conflict target of the first record;
//! `spot_mail_intake_seen_at_idx (site, seen_at)` serves the retention delete (ADR-045: every
//! hot filter is covered).
//!
//! `updated_at` of the cursor is stamped by the database clock, one clock for every node;
//! `seen_at` is the instant the engine passed (time is an argument in the engine).

use deadpool_postgres::Pool;
use spot_mail_intake::StoreError;

use crate::{conn, unavailable};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x4D41_494C_0012;

/// The idempotent DDL. Every statement can run again.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_mail_intake_cursor (
    site         text   NOT NULL,
    account      text   NOT NULL,
    uid_validity bigint NOT NULL,
    last_uid     bigint NOT NULL,
    fence        bigint NOT NULL,
    updated_at   bigint NOT NULL DEFAULT (EXTRACT(EPOCH FROM now()) * 1000000)::bigint,
    PRIMARY KEY (site, account)
);

CREATE TABLE IF NOT EXISTS spot_mail_intake_seen (
    site    text   NOT NULL,
    account text   NOT NULL,
    digest  text   NOT NULL,
    seen_at bigint NOT NULL,
    PRIMARY KEY (site, account, digest)
);

CREATE INDEX IF NOT EXISTS spot_mail_intake_seen_at_idx
    ON spot_mail_intake_seen (site, seen_at);
"#;

/// Creates the tables and the index if they are missing. Idempotent and safe to run from several
/// nodes at once.
///
/// # Errors
/// `Unavailable` when the database cannot apply the DDL.
pub async fn prepare(pool: &Pool) -> Result<(), StoreError> {
    let mut connection = conn(pool).await?;
    let tx = connection
        .transaction()
        .await
        .map_err(|e| unavailable(&e))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(|e| unavailable(&e))?;
    tx.batch_execute(DDL).await.map_err(|e| unavailable(&e))?;
    tx.commit().await.map_err(|e| unavailable(&e))
}
