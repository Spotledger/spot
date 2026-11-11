//! The conformance suite (and fixture-specific checks) running against the real
//! [`AnthropicProvider`] on recorded exchanges — no network, no real key.
//!
//! One design note: the replaying transport matches a recording by method and URL, and the
//! provider sends one endpoint — `{base}/v1/messages`. So each recorded exchange lives at
//! its own base URL, and each conformance scenario runs against the provider built for the
//! base its exchange was recorded under. [`suite_on`] runs the whole suite once per base
//! and filters only the *expected* misses (the foreign scenarios fail with `llm.protocol`
//! naming the unrecorded URL); any real divergence stays in the list and fails the test.

use std::sync::Arc;

use spot_llm::conformance::{Failure, Fixture};
use spot_llm::message::Part;
use spot_llm::provider::Provider as _;
use spot_llm::response::{Response, StopReason};
use spot_llm::transport::HttpMethod;
use spot_llm::transport::mock::{MockTransport, RecordedExchange};
use spot_llm_anthropic::test_hooks::{
    events_of_for_test, outbound_for, provider_base_url, temperature_text_for,
};
use spot_llm_anthropic::{AnthropicOptions, AnthropicProvider, CacheMarker, ModelInfo};

/// The fixture conformance oracle, shared with the recordings.
fn fixture() -> Fixture {
    let mut sample = Fixture::sample();
    sample.model = "claude-sonnet-4-5".to_owned();
    sample
}

/// The model catalogue the provider is built with.
fn models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "claude-sonnet-4-5".to_owned(),
            context_window: 200_000,
            max_output: 64,
            vision: true,
        },
        ModelInfo {
            id: "claude-opus-4-1".to_owned(),
            context_window: 200_000,
            max_output: 32_000,
            vision: true,
        },
    ]
}

/// Builds the provider over the replaying transport loaded from `file`, at the base URL the
/// tested exchange was recorded under. Used by the scenario tests, where one base URL holds
/// one exchange.
fn provider_at(base_url: &str, file: &str) -> AnthropicProvider {
    let path = format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
    let transport = MockTransport::load_file(&path)
        .unwrap_or_else(|error| panic!("fixture {file} is a broken recording: {error}"));
    AnthropicProvider::new(
        base_url,
        spot_llm::ApiKey::new("test-key-not-real"),
        Arc::new(transport),
        models(),
    )
}

/// The base URL whose recorded exchange answers the streaming-tool scenario.
const BASE_URL_STREAM_TOOL: &str = "https://stream-tool.test";

/// The base URL whose recorded exchange answers the rate-limit scenario (with retry-after).
const BASE_URL_RATE_LIMIT: &str = "https://rate-limit.test";

/// The base URL whose recorded exchange answers the auth-failure scenario.
const BASE_URL_AUTH: &str = "https://auth-error.test";

/// The base URL whose recorded exchange answers the tool-result scenario.
const BASE_URL_TOOL_RESULT: &str = "https://tool-result.test";

/// Picks the recorded exchange by the request's `request-id` header and `stream` body flag —
/// the two discriminating parts of the provider's outbound request.
struct RoutingTransport {
    exchanges: Vec<RecordedExchange>,
}

