//! The `occupancy` outbox events (ADR-087 section 6.2, ADR-056, ADR-11).
//!
//! Capture only: every event is enqueued inside the business transaction and published after it
//! commits. Nothing here calls out. The topic names are storage names (`[a-z][a-z0-9_]*`), so the
//! dotted ids of the event registry (`occupancy.held`) are written with an underscore here; the
//! registry entry is a contract change, listed in the task report.

use spot_storage_contract::{
    ErrorKind, Name, NodeRef, OutboxEvent, RangePoint, RangeValue, RecordId, StorageError,
    StorageResult, Version,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::model::{Holder, State};

/// A hold was taken (a tentative insert).
pub const TOPIC_HELD: &str = "occupancy_held";
/// A hold became a booking, or a booking was made in one insert (book now).
pub const TOPIC_CONFIRMED: &str = "occupancy_confirmed";
/// A record was released (cancelled): the row is deleted.
pub const TOPIC_CANCELLED: &str = "occupancy_cancelled";
/// A tentative hold passed its expiry and its row was deleted (by the sweeper or by a takeover).
pub const TOPIC_EXPIRED: &str = "occupancy_expired";
/// The payload schema version of all four topics.
pub const SCHEMA_VERSION: u32 = 1;

/// The immutable facts of one event.
pub struct Facts<'a> {
    /// The occupancy record.
    pub record: RecordId,
    /// The unit.
    pub unit: RecordId,
    /// The range.
    pub during: &'a RangeValue,
    /// The state the record had when the event happened.
    pub state: State,
    /// The holder.
    pub holder: &'a Holder,
    /// The expiry, for a tentative record.
    pub expires_at: Option<OffsetDateTime>,
    /// The record's version the event belongs to.
    pub version: Version,
}

/// Builds the event for `topic`. The event id is new and unique, so a replayed transaction cannot
/// enqueue the same id twice by accident.
pub fn event(entity: &Name, topic: &str, facts: &Facts<'_>) -> StorageResult<OutboxEvent> {
    let (start, end) = facts.during.bounds().ok_or_else(|| {
        StorageError::new(
            ErrorKind::Internal,
            "an occupancy record holds a range without two finite bounds",
        )
    })?;
    let expires_at = match facts.expires_at {
        Some(t) => Some(t.format(&Rfc3339).map_err(|e| {
            StorageError::new(
                ErrorKind::Internal,
                format!("cannot format the expiry: {e}"),
            )
        })?),
        None => None,
    };
    let payload = serde_json::json!({
        "record": facts.record.to_string(),
        "unit": facts.unit.to_string(),
        "start": point_text(start)?,
        "end": point_text(end)?,
        "state": facts.state.as_str(),
        "holder": facts.holder.to_text(),
        "expires_at": expires_at,
    });
    Ok(OutboxEvent {
        id: RecordId::new_v7(),
        topic: Name::new(topic)?,
        subject: NodeRef {
            entity: entity.clone(),
            id: facts.record,
        },
        subject_version: Some(facts.version),
        schema_version: SCHEMA_VERSION,
        payload: payload.to_string(),
    })
}

fn point_text(p: RangePoint) -> StorageResult<String> {
    Ok(match p {
        RangePoint::Int(v) => v.to_string(),
        RangePoint::Decimal(v) => v.to_string(),
        RangePoint::Date(v) => v.to_string(),
        RangePoint::DateTime(v) => v.format(&Rfc3339).map_err(|e| {
            StorageError::new(
                ErrorKind::Internal,
                format!("cannot format an instant: {e}"),
            )
        })?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use time::{Date, Month};

    #[test]
    fn the_payload_carries_ids_range_state_and_holder_and_nothing_else() {
        let d = |day| Date::from_calendar_date(2026, Month::November, day).expect("date");
        let during = RangeValue::half_open(RangePoint::Date(d(1)), RangePoint::Date(d(3)));
        let holder = Holder::consumer("c1").expect("holder");
        let facts = Facts {
            record: RecordId::new_v7(),
            unit: RecordId::new_v7(),
            during: &during,
            state: State::Tentative,
            holder: &holder,
            expires_at: None,
            version: Version::FIRST,
        };
        let e = event(&Name::new("occupancy").expect("name"), TOPIC_HELD, &facts).expect("event");
        assert_eq!(e.topic.as_str(), "occupancy_held");
        assert_eq!(e.subject_version, Some(Version::FIRST));
        let v: serde_json::Value = serde_json::from_str(&e.payload).expect("json");
        assert_eq!(v["start"], "2026-11-01");
        assert_eq!(v["end"], "2026-11-03");
        assert_eq!(v["state"], "tentative");
        assert_eq!(v["holder"], "consumer:c1");
        assert!(v["expires_at"].is_null());
        assert_eq!(v.as_object().expect("object").len(), 7);
    }
}
