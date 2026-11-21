//! Every statement the store runs: fixed text with bound parameters. No identifier or fragment
//! is ever built from input (ADR-45; this crate is a storage crate and the only place SQL lives).
//!
//! Parameter `$1` is always the store's tenant, taken from its constructor, never from a method
//! argument.

/// The columns every record read selects, in the order `codec::record_from_row` reads them by
/// name.
macro_rules! columns {
    () => {
        "link_id, conversation, version, case_id, open, waiting, body"
    };
}

/// Inserts a fresh record unless the key is taken.
pub const INSERT: &str = "\
INSERT INTO spot_bpm_conversation \
    (tenant, link_id, conversation, version, case_id, open, waiting, body) \
VALUES ($1, $2, $3, 1, $4, $5, $6, $7) \
ON CONFLICT DO NOTHING \
RETURNING version";

/// The compare-and-set: replaces the record only when the stored version is `$4`.
pub const SAVE: &str = "\
UPDATE spot_bpm_conversation SET \
    version = $4 + 1, case_id = $5, open = $6, waiting = $7, body = $8 \
WHERE tenant = $1 AND link_id = $2 AND conversation = $3 AND version = $4 \
RETURNING version";

/// One record by key.
pub const LOAD: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM spot_bpm_conversation WHERE tenant = $1 AND link_id = $2 AND conversation = $3"
);

/// Every record with a wait, in key order.
pub const WAITING: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM spot_bpm_conversation WHERE tenant = $1 AND waiting \
      ORDER BY link_id COLLATE \"C\", conversation COLLATE \"C\""
);

/// Every record of a request, in key order.
pub const OF_CASE: &str = concat!(
    "SELECT ",
    columns!(),
    " FROM spot_bpm_conversation WHERE tenant = $1 AND case_id = $2 \
      ORDER BY link_id COLLATE \"C\", conversation COLLATE \"C\""
);

/// How many open records a link holds.
pub const OPEN_ON: &str = "\
SELECT count(*) FROM spot_bpm_conversation WHERE tenant = $1 AND link_id = $2 AND open";

/// The code a message was rejected with.
pub const REJECTION: &str = "\
SELECT code FROM spot_bpm_conversation_rejection \
WHERE tenant = $1 AND link_id = $2 AND message_id = $3";

/// Remembers a rejection (a later one for the same key replaces the earlier).
pub const NOTE_REJECTION: &str = "\
INSERT INTO spot_bpm_conversation_rejection (tenant, link_id, message_id, code) \
VALUES ($1, $2, $3, $4) \
ON CONFLICT (tenant, link_id, message_id) DO UPDATE SET code = EXCLUDED.code";

/// Forgets the oldest rejections beyond `$2` (ids are UUIDv7: the smallest is the oldest, the
/// same order the reference `RejectionCache` uses).
pub const TRIM_REJECTIONS: &str = "\
DELETE FROM spot_bpm_conversation_rejection r \
WHERE r.tenant = $1 AND (r.message_id, r.link_id) NOT IN ( \
    SELECT message_id, link_id FROM spot_bpm_conversation_rejection \
    WHERE tenant = $1 \
    ORDER BY message_id COLLATE \"C\" DESC, link_id COLLATE \"C\" DESC \
    LIMIT $2)";
