//! Moving values between the queue's types and database rows. Every conversion that can fail
//! (an unsigned value past `i64`, a stored value the types refuse) is an error, never a clamp.

use spot_quarantine::{ItemId, QuarantineError, QuarantineItem, RecordRef, State};
use spot_replica_apply::ReceivedRecord;
use spot_storage_contract::Name;
use spot_sync_protocol::{RecordName, SourceId};
use tokio_postgres::Row;

use crate::error::invalid;

/// An unsigned value as a database `bigint`.
pub(crate) fn to_i64(value: u64, what: &str) -> Result<i64, QuarantineError> {
    i64::try_from(value).map_err(|_| invalid(&format!("{what} is too large to store")))
}

/// A stored `bigint` as an unsigned value.
pub(crate) fn to_u64(value: i64, what: &str) -> Result<u64, QuarantineError> {
    u64::try_from(value).map_err(|_| invalid(&format!("stored {what} is negative")))
}

/// The state's stored text.
pub(crate) fn state_text(state: State) -> &'static str {
    state.as_str()
}

/// A stored state text as a state.
pub(crate) fn state_from(text: &str) -> Result<State, QuarantineError> {
    match text {
        "held" => Ok(State::Held),
        "retrying" => Ok(State::Retrying),
        "released" => Ok(State::Released),
        "discarded" => Ok(State::Discarded),
        _ => Err(invalid("stored state is unknown")),
    }
}

/// One `spot_quarantine_item` row, with the columns of [`crate::sql::ITEM_COLUMNS`], as an item.
pub(crate) fn item_from_row(row: &Row) -> Result<QuarantineItem, QuarantineError> {
    let id: i64 = row.get(0);
    let source: String = row.get(1);
    let seq: i64 = row.get(2);
    let name: String = row.get(3);
    let version: i64 = row.get(4);
    let entity: String = row.get(5);
    let record_id: String = row.get(6);
    let attempts: i32 = row.get(10);
    let state: String = row.get(11);
    Ok(QuarantineItem {
        id: ItemId(to_u64(id, "item id")?),
        source: SourceId::new(&source).map_err(|_| invalid("stored source is invalid"))?,
        seq: to_u64(seq, "sequence")?,
        record_ref: RecordRef {
            name: RecordName::new(&name).map_err(|_| invalid("stored record name is invalid"))?,
            version: to_u64(version, "record version")?,
            entity,
            id: record_id,
        },
        reason_code: row.get(7),
        detail: row.get(8),
        first_seen: row.get(9),
        attempts: u32::try_from(attempts).map_err(|_| invalid("stored attempts is negative"))?,
        state: state_from(&state)?,
    })
}

/// A payload row (see [`crate::sql::SELECT_PAYLOAD`]) as the record that arrived.
pub(crate) fn record_from_row(row: &Row) -> Result<ReceivedRecord, QuarantineError> {
    let source: String = row.get(0);
    let name: String = row.get(1);
    let version: i64 = row.get(2);
    let seq: i64 = row.get(3);
    let payload: String = row.get(4);
    let topic: Option<String> = row.get(5);
    let conversation: Option<String> = row.get(6);
    let source = SourceId::new(&source).map_err(|_| invalid("stored source is invalid"))?;
    let name = RecordName::new(&name).map_err(|_| invalid("stored record name is invalid"))?;
    let mut record = ReceivedRecord::from_payload(
        &source,
        name,
        to_u64(version, "record version")?,
        to_u64(seq, "sequence")?,
        &payload,
    )
    .map_err(|_| invalid("stored payload is not a record image"))?;
    record.topic = match topic {
        Some(text) => Some(Name::new(&text).map_err(|_| invalid("stored topic is invalid"))?),
        None => None,
    };
    record.conversation = conversation;
    Ok(record)
}