impl RoutingTransport {
    fn pick(&self, request: &spot_llm::transport::HttpRequest) -> Option<&RecordedExchange> {
        let request_id = request
            .headers
            .iter()
            .find(|(name, _)| name == "request-id")
            .map(|(_, value)| value.as_str());
        let body: serde_json::Value = serde_json::from_str(&request.body).ok()?;
        let streaming = body
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        self.exchanges.iter().find(|exchange| {
            let recorded_id = exchange
                .request
                .headers
                .iter()
                .find(|(name, _)| name == "request-id")
                .map(|(_, value)| value.as_str());
            let recorded_body: Option<serde_json::Value> =
                serde_json::from_str(&exchange.request.body).ok();
            let recorded_stream = recorded_body
                .as_ref()
                .and_then(|body| body.get("stream"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            recorded_id == request_id && recorded_stream == streaming
        })
    }
}

impl spot_llm::transport::HttpTransport for RoutingTransport {
    fn send<'a>(
        &'a self,
        request: spot_llm::transport::HttpRequest,
    ) -> spot_llm::provider::BoxFuture<
        'a,
        Result<spot_llm::transport::HttpResponse, spot_llm::transport::TransportError>,
    > {
        Box::pin(async move {
            match self.pick(&request) {
                Some(exchange) => match &exchange.response {
                    spot_llm::transport::mock::RecordedResponse::Complete {
                        status,
                        headers,
                        body,
                    } => Ok(spot_llm::transport::HttpResponse {
                        status: *status,
                        headers: headers.clone(),
                        body: body.clone(),
                    }),
                    spot_llm::transport::mock::RecordedResponse::Stream {
                        status,
                        headers,
                        chunks,
                    } => Ok(spot_llm::transport::HttpResponse {
                        status: *status,
                        headers: headers.clone(),
                        body: chunks.concat(),
                    }),
                },
                None => Err(spot_llm::transport::TransportError::Protocol(format!(
                    "no recorded exchange for {} {}",
                    request.method.as_str(),
                    request.url
                ))),
            }
        })
    }

    fn send_stream<'a>(
        &'a self,
        request: spot_llm::transport::HttpRequest,
    ) -> spot_llm::provider::BoxFuture<
        'a,
        Result<spot_llm::transport::ByteStream, spot_llm::transport::TransportError>,
    > {
        Box::pin(async move {
            let items: Vec<Result<Vec<u8>, spot_llm::transport::TransportError>> = match self
                .pick(&request)
            {
                Some(exchange) => match &exchange.response {
                    spot_llm::transport::mock::RecordedResponse::Complete { body, .. } => {
                        vec![Ok(body.clone().into_bytes())]
                    }
                    spot_llm::transport::mock::RecordedResponse::Stream { chunks, .. } => chunks
                        .iter()
                        .map(|chunk| Ok(chunk.clone().into_bytes()))
                        .collect(),
                },
                None => {
                    return Err(spot_llm::transport::TransportError::Protocol(format!(
                        "no recorded exchange for {} {}",
                        request.method.as_str(),
                        request.url
                    )));
                }
            };
            Ok(Box::pin(spot_llm::stream::IterStream::new(items))
                as spot_llm::transport::ByteStream)
        })
    }
}

/// Runs the whole suite against the routed provider and keeps only real failures.
async fn suite_on(file: &str) -> Vec<Failure> {
    let fixture = fixture();
    let path = format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
    let recording = MockTransport::load_file(&path)
        .unwrap_or_else(|error| panic!("fixture {file} is a broken recording: {error}"));
    let provider = AnthropicProvider::new(
        "https://api.anthropic.com",
        spot_llm::ApiKey::new("test-key-not-real"),
        Arc::new(RoutingTransport {
            exchanges: recording.exchanges().to_vec(),
        }),
        models(),
    );
    spot_llm::conformance::run(&provider, &fixture).await
}

/// The conformance suite is green on the recorded exchanges: text, stream-equals-complete,
/// the tool round trip, the rate-limit error on both paths, the serde and capability checks.
#[tokio::test]
async fn conformance_runs_green_on_the_recorded_exchanges() {
    let failures = suite_on("recorded.json").await;
    assert!(
        failures.is_empty(),
        "conformance failures on the recordings: {failures:?}"
    );
}

