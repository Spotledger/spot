//! The wire shapes keep their exact names: serde round trips over every public type.

use serde_json::json;
use spot_llm::{LlmError, Message, Part, Request, Role, StopReason, StreamEvent, ToolSpec, Usage};

/// Round trips `value` through the wire, panicking with the serde error when it breaks.
fn round<T>(value: &T) -> T
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de>,
{
    let wire = match serde_json::to_value(value) {
        Ok(wire) => wire,
        Err(e) => panic!("did not serialize: {e}"),
    };
    match serde_json::from_value(wire) {
        Ok(back) => back,
        Err(e) => panic!("did not deserialize: {e}"),
    }
}

#[test]
fn message_parts_round_trip_with_stable_names() {
    let message = Message {
        role: Role::Tool,
        parts: vec![
            Part::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: "42".to_owned(),
                is_error: false,
            },
            Part::ToolResult {
                tool_use_id: "t2".to_owned(),
                content: "nope".to_owned(),
                is_error: true,
            },
        ],
    };
    let wire = serde_json::to_value(&message).ok().unwrap_or(json!(null));
    assert_eq!(wire["parts"][0]["type"], "tool_result");
    assert_eq!(wire["parts"][0]["tool_use_id"], "t1");
    // A false flag is skip-serialised; a true one carries its name.
    assert_eq!(wire["parts"][0].get("is_error"), None);
    assert_eq!(wire["parts"][1]["is_error"], true);
    assert_eq!(round(&message), message);
}

#[test]
fn tool_use_part_keeps_its_names() {
    let message = Message {
        role: Role::Assistant,
        parts: vec![Part::ToolUse {
            id: "call_1".to_owned(),
            name: "add".to_owned(),
            arguments: json!({ "a": 2 }),
        }],
    };
    let wire = serde_json::to_value(&message).ok().unwrap_or(json!(null));
    assert_eq!(wire["parts"][0]["type"], "tool_use");
    assert_eq!(wire["parts"][0]["id"], "call_1");
    assert_eq!(wire["parts"][0]["name"], "add");
    assert_eq!(wire["parts"][0]["arguments"]["a"], 2);
    assert_eq!(round(&message), message);
}

#[test]
fn request_keeps_its_names_and_defaults() {
    let request = Request::new("sample-model", "r1")
        .with_system("sys")
        .with_message(Message::user("hi"))
        .with_tool(ToolSpec {
            name: "add".to_owned(),
            description: "Adds two integers.".to_owned(),
            input_schema: json!({ "type": "object" }),
        })
        .with_max_output_tokens(64)
        .with_temperature_permille(750)
        .with_stop_sequence("END");
    let wire = serde_json::to_value(&request).ok().unwrap_or(json!(null));
    assert_eq!(wire["model"], "sample-model");
    assert_eq!(wire["request_id"], "r1");
    assert_eq!(wire["system"], "sys");
    assert_eq!(wire["temperature"], 750);
    assert_eq!(wire["max_output_tokens"], 64);
    assert_eq!(wire["stop_sequences"][0], "END");
    assert_eq!(wire["tools"][0]["input_schema"]["type"], "object");
    // A bare request round trips with its defaults filled in.
    let bare = Request::new("m", "r2");
    assert_eq!(round(&bare), bare);
    assert_eq!(round(&request), request);
}

#[test]
fn stream_events_keep_their_type_tags() {
    let events = vec![
        StreamEvent::TextDelta {
            delta: "hi".to_owned(),
        },
        StreamEvent::ToolUseStart {
            index: 0,
            id: "c1".to_owned(),
            name: "add".to_owned(),
        },
        StreamEvent::ToolUseDelta {
            index: 0,
            delta: "{\"a\":2}".to_owned(),
        },
        StreamEvent::ToolUseEnd { index: 0 },
        StreamEvent::Usage {
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 3,
                cache_write_tokens: 4,
            },
        },
        StreamEvent::Stop {
            reason: StopReason::ToolUse,
        },
    ];
    let wire = serde_json::to_value(&events).ok().unwrap_or(json!(null));
    for (index, expected) in [
        "text_delta",
        "tool_use_start",
        "tool_use_delta",
        "tool_use_end",
        "usage",
        "stop",
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(wire[index]["type"], expected, "event {index} lost its tag");
    }
    assert_eq!(wire[4]["usage"]["cache_read_tokens"], 3);
    assert_eq!(wire[5]["reason"], "tool_use");
    assert_eq!(round(&events.clone()), events);
}

#[test]
fn stop_reason_wire_names() {
    for (reason, name) in [
        (StopReason::Stop, "stop"),
        (StopReason::Length, "length"),
        (StopReason::ToolUse, "tool_use"),
        (StopReason::ContentFilter, "content_filter"),
    ] {
        let wire = serde_json::to_value(reason).ok().unwrap_or(json!(null));
        assert_eq!(wire, json!(name), "{name} must stay its wire name");
    }
}

#[test]
fn error_wire_shape_and_codes() {
    let rate_limited = LlmError::RateLimited {
        retry_after: Some(5),
    };
    let wire = serde_json::to_value(&rate_limited)
        .ok()
        .unwrap_or(json!(null));
    assert_eq!(
        wire,
        json!({ "code": "llm.rate_limited", "retry_after": 5 })
    );
    assert_eq!(rate_limited.code(), "llm.rate_limited");
    assert!(rate_limited.retryable());
    assert_eq!(
        rate_limited.retry_after(),
        Some(std::time::Duration::from_secs(5))
    );

    let bad = LlmError::BadRequest {
        detail: "tool name unknown".to_owned(),
    };
    let wire = serde_json::to_value(&bad).ok().unwrap_or(json!(null));
    assert_eq!(
        wire,
        json!({ "code": "llm.bad_request", "detail": "tool name unknown" })
    );

    for (error, code, retryable) in [
        (LlmError::Auth, "llm.auth", false),
        (LlmError::Overloaded, "llm.overloaded", true),
        (LlmError::ContextTooLong, "llm.context_too_long", false),
        (LlmError::Timeout, "llm.timeout", true),
        (LlmError::Unavailable, "llm.unavailable", true),
        (LlmError::ContentFiltered, "llm.content_filtered", false),
        (LlmError::BudgetExceeded, "llm.budget_exceeded", false),
        (
            LlmError::Protocol {
                detail: "junk".to_owned(),
            },
            "llm.protocol",
            false,
        ),
    ] {
        assert_eq!(error.code(), code);
        assert_eq!(error.retryable(), retryable);
        assert_eq!(round(&error), error);
        assert_eq!(
            LlmError::from_code(code).map(|rebuilt| rebuilt.code()),
            Some(code)
        );
    }
    assert!(LlmError::from_code("llm.nope").is_none());
}

#[test]
fn unknown_wire_fields_are_ignored() {
    let error = serde_json::from_value::<LlmError>(json!({ "code": "llm.timeout", "extra": 1 }))
        .ok()
        .unwrap_or(LlmError::Auth);
    assert_eq!(error, LlmError::Timeout);
}
