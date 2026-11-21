//! The conformance suite running against the real [`OpenAiProvider`] on recorded
//! exchanges, plus the scenario-level response and error tests — no network, no real key.

mod common;

use std::sync::Arc;

use common::{
    RoutingTransport, base_stream, base_stream_tool, base_text, base_tool, base_tool_result,
    bench_by_name, benches, build_t, expected_text_response, fixture_for, key_for, must_assemble,
    provider_at, transport_of,
};
use spot_llm::provider::Provider as _;
use spot_llm::response::StopReason;
use spot_llm::stream::drain;
use spot_llm::transport::mock::{MockTransport, RecordedExchange};
use spot_llm_openai::Profile;

/// The conformance suite is green per bench on the recorded exchanges: text,
/// stream-equals-complete, the tool round trip, the recorded error on both paths, the
/// serde and capability checks. The routing transport keys the recordings by the
/// `request-id` header and the `stream` body flag, so every scenario finds its exchange
/// at one endpoint — no miss filtering, any real divergence fails.
#[tokio::test]
async fn conformance_runs_green_per_profile() {
    for bench in &benches() {
        let fixture = fixture_for(bench.model);
        // The truncated-arguments exchange answers a scenario test, not the suite; it
        // carries the same request id as the suite's tool leg and would shadow it.
        let provider = build_t(
            &bench.profile,
            "https://conformance.test",
            key_for(&bench.profile),
            Arc::new(RoutingTransport::over_bench(bench)),
            bench.model,
        );
        let failures: Vec<spot_llm::conformance::Failure> =
            spot_llm::conformance::run(&provider, &fixture).await;
        assert!(
            failures.is_empty(),
            "conformance failures on {}: {:?}",
            bench.name,
            failures
        );
    }
}

/// The plain-completion leg maps the recorded reply to the oracle response, per profile.
#[tokio::test]
async fn the_plain_reply_maps_to_the_documented_parts() {
    for bench in &benches() {
        let fixture = fixture_for(bench.model);
        let provider = provider_at(base_text(bench), bench);
        let response = match provider.complete(fixture.text_request()).await {
            Ok(response) => response,
            Err(error) => panic!("{} text leg must complete: {error}", bench.name),
        };
        assert_eq!(response, expected_text_response(&fixture), "{}", bench.name);
    }
}

/// The streaming leg of the plain reply assembles to the same response — per profile,
/// proving indexed assembly (no tool calls here) and usage from the final chunk.
#[tokio::test]
async fn the_streamed_plain_reply_assembles_to_the_buffered_one() {
    for bench in &benches() {
        let fixture = fixture_for(bench.model);
        let provider = provider_at(base_stream(bench), bench);
        let events =
            common::events_from(&provider, fixture.text_request(), "the plain stream").await;
        let assembled = must_assemble(&events, "the plain events");
        assert_eq!(
            assembled,
            expected_text_response(&fixture),
            "{}",
            bench.name
        );
    }
}

/// The tool-use round trip: leg one answers with the documented tool_calls shape (the
/// model's arguments arriving as a JSON string), mapped onto a ToolUse part with a
/// ToolUse stop; the tool result rides the `tool` role quoting the call id; leg two
/// replies with the follow-up text.
#[tokio::test]
async fn the_tool_round_trip_maps_both_legs() {
    for bench in &benches() {
        let fixture = fixture_for(bench.model);
        let provider = provider_at(base_tool(bench), bench);
        let response = match provider.complete(fixture.tool_request()).await {
            Ok(response) => response,
            Err(error) => panic!("{} tool leg must complete: {error}", bench.name),
        };
        let uses = response.message.tool_uses();
        assert_eq!(uses.len(), 1, "{}", bench.name);
        assert_eq!(uses[0].id, fixture.tool.call_id, "{}", bench.name);
        assert_eq!(uses[0].name, fixture.tool.call_name, "{}", bench.name);
        assert_eq!(
            uses[0].arguments, fixture.tool.call_arguments,
            "{}",
            bench.name
        );
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.usage, fixture.tool.usage);

        let provider_two = provider_at(base_tool_result(bench), bench);
        let second = match provider_two.complete(fixture.tool_result_request()).await {
            Ok(second) => second,
            Err(error) => panic!("{} tool-result leg must complete: {error}", bench.name),
        };
        assert_eq!(second.text(), fixture.tool.result_reply, "{}", bench.name);
        assert_eq!(second.stop_reason, StopReason::Stop);
        assert_eq!(second.usage, fixture.tool.usage);
    }
}