/// Streaming tool use: the SSE tool events bracket the same call `complete` returns, with
/// the oracle's usage attached.
#[tokio::test]
async fn streaming_tool_use_matches_the_buffered_reply() {
    let fixture = fixture();
    let provider = provider_at(BASE_URL_STREAM_TOOL, "recorded.json");
    let mut stream = match provider.stream(fixture.tool_request()).await {
        Ok(stream) => stream,
        Err(error) => panic!("the tool stream must open: {error}"),
    };
    let items = spot_llm::stream::drain(&mut stream).await;
    let errors: Vec<String> = items
        .iter()
        .filter_map(|item| item.as_ref().err().map(|error| error.to_string()))
        .collect();
    assert!(
        errors.is_empty(),
        "the tool stream must not fail: {errors:?}"
    );
    let events: Vec<spot_llm::StreamEvent> =
        items.into_iter().filter_map(|item| item.ok()).collect();

    let assembled = spot_llm::stream::assemble(&events)
        .unwrap_or_else(|error| panic!("the tool events must assemble: {error}"));
    assert_eq!(assembled.stop_reason, StopReason::ToolUse);
    let uses = assembled.message.tool_uses();
    assert_eq!(uses.len(), 1, "one streamed tool call: {uses:?}");
    assert_eq!(uses[0].id, fixture.tool.call_id);
    assert_eq!(uses[0].name, fixture.tool.call_name);
    assert_eq!(uses[0].arguments, fixture.tool.call_arguments);
    assert_eq!(assembled.usage, fixture.tool.usage);
}

/// Every documented error family maps from its recorded exchange, one base per scenario.
#[tokio::test]
async fn each_recorded_error_maps_to_its_code() {
    for (detail, expected_code, expected_retryable, base) in [
        (
            "authentication_error recorded",
            "llm.auth",
            false,
            BASE_URL_AUTH,
        ),
        (
            "rate_limit_error recorded",
            "llm.rate_limited",
            true,
            BASE_URL_RATE_LIMIT,
        ),
        (
            "overloaded_error recorded",
            "llm.overloaded",
            true,
            "https://overloaded.test",
        ),
        (
            "invalid_request_error recorded",
            "llm.bad_request",
            false,
            "https://bad-request.test",
        ),
        (
            "api_error recorded",
            "llm.overloaded",
            true,
            "https://api-error.test",
        ),
        (
            "not_found_error recorded",
            "llm.unavailable",
            true,
            "https://not-found.test",
        ),
    ] {
        let provider = provider_at(base, "errors.json");
        let error = match provider.complete(fixture().error_request()).await {
            Ok(response) => panic!("{detail} must fail, not return {:?}", response.text()),
            Err(error) => error,
        };
        assert_eq!(error.code(), expected_code, "{detail}: {error}");
        assert_eq!(error.retryable(), expected_retryable, "{detail}: {error}");
    }
}

/// The rate-limited recording carries `retry-after: 2`; the mapped error carries it.
#[tokio::test]
async fn retry_after_is_honoured() {
    let provider = provider_at(BASE_URL_RATE_LIMIT, "errors.json");
    let error = match provider.complete(fixture().error_request()).await {
        Ok(_) => panic!("the rate-limited exchange must fail"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "llm.rate_limited", "{error}");
    assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(2)));
}

