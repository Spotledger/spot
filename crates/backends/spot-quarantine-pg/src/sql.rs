//! The statements the store runs. Every value is a bind parameter; the only text assembled at
//! run time is [`list_sql`], which joins fixed `WHERE` fragments and numbers their placeholders.

/// The item columns, in the order [`crate::convert::item_from_row`] reads them.
pub(crate) const ITEM_COLUMNS: &str = "id, source, seq, record_name, record_version, entity, \
    record_id, reason_code, detail, first_seen, attempts, state";

/// Inserts an item and returns its assigned id.
pub(crate) const INSERT_ITEM: &str = "INSERT INTO spot_quarantine_item \
    (source, seq, record_name, record_version, entity, record_id, reason_code, detail, \
     first_seen, attempts, state) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 0, 'held') RETURNING id";

/// Inserts the write-once payload of an item.
pub(crate) const INSERT_PAYLOAD: &str = "INSERT INTO spot_quarantine_payload \
    (item_id, payload, topic, conversation) VALUES ($1, $2, $3, $4)";

/// One item by id.
pub(crate) const SELECT_ITEM: &str = "SELECT id, source, seq, record_name, record_version, \
    entity, record_id, reason_code, detail, first_seen, attempts, state \
    FROM spot_quarantine_item WHERE id = $1";

/// The payload of one item with the item columns needed to rebuild the record.
pub(crate) const SELECT_PAYLOAD: &str = "SELECT i.source, i.record_name, i.record_version, \
    i.seq, p.payload, p.topic, p.conversation \
    FROM spot_quarantine_payload p JOIN spot_quarantine_item i ON i.id = p.item_id \
    WHERE p.item_id = $1";

/// Replaces the metadata of an **open** item. The identity columns are in the `WHERE` clause, so a
/// change to any of them matches no row; a closed item matches no row either, and a trigger
/// refuses the same for a raw client. The new state must be open (the store checks it): closing
/// is [`CLOSE_ITEM`].
pub(crate) const UPDATE_ITEM: &str = "UPDATE spot_quarantine_item \
    SET reason_code = $2, detail = $3, attempts = $4, state = $5 \
    WHERE id = $1 AND source = $6 AND seq = $7 AND record_name = $8 AND record_version = $9 \
      AND entity = $10 AND record_id = $11 AND first_seen = $12 \
      AND state IN ('held', 'retrying')";

/// Closes an open item once, recording who and when; returns the closed row.
pub(crate) const CLOSE_ITEM: &str = "UPDATE spot_quarantine_item \
    SET state = $2, closed_by = $3, closed_at = $4 \
    WHERE id = $1 AND state IN ('held', 'retrying') \
    RETURNING id, source, seq, record_name, record_version, entity, record_id, reason_code, \
    detail, first_seen, attempts, state";

/// Who closed an item and when (no row when it is open).
pub(crate) const SELECT_CLOSURE: &str = "SELECT closed_by, closed_at FROM spot_quarantine_item \
    WHERE id = $1 AND closed_by IS NOT NULL AND closed_at IS NOT NULL";

/// The state of an item (to tell an unknown id from a closed one after a refused change).
pub(crate) const ITEM_STATE: &str = "SELECT state FROM spot_quarantine_item WHERE id = $1";

/// The open item with an apply key from a sender, the oldest first.
pub(crate) const FIND_OPEN: &str = "SELECT id, source, seq, record_name, record_version, \
    entity, record_id, reason_code, detail, first_seen, attempts, state \
    FROM spot_quarantine_item \
    WHERE source = $1 AND record_name = $2 AND record_version = $3 \
      AND state IN ('held', 'retrying') \
    ORDER BY id LIMIT 1";

/// The list query for the filters that are set, in insertion order.
///
/// `filters` says which of state, reason code and source are bound, in that order; the
/// placeholders are numbered from 1 in the same order.
pub(crate) fn list_sql(state: bool, reason: bool, source: bool) -> String {
    let mut clauses: Vec<String> = Vec::new();
    let mut next = 1_u32;
    for (set, column) in [
        (state, "state"),
        (reason, "reason_code"),
        (source, "source"),
    ] {
        if set {
            clauses.push(format!("{column} = ${next}"));
            next += 1;
        }
    }
    let filter = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    format!("SELECT {ITEM_COLUMNS} FROM spot_quarantine_item{filter} ORDER BY id")
}
