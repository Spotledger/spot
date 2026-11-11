//! The request and SSE mapping tests: the outbound body's every field under every
//! profile, streamed shapes driven directly through the parser, the partial-JSON closer,
//! and the wire `finish_reason` mapping.

mod common;

use common::{
    bench_by_name, fixture_for, generic_profile, must_assemble, provider_at, provider_nowhere,
};
use spot_llm::message::{Message, Part, Role};
use spot_llm::provider::Provider as _;
use spot_llm::request::{Request, ToolSpec};
use spot_llm::response::StopReason;
use spot_llm::stream::assemble;
use spot_llm::transport::HttpMethod;
use spot_llm_openai::test_hooks::{
    finish_reason_of_str, outbound_for, profile_of, temperature_text_for,
};
use spot_llm_openai::{MaxTokensField, Profile};

/// The fixture's add-tool spec, re-quoted by the outbound-body tests.
fn add_tool() -> ToolSpec {
    ToolSpec {
        name: "add".to_owned(),
        description: "Adds two integers.".to_owned(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "a": {"type": "integer"},
                "b": {"type": "integer"}
            },
            "required": ["a", "b"]
        }),
    }
}

/// The outbound body maps everything: system as the first message, `max_tokens` under the
/// profile's field name, the formatted temperature, tools under the documented
/// `function` shape, streaming flags and `stream_options`.
#[test]
fn the_outbound_body_maps_everything() {
    let provider = provider_nowhere(
        &Profile::openrouter(),
        "https://openrouter.test",
        "openai/gpt-4o",
    );
    let request = fixture_for("openai/gpt-4o")
        .tool_request()
        .with_max_output_tokens(256)
        .with_temperature_permille(700)
        .with_stop_sequence("END");
    let outbound = outbound_for(&provider, &request, true);

    assert_eq!(outbound.method, HttpMethod::Post);
    assert_eq!(outbound.url, "https://openrouter.test/v1/chat/completions");
    assert!(
        outbound
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer test-key-not-real"),
        "the key rides as a bearer header"
    );
    assert!(
        outbound
            .headers
            .iter()
            .any(|(name, value)| name == "request-id" && value == "cf-tool"),
        "the request id rides as a header"
    );

    let body: serde_json::Value = serde_json::from_str(&outbound.body)
        .unwrap_or_else(|error| panic!("the outbound body is JSON: {error}"));
    assert_eq!(body["model"], "openai/gpt-4o");
    assert_eq!(
        body["max_tokens"], 256,
        "the profile's field name carries the cap"
    );
    assert_eq!(body["stop"], serde_json::json!(["END"]));
    assert_eq!(
        body["temperature"].to_string(),
        "0.7",
        "the permille formatted itself"
    );
    assert_eq!(
        body["messages"][0],
        serde_json::json!({
            "role": "system",
            "content": "You are Spotledger's conformance probe."
        }),
        "the system prompt rides as the first message"
    );
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(
        body["tools"][0],
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "add",
                "description": "Adds two integers.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "a": {"type": "integer"},
                        "b": {"type": "integer"}
                    },
                    "required": ["a", "b"]
                }
            }
        }),
        "the tool maps to the documented function shape"
    );
    assert_eq!(body["stream"], serde_json::json!(true));
    assert_eq!(
        body["stream_options"],
        serde_json::json!({"include_usage": true}),
        "the streaming-usage profile asks for usage"
    );
}

