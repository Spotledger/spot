//! The DDL of the idempotency table and its indexes.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_idempotency_key` | one row per `Idempotency-Key` in a scope: a pending reservation (its fingerprint and lease) or a stored response (status, allow-listed headers, body, and the instant it stops being replayed) |
//!
//! Columns:
//!
//! | Column | Holds |
//! | --- | --- |
//! | `site` | the site the key belongs to |
//! | `credential` | the router's SHA-256 (hex) of the bearer token; never the token |
//! | `key` | the `Idempotency-Key` header value, 1..=128 visible ASCII characters |
//! | `fingerprint` | SHA-256 (hex) over method, path and body |
//! | `state` | `pending` (running, holds a lease) or `done` (a response is stored) |
//! | `lease_expires_at` | pending only: Unix seconds after which another request may take the key |
//! | `expires_at` | done only: Unix seconds at which the response stops being replayed, recorded at write time from the retention policy |
//! | `status`, `headers`, `body` | done only: the response. `headers` is a JSON array of `[name, value]` pairs of the four allow-listed names |
//! | `created_at` | Unix seconds of the reservation |
//!
//! The primary key is `(site, credential, key)`: one caller can never read another's row, and the
//! claim's `ON CONFLICT` resolves a race inside the database.
//!
//! Indexes: `spot_idempotency_key_expires_idx (expires_at)` serves the prune of finished rows;
//! `spot_idempotency_key_lease_idx (lease_expires_at) WHERE state = 'pending'` serves the prune
//! of abandoned reservations.
//!
//! Constraints: `spot_idempotency_key_shape` (key sizes and charset, a fingerprint of exactly 64
//! lowercase hex digits, a state that is `pending` or `done`), `spot_idempotency_key_state` (a
//! pending row has a lease; a done row has an expiry, status, headers and body and no lease) and
//! `spot_idempotency_key_body` (`octet_length(body) <= 1048576`, the last line of defence behind
//! the store's own cap).
//!
//! The DDL is created by [`prepare`], idempotent and serialised across nodes by a transaction
//! scoped advisory lock, so several nodes starting at once is safe.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;

use crate::error::{migration, migration_pool};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x4944_454d_0001;

/// The idempotent DDL. Every statement can run again.
pub const DDL: &str = r"
CREATE TABLE IF NOT EXISTS spot_idempotency_key (
    site             text   NOT NULL,
    credential       text   NOT NULL,
    key              text   NOT NULL,
    fingerprint      text   NOT NULL,
    state            text   NOT NULL,
    lease_expires_at bigint,
    expires_at       bigint,
    status           integer,
    headers          jsonb,
    body             bytea,
    created_at       bigint NOT NULL,
    PRIMARY KEY (site, credential, key),
    CONSTRAINT spot_idempotency_key_shape CHECK (
        octet_length(site) BETWEEN 1 AND 256
        AND octet_length(credential) BETWEEN 1 AND 128
        AND octet_length(key) BETWEEN 1 AND 128
        AND key ~ '^[\x21-\x7e]+$'
        AND fingerprint ~ '^[0-9a-f]{64}$'
        AND state IN ('pending', 'done')),
    CONSTRAINT spot_idempotency_key_state CHECK (
        (state = 'pending' AND lease_expires_at IS NOT NULL)
        OR (state = 'done' AND lease_expires_at IS NULL AND expires_at IS NOT NULL
            AND status IS NOT NULL AND headers IS NOT NULL AND body IS NOT NULL)),
    CONSTRAINT spot_idempotency_key_body CHECK (
        body IS NULL OR octet_length(body) <= 1048576)
);

CREATE INDEX IF NOT EXISTS spot_idempotency_key_expires_idx
    ON spot_idempotency_key (expires_at);
CREATE INDEX IF NOT EXISTS spot_idempotency_key_lease_idx
    ON spot_idempotency_key (lease_expires_at) WHERE state = 'pending';
";

/// Creates the table and its indexes if missing. Idempotent; safe from several nodes at once.
///
/// # Errors
/// `idempotency_pg.migration_failed` when the database cannot apply the DDL.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(|_| migration_pool())?;
    let tx = conn.transaction().await.map_err(|e| migration(&e))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(|e| migration(&e))?;
    tx.batch_execute(DDL).await.map_err(|e| migration(&e))?;
    tx.commit().await.map_err(|e| migration(&e))
}
