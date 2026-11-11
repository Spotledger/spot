//! The wire messages of the native realtime protocol (contract `realtime.md` v0.1),
//! `RealtimeClientMessage` and `RealtimeServerMessage` of the contract's `openapi.yaml`, and
//! the wire validation the transport applies to room names, event kinds and timestamps.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;

use crate::Room;

/// The one subprotocol this transport speaks, per the contract.
pub const SUBPROTOCOL: &str = "spot.v1";

/// The largest text frame the transport parses, in bytes (contract: a frame over 64 KiB
/// closes the socket with 1009).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// How many rooms one connection may be in (contract: the join that would exceed 200 fails
/// with `realtime.too_many_rooms`).
pub const MAX_ROOMS: usize = 200;

/// The event kinds the contract's server-message schema pins. Events published with any
/// other `kind` are not forwardable on the native wire and are dropped (with a log) instead
/// of being sent in a shape a schema-conformant client would have to reject.
pub const EVENT_KINDS: [&str; 7] = [
    "doc_created",
    "doc_updated",
    "doc_deleted",
    "doc_posted",
    "doc_cancelled",
    "doc_renamed",
    "notification",
];

/// The room names the contract admits on the wire.
///
/// The bus's [`Room`] grammar is deliberately wider (it predates the contract and serves the
/// bus's own callers); the wire is the contract's `Room` schema, so the transport admits only
/// these four shapes and answers anything else with `realtime.unknown_room`:
///
/// ```text
/// doc:<doctype>:<id>       doctype: [a-z][a-z0-9_]{0,62}, id: [0-9a-f-]{36}
/// doctype:<doctype>        doctype: [a-z][a-z0-9_]{0,62}
/// user:<id>                id: [0-9a-f-]{36}
/// site
/// ```
pub fn wire_room(name: &str) -> Option<Room> {
    if name == "site" {
        return Some(Room::site());
    }
    if let Some(rest) = name.strip_prefix("doc:") {
        let (doctype, id) = rest.split_once(':')?;
        if is_wire_doctype(doctype) && is_wire_uuid36(id) {
            return Room::document(doctype, id).ok();
        }
        return None;
    }
    if let Some(doctype) = name.strip_prefix("doctype:") {
        if is_wire_doctype(doctype) {
            return Room::doctype(doctype).ok();
        }
        return None;
    }
    if let Some(user) = name.strip_prefix("user:") {
        if is_wire_uuid36(user) {
            return Room::user(user).ok();
        }
        return None;
    }
    None
}

/// `[a-z][a-z0-9_]{0,62}` — the contract's doctype shape.
fn is_wire_doctype(name: &str) -> bool {
    !(name.is_empty() || name.len() > 63)
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
}

/// Exactly 36 characters of `[0-9a-f-]` — the contract's record-id shape.
fn is_wire_uuid36(id: &str) -> bool {
    id.len() == 36
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

/// A client message, exactly the contract's `RealtimeClientMessage` (`oneOf`, tagged
/// `type`). Unknown extra JSON fields are tolerated and ignored — they change nothing the
/// transport keys on; a frame that does not parse as one of these variants is a protocol
/// violation and closes 1008.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// First message only; the token is verified by the
    /// [`TokenVerifier`](crate::ws::TokenVerifier).
    Auth {
        /// The session token or API key.
        token: String,
    },
    /// Join `room`, optionally resuming after the last `seq` the client saw there.
    Join {
        /// Echoes the client's own correlation id.
        #[serde(rename = "ref")]
        client_ref: String,
        /// A wire room name ([`wire_room`]).
        room: String,
        /// The last `seq` the client saw in this room, or none for a fresh join.
        resume_after: Option<u64>,
    },
    /// Leave `room`; always silent, whether in the room or not.
    Leave {
        /// A wire room name ([`wire_room`]).
        room: String,
    },
    /// Application-level liveness; answered with `pong`.
    Ping,
}

/// The `outcome` field of a [`ServerMessage::Joined`] reply, per the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinOutcomeReport {
    /// No resume was requested (or the join was a repeated no-op).
    Joined,
    /// The events after `resume_after` follow immediately; `replayed` says how many.
    Replayed,
    /// The events after `resume_after` are no longer replayable; the client must refetch.
    ResyncRequired,
}

/// The `error` field of a [`ServerMessage::JoinError`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WireError {
    /// A dotted code from the contract's `x-spot-error-codes` registry.
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
}

