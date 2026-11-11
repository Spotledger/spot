//! The statements the store runs.
//!
//! Every statement is a constant and every value is a bind parameter, the site included, so no
//! SQL is ever assembled from data. Each one is a point operation on the primary key except the
//! retention delete, which takes a bounded batch of one site's oldest receipts.

/// Whether the key has a receipt. A point read of the primary key that returns at most one row
/// and no column: the answer is the row's existence.
pub(crate) const IS_SENT: &str =
    "SELECT 1 FROM spot_mail_sent_log WHERE site = $1 AND message_id = $2";

/// Writes a receipt, first write wins.
///
/// `ON CONFLICT (site, message_id) DO NOTHING` is what makes the claim atomic: the conflict is
/// resolved by the primary key inside the database, so of two concurrent inserts of one key
/// exactly one modifies a row (`rows_affected() == 1`) and the other modifies none. The existing
/// row is left exactly as the first writer wrote it — nothing is overwritten and no column is
/// updated on conflict.
pub(crate) const RECORD_SENT: &str = "INSERT INTO spot_mail_sent_log \
    (site, message_id, accepted, rejected, rejected_code) \
    VALUES ($1, $2, $3, $4, $5) ON CONFLICT (site, message_id) DO NOTHING";

/// The receipt of one key, or no row.
pub(crate) const SELECT_RECEIPT: &str = "SELECT accepted, rejected, rejected_code \
    FROM spot_mail_sent_log WHERE site = $1 AND message_id = $2";

/// Removes a bounded batch of one site's receipts recorded before `$2` (Unix microseconds), the
/// oldest first, served by `spot_mail_sent_log_recorded_idx`. The sub-select is limited so one call
/// never deletes an unbounded row set; the rows are identified by their whole key, so the outer
/// delete touches exactly the batch the sub-select chose.
pub(crate) const PURGE_BATCH: &str = "DELETE FROM spot_mail_sent_log \
    WHERE (site, message_id) IN (\
        SELECT site, message_id FROM spot_mail_sent_log \
        WHERE site = $1 AND recorded_at < $2 \
        ORDER BY recorded_at LIMIT $3)";

/// How many rows one retention statement removes; a purge loops until a batch comes back short.
pub(crate) const PURGE_BATCH_ROWS: u64 = 2_000;