/// Streaming tool calls: the indexed deltas assemble into the same call the buffered
/// reply carries (id, name, arguments, usage), per profile.
#[tokio::test]
async fn streaming_tool_calls_assemble_from_indexed_deltas() {
    for bench in &benches() {
        let fixture = fixture_for(bench.model);
        let provider = provider_at(base_stream_tool(bench), bench);
        let events =
            common::events_from(&provider, fixture.tool_request(), "the tool stream").await;
        let assembled = must_assemble(&events, "the tool events");
        assert_eq!(assembled.stop_reason, StopReason::ToolUse, "{}", bench.name);
        let uses = assembled.message.tool_uses();
        assert_eq!(uses.len(), 1, "{}: {uses:?}", bench.name);
        assert_eq!(uses[0].id, fixture.tool.call_id, "{}", bench.name);
        assert_eq!(uses[0].name, fixture.tool.call_name, "{}", bench.name);
        assert_eq!(
            uses[0].arguments, fixture.tool.call_arguments,
            "{}",
            bench.name
        );
        assert_eq!(assembled.usage, fixture.tool.usage, "{}", bench.name);
    }
}

/// The recorded rate-limit error maps with its `retry-after` — buffered, and on the
/// stream-shaped request answered with the same status buffer.
#[tokio::test]
async fn the_rate_limited_exchange_maps_with_retry_after() {
    let bench = &bench_by_name("generic");
    let fixture = fixture_for(bench.model);
    for (url, path) in [
        ("https://generic-rate-limit.test", "complete"),
        ("https://generic-stream-rate-limit.test", "stream"),
    ] {
        let error = match provider_at(url, bench)
            .complete(fixture.error_request())
            .await
        {
            Ok(response) => panic!("the {path} rate limit must fail, not {:?}", response.text()),
            Err(error) => error,
        };
        assert_eq!(error.code(), "llm.rate_limited", "{path}: {error}");
        assert!(error.retryable(), "{path}");
        assert_eq!(
            error.retry_after(),
            Some(std::time::Duration::from_secs(2)),
            "{path}"
        );
    }
    // OpenRouter's recorded rate limit, with the profile's streaming shape:
    let openrouter = &bench_by_name("openrouter");
    let openrouter_fixture = fixture_for(openrouter.model);
    for url in ["https://or-error.test", "https://or-error-stream.test"] {
        let error = match provider_at(url, openrouter)
            .complete(openrouter_fixture.error_request())
            .await
        {
            Ok(response) => panic!(
                "the rate limit at {url} must fail, not {:?}",
                response.text()
            ),
            Err(error) => error,
        };
        assert_eq!(error.code(), "llm.rate_limited", "{url}: {error}");
        assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(2)));
    }
}

/// The rate-limited shape holds on the streaming path too: the status error surfaces
/// through the stream with the fixture's code, its retry delay, and nothing of the
/// request in it.
#[tokio::test]
async fn the_streamed_rate_limited_exchange_maps_with_retry_after() {
    let bench = &bench_by_name("openrouter");
    let fixture = fixture_for(bench.model);
    let provider = provider_at("https://or-error-stream.test", bench);
    let mut stream = match provider.stream(fixture.error_request()).await {
        Ok(stream) => stream,
        Err(error) => panic!("the failed stream must open: {error}"),
    };
    let items = drain(&mut stream).await;
    let error = items
        .into_iter()
        .filter_map(Result::err)
        .next()
        .unwrap_or_else(|| panic!("the failed stream must fail somewhere"));
    assert_eq!(error.code(), "llm.rate_limited", "{error}");
    assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(2)));
    assert!(
        !error.to_string().contains(&fixture.error.must_not_leak),
        "the request text leaked into {error}"
    );
}