/// A server message, exactly the contract's `RealtimeServerMessage` (`oneOf`, tagged
/// `type`). Only the fields each variant carries are serialized (`joined.replayed` is
/// present exactly when the outcome is `replayed`).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Sent after a successful `auth`; the client is auto-joined to `user:<id>` right after.
    Ready {
        /// The authenticated user's id (the principal the verifier returned).
        user: String,
        /// The heartbeat cadence, in seconds (see the router's `WsConfig`).
        heartbeat_seconds: u64,
    },
    /// A join success. `replayed` is carried exactly for `outcome: replayed`.
    Joined {
        /// Echoes the join's `ref`.
        #[serde(rename = "ref")]
        client_ref: String,
        /// The room name (as the client sent it, which is the bus's canonical name).
        room: String,
        /// How the join ended up (fresh, replayed, or needing a refetch).
        outcome: JoinOutcomeReport,
        /// How many events were replayed into the stream; absent when not replaying.
        #[serde(skip_serializing_if = "Option::is_none")]
        replayed: Option<u64>,
    },
    /// A join failure; the connection stays open.
    JoinError {
        /// Echoes the join's `ref`.
        #[serde(rename = "ref")]
        client_ref: String,
        /// The room name exactly as the client sends it.
        room: String,
        /// Why the join failed.
        error: WireError,
    },
    /// A published event; `seq` is per-room and comes from the bus unchanged.
    Event {
        /// The room the event was published to.
        room: String,
        /// The per-room sequence number from the bus.
        seq: u64,
        /// One of the contract's `EVENT_KINDS`.
        kind: String,
        /// The event payload; an object with identifiers only (ADR-25).
        payload: serde_json::Value,
        /// The publish time, RFC 3339 UTC with `Z`.
        published_at: String,
    },
    /// The answer to the client's app-level `ping`.
    Pong,
}

