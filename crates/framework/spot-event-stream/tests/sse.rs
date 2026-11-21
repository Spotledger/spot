//! The SSE frame encoder against the grammar: single-line data, control characters, heartbeats,
//! the resync frame and the opaque, consumer-bound position token.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use base64::Engine as _;
use spot_event_stream::sse;
use spot_events_contract::EventEnvelope;
use spot_events_contract::names::SourceId;
use support::{record, rid};

fn envelope() -> EventEnvelope {
    let source = SourceId::new("acme.spotledger.app").unwrap();
    let rec = record(
        0,
        3,
        "doc_created",
        "task",
        rid(1),
        1,
        serde_json::json!({"status": "Open"}),
    );
    EventEnvelope::from_record(&source, &rec).unwrap()
}

fn text(frame: &[u8]) -> String {
    String::from_utf8(frame.to_vec()).unwrap()
}

#[test]
fn the_frame_has_the_expected_lines() {
    let env = envelope();
    let frame = sse::frame("c1", &env);
    let body = text(&frame);
    assert!(body.starts_with("id: "), "{body}");
    assert!(body.contains("\nevent: doc_created\n"), "{body}");
    assert!(body.contains("\ndata: {"), "{body}");
    assert!(body.ends_with("\n\n"), "{body}");
    assert_eq!(
        body.matches('\n').count(),
        4,
        "unexpected line count: {body}"
    );
}

#[test]
fn a_payload_with_control_characters_stays_one_data_line() {
    let mut env = envelope();
    env.data = serde_json::json!({
        "text": "line1\nline2\rline3\r\n",
        "note": "data: value",
    });
    let frame = sse::frame("c1", &env);
    let body = text(&frame);
    assert_eq!(
        body.matches('\n').count(),
        4,
        "the payload broke the grammar"
    );
    assert!(!body.contains('\r'), "a carriage return reached the frame");
    let data_lines = body
        .lines()
        .filter(|line| line.starts_with("data:"))
        .count();
    assert_eq!(
        data_lines, 1,
        "the payload produced more than one data line"
    );
}

#[test]
fn the_position_token_round_trips_and_is_bound_to_the_consumer() {
    let env = envelope();
    let frame = sse::frame("c1", &env);
    let body = text(&frame);
    let token = body
        .lines()
        .next()
        .unwrap()
        .strip_prefix("id: ")
        .unwrap()
        .to_owned();
    let parsed = sse::parse_last_event_id("c1", &token).unwrap();
    assert_eq!(parsed, env.position);
    // Another consumer refuses it.
    let error = sse::parse_last_event_id("c2", &token).unwrap_err();
    assert_eq!(
        error.code(),
        spot_event_stream::error::CODE_POSITION_INVALID
    );
}

#[test]
fn a_forged_or_unknown_version_token_is_refused() {
    let error = sse::parse_last_event_id("c1", "not base64!!").unwrap_err();
    assert_eq!(
        error.code(),
        spot_event_stream::error::CODE_POSITION_INVALID
    );

    let wrong_version = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"v2|c1|1|2");
    let error = sse::parse_last_event_id("c1", &wrong_version).unwrap_err();
    assert_eq!(
        error.code(),
        spot_event_stream::error::CODE_POSITION_INVALID
    );

    let missing_fields = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"v1|c1|1");
    let error = sse::parse_last_event_id("c1", &missing_fields).unwrap_err();
    assert_eq!(
        error.code(),
        spot_event_stream::error::CODE_POSITION_INVALID
    );
}

#[test]
fn heartbeat_and_resync_frames() {
    assert_eq!(sse::heartbeat(), b": keepalive\n\n");
    let frame = sse::resync_required_frame(
        "c1",
        spot_events_contract::envelope::Position {
            generation: 2,
            seq: 7,
        },
    );
    let body = text(&frame);
    assert!(body.starts_with("event: resync_required\n"), "{body}");
    assert!(body.contains("\"generation\":2"), "{body}");
    assert!(body.contains("\"seq\":7"), "{body}");
    assert!(body.ends_with("\n\n"), "{body}");
}