/// Every recorded endpoint error maps to its `llm.*` code: auth, overload, bad request,
/// context overflow, and the same shape under a 200 status.
#[tokio::test]
async fn each_recorded_error_maps_to_its_code() {
    let bench = &bench_by_name("generic");
    let fixture = fixture_for(bench.model);
    for (url, expected_code, expected_retryable) in [
        ("https://auth-error.test", "llm.auth", false),
        ("https://overloaded.test", "llm.overloaded", true),
        ("https://bad-request.test", "llm.bad_request", false),
        ("https://context-long.test", "llm.context_too_long", false),
        ("https://body-200-error.test", "llm.overloaded", true),
    ] {
        let error = match provider_at(url, bench)
            .complete(fixture.error_request())
            .await
        {
            Ok(response) => panic!("the exchange at {url} must fail, not {:?}", response.text()),
            Err(error) => error,
        };
        assert_eq!(error.code(), expected_code, "{url}: {error}");
        assert_eq!(error.retryable(), expected_retryable, "{url}: {error}");
    }
}

/// The no-leak rule: the user text's secret token and the key value never surface in any
/// mapped error's text.
#[tokio::test]
async fn errors_never_leak_the_token_or_the_key() {
    let bench = &bench_by_name("generic");
    let fixture = fixture_for(bench.model);
    let error = match provider_at("https://generic-rate-limit.test", bench)
        .complete(fixture.error_request())
        .await
    {
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

/// A streamed error object fails the stream with `llm.protocol` naming the provider's
/// own message — Ollama's and vLLM's failure style mid-stream.
#[tokio::test]
async fn a_streamed_error_object_is_protocol() {
    let bench = &bench_by_name("openrouter");
    let fixture = fixture_for(bench.model);
    let provider = provider_at("https://stream-error.test", bench);
    let mut stream = match provider.stream(fixture.text_request()).await {
        Ok(stream) => stream,
        Err(error) => panic!("the error stream must open: {error}"),
    };
    let items = drain(&mut stream).await;
    let failed = items
        .into_iter()
        .filter_map(Result::err)
        .next()
        .unwrap_or_else(|| panic!("the error stream must fail somewhere"));
    assert_eq!(failed.code(), "llm.protocol", "{failed}");
    assert!(
        failed.to_string().contains("overloaded"),
        "the provider's own message rides the detail: {failed}"
    );
}

/// A non-bearer provider (the `None`-auth profile) sends no authorization header; a
/// missing key under a bearer profile fails fast with `llm.auth` before any transport
/// call; the profile's extra headers ride every request.
#[tokio::test]
async fn auth_headers_follow_the_profile() {
    // Ollama: no auth header, no key.
    let bench = &bench_by_name("ollama");
    let fixture = fixture_for(bench.model);
    let provider = provider_at(base_text(bench), bench);
    let outbound = common::outbound_of(&provider, &fixture.text_request(), false);
    assert!(
        !outbound
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization")),
        "a None-auth profile sends no authorization header"
    );
    let response = match provider.complete(fixture.text_request()).await {
        Ok(response) => response,
        Err(error) => panic!("the keyless Ollama call must complete: {error}"),
    };
    assert_eq!(response.text(), fixture.text.reply);

    // Bearer with no key: llm.auth before the transport. The model check precedes the
    // auth check in the outbound build (the catalogue read happens first), so the
    // keyless assertion must use a request whose model IS in the catalogue.
    let keyless = build_t(
        &Profile::openrouter(),
        "https://no-key.test",
        None,
        Arc::new(MockTransport::new(Vec::<RecordedExchange>::new())),
        "openai/gpt-4o",
    );
    let openrouter_fixture = fixture_for("openai/gpt-4o");
    let error = match keyless.complete(openrouter_fixture.text_request()).await {
        Ok(response) => panic!("a keyless bearer call must fail: {response:?}"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "llm.auth", "{error}");

    // OpenRouter extras: referer and title ride as caller-supplied extra headers.
    let profile = Profile {
        extra_headers: vec![
            (
                "http-referer".to_owned(),
                "https://spotledger.example".to_owned(),
            ),
            ("x-title".to_owned(), "Spotledger".to_owned()),
        ],
        ..Profile::openrouter()
    };
    let routed = build_t(
        &profile,
        "https://openrouter.test",
        key_for(&profile),
        Arc::new(MockTransport::new(Vec::<RecordedExchange>::new())),
        "openai/gpt-4o",
    );
    let outbound = common::outbound_of(&routed, &openrouter_fixture.text_request(), false);
    assert!(
        outbound
            .headers
            .iter()
            .any(|(name, value)| name == "http-referer" && value == "https://spotledger.example"),
        "the referer rides"
    );
    assert!(
        outbound
            .headers
            .iter()
            .any(|(name, value)| name == "x-title" && value == "Spotledger"),
        "the title rides"
    );
}

/// A request declaring tools against a no-tools profile fails before the transport; an
/// unknown model fails with `llm.bad_request` naming the model.
#[tokio::test]
async fn unsupported_tools_and_unknown_models_fail_before_the_transport() {
    let no_tools = Profile {
        supports_tools: false,
        ..Profile::generic()
    };
    let fixture = fixture_for("gpt-4o");
    let provider = common::provider_nowhere(&no_tools, "https://no-tools.test", "gpt-4o");
    let error = match provider.complete(fixture.tool_request()).await {
        Ok(response) => panic!("tools must fail on a no-tools profile: {response:?}"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "llm.bad_request", "{error}");

    let provider = common::provider_nowhere(&Profile::generic(), "https://unknown.test", "gpt-4o");
    let mut request = fixture.text_request();
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

/// Malformed tool-call arguments: the strict profile still maps the well-formed recorded
/// reply, the lenient profile (Ollama) closes and parses the truncated recorded string,
/// and a strict profile over the same recording refuses it with `llm.protocol`.
#[tokio::test]
async fn malformed_arguments_parse_leniently_only_where_the_profile_says() {
    let ollama = &bench_by_name("ollama");
    let generic = &bench_by_name("generic");
    // Strict, well-formed: the generic profile completes against the recorded reply.
    let fixture = fixture_for(generic.model);
    let strict = provider_at("https://generic-tool.test", generic);
    let strict_response = match strict.complete(fixture.tool_request()).await {
        Ok(response) => response,
        Err(error) => panic!("the well-formed strict exchange must complete: {error}"),
    };
    assert_eq!(
        strict_response.message.tool_uses()[0].arguments,
        fixture.tool.call_arguments
    );

    // The Ollama truncated-arguments recording: parses under the lenient profile only.
    let lenient_fixture = fixture_for(ollama.model);
    let lenient = provider_at("https://ollama-truncated.test", ollama);
    let response = match lenient.complete(lenient_fixture.tool_request()).await {
        Ok(response) => response,
        Err(error) => panic!("the lenient profile must close the truncated string: {error}"),
    };
    let uses = response.message.tool_uses();
    assert_eq!(uses.len(), 1);
    assert_eq!(
        uses[0].arguments,
        serde_json::json!({"a": 2}),
        "the truncated object closed to the complete prefix"
    );

    // The same bytes under a strict profile: llm.protocol.
    let strict_ollama = Profile {
        tool_args_may_be_partial_json: false,
        ..Profile::ollama()
    };
    let refusing = build_t(
        &strict_ollama,
        "https://ollama-truncated.test",
        None,
        std::sync::Arc::new(transport_of(ollama)),
        ollama.model,
    );
    let error = match refusing.complete(lenient_fixture.tool_request()).await {
        Ok(response) => panic!("the strict profile must refuse: {response:?}"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "llm.protocol", "{error}");
}

/// The streamed tool-call argument fragments under the Ollama style assemble to the call
/// the fixture oracle names — including a fragment chunk cut mid-string.
#[tokio::test]
async fn streamed_partial_arguments_close_at_the_profiles_leniency() {
    let bench = &bench_by_name("ollama");
    let fixture = fixture_for(bench.model);
    let provider = provider_at(base_stream_tool(bench), bench);
    let events =
        common::events_from(&provider, fixture.tool_request(), "the ollama tool stream").await;
    let assembled = must_assemble(&events, "the ollama tool events");
    let uses = assembled.message.tool_uses();
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].arguments, fixture.tool.call_arguments, "{uses:?}");
    assert_eq!(assembled.stop_reason, StopReason::ToolUse);
}

/// A recorded exchange whose Sse body ends without `[DONE]` is `llm.protocol`.
#[tokio::test]
async fn a_stream_without_done_is_protocol() {
    // Driven directly against the parser: a full stream minus its terminator.
    let body = concat!(
        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"5\"},",
        "\"finish_reason\":\"stop\"}]}\n\n",
    );
    let error = match spot_llm_openai::sse::parse(body) {
        Ok(_) => panic!("a stream without [DONE] must fail to parse"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "llm.protocol", "{error}");
    assert!(
        error.to_string().contains("DONE"),
        "the failure names the missing terminator: {error}"
    );
}