/// Formats a bus publish time as the contract's `DateTimeUtc`: RFC 3339 in UTC, ending in
/// `Z` with at most nine fractional digits. The bus only stamps UTC times
/// (`Clock::now_utc`); a custom clock's offset is shifted to UTC anyway so the encoding can
/// never break the contract.
pub fn format_published_at(published_at: time::OffsetDateTime) -> String {
    let utc = published_at.to_offset(time::UtcOffset::UTC);
    utc.format(&Rfc3339)
        // An `OffsetDateTime` at UTC always formats under `Rfc3339`; the arm is for
        // exhaustiveness only.
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// Builds the wire `event` framing for a bus event, or `None` when the event cannot be
/// emitted without breaking the server-message schema: the room name is not a wire room, the
/// `kind` is outside the contract's pinned vocabulary, or the payload is not a JSON object.
/// Such events are logged and skipped rather than sent malformed — the room's `seq` stream
/// gains a gap, which tells a conformant client to resync, and ADR-25 treats a missing
/// notification as normal.
pub fn event_frame(event: crate::Event) -> Option<ServerMessage> {
    if wire_room(event.room.as_str()).is_none() {
        tracing::warn!(
            room = %event.room,
            seq = event.seq,
            "dropping event: room name is not a wire room"
        );
        return None;
    }
    if !EVENT_KINDS.contains(&event.kind.as_str()) {
        tracing::warn!(
            room = %event.room,
            seq = event.seq,
            kind = %event.kind,
            "dropping event: kind is outside the contract's vocabulary"
        );
        return None;
    }
    if !event.payload.is_object() {
        tracing::warn!(
            room = %event.room,
            seq = event.seq,
            "dropping event: payload is not a JSON object"
        );
        return None;
    }
    Some(ServerMessage::Event {
        room: event.room.as_str().to_owned(),
        seq: event.seq,
        kind: event.kind,
        payload: Arc::unwrap_or_clone(event.payload),
        published_at: format_published_at(event.published_at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_messages_match_the_contract_examples() {
        let frames = [
            r#"{"type":"auth","token":"3q2-7wEBAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhs"}"#,
            r#"{"type":"join","ref":"j1","room":"doc:sales_invoice:0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f","resume_after":12}"#,
            r#"{"type":"leave","room":"doctype:sales_invoice"}"#,
            r#"{"type":"ping"}"#,
        ];
        let expected = [
            ClientMessage::Auth {
                token: "3q2-7wEBAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhs".to_owned(),
            },
            ClientMessage::Join {
                client_ref: "j1".to_owned(),
                room: "doc:sales_invoice:0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f".to_owned(),
                resume_after: Some(12),
            },
            ClientMessage::Leave {
                room: "doctype:sales_invoice".to_owned(),
            },
            ClientMessage::Ping,
        ];
        for (frame, message) in frames.iter().zip(&expected) {
            assert_eq!(
                serde_json::from_str::<ClientMessage>(frame).ok().as_ref(),
                Some(message)
            );
        }
    }

    #[test]
    fn bad_frames_and_unknown_fields_behave_as_the_transport_assumes() {
        // resume_after is optional.
        assert_eq!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"join","ref":"r","room":"site"}"#)
                .ok(),
            Some(ClientMessage::Join {
                client_ref: "r".to_owned(),
                room: "site".to_owned(),
                resume_after: None,
            })
        );
        // Extra JSON fields (schema-invalid for the client) are tolerated and ignored: they
        // change nothing the transport keys on.
        assert_eq!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"ping","extra":1}"#).ok(),
            Some(ClientMessage::Ping)
        );
        for bad in [
            r#"{"type":"join"}"#,
            r#"{"type":"join","ref":1,"room":"site"}"#,
            r#"{"type":"connect"}"#,
            r#"{"type":"auth"}"#,
            r#"{"type":"auth","token":1}"#,
            "not json",
            "[]",
            "42",
        ] {
            assert!(
                serde_json::from_str::<ClientMessage>(bad).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn server_messages_match_the_contract_examples() {
        let ready = serde_json::to_value(&ServerMessage::Ready {
            user: "0192f3c4-5b6a-7c8d-9e0f-00000000aaaa".to_owned(),
            heartbeat_seconds: 25,
        })
        .unwrap_or_else(|e| panic!("encode: {e}"));
        assert_eq!(
            ready,
            json!({
                "type": "ready",
                "user": "0192f3c4-5b6a-7c8d-9e0f-00000000aaaa",
                "heartbeat_seconds": 25,
            })
        );
        let replayed = serde_json::to_value(&ServerMessage::Joined {
            client_ref: "j1".to_owned(),
            room: "doc:sales_invoice:0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f".to_owned(),
            outcome: JoinOutcomeReport::Replayed,
            replayed: Some(2),
        })
        .unwrap_or_else(|e| panic!("encode: {e}"));
        assert_eq!(replayed["outcome"], "replayed");
        assert_eq!(replayed["replayed"], 2);
        let plain = serde_json::to_value(&ServerMessage::Joined {
            client_ref: "j2".to_owned(),
            room: "site".to_owned(),
            outcome: JoinOutcomeReport::Joined,
            replayed: None,
        })
        .unwrap_or_else(|e| panic!("encode: {e}"));
        assert_eq!(plain["outcome"], "joined");
        assert!(
            plain.get("replayed").is_none(),
            "replayed only when replaying"
        );
        assert_eq!(
            serde_json::to_value(&ServerMessage::Pong).unwrap_or_else(|e| panic!("encode: {e}")),
            json!({"type": "pong"})
        );
        assert_eq!(
            serde_json::to_value(JoinOutcomeReport::ResyncRequired)
                .unwrap_or_else(|e| panic!("encode: {e}")),
            json!("resync_required")
        );
    }

    fn stamp(minute: u8, second: u8, milli: u16) -> time::OffsetDateTime {
        let date = time::Date::from_calendar_date(2026, time::Month::September, 29)
            .unwrap_or_else(|e| panic!("date: {e}"));
        let at = time::Time::from_hms_milli(10, minute, second, milli)
            .unwrap_or_else(|e| panic!("time: {e}"));
        date.with_time(at).assume_utc()
    }

    #[test]
    fn event_frame_carries_the_bus_fields_unchanged() {
        let room = Room::document("sales_invoice", "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f")
            .unwrap_or_else(|e| panic!("room: {e}"));
        let event = crate::Event {
            room,
            seq: 14,
            kind: "doc_updated".to_owned(),
            payload: Arc::new(json!({
                "doctype": "sales_invoice",
                "id": "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f",
                "version": 4,
            })),
            published_at: stamp(15, 2, 121),
        };
        let frame = event_frame(event).unwrap_or_else(|| panic!("frame"));
        assert_eq!(
            serde_json::to_value(&frame).unwrap_or_else(|e| panic!("encode: {e}")),
            json!({
                "type": "event",
                "room": "doc:sales_invoice:0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f",
                "seq": 14,
                "kind": "doc_updated",
                "payload": {
                    "doctype": "sales_invoice",
                    "id": "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f",
                    "version": 4,
                },
                "published_at": "2026-09-29T10:15:02.121Z",
            })
        );
    }

    #[test]
    fn events_outside_the_wire_vocabulary_are_dropped() {
        let room = Room::document("Item", "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f")
            .unwrap_or_else(|e| panic!("room: {e}"));
        let event = |kind: &str, payload: serde_json::Value| crate::Event {
            room: room.clone(),
            seq: 1,
            kind: kind.to_owned(),
            payload: Arc::new(payload),
            published_at: stamp(0, 0, 0),
        };
        assert!(event_frame(event("kick", json!({"n": 1}))).is_none());
        assert!(event_frame(event("doc_updated", json!("a string"))).is_none());
        // A room the bus grammar admits but the wire does not: `user:alice`.
        let offwire = Room::user("alice").unwrap_or_else(|e| panic!("room: {e}"));
        assert!(wire_room(offwire.as_str()).is_none());
        let event = crate::Event {
            room: offwire,
            seq: 1,
            kind: "notification".to_owned(),
            payload: Arc::new(json!({"a": 1})),
            published_at: stamp(0, 0, 0),
        };
        assert!(event_frame(event).is_none());
    }

    #[test]
    fn wire_rooms_admit_exactly_the_contract_shapes() {
        let good = [
            "site",
            "doc:sales_invoice:0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f",
            "doctype:sales_invoice",
            "user:0192f3c4-5b6a-7c8d-9e0f-00000000aaaa",
            "doctype:a",
            "doc:a:00000000-0000-1000-8000-000000000000",
        ];
        for name in good {
            let room = wire_room(name).unwrap_or_else(|| panic!("{name:?} should be a wire room"));
            assert_eq!(room.as_str(), name, "the wire name is the canonical name");
        }
        let bad = [
            "",
            "Site",
            "site:x",
            "doc:sales_invoice",
            "doc:sales_invoice:a:b",
            "doc:Sales_Invoice:0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f",
            "doc:sales_invoice:shortid",
            "doc:sales_invoice:0192F3C4-5B6A-7C8D-9E0F-1A2B3C4D5E6F",
            "user:alice",
            "user:0192f3c4-5b6a-7c8d-9e0f-00000000aaab:extra",
            "doctype:SalesInvoice",
            "doctype:",
            "doctype:a:b",
            "other:x",
        ];
        for name in bad {
            assert!(
                wire_room(name).is_none(),
                "{name:?} should not be a wire room"
            );
        }
        assert!(is_wire_doctype(&"a".repeat(63)));
        assert!(!is_wire_doctype(&"a".repeat(64)));
        assert!(is_wire_uuid36("0192f3c4-5b6a-7c8d-9e0f-00000000aaaa"));
        assert!(!is_wire_uuid36("0192f3c4-5b6a-7c8d-9e0f-00000000aa"));
    }

    #[test]
    fn published_at_is_rfc3339_utc_with_z() {
        // Millisecond precision, and trailing zeros trimmed per time's Rfc3339 (both within
        // the contract pattern's `1-9` fraction-digit allowance).
        assert_eq!(
            format_published_at(stamp(15, 2, 121)),
            "2026-09-29T10:15:02.121Z"
        );
        assert_eq!(
            format_published_at(stamp(15, 2, 120)),
            "2026-09-29T10:15:02.12Z"
        );
        assert_eq!(
            format_published_at(time::OffsetDateTime::UNIX_EPOCH),
            "1970-01-01T00:00:00Z",
            "a whole second has no fraction"
        );
        let shifted = stamp(15, 2, 121).to_offset(
            time::UtcOffset::from_hms(5, 0, 0).unwrap_or_else(|e| panic!("offset: {e}")),
        );
        assert_eq!(format_published_at(shifted), "2026-09-29T10:15:02.121Z");
    }

    #[test]
    fn event_kinds_are_exactly_the_schema_enum() {
        // Mirrors `openapi.yaml`'s `event.kind` enum; a drift fails here loudly.
        assert_eq!(EVENT_KINDS.len(), 7);
        for kind in [
            "doc_created",
            "doc_updated",
            "doc_deleted",
            "doc_posted",
            "doc_cancelled",
            "doc_renamed",
            "notification",
        ] {
            assert!(EVENT_KINDS.contains(&kind));
        }
    }
}
