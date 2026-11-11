//! The two statements the store runs. Both are fixed text; every value is a bind parameter.

/// Claims an id in one statement.
///
/// `$1` is the namespace, `$2` the namespaced id, `$3` the `expires_at` the caller wants recorded
/// and `$4` the caller's `now`. A row is returned exactly when the id was absent (insert) or had
/// expired at `now` (the conflict's `WHERE` holds and the expiry is replaced); a live id matches
/// the `WHERE` not at all, so no row comes back and the stored expiry is untouched.
pub(crate) const CLAIM: &str = "INSERT INTO spot_sso_replay (namespace, id, expires_at) \
    VALUES ($1, $2, $3) \
    ON CONFLICT (namespace, id) DO UPDATE \
    SET expires_at = EXCLUDED.expires_at \
    WHERE spot_sso_replay.expires_at <= $4 \
    RETURNING 1";

/// Deletes at most `$2` rows that are expired at or before `$1`.
///
/// The inner selection is the one `spot_sso_replay_expires_idx` serves; the delete itself goes by
/// primary key, so one call holds its locks for a bounded amount of work.
///
/// `expires_at <= $1` is repeated on the outer row, and that is load-bearing: a claim that
/// re-records an expired id and this delete can reach the same row at once. The delete waits for
/// the claim's row lock, and PostgreSQL then re-checks the outer qualification against the row the
/// claim committed (EvalPlanQual) — the inner selection was materialised from the older snapshot,
/// so on its own it would still list the row and the delete would remove the just-reclaimed id,
/// letting the message be replayed a second time. The repeated predicate sees the new, later
/// `expires_at` and spares the row.
pub(crate) const PRUNE: &str = "DELETE FROM spot_sso_replay \
    WHERE expires_at <= $1 AND (namespace, id) IN ( \
        SELECT namespace, id FROM spot_sso_replay WHERE expires_at <= $1 LIMIT $2)";
