//! The DDL of the job tables, and its indexes.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_job` | one row per job; `state` is a text column with a CHECK; `args` and `context` are `jsonb`; a job is `running` exactly when it has a `claim_token`, a worker and a lease |
//! | `spot_job_intake` | the outbox intake position `(site, generation, seq)`; one row per intake name |
//! | `spot_job_outbox_seen` | the outbox event ids already turned into jobs (replay dedupe) |
//!
//! Indexes (ADR-045: every hot filter is covered, asserted from `EXPLAIN` in `tests/plans.rs`):
//!
//! - `spot_job_claim_idx (queue, priority, not_before, id)` partial on the claimable states
//!   (`pending`, `retrying`): serves the ordered claim scan. `state` is not a key column because
//!   the two claimable states must interleave in one `priority, not_before, id` order.
//! - `spot_job_idem_idx` unique `(site, name, idempotency_key)` partial on the live states
//!   (`pending`, `running`, `retrying`) with a key: idempotency while a job is alive.
//! - `spot_job_token_idx` unique on `claim_token`: complete and heartbeat.
//! - `spot_job_lease_idx (lease_expires_at)` on running jobs: the reaper.
//! - `spot_job_site_running_idx (site)` on running jobs: the per-site cap count.
//! - `spot_job_finished_idx (state, finished_at)` on finished jobs: retention.
//!
//! `priority` is not part of the T1 job model; it is a column for later use, default 0, and
//! lower numbers are claimed first.

use deadpool_postgres::Pool;
use spot_jobs::Result;

use crate::convert::{pg_err, pool_err};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x5107_7ab5;

/// The idempotent DDL. Every statement is `IF NOT EXISTS`.
pub const DDL: &str = r#"
CREATE SEQUENCE IF NOT EXISTS spot_job_claim_seq;

CREATE TABLE IF NOT EXISTS spot_job (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    site             text NOT NULL,
    name             text NOT NULL,
    args             jsonb NOT NULL,
    queue            text NOT NULL,
    state            text NOT NULL
        CHECK (state IN ('pending', 'running', 'succeeded', 'retrying', 'dead', 'cancelled')),
    priority         smallint NOT NULL DEFAULT 0,
    attempt          integer NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    max_attempts     integer NOT NULL CHECK (max_attempts >= 1),
    not_before       timestamptz NOT NULL,
    idempotency_key  text,
    claim_token      bigint,
    claim_worker     text,
    lease_expires_at timestamptz,
    last_error       text,
    context          jsonb NOT NULL DEFAULT 'null'::jsonb,
    created_at       timestamptz NOT NULL DEFAULT now(),
    finished_at      timestamptz,
    CONSTRAINT spot_job_running_has_claim CHECK (
        (state = 'running') = (claim_token IS NOT NULL AND claim_worker IS NOT NULL
                               AND lease_expires_at IS NOT NULL)
        AND (state = 'running' OR (claim_token IS NULL AND claim_worker IS NULL
                                   AND lease_expires_at IS NULL))
    )
);

CREATE INDEX IF NOT EXISTS spot_job_claim_idx
    ON spot_job (queue, priority, not_before, id)
    WHERE state IN ('pending', 'retrying');

CREATE UNIQUE INDEX IF NOT EXISTS spot_job_idem_idx
    ON spot_job (site, name, idempotency_key)
    WHERE idempotency_key IS NOT NULL AND state IN ('pending', 'running', 'retrying');

CREATE UNIQUE INDEX IF NOT EXISTS spot_job_token_idx
    ON spot_job (claim_token) WHERE claim_token IS NOT NULL;

CREATE INDEX IF NOT EXISTS spot_job_lease_idx
    ON spot_job (lease_expires_at) WHERE state = 'running';

CREATE INDEX IF NOT EXISTS spot_job_site_running_idx
    ON spot_job (site) WHERE state = 'running';

CREATE INDEX IF NOT EXISTS spot_job_finished_idx
    ON spot_job (state, finished_at) WHERE finished_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS spot_job_intake (
    site       text PRIMARY KEY,
    generation bigint NOT NULL CHECK (generation >= 0),
    seq        bigint NOT NULL CHECK (seq >= 0),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS spot_job_outbox_seen (
    event_id text PRIMARY KEY,
    job_id   bigint NOT NULL,
    seen_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS spot_job_outbox_seen_at_idx ON spot_job_outbox_seen (seen_at);
"#;

/// Creates the tables and indexes if they are missing. Idempotent and safe to run from several
/// nodes at once (a transaction-scoped advisory lock serialises them).
pub async fn prepare(pool: &Pool) -> Result<()> {
    let mut conn = pool.get().await.map_err(pool_err)?;
    let tx = conn.transaction().await.map_err(pg_err)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(pg_err)?;
    tx.batch_execute(DDL).await.map_err(pg_err)?;
    tx.commit().await.map_err(pg_err)
}
