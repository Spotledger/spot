//! The hot-path statements, as constants, so `tests/plans.rs` can `EXPLAIN` exactly what the
//! store runs (ADR-045: every filter on a hot path is covered by an index, verified from the
//! query plan). Parameter numbers are part of each constant's contract.

/// The columns every job query returns, in the order the row reader expects. A macro, so it can
/// be spliced into the constants below with `concat!`.
macro_rules! job_columns {
    () => {
        "id, site, name, args, queue, state, attempt, max_attempts, not_before, \
         idempotency_key, claim_token, claim_worker, lease_expires_at, last_error, created_at, \
         context"
    };
}

/// Columns for plain `SELECT`s.
pub const JOB_COLUMNS: &str = job_columns!();

/// Claim: `$1` queue, `$2` worker, `$3` lease milliseconds, `$4` limit. Locks the rows nobody
/// else holds (`SKIP LOCKED`) and leases them from the database clock in one statement.
pub const CLAIM: &str = concat!(
    "UPDATE spot_job SET
         state = 'running', attempt = attempt + 1,
         claim_token = nextval('spot_job_claim_seq'),
         claim_worker = $2,
         lease_expires_at = now() + $3::bigint * interval '1 millisecond'
     FROM (
         SELECT id AS cid FROM spot_job
         WHERE queue = $1 AND state IN ('pending', 'retrying')
           AND not_before <= now()
         ORDER BY priority, not_before, id
         FOR UPDATE SKIP LOCKED
         LIMIT $4) c
     WHERE id = c.cid
     RETURNING ",
    job_columns!()
);

/// Claim with a per-site running cap: `$1`..`$4` as [`CLAIM`], `$5` the cap. Candidates are
/// four times the limit (`$4 * 4`); each site gets at most `cap - running` of them.
pub const CLAIM_CAPPED: &str = concat!(
    "WITH cand AS (
         SELECT id, site, priority, not_before FROM spot_job
         WHERE queue = $1 AND state IN ('pending', 'retrying')
           AND not_before <= now()
         ORDER BY priority, not_before, id
         FOR UPDATE SKIP LOCKED
         LIMIT $4::bigint * 4),
     ranked AS (
         SELECT id, site, priority, not_before,
                row_number() OVER (PARTITION BY site ORDER BY priority, not_before, id) AS rn
         FROM cand),
     allowed AS (
         SELECT r.id AS cid FROM ranked r
         WHERE r.rn + (SELECT count(*) FROM spot_job x
                       WHERE x.site = r.site AND x.state = 'running') <= $5
         ORDER BY r.priority, r.not_before, r.id
         LIMIT $4)
     UPDATE spot_job SET
         state = 'running', attempt = attempt + 1,
         claim_token = nextval('spot_job_claim_seq'),
         claim_worker = $2,
         lease_expires_at = now() + $3::bigint * interval '1 millisecond'
     FROM allowed a
     WHERE id = a.cid
     RETURNING ",
    job_columns!()
);

/// The live holder of an idempotency key: `$1` site, `$2` name, `$3` key.
pub const IDEMPOTENCY_LIVE: &str = "SELECT id FROM spot_job
     WHERE site = $1 AND name = $2 AND idempotency_key = $3
       AND state IN ('pending', 'running', 'retrying')";

/// Heartbeat: `$1` claim token, `$2` worker, `$3` lease milliseconds.
pub const HEARTBEAT: &str = "UPDATE spot_job
     SET lease_expires_at = now() + $3::bigint * interval '1 millisecond'
     WHERE claim_token = $1 AND claim_worker = $2 AND state = 'running'";

/// Complete: `$1` claim token, `$2` worker, `$3` new state, `$4` retry time or NULL, `$5` error.
pub const COMPLETE: &str = "UPDATE spot_job SET
         state = $3::text,
         not_before = COALESCE($4::timestamptz, not_before),
         last_error = $5,
         claim_token = NULL, claim_worker = NULL, lease_expires_at = NULL,
         finished_at = CASE WHEN $3::text = 'retrying' THEN NULL ELSE now() END
     WHERE claim_token = $1 AND claim_worker = $2 AND state = 'running'";

/// Reap: `$1` the caller's clock or NULL (database clock alone), `$2` retry delay milliseconds,
/// `$3` batch size.
pub const REAP: &str = "UPDATE spot_job SET
         state = CASE WHEN attempt >= max_attempts THEN 'dead' ELSE 'retrying' END,
         not_before = now() + $2::bigint * interval '1 millisecond',
         claim_token = NULL, claim_worker = NULL, lease_expires_at = NULL,
         last_error = 'job.worker_lost',
         finished_at = CASE WHEN attempt >= max_attempts THEN now() END
     WHERE id IN (
         SELECT id FROM spot_job
         WHERE state = 'running'
           AND lease_expires_at <= GREATEST(now(), $1::timestamptz)
         ORDER BY lease_expires_at
         FOR UPDATE SKIP LOCKED
         LIMIT $3)
     RETURNING id";

/// Retention: `$1` states, `$2` age milliseconds, `$3` batch size.
pub const PURGE: &str = "DELETE FROM spot_job WHERE id IN (
         SELECT id FROM spot_job
         WHERE state = ANY($1::text[])
           AND finished_at < now() - $2::bigint * interval '1 millisecond'
         LIMIT $3)";

/// The running count the per-site cap reads: `$1` site.
pub const SITE_RUNNING: &str =
    "SELECT count(*) FROM spot_job WHERE site = $1 AND state = 'running'";

/// The context of the job a claim still holds, locked for the merge: `$1` job id, `$2` claim
/// token, `$3` worker. No row means the claim is stale (fencing).
pub const CONTEXT_LOCK: &str = "SELECT context FROM spot_job
     WHERE id = $1 AND claim_token = $2 AND claim_worker = $3 AND state = 'running'
     FOR UPDATE";

/// Writes the merged context: `$1` job id, `$2` the merged `jsonb`.
pub const CONTEXT_WRITE: &str = "UPDATE spot_job SET context = $2 WHERE id = $1";