/// The profile's `max_tokens_field` switches the cap's field name.
#[test]
fn the_max_tokens_field_follows_the_profile() {
    for (profile, field) in [
        (Profile::openrouter(), "max_tokens"),
        (Profile::ollama(), "max_tokens"),
        (Profile::generic(), "max_tokens"),
    ] {
        let provider = provider_nowhere(&profile, "https://field.test", "gpt-4o");
        let outbound = outbound_for(
            &provider,
            &fixture_for("gpt-4o")
                .text_request()
                .with_max_output_tokens(128),
            false,
        );
        let body: serde_json::Value = serde_json::from_str(&outbound.body)
            .unwrap_or_else(|error| panic!("the body is JSON: {error}"));
        assert_eq!(body[field], 128, "{field} carries the cap");
        let other = if field == "max_tokens" {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        assert!(body.get(other).is_none(), "only one name in the body");
    }

    // A profile spelled for the newer field.
    let newer = Profile {
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
        ..Profile::generic()
    };
    let provider = provider_nowhere(&newer, "https://newer.test", "gpt-4o");
    let outbound = outbound_for(
        &provider,
        &fixture_for("gpt-4o")
            .text_request()
            .with_max_output_tokens(512),
        false,
    );
    let body: serde_json::Value = serde_json::from_str(&outbound.body)
        .unwrap_or_else(|error| panic!("the body is JSON: {error}"));
    assert_eq!(body["max_completion_tokens"], 512, "the newer name");
    assert!(body.get("max_tokens").is_none());
}

/// An argument-less streamed tool call closes to an empty object rather than an
/// `llm.protocol` fold failure.
#[tokio::test]
async fn an_argument_less_streamed_tool_call_closes_to_an_empty_object() {
    let body = concat!(
        "data: {\"id\":\"no-args\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[",
        "{\"index\":0,\"id\":\"call_9\",\"type\":\"function\",\"function\":{\"name\":\"ping\",\"arguments\":\"\"}}",
        "]}}]}\n\n",
        "data: {\"id\":\"no-args\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let events = spot_llm_openai::sse::parse(body)
        .unwrap_or_else(|error| panic!("the argument-less call must stream: {error}"));
    let assembled =
        assemble(&events).unwrap_or_else(|error| panic!("the events must assemble: {error}"));
    let uses = assembled.message.tool_uses();
    assert_eq!(uses.len(), 1, "{uses:?}");
    assert_eq!(uses[0].arguments, serde_json::json!({}));
    assert_eq!(assembled.stop_reason, StopReason::ToolUse);
}

/// `finish_reason` mapping: every documented name, the tolerated legacy aliases, and the
/// unknown-value failure — plus the streamed tool-call stop bracket closed at
/// `finish_reason`.
#[test]
fn finish_reason_names_map_and_fail() {
    for (name, expected) in [
        ("stop", StopReason::Stop),
        ("length", StopReason::Length),
        ("tool_calls", StopReason::ToolUse),
        ("content_filter", StopReason::ContentFilter),
    ] {
        assert_eq!(finish_reason_of_str(name), Ok(expected), "{name}");
    }
    let error = match finish_reason_of_str("function_call_v9") {
        Ok(_) => panic!("an unknown name must fail"),
        Err(detail) => detail,
    };
    assert!(error.contains("function_call_v9"), "{error}");
}

/// Stop-reason aliases keep their mapping: `function_call` is a tool use, `max_tokens`
/// and `max_output_tokens` are lengths, `end_turn` is a stop.
#[test]
fn stop_reason_aliases_keep_the_mapping() {
    assert_eq!(
        finish_reason_of_str("function_call"),
        Ok(StopReason::ToolUse)
    );
    assert_eq!(finish_reason_of_str("max_tokens"), Ok(StopReason::Length));
    assert_eq!(finish_reason_of_str("end_turn"), Ok(StopReason::Stop));
    assert_eq!(
        finish_reason_of_str("max_output_tokens"),
        Ok(StopReason::Length)
    );
}

/// A stop chunk arriving before `[DONE]`, with a keep-alive comment ignored and unknown
/// body fields tolerated.
#[tokio::test]
async fn stream_framing_tolerates_noise_and_stops_at_reason() {
    let body = concat!(
        ": keep-alive\n\n",
        "data: {\"id\":\"noisy\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},",
        "\"finish_reason\":null,\"logprobs\":null}],\"system_fingerprint\":\"fp_1\"}\n\n",
        "data: {\"id\":\"noisy\",\"choices\":[{\"index\":0,\"delta\":{},",
        "\"finish_reason\":\"stop\",\"logprobs\":null}]}\n\n",
        "data: [DONE]\n\n"
    );
    let events = spot_llm_openai::sse::parse(body)
        .unwrap_or_else(|error| panic!("the noisy stream must parse: {error}"));
    let assembled =
        assemble(&events).unwrap_or_else(|error| panic!("the noisy stream must assemble: {error}"));
    assert_eq!(assembled.text(), "Hi");
    assert_eq!(assembled.stop_reason, StopReason::Stop);
}

/// The temperature formatter: no float ever crosses; every documented decimal shape.
#[test]
fn temperature_formats_without_a_float() {
    for (permille, expected) in [
        (0u16, "0"),
        (5, "0.005"),
        (50, "0.05"),
        (500, "0.5"),
        (120, "0.12"),
        (700, "0.7"),
        (999, "0.999"),
        (1_000, "1"),
    ] {
        let text =
            temperature_text_for(permille).unwrap_or_else(|| panic!("{permille} permille formats"));
        assert_eq!(text, expected, "{permille} permille");
    }
    assert!(
        temperature_text_for(1_234).is_none(),
        "over 1.0 is out of range"
    );

    // And it embeds as a bare JSON number via the wire build:
    let provider = provider_nowhere(&generic_profile(), "https://temp.test", "gpt-4o");
    let outbound = outbound_for(
        &provider,
        &fixture_for("gpt-4o")
            .text_request()
            .with_temperature_permille(50),
        false,
    );
    let body: serde_json::Value = serde_json::from_str(&outbound.body)
        .unwrap_or_else(|error| panic!("the body is JSON: {error}"));
    assert_eq!(body["temperature"].to_string(), "0.05");
}

/// The partial-JSON closer: the documented Ollama truncation shapes close and parse;
/// unrepairable cuts return `None` for the caller to map to `llm.protocol`.
#[test]
fn partial_json_closes_or_refuses() {
    let cases: Vec<(&str, serde_json::Value)> = vec![
        ("{\"a\": 2", serde_json::json!({"a": 2})),
        ("{\"a\": 2,", serde_json::json!({"a": 2})),
        ("{\"a\":", serde_json::json!({})),
        (
            "{\"query\": \"show me the ledger",
            serde_json::json!({"query": "show me the ledger"}),
        ),
        ("", serde_json::json!({})),
    ];
    for (raw, expected) in cases {
        let parsed = spot_llm_openai::close::close_arguments(raw)
            .unwrap_or_else(|| panic!("'{raw}' must close"));
        assert_eq!(parsed, expected, "'{raw}'");
    }
    // A complete string passes through unchanged.
    let parsed = spot_llm_openai::close::close_arguments("{\"a\": 1}")
        .unwrap_or_else(|| panic!("the complete string must parse"));
    assert_eq!(parsed, serde_json::json!({"a": 1}));
    // Unrepairable: a key cut mid-word, and a truncated literal, refuse.
    assert!(
        spot_llm_openai::close::close_arguments("{\"a").is_none(),
        "a truncated key refuses"
    );
    assert!(
        spot_llm_openai::close::close_arguments("{\"flag\": fals").is_none(),
        "a truncated literal refuses"
    );
}

/// An empty base URL falls back to the documented default; the provider's `Debug` never
/// prints the key; the id is the provider's own; the profile stays plain data the caller
/// can inspect.
#[test]
fn defaults_redactions_and_the_plain_profile() {
    let provider = provider_nowhere(&Profile::openrouter(), "", "gpt-4o");
    assert_eq!(
        outbound_for(&provider, &fixture_for("gpt-4o").text_request(), false).url,
        "https://api.openai.com/v1/chat/completions"
    );
    assert_eq!(provider.id(), "openai-compatible");
    let printed = format!("{provider:?}");
    assert!(
        !printed.contains("test-key-not-real"),
        "the key never prints: {printed}"
    );
    assert!(printed.contains("***"), "the key prints as ***: {printed}");

    let profile = profile_of(&provider);
    assert_eq!(profile, Profile::openrouter());
    assert!(
        profile.supports_streaming_usage,
        "the openrouter profile asks for usage"
    );
    let ollama = profile_of(&provider_nowhere(&Profile::ollama(), "", "llama3.2"));
    assert!(
        ollama.tool_args_may_be_partial_json,
        "the ollama profile is lenient"
    );
    assert_eq!(
        ollama.max_tokens_field,
        MaxTokensField::MaxTokens,
        "the ollama profile caps under the classic name"
    );
    assert_eq!(
        generic_profile().max_tokens_field,
        MaxTokensField::MaxTokens,
        "the generic profile caps under the classic name"
    );
}

/// A `tool`-role result message maps to the documented block: the assistant turn quoting
/// the arguments text, then the `tool` role quoting the call id.
#[test]
fn the_tool_result_message_maps_to_the_documented_block() {
    let provider = provider_nowhere(&generic_profile(), "https://toolrole.test", "gpt-4o");
    let outbound = outbound_for(
        &provider,
        &fixture_for("gpt-4o").tool_result_request(),
        false,
    );
    let body: serde_json::Value = serde_json::from_str(&outbound.body)
        .unwrap_or_else(|error| panic!("the body is JSON: {error}"));
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        messages.len(),
        4,
        "system + user + assistant tool use + tool result"
    );
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "add");
    assert_eq!(
        messages[2]["tool_calls"][0]["function"]["arguments"],
        "{\"a\":2,\"b\":3}"
    );
    // OpenAI documents the tool result on its own role, quoting the call id.
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_1");
    assert_eq!(messages[3]["content"], "5");
}

