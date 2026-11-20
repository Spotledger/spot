//! The statements the stores run. Every statement is a constant and every value is a bind
//! parameter, the site and the account included.

/// The cursor of one account: a point read of the primary key.
pub(crate) const LOAD_CURSOR: &str = "SELECT uid_validity, last_uid FROM spot_mail_intake_cursor \
    WHERE site = $1 AND account = $2";

/// The fenced write. A new row is inserted; an existing row is replaced only when its stored
/// fence is not above the writer's. A row the `WHERE` refuses is not modified, so
/// `rows_affected() == 0` means the writer is stale.
pub(crate) const STORE_CURSOR: &str = "INSERT INTO spot_mail_intake_cursor \
    (site, account, uid_validity, last_uid, fence) VALUES ($1, $2, $3, $4, $5) \
    ON CONFLICT (site, account) DO UPDATE SET \
        uid_validity = EXCLUDED.uid_validity, \
        last_uid = EXCLUDED.last_uid, \
        fence = EXCLUDED.fence, \
        updated_at = (EXTRACT(EPOCH FROM now()) * 1000000)::bigint \
    WHERE spot_mail_intake_cursor.fence <= EXCLUDED.fence";

/// Whether the digest is recorded: a point read of the primary key.
pub(crate) const SEEN_CONTAINS: &str = "SELECT 1 FROM spot_mail_intake_seen \
    WHERE site = $1 AND account = $2 AND digest = $3";

/// Records a digest, first write wins: one row modified means this call was first.
pub(crate) const SEEN_INSERT: &str = "INSERT INTO spot_mail_intake_seen \
    (site, account, digest, seen_at) VALUES ($1, $2, $3, $4) \
    ON CONFLICT (site, account, digest) DO NOTHING";

/// Removes a bounded batch of one site's digests recorded before `$2` (Unix microseconds),
/// oldest first, served by `spot_mail_intake_seen_at_idx`. Rows are identified by their whole
/// key, so the outer delete touches exactly the batch the sub-select chose.
pub(crate) const SEEN_PURGE_BATCH: &str = "DELETE FROM spot_mail_intake_seen \
    WHERE (site, account, digest) IN (\
        SELECT site, account, digest FROM spot_mail_intake_seen \
        WHERE site = $1 AND seen_at < $2 \
        ORDER BY seen_at LIMIT $3)";

/// How many rows one retention statement removes; a purge loops until a batch comes back short.
pub(crate) const PURGE_BATCH_ROWS: u64 = 2_000;