/// The context-window overflow recording maps to its own code, not plain bad_request.
#[tokio::test]
async fn context_overflow_maps_to_context_too_long() {
    let provider = provider_at("https://context-long.test", "errors.json");
    let error = match provider.complete(fixture().error_request()).await {
        Ok(_) => panic!("the overflow exchange must fail"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "llm.context_too_long", "{error}");
    assert!(!error.retryable(), "{error}");
}

/// A stream cut before its `message_stop` is `llm.protocol` — the brief's truncated-stream
/// case, both through `stream` and directly against the parser.
#[tokio::test]
async fn a_truncated_stream_is_protocol() {
    let fixture = fixture();
    let provider = provider_at("https://truncated.test", "errors.json");
    let mut stream = match provider.stream(fixture.text_request()).await {
        Ok(stream) => stream,
        Err(error) => panic!("the truncated stream must open: {error}"),
    };
    let items = spot_llm::stream::drain(&mut stream).await;
    let failed = items
        .into_iter()
        .filter_map(|item| item.err())
        .next()
        .unwrap_or_else(|| panic!("the truncated stream must fail somewhere"));
    assert_eq!(failed.code(), "llm.protocol", "{failed}");

    // And directly against the parser: a body missing message_stop.
    let body = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n";
    let error = match spot_llm_anthropic::sse::parse(body) {
        Ok(_) => panic!("a stream without message_stop must fail to parse"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "llm.protocol", "{error}");
    assert!(
        failed.to_string().contains("message_stop"),
        "the failure names the missing terminal: {failed}"
    );
}

/// A stream-level `error` event folds the stream with the provider's message; unknown event
/// kinds, unknown block kinds and keep-alive comments are tolerated and skipped.
#[tokio::test]
async fn stream_error_event_and_unknown_events() {
    let fixture = fixture();
    let error_provider = provider_at("https://stream-error.test", "errors.json");
    let mut stream = match error_provider.stream(fixture.text_request()).await {
        Ok(stream) => stream,
        Err(error) => panic!("the error-event stream must open: {error}"),
    };
    let items = spot_llm::stream::drain(&mut stream).await;
    let failed = items
        .into_iter()
        .filter_map(|item| item.err())
        .next()
        .unwrap_or_else(|| panic!("the error-event stream must fail"));
    assert_eq!(failed.code(), "llm.protocol", "{failed}");
    assert!(
        failed.to_string().contains("overloaded"),
        "the provider's own message rides the detail: {failed}"
    );

    // Unknown events, comments and unknown block kinds still stream fine.
    let unknown_provider = provider_at("https://unknown-events.test", "errors.json");
    let mut unknown_stream = match unknown_provider.stream(fixture.text_request()).await {
        Ok(stream) => stream,
        Err(error) => panic!("the unknown-event stream must open: {error}"),
    };
    let items = spot_llm::stream::drain(&mut unknown_stream).await;
    let events: Vec<spot_llm::StreamEvent> = items
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("the unknown-event stream must not fail: {error}"));
    let assembled = spot_llm::stream::assemble(&events)
        .unwrap_or_else(|error| panic!("the events must assemble: {error}"));
    assert_eq!(assembled.text(), "5");
    assert_eq!(assembled.stop_reason, StopReason::Stop);
    assert_eq!(assembled.usage, fixture.text.usage);
}

/// The no-leak rule on the error path: the user text's secret token and the key value never
/// surface in the mapped error.
#[tokio::test]
async fn errors_never_leak_the_token_or_the_key() {
    let fixture = fixture();
    let provider = provider_at(BASE_URL_RATE_LIMIT, "errors.json");
    let error = match provider.complete(fixture.error_request()).await {
        Ok(response) => panic!("must fail, not {:?}", response.text()),
        Err(error) => error,
    };
    assert!(
        !error.to_string().contains(&fixture.error.must_not_leak),
        "the user text leaked into {error}"
    );
    assert!(
        !error.to_string().contains("test-key-not-real"),
        "the key leaked into {error}"
    );
}

/// The provider's request half: headers, required max_tokens, temperature as a formatted
/// decimal number, tool mapping, cache markers, and the streaming flag — asserted on the
/// outbound body before any transport is involved.
#[test]
fn the_outbound_body_maps_everything() {
    let provider = AnthropicProvider::new(
        "https://api.anthropic.com",
        spot_llm::ApiKey::new("unused-here"),
        Arc::new(MockTransport::new(Vec::<RecordedExchange>::new())),
        models(),
    );
    let request = fixture()
        .tool_request()
        .with_temperature_permille(700)
        .with_stop_sequence("END");
    let outbound = outbound_for(
        &provider,
        &request,
        &AnthropicOptions {
            cache: CacheMarker::SystemAndLastTool,
            max_tokens: None,
        },
    );

    assert_eq!(outbound.method, HttpMethod::Post);
    assert_eq!(outbound.url, "https://api.anthropic.com/v1/messages");
    let headers = &outbound.headers;
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "x-api-key" && value == "unused-here"),
        "the key rides x-api-key"
    );
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "anthropic-version" && value == "2023-06-01"),
        "the version header rides"
    );
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "request-id" && value == "cf-tool"),
        "the request id rides as a header"
    );

    let body: serde_json::Value = serde_json::from_str(&outbound.body)
        .unwrap_or_else(|error| panic!("the outbound body is JSON: {error}"));
    assert_eq!(body["model"], "claude-sonnet-4-5");
    assert_eq!(body["max_tokens"], 64, "max_tokens defaults from ModelInfo");
    assert_eq!(body["stop_sequences"], serde_json::json!(["END"]));
    // Temperature: the formatted decimal, embedded as a bare JSON number, never a float.
    let raw = body["temperature"].to_string();
    assert_eq!(raw, "0.7", "the permille formatted itself: {raw}");
    assert_eq!(
        body["system"],
        serde_json::json!([{
            "type": "text",
            "text": "You are Spotledger's conformance probe.",
            "cache_control": {"type": "ephemeral"}
        }]),
        "the system block is marked when caching asks"
    );
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(
        body["tools"][0]["cache_control"],
        serde_json::json!({"type": "ephemeral"}),
        "the last tool is marked when caching asks"
    );
    assert_eq!(body["tools"][0]["name"], "add");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(
        body.get("stream"),
        None,
        "a buffered call carries no stream flag"
    );
}

