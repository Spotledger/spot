//! Envelopes for published outbox records: the one tested path the relay uses (ADR-056).

use spot_events_contract::{EnvelopeError, EventEnvelope, SourceId};
use spot_storage_contract::OutboxRecord;

/// The envelopes of `records` as published by `source`, in order. Fails on the first record whose
/// payload or topic is not a valid envelope (`EnvelopeError`), so the relay can stop and report
/// rather than skip an event.
pub fn envelopes(
    source: &SourceId,
    records: &[OutboxRecord],
) -> Result<Vec<EventEnvelope>, EnvelopeError> {
    records
        .iter()
        .map(|record| EventEnvelope::from_record(source, record))
        .collect()
}
