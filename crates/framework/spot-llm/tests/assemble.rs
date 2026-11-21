//! The stream assembler: deltas become the response `complete` would return.

use serde_json::json;
use spot_llm::stream::{assemble, drain};
use spot_llm::{
    BoxStream, LlmError, MockProvider, Provider, Request, StopReason, StreamEvent, Usage,
};

/// Runs `events` through the assembler.
fn build(events: Vec<StreamEvent>) -> Result<spot_llm::Response, LlmError> {
    assemble(&events)
}

#[tokio::test]
async fn assembles_text_usage_and_stop() {
    let response = build(vec![
        StreamEvent::TextDelta {
            delta: "Hello, ".to_owned(),
        },
        StreamEvent::TextDelta {
            delta: "world".to_owned(),
        },
        StreamEvent::Usage {
            usage: Usage {
                input_tokens: 9,
                output_tokens: 2,
                cache_read_tokens: 7,
                cache_write_tokens: 0,
            },
        },
        StreamEvent::Stop {
            reason: StopReason::Stop,
        },
    ]);
    let response = match response {
        Ok(response) => response,
        Err(e) => panic!("assembly failed: {e}"),
    };
    assert_eq!(response.text(), "Hello, world");
    assert_eq!(response.stop_reason, StopReason::Stop);
    assert_eq!(response.usage.cache_read_tokens, 7);
}

#[tokio::test]
async fn assembles_a_bracketed_tool_call() {
    let response = build(vec![
        StreamEvent::TextDelta {
            delta: "Adding.".to_owned(),
        },
        StreamEvent::ToolUseStart {
            index: 0,
            id: "c1".to_owned(),
            name: "add".to_owned(),
        },
        StreamEvent::ToolUseDelta {
            index: 0,
            delta: "{\"a\":".to_owned(),
        },
        StreamEvent::ToolUseDelta {
            index: 0,
            delta: "2}".to_owned(),
        },
        StreamEvent::ToolUseEnd { index: 0 },
        StreamEvent::Stop {
            reason: StopReason::ToolUse,
        },
    ]);
    let response = match response {
        Ok(response) => response,
        Err(e) => panic!("assembly failed: {e}"),
    };
    let uses = response.message.tool_uses();
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].id, "c1");
    assert_eq!(uses[0].name, "add");
    assert_eq!(uses[0].arguments, json!({ "a": 2 }));
    assert_eq!(response.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn protocol_failures_for_broken_streams() {
    let junk = vec![
        StreamEvent::ToolUseDelta {
            index: 0,
            delta: "{}".to_owned(),
        },
        StreamEvent::Stop {
            reason: StopReason::Stop,
        },
    ];
    let error = match build(junk) {
        Err(error) => error,
        Ok(_) => panic!("a delta without a start must fail"),
    };
    assert_eq!(error.code(), "llm.protocol");

    let unended = vec![StreamEvent::ToolUseStart {
        index: 0,
        id: "c".to_owned(),
        name: "t".to_owned(),
    }];
    let error = match build(unended) {
        Err(error) => error,
        Ok(_) => panic!("a stream without a stop must fail"),
    };
    assert_eq!(error.code(), "llm.protocol");
    assert!(
        error.to_string().contains("stop"),
        "the detail explains the missing stop"
    );

    let no_args = vec![
        StreamEvent::ToolUseStart {
            index: 0,
            id: "c".to_owned(),
            name: "t".to_owned(),
        },
        StreamEvent::ToolUseEnd { index: 0 },
        StreamEvent::Stop {
            reason: StopReason::ToolUse,
        },
    ];
    let error = match build(no_args) {
        Err(error) => error,
        Ok(_) => panic!("a call without argument fragments must fail"),
    };
    assert_eq!(error.code(), "llm.protocol");
    assert!(error.to_string().contains("t"), "the detail names the tool");

    let not_json = vec![
        StreamEvent::ToolUseStart {
            index: 0,
            id: "c".to_owned(),
            name: "t".to_owned(),
        },
        StreamEvent::ToolUseDelta {
            index: 0,
            delta: "{a".to_owned(),
        },
        StreamEvent::ToolUseEnd { index: 0 },
        StreamEvent::Stop {
            reason: StopReason::ToolUse,
        },
    ];
    assert_eq!(
        build(not_json).err().map(|error| error.code().to_owned()),
        Some("llm.protocol".to_owned())
    );
}

#[tokio::test]
async fn a_streamed_mock_equal_a_completed_mock() {
    let provider = MockProvider::new().responds_to(
        "eq",
        spot_llm::Response::new(
            spot_llm::Message::assistant("equal streams"),
            StopReason::Stop,
            Usage {
                input_tokens: 2,
                output_tokens: 3,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        ),
    );
    let completed = match provider.complete(Request::new("m", "eq")).await {
        Ok(completed) => completed,
        Err(e) => panic!("complete failed: {e}"),
    };
    let mut stream: BoxStream = match provider.stream(Request::new("m", "eq")).await {
        Ok(stream) => stream,
        Err(e) => panic!("stream failed: {e}"),
    };
    let mut events = Vec::new();
    for item in drain(&mut stream).await {
        match item {
            Ok(event) => events.push(event),
            Err(e) => panic!("stream yielded {e}"),
        }
    }
    let assembled = match assemble(&events) {
        Ok(assembled) => assembled,
        Err(e) => panic!("events did not assemble: {e}"),
    };
    assert_eq!(assembled, completed);
}