/// The tool-result leg maps to the documented tool_result block on the user role.
#[test]
fn the_tool_result_maps_to_the_documented_block() {
    let provider = AnthropicProvider::new(
        "https://api.anthropic.com",
        spot_llm::ApiKey::new("unused-here"),
        Arc::new(MockTransport::new(Vec::<RecordedExchange>::new())),
        models(),
    );
    let outbound = outbound_for(
        &provider,
        &fixture().tool_result_request(),
        &AnthropicOptions::default(),
    );
    let body: serde_json::Value = serde_json::from_str(&outbound.body).unwrap_or_default();
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    assert_eq!(messages.len(), 3, "user + assistant tool use + tool result");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"][0]["type"], "tool_use");
    assert_eq!(messages[1]["content"][0]["id"], "call_1");
    assert_eq!(
        messages[2]["role"], "user",
        "tool results ride the user role"
    );
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_1");
    assert_eq!(messages[2]["content"][0]["content"], "5");
}

/// No cache markers when none were asked; temperature formats the other decimal shapes; the
/// caller's max_tokens override wins over the model cap.
#[test]
fn cache_markers_are_opt_in_and_temperature_formats() {
    let provider = AnthropicProvider::new(
        "https://api.anthropic.com",
        spot_llm::ApiKey::new("unused-here"),
        Arc::new(MockTransport::new(Vec::<RecordedExchange>::new())),
        models(),
    );
    let outbound = outbound_for(
        &provider,
        &fixture().tool_request(),
        &AnthropicOptions::default(),
    );
    let body: serde_json::Value = serde_json::from_str(&outbound.body).unwrap_or_default();
    assert!(body["system"][0].get("cache_control").is_none());
    assert!(body["tools"][0].get("cache_control").is_none());

    for (permille, expected) in [
        (0u16, "0"),
        (50, "0.5"),
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

    let outgoing = outbound_for(
        &provider,
        &fixture().text_request(),
        &AnthropicOptions {
            max_tokens: Some(32),
            cache: CacheMarker::None,
        },
    );
    let body: serde_json::Value = serde_json::from_str(&outgoing.body).unwrap_or_default();
    assert_eq!(body["max_tokens"], 32);
}

/// A request whose model is not in the catalogue fails with llm.bad_request and never
/// reaches the transport; the detail names the model, not the body.
#[tokio::test]
async fn an_unknown_model_is_bad_request() {
    let transport = Arc::new(MockTransport::new(Vec::<RecordedExchange>::new()));
    let provider = AnthropicProvider::new(
        "https://api.anthropic.com",
        spot_llm::ApiKey::new("key"),
        transport,
        models(),
    );
    let mut request = fixture().text_request();
    request.model = "model-xyz".to_owned();
    let error = match provider.complete(request).await {
        Ok(_) => panic!("an unknown model must fail"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "llm.bad_request", "{error}");
    assert!(
        error.to_string().contains("model-xyz"),
        "the detail names the model, not the body: {error}"
    );
}

/// An empty base URL falls back to the documented default; known and unknown models report
/// positive capabilities floors.
#[test]
fn defaults_and_redactions() {
    let provider = AnthropicProvider::new(
        "",
        spot_llm::ApiKey::new("k"),
        Arc::new(MockTransport::new(Vec::<RecordedExchange>::new())),
        models(),
    );
    assert_eq!(provider_base_url(&provider), "https://api.anthropic.com");
    assert_eq!(provider.id(), "anthropic");

    let capabilities = provider.capabilities("claude-sonnet-4-5");
    assert_eq!(capabilities.context_window, 200_000);
    assert_eq!(capabilities.max_output, 64);
    assert!(capabilities.vision);
    assert!(capabilities.tools && capabilities.streaming && capabilities.prompt_caching);
    // A model outside the catalogue reports a positive floor (the caps scenario's need).
    let unknown = provider.capabilities("model-xyz");
    assert!(unknown.context_window > 0 && unknown.max_output > 0);
}

/// The buffered tool-result leg replays the follow-up reply end to end.
#[tokio::test]
async fn the_tool_result_leg_completes() {
    let fixture = fixture();
    let provider = provider_at(BASE_URL_TOOL_RESULT, "recorded.json");
    let response = match provider.complete(fixture.tool_result_request()).await {
        Ok(response) => response,
        Err(error) => panic!("leg two must complete: {error}"),
    };
    let expected = Response::new(
        spot_llm::Message::assistant(fixture.tool.result_reply.clone()),
        StopReason::Stop,
        fixture.tool.usage,
    );
    assert_eq!(response, expected);
}

/// An exchange missing from the recording fails typed (`llm.protocol`, not a panic) — the
/// transport's framing failure mapped, never body text; a tool-result part opens no stream
/// bracket.
#[tokio::test]
async fn an_unrecorded_exchange_is_protocol() {
    let fixture = fixture();
    // errors.json has no exchange at this base URL; drive one.
    let provider = provider_at("https://no-such-base.test", "errors.json");
    let error = match provider.complete(fixture.error_request()).await {
        Ok(response) => panic!("an unrecorded exchange must fail: {response:?}"),
        Err(error) => error,
    };
    // The mock's "no recorded exchange" is a transport-level protocol failure; the
    // provider maps every transport Protocol to llm.protocol per the documented mapping.
    assert_eq!(error.code(), "llm.protocol", "{error}");
    assert!(!error.retryable(), "{error}");

    let response = Response::new(
        spot_llm::Message {
            role: spot_llm::Role::Assistant,
            parts: vec![
                Part::Text {
                    text: "hi".to_owned(),
                },
                Part::ToolResult {
                    tool_use_id: "t".to_owned(),
                    content: "c".to_owned(),
                    is_error: false,
                },
            ],
        },
        StopReason::Stop,
        fixture.text.usage,
    );
    let events = events_of_for_test(&response);
    let assembled = spot_llm::stream::assemble(&events)
        .unwrap_or_else(|error| panic!("tool-result-free events assemble: {error}"));
    assert_eq!(assembled.text(), "hi");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, spot_llm::StreamEvent::ToolUseEnd { .. })),
        "a tool-result part opens no bracket: {events:?}"
    );
}
