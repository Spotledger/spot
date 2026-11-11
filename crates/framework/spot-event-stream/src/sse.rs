//! The server-sent-events frame encoder (ADR-078 section 4).
//!
//! An SSE frame is `id:`, `event:` and one `data:` line of single-line JSON, then a blank
//! line. The `id:` is an opaque, versioned, consumer-bound position token
//! ([`crate::position`]) so `Last-Event-ID` resumes exactly where the consumer stopped. A lost
//! position is answered with `event: resync_required` and the connection closes.
//!
//! The encoder is total: an [`EventEnvelope`] has no map with non-string keys, so its JSON is
//! always one line and `serde_json` escapes every control character (a payload containing
//! `\n`, `\r`, `\r\n` or the text `data:` never breaks the grammar).

use spot_events_contract::envelope::{EventEnvelope, Position};

use crate::error::StreamError;
use crate::position::{encode_position, parse_position};

/// The SSE event name of a lost position (ADR-078 section 4).
pub const RESYNC_EVENT: &str = "resync_required";

/// A comment-line heartbeat, so an intermediary does not close an idle connection.
#[must_use]
pub fn heartbeat() -> &'static [u8] {
    b": keepalive\n\n"
}

/// Encodes one event as an SSE frame bound to `consumer`.
///
/// The frame is `id: <token>\nevent: <topic>\ndata: <json>\n\n`.
#[must_use]
pub fn frame(consumer: &str, envelope: &EventEnvelope) -> Vec<u8> {
    let token = encode_position(consumer, envelope.position);
    let data = serde_json::to_string(envelope).unwrap_or_else(|_| String::from("{}"));
    let mut out = String::with_capacity(data.len() + token.len() + 32);
    out.push_str("id: ");
    out.push_str(&token);
    out.push('\n');
    out.push_str("event: ");
    out.push_str(envelope.topic.as_str());
    out.push('\n');
    out.push_str("data: ");
    out.push_str(&data);
    out.push_str("\n\n");
    out.into_bytes()
}

/// Encodes the `resync_required` frame that closes a stream whose position is lost.
#[must_use]
pub fn resync_required_frame(consumer: &str, retained_from: Position) -> Vec<u8> {
    let body = serde_json::json!({
        "consumer": consumer,
        "retained_from": { "generation": retained_from.generation, "seq": retained_from.seq },
    })
    .to_string();
    let mut out = String::with_capacity(body.len() + 32);
    out.push_str("event: ");
    out.push_str(RESYNC_EVENT);
    out.push('\n');
    out.push_str("data: ");
    out.push_str(&body);
    out.push_str("\n\n");
    out.into_bytes()
}

/// Parses a `Last-Event-ID` (or the `id:` of a frame), refusing a token issued for another
/// consumer.
///
/// # Errors
///
/// [`StreamError::PositionInvalid`] when the token is malformed, forged, of an unknown version
/// or bound to a different consumer.
pub fn parse_last_event_id(consumer: &str, text: &str) -> Result<Position, StreamError> {
    parse_position(consumer, text)
}
