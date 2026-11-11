//! The statements the store runs. All are fixed text; every value is a bind parameter, and no
//! SQL is ever built from a key, a credential, a fingerprint or a body.

/// Claims a key in one statement.
///
/// `$1` site, `$2` credential, `$3` key, `$4` fingerprint, `$5` the lease end, `$6` now. A row is
/// returned exactly when the key was absent (insert), or its stored response's window ended, or a
/// pending reservation's lease ran out (the conflict's `WHERE` holds and the row is replaced by
/// the new reservation). A live row matches the `WHERE` not at all, so nothing is returned and the
/// refused claim extends no lease and no window.
pub(crate) const RESERVE: &str = "INSERT INTO spot_idempotency_key \
    (site, credential, key, fingerprint, state, lease_expires_at, created_at) \
    VALUES ($1, $2, $3, $4, 'pending', $5, $6) \
    ON CONFLICT (site, credential, key) DO UPDATE SET \
        fingerprint = EXCLUDED.fingerprint, state = 'pending', \
        lease_expires_at = EXCLUDED.lease_expires_at, expires_at = NULL, \
        status = NULL, headers = NULL, body = NULL, created_at = EXCLUDED.created_at \
    WHERE (spot_idempotency_key.state = 'done' AND spot_idempotency_key.expires_at <= $6) \
       OR (spot_idempotency_key.state = 'pending' AND spot_idempotency_key.lease_expires_at <= $6) \
    RETURNING 1";

/// Reads the live row of a scope: `$1` site, `$2` credential, `$3` key.
pub(crate) const READ: &str = "SELECT fingerprint, state, status, headers::text, body, \
    expires_at, lease_expires_at \
    FROM spot_idempotency_key WHERE site = $1 AND credential = $2 AND key = $3";

/// Reads only whether the scope is still pending, for the wait poll: `$1`..`$3` the scope.
pub(crate) const POLL: &str = "SELECT state, lease_expires_at \
    FROM spot_idempotency_key WHERE site = $1 AND credential = $2 AND key = $3";

/// Stores the response of a pending reservation. `$1`..`$3` the scope, `$4` the fingerprint,
/// `$5` the status, `$6` the headers (JSON text), `$7` the body, `$8` the instant the response
/// stops being replayed. It touches no row unless the key is pending under that fingerprint, so
/// the first completion wins and a done row is never rewritten.
pub(crate) const COMPLETE: &str = "UPDATE spot_idempotency_key SET state = 'done', \
    status = $5, headers = $6::text::jsonb, body = $7, expires_at = $8, \
    lease_expires_at = NULL \
    WHERE site = $1 AND credential = $2 AND key = $3 \
      AND fingerprint = $4 AND state = 'pending'";

/// Releases a pending reservation. `$1`..`$3` the scope, `$4` the fingerprint. Never a done row.
pub(crate) const FAIL: &str = "DELETE FROM spot_idempotency_key \
    WHERE site = $1 AND credential = $2 AND key = $3 \
      AND fingerprint = $4 AND state = 'pending'";

/// Deletes at most `$3` rows: finished rows whose window ended at or before `$1`, and pending rows
/// whose lease ended at or before `$2`.
///
/// The inner selection takes the row locks (`SKIP LOCKED`, so two pruners never wait for each
/// other) and the outer delete repeats the predicate, and that is load-bearing: a claim that
/// replaces an expired row while this statement runs must not lose its new reservation. The
/// delete waits for the claim's row lock, PostgreSQL re-checks the outer qualification against the
/// row the claim committed, and the new pending row (a live lease, no expiry) no longer matches.
pub(crate) const PRUNE: &str = "DELETE FROM spot_idempotency_key \
    WHERE ctid IN ( \
        SELECT ctid FROM spot_idempotency_key \
        WHERE (state = 'done' AND expires_at <= $1) \
           OR (state = 'pending' AND lease_expires_at <= $2) \
        LIMIT $3 FOR UPDATE SKIP LOCKED) \
      AND ((state = 'done' AND expires_at <= $1) \
        OR (state = 'pending' AND lease_expires_at <= $2))";