/// An `is_error` tool result maps to the wire with its `Error: ` marker.
#[test]
fn an_error_tool_result_marks_its_text() {
    let fixture = fixture_for("gpt-4o");
    let mut request = fixture.tool_result_request();
    let answer = Message {
        role: Role::Tool,
        parts: vec![Part::ToolResult {
            tool_use_id: fixture.tool.call_id.clone(),
            content: "the ledger is locked".to_owned(),
            is_error: true,
        }],
    };
    let last = request
        .messages
        .last_mut()
        .unwrap_or_else(|| panic!("has messages"));
    *last = answer;
    let provider = provider_nowhere(
        &generic_profile(),
        "https://toolresult-error.test",
        "gpt-4o",
    );
    let outbound = outbound_for(&provider, &request, false);
    let body: serde_json::Value = serde_json::from_str(&outbound.body)
        .unwrap_or_else(|error| panic!("the body is JSON: {error}"));
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let tool_message = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .cloned()
        .unwrap_or_else(|| panic!("the tool message rides"));
    assert_eq!(tool_message["content"], "Error: the ledger is locked");
}

/// A multi-part tool message (text plus a result — an unusual but legal shape) maps
/// through the plain content form, not the `tool` role.
#[test]
fn a_mixed_message_maps_through_the_plain_form() {
    let message = Message {
        role: Role::Tool,
        parts: vec![
            Part::Text {
                text: "context before".to_owned(),
            },
            Part::ToolResult {
                tool_use_id: "call_1".to_owned(),
                content: "5".to_owned(),
                is_error: false,
            },
        ],
    };
    let request = Request::new("gpt-4o", "cf-mixed").with_message(message);
    let provider = provider_nowhere(&generic_profile(), "https://mixed.test", "gpt-4o");
    let outbound = outbound_for(&provider, &request, false);
    let body: serde_json::Value = serde_json::from_str(&outbound.body)
        .unwrap_or_else(|error| panic!("the body is JSON: {error}"));
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let sent = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .cloned()
        .unwrap_or_else(|| panic!("the message rides"));
    assert_eq!(
        sent["content"], "context before\n5",
        "the plain form's text: the text part and the ToolResult content joined"
    );
    assert!(
        sent.get("tool_call_id").is_none(),
        "a multi-part tool message carries no id"
    );
}

/// The add tool's declaration serializes to the documented `function` shape in the
/// outbound body.
#[test]
fn the_add_tool_spec_serializes_to_the_documented_shape() {
    let provider = provider_nowhere(&generic_profile(), "https://spec.test", "gpt-4o");
    let outbound = outbound_for(
        &provider,
        &Request::new("gpt-4o", "cf-spec")
            .with_message(Message::user("Add 2 and 3 with the add tool."))
            .with_tool(add_tool()),
        false,
    );
    let body: serde_json::Value = serde_json::from_str(&outbound.body)
        .unwrap_or_else(|error| panic!("the body is JSON: {error}"));
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "add");
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["required"],
        serde_json::json!(["a", "b"])
    );
}

/// The streamed plain shape of the generic profile (no `stream_options` in the request)
/// still reads the final chunk's usage — the fixture records it as a separate chunk.
#[tokio::test]
async fn the_generic_stream_reads_usage_without_the_option() {
    let bench = &bench_by_name("generic");
    let fixture = fixture_for(bench.model);
    let provider = provider_at("https://generic-stream.test", bench);
    let events = common::events_from(&provider, fixture.text_request(), "the generic stream").await;
    let assembled = must_assemble(&events, "the generic events");
    assert_eq!(
        assembled.usage, fixture.text.usage,
        "the final chunk's usage rides"
    );
}
